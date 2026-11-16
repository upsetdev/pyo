//! Sandbox templates on the servers. Pyo doesn't build them: it tells each
//! server that may use a template to prepare it from its image, and the
//! server pulls, unpacks, boots and snapshots it itself (pyo-sandbox
//! `templates`). Pyo reads how far each server got from its report.
//!
//! - On create, and in every runner round for a server that doesn't have
//!   one yet (a new server, or one that was down), Pyo sends
//!   `PrepareTemplate`. Pyo's `base` is the exception: a server makes it
//!   the first time a sandbox needs it there ([`super::firstuse`]).
//! - An image in Pyo's own registry is pulled over the tailnet from the
//!   registry door, with a registry token for the template's team that
//!   lasts an hour. Any other image is pulled from its registry as is.
//! - A template's state is rolled up from its servers: ready once one has
//!   it ready, building while one builds, failed once every server that
//!   could make it failed.
//! - A template Pyo no longer has is deleted from the servers that made
//!   it. One put on a server by hand is never touched.
//! - A template with a revision (Pyo's built-in `base`, see [`super::base`])
//!   is kept on the servers as `<id>.<revision>`. A new image is a new
//!   revision: servers make it beside the old one, new sandboxes move to it
//!   once it's ready there, and the old one is deleted once no sandbox on
//!   that server uses it (running and paused ones keep their disk).
//!
//! TODO(sandbox): templates from a repo or a Dockerfile, through Pyo's
//! builder. The builder runs as a deployment's build job today (deploy.rs
//! `build_job`), tied to a service. Once it can build an image on its own,
//! it pushes to `pyo/templates/<id>` and this pulls it like any image in
//! Pyo's registry.

use std::collections::HashSet;

use pyo_proto::sandbox::token::SigningKey;
use pyo_proto::sandbox::{
    DaemonCommand, ErrorCode, RegistryAuth, Runtime, ServerReport, TemplateReport, TemplateState,
};
use serde_json::Value;
use sqlx::PgPool;

use super::{daemon::Daemon, keys};
use crate::AppState;

/// A template as Pyo has it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Template {
    pub id: String,
    pub team_id: Option<String>,
    pub source: Value,
    pub revision: Option<String>,
}

impl Template {
    /// The name servers keep it under.
    pub fn server_name(&self) -> String {
        server_name(&self.id, self.revision.as_deref())
    }
}

/// The name servers keep a template under: its id, and its revision when
/// it has one.
pub fn server_name(id: &str, revision: Option<&str>) -> String {
    match revision {
        Some(r) => format!("{id}.{r}"),
        None => id.to_owned(),
    }
}

/// Whether `name`, as a server keeps it, is a revision of the template `id`
/// (the current one or an older one).
pub fn is_revision_of(name: &str, id: &str) -> bool {
    name == id
        || name
            .strip_prefix(id)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Revisions of template `id` a server has ready, other than `current`.
pub fn older_ready<'a>(
    reports: &'a [TemplateReport],
    id: &'a str,
    current: &'a str,
) -> impl Iterator<Item = &'a str> {
    reports
        .iter()
        .filter(move |r| {
            r.name != current && is_revision_of(&r.name, id) && r.state == TemplateState::Ready
        })
        .map(|r| r.name.as_str())
}

/// Whether a server may use a template: an installation's server takes
/// every template, a team's server its team's and the installation's.
pub fn eligible(server_team: Option<&str>, template_team: Option<&str>) -> bool {
    match (server_team, template_team) {
        (None, _) | (_, None) => true,
        (Some(s), Some(t)) => s == t,
    }
}

/// The repository path of an image in Pyo's registry, by any of the
/// addresses it's known by. None for any other registry.
pub fn in_pyos_registry<'a>(image: &'a str, doors: &[String]) -> Option<&'a str> {
    doors
        .iter()
        .filter(|d| !d.is_empty())
        .find_map(|d| image.strip_prefix(d.as_str())?.strip_prefix('/'))
}

/// Whether a server makes template `id` before a sandbox needs it: every
/// template but one made on first use ([`super::base`]), which only once
/// the server has some revision of it.
pub fn made_ahead(id: &str, reports: &[TemplateReport]) -> bool {
    !super::base::on_demand(id) || reports.iter().any(|r| is_revision_of(&r.name, id))
}

/// The command that has a server prepare `t`.
pub fn prepare_command(key: &SigningKey, t: &Template) -> DaemonCommand {
    let image = t.source["image"].as_str().unwrap_or_default().trim();
    let push = crate::deploy::registry_push();
    let doors: Vec<String> = [crate::registrydoor::address(), Some(push.clone())]
        .into_iter()
        .flatten()
        .collect();
    match (in_pyos_registry(image, &doors), push.is_empty()) {
        (Some(path), false) => DaemonCommand::PrepareTemplate {
            template: t.server_name(),
            // The door on the tailnet, as other machines pull from it.
            image: format!("{push}/{path}"),
            plain_http: true,
            auth: t.team_id.as_ref().map(|team| RegistryAuth {
                username: crate::registrydoor::SANDBOX_USER.into(),
                password: keys::registry_token(key, &t.id, team),
            }),
            max_mib: None,
        },
        _ => DaemonCommand::PrepareTemplate {
            template: t.server_name(),
            image: image.to_owned(),
            plain_http: false,
            auth: None,
            max_mib: None,
        },
    }
}

/// What to tell a server after its report: templates to prepare, and
/// templates it made that Pyo no longer has (or that aren't for it). Both
/// by the name the server keeps them under. An older revision stays until
/// the current one is ready there, so new sandboxes have one to start from.
pub fn plan(
    server_team: Option<&str>,
    report: &ServerReport,
    templates: &[Template],
) -> (Vec<String>, Vec<String>) {
    let mine: Vec<&Template> = templates
        .iter()
        .filter(|t| eligible(server_team, t.team_id.as_deref()))
        .collect();
    let names: Vec<String> = mine.iter().map(|t| t.server_name()).collect();
    let wanted: HashSet<&str> = names.iter().map(String::as_str).collect();
    // Templates whose current revision isn't ready here yet.
    let waiting: Vec<&str> = mine
        .iter()
        .zip(&names)
        .filter(|(_, n)| {
            !report
                .templates
                .iter()
                .any(|r| &r.name == *n && r.state == TemplateState::Ready)
        })
        .map(|(t, _)| t.id.as_str())
        .collect();
    let has: HashSet<&str> = report.templates.iter().map(|t| t.name.as_str()).collect();
    // Firecracker servers boot and snapshot a template. gVisor servers make
    // the same disk and start sandboxes cold from it. One made on first use
    // only once the server has used it: then a new revision too.
    let mut prepare: Vec<String> = if report.capability.runtime != Runtime::None {
        mine.iter()
            .zip(&names)
            .filter(|(_, n)| !has.contains(n.as_str()))
            .filter(|(t, _)| made_ahead(&t.id, &report.templates))
            .map(|(_, n)| n.clone())
            .collect()
    } else {
        Vec::new()
    };
    prepare.sort();
    let mut stale: Vec<String> = report
        .templates
        .iter()
        // Only what the server made for Pyo: one put there by hand stays.
        .filter(|t| t.build.is_some() && !wanted.contains(t.name.as_str()))
        .filter(|t| !waiting.iter().any(|id| is_revision_of(&t.name, id)))
        .map(|t| t.name.clone())
        .collect();
    stale.sort();
    (prepare, stale)
}

pub async fn all(db: &PgPool) -> sqlx::Result<Vec<Template>> {
    sqlx::query_as("SELECT id, team_id, source, revision FROM pyo.sandbox_templates")
        .fetch_all(db)
        .await
}

/// After a server's report: asks it to prepare what it lacks and to delete
/// what Pyo no longer has.
pub async fn sync_server(
    db: &PgPool,
    key: &SigningKey,
    server_id: &str,
    server_team: Option<&str>,
    address: &str,
    report: &ServerReport,
) -> sqlx::Result<()> {
    let templates = all(db).await?;
    let (prepare, stale) = plan(server_team, report, &templates);
    if prepare.is_empty() && stale.is_empty() {
        return Ok(());
    }
    let d = Daemon::new(address);
    let token = keys::admin_token(key, server_id);
    for id in prepare {
        let Some(t) = templates.iter().find(|t| t.server_name() == id) else {
            continue;
        };
        match d.command(&token, &prepare_command(key, t)).await {
            Ok(_) => {
                tracing::info!(server = %server_id, template = %id, "asked the sandbox server to prepare a template")
            }
            Err(e) => {
                tracing::warn!(server = %server_id, template = %id, error = %e, "the sandbox server didn't take a template")
            }
        }
    }
    for id in stale {
        match d
            .command(
                &token,
                &DaemonCommand::DeleteTemplate {
                    template: id.clone(),
                },
            )
            .await
        {
            Ok(_) => {
                tracing::info!(server = %server_id, template = %id, "deleted a template from the sandbox server")
            }
            // Sandboxes still use it: next round.
            Err(e) if e.code == ErrorCode::Conflict => {}
            Err(e) => {
                tracing::warn!(server = %server_id, template = %id, error = %e, "couldn't delete a template from the sandbox server")
            }
        }
    }
    Ok(())
}

/// Tells every server that may use the template to prepare it now, rather
/// than at the next runner round.
pub async fn push(s: AppState, template_id: String) {
    let res = async {
        let Some(t) = sqlx::query_as::<_, Template>(
            "SELECT id, team_id, source, revision FROM pyo.sandbox_templates WHERE id = $1",
        )
        .bind(&template_id)
        .fetch_optional(&s.db)
        .await?
        else {
            return anyhow::Ok(());
        };
        let servers: Vec<(String, Option<String>, String, Value)> = sqlx::query_as(
            "SELECT id, team_id, address, templates FROM pyo.sandbox_servers
              WHERE state = 'ready' AND enabled AND address IS NOT NULL
                AND capability->>'runtime' IN ('firecracker', 'gvisor')",
        )
        .fetch_all(&s.db)
        .await?;
        // No server yet: each makes it once it joins.
        if servers.is_empty() {
            return anyhow::Ok(());
        }
        let key = keys::signing_key(&s.db, &s.sealer)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        for (id, team, address, reports) in servers {
            let reports: Vec<TemplateReport> = serde_json::from_value(reports).unwrap_or_default();
            if !eligible(team.as_deref(), t.team_id.as_deref()) || !made_ahead(&t.id, &reports) {
                continue;
            }
            let token = keys::admin_token(&key, &id);
            if let Err(e) = Daemon::new(&address)
                .command(&token, &prepare_command(&key, &t))
                .await
            {
                tracing::warn!(server = %id, template = %t.id, error = %e, "the sandbox server didn't take a template");
            }
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(template = %template_id, error = %format!("{e:#}"), "couldn't send a template to the sandbox servers");
    }
}

/// Tells the servers that have a deleted template to delete it, every
/// revision of it. The runner catches the ones that don't answer now.
pub async fn forget(s: AppState, template_id: String) {
    let servers: Vec<(String, String, Value)> = match sqlx::query_as(
        "SELECT id, address, templates FROM pyo.sandbox_servers WHERE address IS NOT NULL",
    )
    .fetch_all(&s.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox templates: couldn't list servers");
            return;
        }
    };
    let Ok(key) = keys::signing_key(&s.db, &s.sealer).await else {
        return;
    };
    for (id, address, templates) in servers {
        let reports: Vec<TemplateReport> = serde_json::from_value(templates).unwrap_or_default();
        let token = keys::admin_token(&key, &id);
        for r in reports
            .iter()
            .filter(|r| is_revision_of(&r.name, &template_id))
        {
            let _ = Daemon::new(&address)
                .command(
                    &token,
                    &DaemonCommand::DeleteTemplate {
                        template: r.name.clone(),
                    },
                )
                .await;
        }
    }
}

/// A template's state from what its servers report, and the error to
/// show. `missing`: servers that could make it but don't report it yet.
pub fn roll_up(reports: &[TemplateReport], missing: usize) -> (&'static str, Option<String>) {
    let has = |s: TemplateState| reports.iter().any(|r| r.state == s);
    if has(TemplateState::Ready) {
        return ("ready", None);
    }
    if has(TemplateState::Building) {
        return ("building", None);
    }
    if has(TemplateState::Failed) && missing == 0 {
        let why = reports
            .iter()
            .find_map(|r| r.build.as_ref()?.error.clone())
            .unwrap_or_else(|| "It failed to build.".into());
        return ("failed", Some(why));
    }
    ("pending", None)
}

/// Rolls every template's state up from the servers' latest reports, by
/// its current revision. While a new revision is made, a template whose
/// older one is ready on a server stays ready: new sandboxes start from it.
pub async fn update_states(db: &PgPool) -> sqlx::Result<()> {
    let rows: Vec<(String, String, Option<String>, Value, i64, bool)> = sqlx::query_as(
        "SELECT t.id, t.state, t.error,
                coalesce(jsonb_agg(m.e) FILTER (WHERE m.e IS NOT NULL), '[]'),
                count(s.id) FILTER (WHERE m.e IS NULL AND s.state = 'ready' AND s.enabled
                                      AND s.capability->>'runtime' IN ('firecracker', 'gvisor')),
                coalesce(bool_or(o.ready), false)
           FROM pyo.sandbox_templates t
           LEFT JOIN pyo.sandbox_servers s
             ON s.team_id IS NULL OR t.team_id IS NULL OR s.team_id = t.team_id
           LEFT JOIN LATERAL (SELECT x AS e FROM jsonb_array_elements(s.templates) x
                               WHERE x->>'name' = t.id || coalesce('.' || t.revision, '') LIMIT 1) m ON true
           LEFT JOIN LATERAL (SELECT true AS ready FROM jsonb_array_elements(s.templates) x
                               WHERE t.revision IS NOT NULL AND starts_with(x->>'name', t.id || '.')
                                 AND x->>'name' <> t.id || '.' || t.revision
                                 AND coalesce(x->>'state', 'ready') = 'ready' LIMIT 1) o ON true
          GROUP BY t.id",
    )
    .fetch_all(db)
    .await?;
    for (id, state, error, reports, missing, older_ready) in rows {
        let reports: Vec<TemplateReport> = serde_json::from_value(reports).unwrap_or_default();
        let (new, why) = match roll_up(&reports, missing.max(0) as usize) {
            ("ready", why) => ("ready", why),
            _ if older_ready => ("ready", None),
            // Made on first use: ready to use on any server.
            _ if super::base::on_demand(&id) => ("ready", None),
            r => r,
        };
        if new != state || why != error {
            sqlx::query(
                "UPDATE pyo.sandbox_templates SET state = $2, error = $3, updated_at = now() WHERE id = $1",
            )
            .bind(&id)
            .bind(new)
            .bind(&why)
            .execute(db)
            .await?;
            tracing::info!(template = %id, state = new, "sandbox template is {new}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::{CapabilityReport, CapacityReport, TemplateBuild};
    use serde_json::json;

    fn t(id: &str, team: Option<&str>) -> Template {
        Template {
            id: id.into(),
            team_id: team.map(str::to_owned),
            source: json!({ "kind": "image", "image": "alpine" }),
            revision: None,
        }
    }

    fn report(runtime: Runtime, templates: Vec<TemplateReport>) -> ServerReport {
        ServerReport {
            protocol: 1,
            daemon_version: "0.4.0".into(),
            server_id: "sbs_a".into(),
            capability: CapabilityReport {
                runtime,
                ..Default::default()
            },
            capacity: CapacityReport::default(),
            sandboxes: vec![],
            templates,
        }
    }

    fn made(name: &str, state: TemplateState, error: Option<&str>) -> TemplateReport {
        TemplateReport {
            name: name.into(),
            state,
            build: Some(TemplateBuild {
                image: "alpine".into(),
                error: error.map(str::to_owned),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn which_servers_take_which_templates() {
        assert!(eligible(None, Some("tm_a")), "the installation's server");
        assert!(eligible(Some("tm_a"), None), "the installation's template");
        assert!(eligible(Some("tm_a"), Some("tm_a")));
        assert!(!eligible(Some("tm_a"), Some("tm_b")));
    }

    #[test]
    fn plans_prepares_and_deletes() {
        let templates = [
            t("stp_a", Some("tm_a")),
            t("stp_b", Some("tm_b")),
            t("stp_i", None),
        ];
        let hand = TemplateReport {
            name: "busybox".into(),
            ..Default::default()
        };
        let r = report(
            Runtime::Firecracker,
            vec![
                made("stp_i", TemplateState::Ready, None),
                made("stp_gone", TemplateState::Failed, Some("x")),
                made("stp_b", TemplateState::Building, None),
                hand,
            ],
        );
        let (prepare, stale) = plan(Some("tm_a"), &r, &templates);
        assert_eq!(prepare, vec!["stp_a"]);
        assert_eq!(
            stale,
            vec!["stp_b", "stp_gone"],
            "another team's and a deleted one go, the hand-made one stays"
        );
        let (prepare, _) = plan(None, &r, &templates);
        assert_eq!(
            prepare,
            vec!["stp_a"],
            "the installation's server takes all"
        );
        let (prepare, stale) = plan(Some("tm_a"), &report(Runtime::Gvisor, vec![]), &templates);
        assert_eq!(
            prepare,
            vec!["stp_a", "stp_i"],
            "gVisor servers make them too"
        );
        assert!(stale.is_empty());
        let (prepare, _) = plan(Some("tm_a"), &report(Runtime::None, vec![]), &templates);
        assert!(
            prepare.is_empty(),
            "a server that runs nothing makes nothing"
        );
    }

    #[test]
    fn revisions_are_made_beside_the_old_one() {
        let mut base = t("stp_base", None);
        base.revision = Some("bbbb".into());
        assert_eq!(base.server_name(), "stp_base.bbbb");
        assert!(is_revision_of("stp_base.aaaa", "stp_base"));
        assert!(is_revision_of("stp_base", "stp_base"));
        assert!(!is_revision_of("stp_basement", "stp_base"));
        let templates = [base];
        // The new revision is made, the old one kept until it's ready.
        let r = report(
            Runtime::Firecracker,
            vec![made("stp_base.aaaa", TemplateState::Ready, None)],
        );
        let (prepare, stale) = plan(None, &r, &templates);
        assert_eq!(prepare, vec!["stp_base.bbbb"]);
        assert!(stale.is_empty(), "kept while the new one isn't ready");
        let r = report(
            Runtime::Firecracker,
            vec![
                made("stp_base.aaaa", TemplateState::Ready, None),
                made("stp_base.bbbb", TemplateState::Building, None),
            ],
        );
        let (prepare, stale) = plan(None, &r, &templates);
        assert!(prepare.is_empty() && stale.is_empty());
        assert_eq!(
            older_ready(&r.templates, "stp_base", "stp_base.bbbb").collect::<Vec<_>>(),
            vec!["stp_base.aaaa"],
            "new sandboxes still start from the old one"
        );
        // Once the new one is ready, the old one goes.
        let r = report(
            Runtime::Firecracker,
            vec![
                made("stp_base.aaaa", TemplateState::Ready, None),
                made("stp_base.bbbb", TemplateState::Ready, None),
            ],
        );
        let (prepare, stale) = plan(None, &r, &templates);
        assert!(prepare.is_empty());
        assert_eq!(stale, vec!["stp_base.aaaa"]);
        assert_eq!(
            older_ready(&r.templates, "stp_base", "stp_base.bbbb").count(),
            1
        );
    }

    #[test]
    fn base_is_made_on_first_use_then_kept_up() {
        let mut base = t("stp_base", None);
        base.revision = Some("bbbb".into());
        let templates = [base, t("stp_a", None)];
        // A new server makes the team's templates, not base.
        let (prepare, stale) = plan(None, &report(Runtime::Firecracker, vec![]), &templates);
        assert_eq!(prepare, vec!["stp_a"]);
        assert!(stale.is_empty());
        // Once a sandbox made it there, it's kept, not deleted.
        let r = report(
            Runtime::Firecracker,
            vec![
                made("stp_a", TemplateState::Ready, None),
                made("stp_base.bbbb", TemplateState::Ready, None),
            ],
        );
        assert_eq!(plan(None, &r, &templates), (vec![], vec![]));
        // A new release: a server that used it makes the new revision.
        let r = report(
            Runtime::Firecracker,
            vec![made("stp_base.aaaa", TemplateState::Ready, None)],
        );
        let (prepare, stale) = plan(None, &r, &templates);
        assert_eq!(prepare, vec!["stp_a", "stp_base.bbbb"]);
        assert!(stale.is_empty());
        assert!(!made_ahead("stp_base", &[]));
        assert!(made_ahead("stp_a", &[]));
    }

    #[test]
    fn a_revision_is_prepared_under_its_name() {
        let key = SigningKey::generate("sbk_t");
        let mut base = t("stp_base", None);
        base.revision = Some("bbbb".into());
        let DaemonCommand::PrepareTemplate { template, .. } = prepare_command(&key, &base) else {
            panic!("not a prepare");
        };
        assert_eq!(template, "stp_base.bbbb");
    }

    #[test]
    fn states_roll_up() {
        use TemplateState::*;
        assert_eq!(roll_up(&[], 0), ("pending", None));
        assert_eq!(roll_up(&[], 2), ("pending", None));
        let failed = made("x", Failed, Some("no such image"));
        assert_eq!(
            roll_up(std::slice::from_ref(&failed), 0),
            ("failed", Some("no such image".into()))
        );
        assert_eq!(
            roll_up(std::slice::from_ref(&failed), 1).0,
            "pending",
            "another server may still make it"
        );
        assert_eq!(
            roll_up(&[failed.clone(), made("x", Building, None)], 0).0,
            "building"
        );
        assert_eq!(roll_up(&[failed, made("x", Ready, None)], 0).0, "ready");
    }

    #[test]
    fn images_in_pyos_registry() {
        let doors = vec![
            "registry.tail1.ts.net".to_string(),
            "100.64.0.1:5006".to_string(),
        ];
        assert_eq!(
            in_pyos_registry("registry.tail1.ts.net/acme/app/api:1", &doors),
            Some("acme/app/api:1")
        );
        assert_eq!(
            in_pyos_registry("100.64.0.1:5006/acme/app/api@sha256:abc", &doors),
            Some("acme/app/api@sha256:abc")
        );
        assert_eq!(in_pyos_registry("python:3.13", &doors), None);
        assert_eq!(
            in_pyos_registry("registry.tail1.ts.net.evil.com/x", &doors),
            None
        );
        assert_eq!(in_pyos_registry("ghcr.io/o/r", &[String::new()]), None);
    }

    #[test]
    fn a_public_image_goes_as_is() {
        let key = SigningKey::generate("sbk_t");
        let DaemonCommand::PrepareTemplate {
            template,
            image,
            plain_http,
            auth,
            ..
        } = prepare_command(&key, &t("stp_a", Some("tm_a")))
        else {
            panic!("not a prepare");
        };
        assert_eq!((template.as_str(), image.as_str()), ("stp_a", "alpine"));
        assert!(!plain_http && auth.is_none());
    }
}

//! Pyo's built-in sandbox templates ([`ALL`]):
//! - `base`: Debian 13 with the common tools
//!   (deploy/images/sandbox-base.Dockerfile). A sandbox created without a
//!   template starts from it.
//! - `desktop`: `base` with a desktop, noVNC on port 6080 and Chromium with
//!   CDP on port 9222, started on first use
//!   (deploy/images/sandbox-desktop.Dockerfile). Its sandboxes get 2 CPUs
//!   and 2 GiB unless the create says otherwise.
//!
//! They're the installation's, so every team has them. For each:
//! - Seeding: when a worker starts, and in every sandbox round, Pyo makes
//!   sure it exists and points at its release's image (its env var, like
//!   `PYO_SANDBOX_BASE_REF`, pinned by digest in release.json). It's
//!   idempotent. It's ready to use from the start, with or without servers.
//!   Lists show it even before it's added ([`listed`]).
//! - On first use: servers don't make it when they join. The first sandbox
//!   that needs it on a server has the server make it, then starts
//!   ([`super::firstuse`]). Later ones there start from that copy.
//! - On update: a new release has a new image, so it gets a new revision
//!   (the digest's start). Servers that have an older one make it beside
//!   it in the background, and [`super::templates`] moves new sandboxes
//!   over once it's ready and deletes the old one once no sandbox uses it.
//!   Servers that never used it stay as they are. An older Pyo still
//!   running during the update never moves it back.
//! - Dev builds have no image of their own unless scripts/pyo-up.sh gives
//!   one (it builds `desktop`'s): they use the latest release Pyo found when
//!   it checked for updates, or wait for one.
//! - Nobody deletes it, and no other template may take its name.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use pyo_proto::{ReleaseImages, parse_version, pinned_image};
use serde_json::{Value, json};

use crate::AppState;

/// One of Pyo's built-in templates.
#[derive(Debug)]
pub struct Builtin {
    /// Its name, which sandboxes ask for.
    pub name: &'static str,
    /// Its id, the same on every installation.
    pub id: &'static str,
    /// What it runs, shown beside its image.
    pub os: &'static str,
    /// The env var this release's image comes in.
    pub env: &'static str,
    /// Its image in a release.json.
    pub image: fn(&ReleaseImages) -> &str,
    /// The size its sandboxes get when the create gives none.
    pub cpus: u32,
    pub memory_mib: u32,
}

pub const BASE: Builtin = Builtin {
    name: "base",
    id: "stp_base",
    os: "Debian 13",
    env: pyo_proto::SANDBOX_BASE_ENV,
    image: |i| &i.sandbox_base,
    cpus: pyo_proto::sandbox::DEFAULT_CPUS,
    memory_mib: pyo_proto::sandbox::DEFAULT_MEMORY_MIB,
};

pub const DESKTOP: Builtin = Builtin {
    name: "desktop",
    id: "stp_desktop",
    os: "Debian 13",
    env: pyo_proto::SANDBOX_DESKTOP_ENV,
    image: |i| &i.sandbox_desktop,
    // Chromium and the screen need room.
    cpus: 2,
    memory_mib: 2048,
};

/// Every built-in template, in the order lists show them.
pub const ALL: [&Builtin; 2] = [&BASE, &DESKTOP];

/// `base`'s name, which a create without a template asks for.
pub const NAME: &str = BASE.name;

/// The built-in template named `name`.
pub fn by_name(name: &str) -> Option<&'static Builtin> {
    ALL.into_iter().find(|b| b.name == name)
}

/// The built-in template with id `id`.
pub fn by_id(id: &str) -> Option<&'static Builtin> {
    ALL.into_iter().find(|b| b.id == id)
}

/// Whether servers make template `id` on first use instead of when they
/// join: only the built-in ones.
pub fn on_demand(id: &str) -> bool {
    by_id(id).is_some()
}

/// The image to seed a built-in template from, and the release it belongs
/// to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub image: String,
    pub release: String,
}

/// This release's image for `b`, from its env var. None for a build that
/// isn't a release (scripts/pyo-up.sh leaves base's empty).
pub fn from_env(b: &Builtin) -> Option<Wanted> {
    let image = std::env::var(b.env).ok()?;
    let image = image.trim();
    pinned_image(image).then(|| Wanted {
        image: image.to_owned(),
        release: crate::updates::current().to_owned(),
    })
}

/// The revision servers keep an image under: the first 12 hex digits of
/// its digest.
pub fn revision(image: &str) -> Option<String> {
    let (_, digest) = image.split_once("@sha256:")?;
    (digest.len() >= 12).then(|| digest[..12].to_owned())
}

/// The template's source for an image.
pub fn source(b: &Builtin, w: &Wanted) -> Value {
    json!({ "kind": "image", "image": w.image, "os": b.os, "release": w.release })
}

/// Why `b` can't be used yet, when Pyo has no image for it.
pub fn unavailable(b: &Builtin) -> String {
    format!(
        "Pyo has no image for {} yet. It gets one when it checks for updates.",
        b.name
    )
}

/// `b` as a list shows it before it's added: as it will be, from the image
/// it would have. Unavailable, with why, when there's none yet.
pub fn view(b: &Builtin, want: Option<&Wanted>) -> Value {
    let (source, state, error) = match want {
        Some(w) => (source(b, w), "ready", None),
        None => (
            json!({ "kind": "image", "os": b.os }),
            "unavailable",
            Some(unavailable(b)),
        ),
    };
    json!({
        "id": b.id, "teamId": null, "name": b.name, "source": source, "state": state,
        "builtin": true, "revision": want.and_then(|w| revision(&w.image)),
        "snapshots": {}, "error": error, "createdBy": "Pyo",
        "createdAt": null, "updatedAt": null, "servers": [],
    })
}

/// Whether `b` is in the database yet.
pub async fn added(db: &sqlx::PgPool, b: &Builtin) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.sandbox_templates WHERE id = $1)")
        .bind(b.id)
        .fetch_one(db)
        .await
}

/// [`view`] with the image Pyo would add it with now.
pub async fn listed(db: &sqlx::PgPool, b: &Builtin) -> Value {
    view(b, wanted(db, b).await.as_ref())
}

/// What seeding does, given the template's source now (None: it isn't
/// there yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seed {
    Insert,
    /// A new image: a new revision.
    Move,
    Keep,
}

pub fn decide(have: Option<&Value>, want: &Wanted) -> Seed {
    let Some(src) = have else {
        return Seed::Insert;
    };
    if src["image"].as_str() == Some(want.image.as_str()) {
        return Seed::Keep;
    }
    // A newer Pyo already moved it: this one is on its way out.
    let theirs = src["release"].as_str().and_then(parse_version);
    match (theirs, parse_version(&want.release)) {
        (Some(a), Some(b)) if a > b => Seed::Keep,
        _ => Seed::Move,
    }
}

/// `b`'s image in what an update check found.
fn in_checked(b: &Builtin, c: &crate::updates::Checked) -> Option<Wanted> {
    let found = c.found.as_ref()?;
    let image = (b.image)(&found.release.images);
    pinned_image(image).then(|| Wanted {
        image: image.to_owned(),
        release: found.release.version.clone(),
    })
}

/// A dev build checks for updates itself, once per process, rather than
/// wait for the next scheduled check (up to six hours).
static CHECKED: AtomicBool = AtomicBool::new(false);

/// The image `b` should have now: this release's, else (a dev build) the
/// latest release found by the update check. When that found none, or
/// failed, it checks once now.
pub async fn wanted(db: &sqlx::PgPool, b: &Builtin) -> Option<Wanted> {
    if let Some(w) = from_env(b) {
        return Some(w);
    }
    let c = crate::updates::checked(db).await.ok()?;
    let have = in_checked(b, &c);
    if (have.is_none() || c.error.is_some())
        && !crate::updates::checks_off_by_env()
        && !CHECKED.swap(true, Ordering::Relaxed)
        && let Ok(now) = crate::updates::check(db).await
    {
        return in_checked(b, &now).or(have);
    }
    have
}

/// Which templates' waiting was said (a bit each, by place in [`ALL`]),
/// once per process, so a dev build doesn't repeat it every round.
static SAID_WAITING: AtomicU8 = AtomicU8::new(0);

/// Makes sure `b` exists and has the image it should. Tells the servers at
/// once when that changed. False when there's no image for it yet.
pub async fn ensure(s: &AppState, b: &'static Builtin) -> anyhow::Result<bool> {
    let Some(want) = wanted(&s.db, b).await else {
        let bit = 1 << ALL.iter().position(|x| x.id == b.id).unwrap_or(0);
        if SAID_WAITING.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            tracing::info!(
                "sandbox template {}: this build has no image for it, and no release was found yet, so it's added later",
                b.name
            );
        }
        return Ok(false);
    };
    seed_from(s, b, &want).await.map(|_| true)
}

/// [`ensure`] for every built-in template. True once they all have an
/// image. The first error, after trying them all.
pub async fn ensure_all(s: &AppState) -> anyhow::Result<bool> {
    let mut all = true;
    let mut first = None;
    for b in ALL {
        match ensure(s, b).await {
            Ok(had) => all &= had,
            Err(e) => {
                all = false;
                first.get_or_insert(e);
            }
        }
    }
    match first {
        Some(e) => Err(e),
        None => Ok(all),
    }
}

/// Seeds `b` from `want`, and says what it did.
pub async fn seed_from(s: &AppState, b: &Builtin, want: &Wanted) -> anyhow::Result<Seed> {
    let want = want.clone();
    let rev = revision(&want.image);
    let mut tx = s.db.begin().await?;
    let have: Option<Value> =
        sqlx::query_scalar("SELECT source FROM pyo.sandbox_templates WHERE id = $1 FOR UPDATE")
            .bind(b.id)
            .fetch_optional(&mut *tx)
            .await?;
    let seed = decide(have.as_ref(), &want);
    match seed {
        Seed::Keep => return Ok(Seed::Keep),
        Seed::Insert => {
            let res = sqlx::query(
                "INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by, builtin, revision)
                 VALUES ($1, NULL, $2, $3, 'ready', 'Pyo', true, $4)
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(b.id)
            .bind(b.name)
            .bind(source(b, &want))
            .bind(&rev)
            .execute(&mut *tx)
            .await;
            if let Err(e) = res {
                if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
                    tracing::warn!(
                        "sandbox template {0}: the installation has another template named {0}, so Pyo's isn't added",
                        b.name
                    );
                    return Ok(Seed::Keep);
                }
                return Err(e.into());
            }
            tracing::info!(image = %want.image, "sandbox template {} added", b.name);
        }
        Seed::Move => {
            sqlx::query(
                "UPDATE pyo.sandbox_templates
                    SET source = $2, revision = $3, builtin = true, error = NULL, updated_at = now()
                  WHERE id = $1",
            )
            .bind(b.id)
            .bind(source(b, &want))
            .bind(&rev)
            .execute(&mut *tx)
            .await?;
            tracing::info!(image = %want.image, "sandbox template {} moved to a new image", b.name);
        }
    }
    tx.commit().await?;
    tokio::spawn(super::templates::push(s.clone(), b.id.to_owned()));
    Ok(seed)
}

/// On a worker's start: [`ensure_all`], retried while the database is
/// busy, and every ten minutes while one has no image (a dev build before
/// an update check finds a release). Sandbox rounds keep them in step
/// after that, once there's a sandbox server.
pub async fn seed(s: AppState) {
    let mut attempt = 0u64;
    while attempt < 10 && !s.stopping() {
        match ensure_all(&s).await {
            Ok(true) => return,
            Ok(false) => tokio::time::sleep(std::time::Duration::from_secs(600)).await,
            Err(e) => {
                attempt += 1;
                tracing::warn!(attempt, error = %format!("{e:#}"), "sandbox templates: couldn't add Pyo's, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(15 * attempt)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "ghcr.io/upsetdev/pyo/sandbox-base:0.4.1@sha256:aaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const B: &str = "ghcr.io/upsetdev/pyo/sandbox-base:0.4.2@sha256:cccccccccccc0000000000000000000000000000000000000000000000000000";

    fn want(image: &str, release: &str) -> Wanted {
        Wanted {
            image: image.into(),
            release: release.into(),
        }
    }

    #[test]
    fn seeding_is_idempotent() {
        let w = want(A, "0.4.1");
        assert_eq!(decide(None, &w), Seed::Insert);
        let src = source(&BASE, &w);
        assert_eq!(decide(Some(&src), &w), Seed::Keep, "the same image again");
        assert_eq!(src["os"], "Debian 13");
        assert_eq!(src["kind"], "image");
    }

    #[test]
    fn a_new_release_moves_it_forward_only() {
        let old = source(&BASE, &want(A, "0.4.1"));
        let new = want(B, "0.4.2");
        assert_eq!(decide(Some(&old), &new), Seed::Move);
        // The older Pyo, still running during the update, leaves it.
        assert_eq!(
            decide(Some(&source(&BASE, &new)), &want(A, "0.4.1")),
            Seed::Keep
        );
        // A rebuilt image of the same release moves it too.
        assert_eq!(decide(Some(&old), &want(B, "0.4.1")), Seed::Move);
        // One without a release (made by hand) is taken over.
        assert_eq!(
            decide(
                Some(&json!({ "kind": "image", "image": "debian:13" })),
                &new
            ),
            Seed::Move
        );
    }

    #[test]
    fn listed_before_it_is_added() {
        let v = view(&BASE, Some(&want(A, "0.4.1")));
        assert_eq!(
            (v["id"].as_str(), v["name"].as_str(), v["state"].as_str()),
            (Some(BASE.id), Some(NAME), Some("ready"))
        );
        assert_eq!(v["source"]["image"], A);
        assert_eq!(v["revision"], "aaaaaaaaaaaa");
        assert_eq!(v["builtin"], true);
        assert_eq!(v["servers"], json!([]));
        // No image anywhere: listed, and why it can't be used yet.
        let v = view(&BASE, None);
        assert_eq!(v["state"], "unavailable");
        assert_eq!(
            v["error"],
            "Pyo has no image for base yet. It gets one when it checks for updates."
        );
        assert_eq!(v["source"]["os"], BASE.os);
        assert!(!unavailable(&BASE).contains(';'));
        // desktop the same way, by its own name.
        let v = view(&DESKTOP, None);
        assert_eq!(
            (v["id"].as_str(), v["name"].as_str()),
            (Some("stp_desktop"), Some("desktop"))
        );
        assert!(v["error"].as_str().unwrap().contains("image for desktop"));
    }

    #[test]
    fn the_builtins() {
        assert_eq!(ALL.map(|b| b.name), ["base", "desktop"]);
        assert!(on_demand("stp_base") && on_demand("stp_desktop"));
        assert!(!on_demand("stp_other"));
        assert_eq!(by_name("desktop").map(|b| b.id), Some("stp_desktop"));
        assert!(by_name("node-22").is_none());
        // desktop's sandboxes get room for Chromium.
        assert!(DESKTOP.cpus >= 2 && DESKTOP.memory_mib >= 2048);
        let images = pyo_proto::ReleaseImages {
            sandbox_base: A.into(),
            sandbox_desktop: B.into(),
            ..Default::default()
        };
        assert_eq!(((BASE.image)(&images), (DESKTOP.image)(&images)), (A, B));
        for b in ALL {
            assert!(pyo_proto::sandbox::is_template_name(b.name));
            assert!(!b.env.ends_with("_IMAGE"), "older Pyos refuse new *_IMAGE vars");
        }
    }

    #[test]
    fn revisions_come_from_the_digest() {
        assert_eq!(revision(A).as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(revision(B).as_deref(), Some("cccccccccccc"));
        assert_eq!(revision("debian:13"), None);
        let name = super::super::templates::server_name(BASE.id, revision(A).as_deref());
        assert_eq!(name, "stp_base.aaaaaaaaaaaa");
        assert!(pyo_proto::sandbox::is_template_name(&name));
    }

    #[test]
    fn only_a_pinned_image_from_the_env_counts() {
        // SAFETY: tests in this module don't read it concurrently.
        unsafe { std::env::set_var("PYO_SANDBOX_BASE_REF", "") };
        assert_eq!(from_env(&BASE), None, "a dev build (pyo-up.sh)");
        unsafe {
            std::env::set_var(
                "PYO_SANDBOX_BASE_REF",
                "127.0.0.1:5005/pyo/sandbox-base:dev",
            )
        };
        assert_eq!(from_env(&BASE), None, "not pinned");
        unsafe { std::env::set_var("PYO_SANDBOX_BASE_REF", A) };
        assert_eq!(from_env(&BASE), Some(want(A, crate::updates::current())));
        assert_eq!(from_env(&DESKTOP), None, "each has its own");
        unsafe { std::env::remove_var("PYO_SANDBOX_BASE_REF") };
        assert_eq!(from_env(&BASE), None);
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one, in a database
    /// of its own): seeding adds `base` once, moves it to a new release's
    /// image, never back, and nobody deletes it or takes its name.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn seeds_base_in_the_database() {
        use sqlx::PgPool;
        use sqlx::postgres::PgConnectOptions;
        let opts: PgConnectOptions = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        let admin = PgPool::connect_with(opts.clone()).await.unwrap();
        let name = format!("pyo_base_{}", std::process::id());
        for q in [
            format!("DROP DATABASE IF EXISTS {name}"),
            format!("CREATE DATABASE {name}"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(q))
                .execute(&admin)
                .await
                .unwrap();
        }
        let db = PgPool::connect_with(opts.database(&name)).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        let s = super::super::e2e::test_state(db.clone());
        let row = || async {
            sqlx::query_as::<_, (String, String, Value, Option<String>, bool, Option<String>)>(
                "SELECT name, state, source, revision, builtin, team_id FROM pyo.sandbox_templates WHERE id = $1",
            )
            .bind(BASE.id)
            .fetch_one(&db)
            .await
            .unwrap()
        };

        let old = want(A, "0.4.1");
        assert_eq!(seed_from(&s, &BASE, &old).await.unwrap(), Seed::Insert);
        assert_eq!(seed_from(&s, &BASE, &old).await.unwrap(), Seed::Keep, "idempotent");
        let (n, state, src, rev, builtin, team) = row().await;
        assert_eq!(
            (n.as_str(), state.as_str(), rev.as_deref(), builtin, team),
            ("base", "ready", Some("aaaaaaaaaaaa"), true, None)
        );
        assert_eq!(src["image"], A);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.sandbox_templates")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Every team sees it, as the installation's, at its current revision.
        let views = super::super::template_views(&db, &["tm_any".into()], None)
            .await
            .unwrap();
        assert_eq!(views[0]["name"], "base");
        assert_eq!(views[0]["builtin"], true);
        assert_eq!(views[0]["source"]["os"], "Debian 13");

        // A new release moves it. The old Pyo, still running, doesn't move it back.
        let new = want(B, "0.4.2");
        assert_eq!(seed_from(&s, &BASE, &new).await.unwrap(), Seed::Move);
        assert_eq!(seed_from(&s, &BASE, &old).await.unwrap(), Seed::Keep);
        let (_, _, src, rev, ..) = row().await;
        assert_eq!(
            (src["image"].as_str(), rev.as_deref()),
            (Some(B), Some("cccccccccccc"))
        );

        // Nobody deletes it, and no team takes its name.
        let user = crate::auth::User {
            id: "usr_base".into(),
            name: "Admin".into(),
            email: "admin@base.test".into(),
            timezone: "UTC".into(),
            is_admin: true,
            must_set_up: false,
            key: None,
        };
        let e = super::super::delete_template(&s, &user, BASE.id)
            .await
            .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT, "{}", e.message);
        let e = super::super::create_template(
            &s,
            &user,
            serde_json::from_value(json!({ "name": "base", "image": "debian:13" })).unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT, "{}", e.message);

        db.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {name} WITH (FORCE)"
        )))
        .execute(&admin)
        .await
        .unwrap();
    }
}

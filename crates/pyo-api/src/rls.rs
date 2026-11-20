//! Row security on team_id (migrations 0022 and 0032), behind the API's
//! own team checks: a check the code forgot still can't read or change
//! another team's rows.
//!
//! Pyo connects as its tables' owner, which row security doesn't hold back.
//! That is the bypass, and it stays the default: migrations, the agents'
//! and builders' endpoints, webhooks, background jobs and anything an
//! operator runs see every row. A request made for a person or an API key
//! lowers its transaction ([`scope`], [`begin`]) to the `pyo_team` role,
//! with the teams it may see in `pyo.team_ids`. There the policies show and
//! accept only those teams' rows: operations, the dashboard's `/state`
//! (and the MCP tools that read it), and the REST reads. Work that crosses
//! teams on purpose (a hostname is one team's across the installation, a
//! hosted build's billing tables) lifts the transaction back for that part
//! ([`lift`], then [`Lifted::lower`]).
//!
//! Both settings are local to the transaction: its connection goes back
//! to the pool as the owner. Self-hosted and hosted run the same policies.

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::auth::User;

/// Lowers the transaction to the teams `user` may see: the ones they're an
/// active member of, within an API key's teams, leaving out the teams that
/// require two-factor while the account has none (all of them while it
/// must set it up first: twofactor.rs). One round trip. Again on a
/// lowered transaction it picks up teams joined since (a new team).
pub async fn scope(conn: &mut PgConnection, user: &User) -> sqlx::Result<()> {
    let key_teams: Option<Vec<String>> = user.key.as_ref().and_then(|k| k.teams.clone());
    sqlx::query(
        "SELECT set_config('pyo.team_ids', pyo.member_team_ids($1, $2, $3), true),
                set_config('role', 'pyo_team', true)",
    )
    .bind(&user.id)
    .bind(key_teams)
    .bind(crate::hosted())
    .execute(conn)
    .await?;
    Ok(())
}

/// A transaction for a person's or API key's request, lowered to their
/// teams ([`scope`]). Reads go through it too, so a query that forgot its
/// team check still sees only theirs. Hold it only for the request's own
/// queries: anything that takes another connection from the pool meanwhile
/// waits on a pool this request already holds one of.
pub async fn begin(db: &PgPool, user: &User) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.begin().await?;
    scope(&mut tx, user).await?;
    Ok(tx)
}

/// The teams a lowered transaction may see ([`scope`]): what its queries'
/// `team_id = ANY($1)` bind. Empty when it isn't lowered.
pub async fn team_ids(conn: &mut PgConnection) -> sqlx::Result<Vec<String>> {
    let ids: Option<Vec<String>> = sqlx::query_scalar("SELECT pyo.team_scope()")
        .fetch_one(conn)
        .await?;
    Ok(ids.unwrap_or_default())
}

/// A transaction lifted out of its team scope by [`lift`].
#[must_use = "lower it again once the work across teams is done"]
pub struct Lifted {
    was_scoped: bool,
}

/// Lifts the transaction out of its team scope, for work that has to see
/// or change other teams' rows on purpose. Keep it to that work, then
/// [`Lifted::lower`] it again. A transaction that wasn't scoped stays as
/// it is.
pub async fn lift(conn: &mut PgConnection) -> sqlx::Result<Lifted> {
    let was_scoped: bool = sqlx::query_scalar("SELECT current_user::text = 'pyo_team'")
        .fetch_one(&mut *conn)
        .await?;
    if was_scoped {
        sqlx::query("SELECT set_config('role', 'none', true)")
            .execute(conn)
            .await?;
    }
    Ok(Lifted { was_scoped })
}

impl Lifted {
    /// Whether the transaction was lowered to a person's or API key's
    /// teams: it's their request.
    pub fn was_scoped(&self) -> bool {
        self.was_scoped
    }

    /// Back to the team scope the transaction had before [`lift`].
    pub async fn lower(self, conn: &mut PgConnection) -> sqlx::Result<()> {
        if self.was_scoped {
            sqlx::query("SELECT set_config('role', 'pyo_team', true)")
                .execute(conn)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::PgPool;

    /// Tables with a team_id that row security doesn't cover, each with
    /// why. A new team-owned table gets a policy in its migration, or a
    /// place here with its reason.
    const NOT_COVERED: &[(&str, &str)] = &[];

    fn run_big<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(f)
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn db() -> PgPool {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        db
    }

    /// Against a throwaway Postgres: every table with a team_id has row
    /// security, or is one of [`NOT_COVERED`].
    #[test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    fn every_team_table_has_row_security_or_is_listed() {
        run_big(async {
            let db = db().await;
            let rows: Vec<(String, bool)> = sqlx::query_as(
                "SELECT c.relname::text, c.relrowsecurity FROM pg_class c
                   JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = 'pyo' AND c.relkind = 'r'
                    AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'team_id' AND NOT a.attisdropped)
                  ORDER BY 1",
            )
            .fetch_all(&db)
            .await
            .unwrap();
            let mut open: Vec<&str> = rows
                .iter()
                .filter(|(_, rls)| !rls)
                .map(|(t, _)| t.as_str())
                .collect();
            let mut listed: Vec<&str> = NOT_COVERED.iter().map(|(t, _)| *t).collect();
            open.sort();
            listed.sort();
            assert_eq!(open, listed);
            for t in [
                "services",
                "deployments",
                "data_keys",
                "certificates",
                "registry_logins",
                "git_connections",
                "alert_channels",
                "secret_stores",
                "clusters",
                "projects",
                "members",
                "volumes",
                "activity",
                "audit_events",
                "sandboxes",
            ] {
                assert!(
                    rows.iter().any(|(n, r)| n == t && *r),
                    "{t} has row security"
                );
            }
        });
    }

    /// Team A's request can't read or change team B's rows even when the
    /// code checks nothing, and a check across teams still sees them.
    #[test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    fn a_team_sees_only_its_own_rows() {
        run_big(async {
            let db = db().await;
            crate::vault::init(crate::vault::Vault::new(
                db.clone(),
                crate::vault::Provider::from_env(),
            ));
            let run = crate::ids::random_string(8).to_lowercase();
            let id = |p: &str, t: &str| format!("{p}_{t}{run}");
            let mut users = vec![];
            for t in ["a", "b"] {
                let user = User {
                    id: id("usr", t),
                    name: t.into(),
                    email: format!("{t}{run}@rls.test"),
                    timezone: "UTC".into(),
                    is_admin: false,
                    must_set_up: false,
                    key: None,
                };
                let (team, cluster, project) = (id("team", t), id("cl", t), id("prj", t));
                let q = |sql: &'static str| sqlx::query(sql);
                q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $1, '')")
                    .bind(&user.id)
                    .bind(&user.email)
                    .execute(&db)
                    .await
                    .unwrap();
                q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                    .bind(&team)
                    .execute(&db)
                    .await
                    .unwrap();
                q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'admin', 'active')").bind(id("mem", t)).bind(&team).bind(&user.id).bind(&user.email).execute(&db).await.unwrap();
                q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
                let spec = json!({
                    "source": { "type": "image", "image": "nginx", "reference": "1.27" },
                    "replicas": 1, "env": [], "resources": {}, "healthCheck": { "type": "none" },
                    "ports": [{ "id": "port_1", "name": "http", "protocol": "HTTP", "containerPort": 80, "exposure": "http" }],
                    "domains": [{ "id": "dom_1", "hostname": format!("{t}-{run}.example.com"), "portName": "http", "status": "active", "tls": "pending", "tlsMode": "auto", "redirectHttps": true, "createdAt": 1 }],
                });
                q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', $5, 'running')").bind(id("svc", t)).bind(&team).bind(&project).bind(&cluster).bind(&spec).execute(&db).await.unwrap();
                q("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', '{}')").bind(id("dpl", t)).bind(&team).bind(&project).bind(id("svc", t)).bind(&cluster).execute(&db).await.unwrap();
                q("INSERT INTO pyo.deployment_logs (deployment_id, at, level, source, message) VALUES ($1, now(), 'info', 'build', 'secret line')").bind(id("dpl", t)).execute(&db).await.unwrap();
                q("INSERT INTO pyo.certificates (id, team_id, name, hostnames, issuer, not_before, not_after, fingerprint, chain_pem, key_sealed) VALUES ($1, $2, $1, '{}', 'x', now(), now(), 'f', 'pem', 'key')").bind(id("crt", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.registry_logins (id, team_id, host, username, secret_sealed) VALUES ($1, $2, 'ghcr.io', 'u', 'sealed')").bind(id("rgl", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.alert_channels (id, team_id, kind, name, target_sealed) VALUES ($1, $2, 'webhook', 'hook', 'sealed')").bind(id("alc", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.git_connections (id, team_id, provider, base_url, token, webhook_secret) VALUES ($1, $2, 'gitlab', 'https://gitlab.com', 'sealed', 'sealed')").bind(id("gitc", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.git_webhooks (connection_id, repo) VALUES ($1, 'group/repo')")
                    .bind(id("gitc", t))
                    .execute(&db)
                    .await
                    .unwrap();
                q("INSERT INTO pyo.secret_stores (id, team_id, kind, name, addr, credential) VALUES ($1, $2, 'vault', 'v', 'https://v', 'sealed')").bind(id("sst", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.data_keys (id, team_id, kek_id, wrapped) VALUES ($1, $2, 'local:x', 'wrapped')").bind(id("dk", t)).bind(&team).execute(&db).await.unwrap();
                q("INSERT INTO pyo.deploy_hooks (id, team_id, project_id, name, token_hash, token_sealed, created_by) VALUES ($1, $2, $3, 'h', $1, 'sealed', 'x')").bind(id("hook", t)).bind(&team).bind(&project).execute(&db).await.unwrap();
                q("INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at) VALUES ($1, $2, 'pk', now() + interval '1 day')").bind(id("ak", t)).bind(&cluster).execute(&db).await.unwrap();
                q("INSERT INTO pyo.commands (id, cluster_id, command) VALUES ($1, $2, '{\"kind\":\"deploy\",\"env\":[]}')").bind(id("cmd", t)).bind(&cluster).execute(&db).await.unwrap();
                q("INSERT INTO pyo.activity (id, team_id, data) VALUES ($1, $2, '{}')")
                    .bind(id("act", t))
                    .bind(&team)
                    .execute(&db)
                    .await
                    .unwrap();
                q("INSERT INTO pyo.servers (id, cluster_id, name, report) VALUES ($1, $2, $1, '{}')").bind(id("srv", t)).bind(&cluster).execute(&db).await.unwrap();
                q("INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, token_hash) VALUES ($1, $2, $1, 'cluster', $3, $1)").bind(id("bld", t)).bind(&team).bind(&cluster).execute(&db).await.unwrap();
                q("INSERT INTO pyo.firewall_rules (id, team_id, service_id, action) VALUES ($1, $2, $3, 'block')").bind(id("fw", t)).bind(&team).bind(id("svc", t)).execute(&db).await.unwrap();
                q("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, data) VALUES ($1, $2, $3, $4, '{\"name\":\"data\"}')").bind(id("vol", t)).bind(&team).bind(&cluster).bind(&project).execute(&db).await.unwrap();
                q("INSERT INTO pyo.backups (id, team_id, cluster_id, kind, trigger, label) VALUES ($1, $2, $3, 'volume', 'manual', 'x')").bind(id("bak", t)).bind(&team).bind(&cluster).execute(&db).await.unwrap();
                users.push(user);
            }
            let (a, b) = (&users[0], &users[1]);

            // (table, row of team a, row of team b), by the column that names it.
            let rows: &[(&str, &str, &str)] = &[
                ("services", "id", "svc"),
                ("deployments", "id", "dpl"),
                ("deployment_logs", "deployment_id", "dpl"),
                ("certificates", "id", "crt"),
                ("registry_logins", "id", "rgl"),
                ("alert_channels", "id", "alc"),
                ("git_connections", "id", "gitc"),
                ("git_webhooks", "connection_id", "gitc"),
                ("secret_stores", "id", "sst"),
                ("data_keys", "id", "dk"),
                ("deploy_hooks", "id", "hook"),
                ("agent_keys", "id", "ak"),
                ("commands", "id", "cmd"),
                ("clusters", "id", "cl"),
                ("projects", "id", "prj"),
                ("members", "id", "mem"),
                ("activity", "id", "act"),
                ("servers", "id", "srv"),
                ("builders", "id", "bld"),
                ("firewall_rules", "id", "fw"),
                ("volumes", "id", "vol"),
                ("backups", "id", "bak"),
            ];
            let count = |table: &str, col: &str| {
                sqlx::AssertSqlSafe(format!("SELECT count(*) FROM pyo.{table} WHERE {col} = $1"))
            };

            let mut tx = db.begin().await.unwrap();
            scope(&mut tx, a).await.unwrap();
            for (table, col, prefix) in rows {
                let n: i64 = sqlx::query_scalar(count(table, col))
                    .bind(id(prefix, "a"))
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                assert_eq!(n, 1, "team a sees its own {table}");
                let n: i64 = sqlx::query_scalar(count(table, col))
                    .bind(id(prefix, "b"))
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                assert_eq!(n, 0, "team a doesn't see team b's {table}");
            }
            // A query that forgot its team check reads and changes nothing of b's.
            let spec: Option<Value> =
                sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
                    .bind(id("svc", "b"))
                    .fetch_optional(&mut *tx)
                    .await
                    .unwrap();
            assert!(spec.is_none());
            let changed = sqlx::query("UPDATE pyo.services SET name = 'mine' WHERE id = $1")
                .bind(id("svc", "b"))
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected();
            assert_eq!(changed, 0);
            let gone = sqlx::query("DELETE FROM pyo.certificates WHERE team_id = $1")
                .bind(id("team", "b"))
                .execute(&mut *tx)
                .await
                .unwrap()
                .rows_affected();
            assert_eq!(gone, 0);
            let all: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pyo.registry_logins WHERE id LIKE '%' || $1",
            )
            .bind(&run)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            assert_eq!(all, 1, "only its own, also without naming one");
            // Nor writes a row for b.
            let e = sqlx::query("INSERT INTO pyo.registry_logins (id, team_id, host, username, secret_sealed) VALUES ($1, $2, 'docker.io', 'u', 's')")
                .bind(id("rgl", "x"))
                .bind(id("team", "b"))
                .execute(&mut *tx)
                .await
                .unwrap_err();
            assert_eq!(
                e.as_database_error().and_then(|d| d.code()).as_deref(),
                Some("42501"),
                "{e}"
            );
            tx.rollback().await.unwrap();

            // Lifted for work across teams, then lowered again.
            let mut tx = db.begin().await.unwrap();
            scope(&mut tx, a).await.unwrap();
            let lifted = lift(&mut tx).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE id = $1")
                .bind(id("svc", "b"))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(n, 1);
            lifted.lower(&mut tx).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE id = $1")
                .bind(id("svc", "b"))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(n, 0);
            tx.rollback().await.unwrap();

            // An API key limited to other teams sees none of a's.
            let mut keyed = a.clone();
            keyed.key = Some(crate::auth::KeyScope {
                access: crate::auth::KeyAccess::Full,
                teams: Some(vec![id("team", "z")]),
            });
            let mut tx = db.begin().await.unwrap();
            scope(&mut tx, &keyed).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE id = $1")
                .bind(id("svc", "a"))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(n, 0);
            tx.rollback().await.unwrap();

            // The pool's own connections (agents, jobs) see every team's.
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE id = ANY($1)")
                .bind(vec![id("svc", "a"), id("svc", "b")])
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(n, 2, "the pool is back to the owner");

            // Through the API: b's service isn't there for a, and b's
            // hostname is still taken (that check goes across teams).
            let s = state(db.clone());
            let svc = |t: &str, host: &str| {
                json!({
                    "id": id("svc", t), "projectId": id("prj", t), "clusterId": id("cl", t), "name": "web",
                    "source": { "type": "image", "image": "nginx", "reference": "1.27" },
                    "replicas": 1, "env": [], "resources": {}, "healthCheck": { "type": "none" },
                    "ports": [{ "id": "port_1", "name": "http", "protocol": "HTTP", "containerPort": 80, "exposure": "http" }],
                    "domains": [{ "id": "dom_1", "hostname": host, "portName": "http", "status": "active", "tls": "pending", "tlsMode": "auto", "redirectHttps": true, "createdAt": 1 }],
                })
            };
            let e = crate::ops::run_json(&s, a, json!({ "op": "saveService", "service": svc("b", &format!("b-{run}.example.com")) })).await.unwrap_err();
            assert_eq!(e.status, StatusCode::NOT_FOUND, "{}", e.message);
            let e = crate::ops::run_json(&s, a, json!({ "op": "saveService", "service": svc("a", &format!("b-{run}.example.com")) })).await.unwrap_err();
            assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
            crate::ops::run_json(&s, a, json!({ "op": "saveService", "service": svc("a", &format!("a2-{run}.example.com")) })).await.unwrap();
            let _ = b;

            // The dashboard's snapshot (and MCP's reads, made from it) has
            // all of a's and nothing of b's.
            let snap = crate::snapshot::build(&db, a, "").await.unwrap();
            let shown = snap.to_string();
            for p in ["svc", "cl", "prj", "dpl", "srv", "bld", "vol", "act", "fw"] {
                assert!(shown.contains(&id(p, "a")), "a's {p} in /state");
            }
            assert!(
                !shown.contains(&format!("b{run}")),
                "nothing of b's in /state"
            );
            // Its parts read only a's teams' rows even when handed b's team.
            let mut tx = crate::rls::begin(&db, a).await.unwrap();
            assert_eq!(team_ids(&mut tx).await.unwrap(), vec![id("team", "a")]);
            let teams_b = [id("team", "b")];
            assert!(
                crate::databases::listing(&mut tx, &teams_b, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                crate::teambackup::volume_statuses(&mut tx, &teams_b[0])
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (_, servers) = crate::cloud::snapshot(&mut tx, &teams_b).await.unwrap();
            assert_eq!(servers, json!([]));
            assert!(
                crate::sandbox::server_views(&mut tx, &teams_b, None)
                    .await
                    .unwrap()
                    .iter()
                    .all(|v| v["teamId"].is_null())
            );
            assert!(
                crate::teambackup::snapshot(&mut tx, &teams_b)
                    .await
                    .unwrap()
                    .iter()
                    .all(|t| t["history"] == json!([]))
            );
            tx.rollback().await.unwrap();

            // REST reads: b's things aren't there for a.
            use axum::extract::{Path, State};
            let session = |u: &User| crate::auth::SessionUser(u.clone());
            let Err(e) =
                crate::ops::service_preview(State(s.clone()), session(a), Path(id("svc", "b")))
                    .await
            else {
                panic!("b's service preview");
            };
            assert_eq!(e.status, StatusCode::NOT_FOUND);
            let e = crate::ops::fetch_service_logs(&s, a, &id("svc", "b"), Default::default())
                .await
                .unwrap_err();
            assert_eq!(e.status, StatusCode::NOT_FOUND);
            let e = crate::volumes::snapshots(State(s.clone()), session(a), Path(id("vol", "b")))
                .await
                .unwrap_err();
            assert_eq!(e.status, StatusCode::NOT_FOUND);
            // The scoped transaction is what hides it: the same lookup as
            // the owner finds it.
            let mut tx = crate::rls::begin(&db, a).await.unwrap();
            let e = crate::ops::team_of(&mut tx, "volumes", &id("vol", "b"))
                .await
                .unwrap_err();
            assert_eq!(e.status, StatusCode::NOT_FOUND);
            tx.rollback().await.unwrap();
            let mut tx = db.begin().await.unwrap();
            assert_eq!(
                crate::ops::team_of(&mut tx, "volumes", &id("vol", "b"))
                    .await
                    .unwrap(),
                id("team", "b")
            );
            tx.rollback().await.unwrap();

            // MCP: a's service is there, b's isn't, and b's team isn't a's.
            let got = crate::mcp::run_tool(
                &s,
                a,
                "get_service",
                &json!({ "service_id": id("svc", "a") }),
            )
            .await
            .unwrap();
            assert!(got.to_string().contains(&id("svc", "a")));
            assert!(
                crate::mcp::run_tool(
                    &s,
                    a,
                    "get_service",
                    &json!({ "service_id": id("svc", "b") })
                )
                .await
                .is_err()
            );
            assert!(
                crate::mcp::run_tool(&s, a, "list_services", &json!({ "team": id("team", "b") }))
                    .await
                    .is_err()
            );
            let listed = crate::mcp::run_tool(&s, a, "list_servers", &json!({}))
                .await
                .unwrap();
            assert!(!listed.to_string().contains(&format!("b{run}")));

            for t in ["a", "b"] {
                sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
                    .bind(id("team", t))
                    .execute(&db)
                    .await
                    .unwrap();
                sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                    .bind(id("usr", t))
                    .execute(&db)
                    .await
                    .unwrap();
            }
        });
    }

    pub(crate) fn state(db: PgPool) -> crate::AppState {
        let config = crate::Config {
            agent_api_url: String::new(),
            agent_image: String::new(),
            waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
            geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: String::new(),
            relay_addr: None,
        };
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        crate::AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::for_tests()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        }
    }
}

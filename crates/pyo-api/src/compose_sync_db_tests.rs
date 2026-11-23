//! Compose sync in the database, against a throwaway Postgres
//! (DATABASE_URL): what `syncCompose` writes, as `pyo deploy -f` runs it.
//! The plan comes from the `planComposeSync` route's handler and the apply
//! goes through `run_json`, so row security and the role and key checks
//! are the real ones.
//!
//! DATABASE_URL=… cargo test -p pyo-api --lib -- --ignored compose_sync::db_tests
//!
//! Run them on their own, as above: the process has one vault, and other
//! modules' tests set a secret key for the whole process.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::auth::{KeyAccess, KeyScope, User};
use crate::ids::random_string;
use crate::teams_rls_tests::{cloud_db, drop_db, installation_db};
use crate::teamsec_tests::run_big;

/// A team with a connected cluster of one server (tagged `edge`), and its
/// owner.
struct Team {
    id: String,
    cluster: String,
    owner: User,
}

struct World {
    s: crate::AppState,
    db: PgPool,
    run: String,
}

/// The file every test starts from. Its domain is the run's own: a
/// hostname is one service's across the whole installation.
fn file(tag: &str) -> String {
    format!(
        r#"name: cloud
services:
  api:
    image: ghcr.io/acme/api:1.2
    environment:
      LOG_LEVEL: info
    ports: ["8080:8080"]
    x-pyo:
      domains: [api-{tag}.example.com]
      behindCdn: cloudflare
      health: /readyz
      replicas: 2
      secrets: [API_TOKEN]
  worker:
    image: ghcr.io/acme/worker:1.2
    volumes: ["work:/data"]
volumes:
  work: {{}}
"#
    )
}

/// The file with one more service (`yaml`, indented as a service is).
fn with_service(text: &str, yaml: &str) -> String {
    text.replace("volumes:\n  work:", &format!("{yaml}volumes:\n  work:"))
}

/// The file without its worker.
fn without_worker(text: &str) -> String {
    text.split("  worker:").next().unwrap().to_owned()
}

fn status(e: &ApiError) -> StatusCode {
    e.status
}

impl World {
    async fn new(db: PgPool) -> Self {
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        Self {
            s: crate::sandbox::e2e::test_state(db.clone()),
            db,
            run: random_string(8).to_lowercase(),
        }
    }

    /// An account. Not in any team yet.
    async fn account(&self, name: &str) -> User {
        let id = format!("usr_{}{name}", self.run);
        let user = User {
            id: id.clone(),
            name: name.to_owned(),
            email: format!("{name}{}@sync.test", self.run),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $3, '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .bind(name)
        .execute(&self.db)
        .await
        .unwrap();
        user
    }

    /// An account in `team` with `role`.
    async fn member(&self, team: &str, name: &str, role: &str) -> User {
        let user = self.account(name).await;
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
            .bind(format!("mem_{}{name}", self.run))
            .bind(team)
            .bind(&user.id)
            .bind(&user.email)
            .bind(role)
            .execute(&self.db)
            .await
            .unwrap();
        user
    }

    async fn team(&self, tag: &str) -> Team {
        let id = format!("team_{}{tag}", self.run);
        let cluster = format!("cl_{}{tag}", self.run);
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&id)
            .execute(&self.db)
            .await
            .unwrap();
        let owner = self.member(&id, &format!("owner{tag}"), "owner").await;
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())")
            .bind(&cluster)
            .bind(&id)
            .execute(&self.db)
            .await
            .unwrap();
        sqlx::query("UPDATE pyo.teams SET default_cluster_id = $2 WHERE id = $1")
            .bind(&id)
            .bind(&cluster)
            .execute(&self.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO pyo.servers (id, cluster_id, name, report) VALUES ($1, $2, 'one', '{}')",
        )
        .bind(format!("srv_{}{tag}", self.run))
        .bind(&cluster)
        .execute(&self.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pyo.server_tags (cluster_id, name, tags) VALUES ($1, 'one', '{edge}')",
        )
        .bind(&cluster)
        .execute(&self.db)
        .await
        .unwrap();
        Team { id, cluster, owner }
    }

    /// `planComposeSync`, as `pyo deploy -f compose.yaml` (and its
    /// `--dry-run`) asks for it. `project` is `--project`.
    async fn plan(
        &self,
        user: &User,
        team: &str,
        project: Option<&str>,
        text: &str,
    ) -> ApiResult<SyncPlan> {
        super::plan(
            axum::extract::State(self.s.clone()),
            crate::auth::SessionUser(user.clone()),
            crate::error::Json(SyncBody {
                team_id: team.to_owned(),
                project_id: project.map(str::to_owned),
                file: Some(text.to_owned()),
                env: None,
                repo: None,
                cluster_id: None,
                name: Some("cloud".into()),
                file_name: Some("compose.yaml".into()),
            }),
        )
        .await
        .map(|j| j.0)
    }

    /// `syncCompose` with a plan's file.
    async fn sync(
        &self,
        user: &User,
        project: &str,
        file: &ComposePlan,
        prune: bool,
    ) -> ApiResult<Value> {
        crate::ops::run_json(
            &self.s,
            user,
            json!({ "op": "syncCompose", "projectId": project, "plan": file, "prune": prune }),
        )
        .await
    }

    /// Plans `text` and applies it, as the command does without `--dry-run`.
    async fn deploy(
        &self,
        user: &User,
        team: &str,
        project: Option<&str>,
        text: &str,
        prune: bool,
    ) -> ApiResult<Value> {
        let plan = self.plan(user, team, project, text).await?;
        let project = plan.project.expect("a project to sync").id;
        self.sync(user, &project, &plan.file, prune).await
    }

    /// The first `pyo deploy -f`: there's no project for the file yet, so
    /// it's imported. Returns the project.
    async fn import(&self, team: &Team, text: &str) -> String {
        let plan = self.plan(&team.owner, &team.id, None, text).await.unwrap();
        assert!(plan.project.is_none(), "no project for this file yet");
        assert!(plan.services.iter().all(|s| s.status == "added"));
        let out = crate::ops::run_json(
            &self.s,
            &team.owner,
            json!({ "op": "importCompose", "teamId": team.id, "plan": plan.file }),
        )
        .await
        .unwrap();
        self.settle(&team.id).await;
        out["projectId"].as_str().unwrap().to_owned()
    }

    /// The agent finished every deploy of the team (there's none here).
    async fn settle(&self, team: &str) {
        sqlx::query("UPDATE pyo.deployments SET status = 'succeeded' WHERE team_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled')")
            .bind(team)
            .execute(&self.db)
            .await
            .unwrap();
    }

    async fn project(&self, team: &Team, namespace: &str) -> String {
        let id = format!("prj_{}{namespace}", self.run);
        crate::ops::run_json(
            &self.s,
            &team.owner,
            json!({ "op": "createProject", "id": id, "teamId": team.id, "name": namespace, "namespace": namespace, "description": "" }),
        )
        .await
        .unwrap();
        id
    }

    /// A service made in the dashboard (`file` None), or one that
    /// remembers another Compose file.
    async fn service(&self, team: &Team, project: &str, name: &str, file: Option<&str>) -> String {
        let text = format!("services:\n  {name}:\n    image: ghcr.io/acme/{name}:1\n");
        let plan = compose::plan(&compose::Input {
            file: &text,
            name_hint: "x",
            ..Default::default()
        })
        .unwrap();
        let id = crate::ids::new_id("svc");
        let new_id = |p: &str| crate::ids::new_id(p);
        let spec = compose::service_spec(
            &plan.services[0],
            file,
            &id,
            &team.id,
            project,
            &team.cluster,
            pyo_proto::now_ms(),
            &new_id,
        );
        crate::ops::run_json(
            &self.s,
            &team.owner,
            json!({ "op": "createService", "service": spec, "deploymentId": crate::ids::new_id("dpl") }),
        )
        .await
        .unwrap();
        self.settle(&team.id).await;
        id
    }

    /// Everything of a team a sync could write: its projects, services,
    /// deployments, volumes and activity, whole rows.
    async fn snapshot(&self, team: &str) -> Value {
        let mut out = json!({});
        for table in ["projects", "services", "deployments", "volumes", "activity"] {
            let rows: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) FROM pyo.{table} a WHERE a.team_id = $1"
            )))
            .bind(team)
            .fetch_one(&self.db)
            .await
            .unwrap();
            out[table] = rows;
        }
        out
    }

    /// A project's service by name: id, spec as stored, behind_cdn, deleting.
    async fn find(&self, project: &str, name: &str) -> Option<(String, Value, Option<String>, bool)> {
        sqlx::query_as(
            "SELECT id, spec, behind_cdn, deleting FROM pyo.services WHERE project_id = $1 AND name = $2",
        )
        .bind(project)
        .bind(name)
        .fetch_optional(&self.db)
        .await
        .unwrap()
    }

    async fn get(&self, project: &str, name: &str) -> (String, Value, Option<String>, bool) {
        self.find(project, name).await.expect("the service")
    }

    async fn names(&self, project: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT name FROM pyo.services WHERE project_id = $1 AND NOT deleting ORDER BY name",
        )
        .bind(project)
        .fetch_all(&self.db)
        .await
        .unwrap()
    }

    async fn deployments(&self, service: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pyo.deployments WHERE service_id = $1")
            .bind(service)
            .fetch_one(&self.db)
            .await
            .unwrap()
    }
}

/// A variable of a spec as stored: (its stored value, the value opened, secret).
async fn env(spec: &Value, key: &str) -> Option<(String, String, bool)> {
    let e = spec["env"]
        .as_array()?
        .iter()
        .find(|e| e["key"] == json!(key))?;
    let stored = e["value"].as_str().unwrap_or("").to_owned();
    let opened = crate::vault::get().open(&stored).await.unwrap();
    Some((stored, opened, e["secret"] == json!(true)))
}

fn hostnames(spec: &Value) -> Vec<&str> {
    spec["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["hostname"].as_str())
        .collect()
}

fn statuses(plan: &SyncPlan) -> Vec<(&str, &str)> {
    plan.services
        .iter()
        .map(|s| (s.name.as_str(), s.status.as_str()))
        .collect()
}

/// What a sync says it did: (name, status) in its order.
fn done(out: &Value) -> Vec<(String, String)> {
    out["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_owned(),
                v["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
}

/// The first apply makes the project and its services as the file and its
/// `x-pyo` say. The same file again changes nothing: no row is written,
/// nothing is deployed, nothing is logged. Into an empty project named
/// with `--project`, the sync itself makes them, once.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn the_first_apply_makes_the_services_and_the_second_changes_nothing() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let project = w.import(&t, &text).await;
        assert_eq!(w.names(&project).await, ["api", "worker"]);

        let (api, spec, cdn, _) = w.get(&project, "api").await;
        assert_eq!(spec["compose"], json!({ "file": "compose.yaml", "service": "api" }));
        assert_eq!(spec["replicas"], 2);
        assert_eq!(spec["healthCheck"]["path"], "/readyz");
        assert_eq!(hostnames(&spec), [format!("api-{}.example.com", w.run)]);
        assert_eq!(cdn.as_deref(), Some("cloudflare"));
        let (_, token, secret) = env(&spec, "API_TOKEN").await.unwrap();
        assert!(secret && token.is_empty(), "a secret comes without a value");
        assert_eq!(w.deployments(&api).await, 1);

        // Again: found by the file's name, nothing to do, nothing written.
        let before = w.snapshot(&t.id).await;
        for prune in [false, true] {
            let plan = w.plan(&t.owner, &t.id, None, &text).await.unwrap();
            assert_eq!(plan.project.as_ref().map(|p| p.id.as_str()), Some(project.as_str()));
            assert_eq!(statuses(&plan), [("api", "unchanged"), ("worker", "unchanged")]);
            assert!(plan.services.iter().all(|s| s.changes.is_empty() && !s.deploy));
            let out = w.sync(&t.owner, &project, &plan.file, prune).await.unwrap();
            assert_eq!(out["services"], json!([]));
            assert_eq!(w.snapshot(&t.id).await, before, "prune: {prune}");
        }

        // Into an empty project: the sync adds them, and only once.
        let empty = w.project(&t, "empty").await;
        let other = file(&format!("{}b", w.run));
        let plan = w.plan(&t.owner, &t.id, Some(&empty), &other).await.unwrap();
        assert_eq!(statuses(&plan), [("api", "added"), ("worker", "added")]);
        let out = w.sync(&t.owner, &empty, &plan.file, false).await.unwrap();
        assert_eq!(done(&out), pairs(&[("api", "added"), ("worker", "added")]));
        assert_eq!(w.names(&empty).await, ["api", "worker"]);
        let (id, spec, cdn, _) = w.get(&empty, "api").await;
        assert_eq!(cdn.as_deref(), Some("cloudflare"));
        assert_eq!(spec["compose"]["service"], "api");
        assert_eq!(w.deployments(&id).await, 1);
        w.settle(&t.id).await;
        let before = w.snapshot(&t.id).await;
        let out = w.deploy(&t.owner, &t.id, Some(&empty), &other, false).await.unwrap();
        assert_eq!(out["services"], json!([]));
        assert_eq!(w.snapshot(&t.id).await, before);
    });
}

/// A new image, variables, replicas, health check, placement, domain and
/// CDN setting in the file change the service it was made from, in place:
/// the same row, one new deployment, and no second service. The service
/// the file didn't change isn't written.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_changed_file_updates_the_service_it_made() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let project = w.import(&t, &text).await;
        let (api, _, _, _) = w.get(&project, "api").await;
        let (worker, ..) = w.get(&project, "worker").await;
        let worker_row = |snap: &Value| {
            snap["services"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == json!(worker))
                .cloned()
        };
        let before = w.snapshot(&t.id).await;

        let next = text
            .replace("api:1.2", "api:1.3")
            .replace("LOG_LEVEL: info", "LOG_LEVEL: debug\n      REGION: sin")
            .replace(
                &format!("domains: [api-{}.example.com]", w.run),
                &format!("domains: [api-{0}.example.com, www-{0}.example.com]", w.run),
            )
            .replace("behindCdn: cloudflare", "behindCdn: none")
            .replace("health: /readyz", "health: /healthz")
            .replace("replicas: 2", "replicas: 3\n      placement: { tags: [edge] }");
        let plan = w.plan(&t.owner, &t.id, None, &next).await.unwrap();
        assert_eq!(statuses(&plan), [("api", "changed"), ("worker", "unchanged")]);
        let mut fields: Vec<&str> = plan.services[0].changes.iter().map(|c| c.field.as_str()).collect();
        fields.sort();
        let www = format!("domain www-{}.example.com", w.run);
        assert_eq!(
            fields,
            ["behind CDN", www.as_str(), "env LOG_LEVEL", "env REGION", "health check", "image", "placement", "replicas"]
        );

        let out = w.sync(&t.owner, &project, &plan.file, false).await.unwrap();
        assert_eq!(done(&out), pairs(&[("api", "changed")]));
        assert_eq!(out["services"][0]["id"], json!(api));
        assert_eq!(w.names(&project).await, ["api", "worker"], "no second service");
        let (id, spec, cdn, _) = w.get(&project, "api").await;
        assert_eq!(id, api);
        assert_eq!(spec["source"]["image"], "ghcr.io/acme/api");
        assert_eq!(spec["source"]["reference"], "1.3");
        assert_eq!(env(&spec, "LOG_LEVEL").await.unwrap().1, "debug");
        assert_eq!(env(&spec, "REGION").await.unwrap().1, "sin");
        assert_eq!(spec["replicas"], 3);
        assert_eq!(spec["healthCheck"]["type"], "http");
        assert_eq!(spec["healthCheck"]["path"], "/healthz");
        assert_eq!(spec["placement"], json!({ "servers": [], "tags": ["edge"] }));
        assert_eq!(
            hostnames(&spec),
            [format!("api-{}.example.com", w.run), format!("www-{}.example.com", w.run)]
        );
        assert_eq!(cdn, None);
        // Deployed once more, with what the file says now.
        assert_eq!(w.deployments(&api).await, 2);
        let dpl = out["services"][0]["deploymentId"].as_str().unwrap();
        let (service, deployed): (String, Value) =
            sqlx::query_as("SELECT service_id, spec FROM pyo.deployments WHERE id = $1")
                .bind(dpl)
                .fetch_one(&w.db)
                .await
                .unwrap();
        assert_eq!(service, api);
        assert_eq!(deployed["image"], "ghcr.io/acme/api:1.3");
        assert_eq!(deployed["replicas"], 3);
        // The worker: not written, not deployed.
        let after = w.snapshot(&t.id).await;
        assert_eq!(worker_row(&after), worker_row(&before));
        assert_eq!(w.deployments(&worker).await, 1);
        assert_eq!(after["volumes"], before["volumes"]);

        // The new file again: nothing.
        w.settle(&t.id).await;
        let before = w.snapshot(&t.id).await;
        let out = w.deploy(&t.owner, &t.id, None, &next, false).await.unwrap();
        assert_eq!(out["services"], json!([]));
        assert_eq!(w.snapshot(&t.id).await, before);
    });
}

/// A service no longer in the file stays, and is only listed. With
/// `prune` it's deleted (its volume kept), and only it: not a service made
/// in the dashboard, not one made from another file, not the same name in
/// another project, not another team's.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn prune_deletes_only_what_this_file_made_in_this_project() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let (a, b) = (w.team("a").await, w.team("b").await);
        let text = file(&w.run);
        let project = w.import(&a, &text).await;
        let manual = w.service(&a, &project, "manual", None).await;
        let legacy = w.service(&a, &project, "legacy", Some("other.yaml")).await;
        // The same worker in another project of the team, and in another team.
        let side = w.project(&a, "side").await;
        let side_text = file(&format!("{}s", w.run));
        w.deploy(&a.owner, &a.id, Some(&side), &side_text, false).await.unwrap();
        w.settle(&a.id).await;
        let theirs = w.import(&b, &file(&format!("{}b", w.run))).await;
        let (worker, ..) = w.get(&project, "worker").await;
        let (side_worker, ..) = w.get(&side, "worker").await;

        let next = without_worker(&text);
        let plan = w.plan(&a.owner, &a.id, None, &next).await.unwrap();
        assert_eq!(plan.project.as_ref().unwrap().id, project);
        let by = |name: &str| plan.services.iter().find(|s| s.name == name).unwrap();
        assert_eq!(by("api").status, "unchanged");
        for name in ["worker", "manual", "legacy"] {
            assert_eq!(by(name).status, "notInFile", "{name}");
        }
        assert!(by("worker").from_file);
        assert!(!by("manual").from_file && !by("legacy").from_file);
        assert_eq!(by("manual").notes[0].message, "It wasn't made from a Compose file.");
        assert_eq!(by("legacy").notes[0].message, "It was made from other.yaml.");

        // Without prune: listed, and nothing is written.
        let (before_a, before_b) = (w.snapshot(&a.id).await, w.snapshot(&b.id).await);
        let out = w.sync(&a.owner, &project, &plan.file, false).await.unwrap();
        assert_eq!(out["services"], json!([]));
        assert_eq!(w.snapshot(&a.id).await, before_a);

        // With it: the worker of this project, and nothing else.
        let out = w.sync(&a.owner, &project, &plan.file, true).await.unwrap();
        assert_eq!(done(&out), pairs(&[("worker", "deleted")]));
        assert_eq!(out["services"][0]["id"], json!(worker));
        let deleting: Vec<String> =
            sqlx::query_scalar("SELECT id FROM pyo.services WHERE team_id = $1 AND deleting")
                .bind(&a.id)
                .fetch_all(&w.db)
                .await
                .unwrap();
        assert_eq!(deleting, std::slice::from_ref(&worker));
        assert_eq!(w.names(&project).await, ["api", "legacy", "manual"]);
        assert_eq!(w.names(&side).await, ["api", "worker"]);
        assert!(!w.get(&side, "worker").await.3);
        assert_eq!(w.get(&project, "manual").await.0, manual);
        assert_eq!(w.get(&project, "legacy").await.0, legacy);
        // Its volume is kept, and the other project's worker keeps its own.
        let volumes: Vec<(Option<String>, bool, String)> = sqlx::query_as(
            "SELECT service_id, deleting, data->>'name' FROM pyo.volumes WHERE team_id = $1 ORDER BY project_id = $2 DESC",
        )
        .bind(&a.id)
        .bind(&project)
        .fetch_all(&w.db)
        .await
        .unwrap();
        assert_eq!(volumes.len(), 2);
        assert!(volumes.iter().all(|v| !v.1), "no volume is deleted");
        assert_eq!(volumes[1].0.as_deref(), Some(side_worker.as_str()));
        // It's still the worker's until the agent has removed the service.
        assert_eq!(volumes[0].0.as_deref(), Some(worker.as_str()));
        assert_eq!(volumes[0].2, "work");
        // The other team: not a row touched.
        assert_eq!(w.snapshot(&b.id).await, before_b);
        assert_eq!(w.names(&theirs).await, ["api", "worker"]);

        // Again with prune: it's gone from the plan, and nothing more goes.
        let before = w.snapshot(&a.id).await;
        let plan = w.plan(&a.owner, &a.id, None, &next).await.unwrap();
        assert!(!plan.services.iter().any(|s| s.name == "worker"));
        let out = w.sync(&a.owner, &project, &plan.file, true).await.unwrap();
        assert_eq!(out["services"], json!([]));
        assert_eq!(w.snapshot(&a.id).await, before);
    });
}

/// A secret named in `x-pyo.secrets` gets its value in Pyo. A sync keeps
/// it, whatever else of the service changes and whatever the file says for
/// that variable: the stored value is the same, it's deployed with it, and
/// neither the plan nor the sync's answer shows it.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_sync_keeps_a_secrets_value() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let project = w.import(&t, &text).await;
        let (api, mut spec, _, _) = w.get(&project, "api").await;

        // Its value, set in Pyo.
        let value = format!("s3cret-{}", w.run);
        crate::vault::get().open_env(&mut spec).await.unwrap();
        spec["id"] = json!(api);
        for e in spec["env"].as_array_mut().unwrap() {
            if e["key"] == json!("API_TOKEN") {
                e["value"] = json!(value);
            }
        }
        crate::ops::run_json(&w.s, &t.owner, json!({ "op": "saveService", "service": spec }))
            .await
            .unwrap();
        let (_, spec, _, _) = w.get(&project, "api").await;
        let (stored, opened, secret) = env(&spec, "API_TOKEN").await.unwrap();
        assert!(secret);
        assert_eq!(opened, value);
        if std::env::var("PYO_SECRET_KEY").is_ok() {
            assert_ne!(stored, value, "sealed with the team's key");
        }

        // The same file: the value set in Pyo is no change.
        let before = w.snapshot(&t.id).await;
        let out = w.deploy(&t.owner, &t.id, None, &text, false).await.unwrap();
        assert_eq!(out["services"], json!([]));
        assert_eq!(w.snapshot(&t.id).await, before);

        // The file now gives it a value, a second secret, and a new image.
        let next = text
            .replace("api:1.2", "api:1.3")
            .replace("LOG_LEVEL: info", "LOG_LEVEL: info\n      API_TOKEN: from-the-file")
            .replace("secrets: [API_TOKEN]", "secrets: [API_TOKEN, SIGNING_KEY]");
        let plan = w.plan(&t.owner, &t.id, None, &next).await.unwrap();
        let s = plan.services.iter().find(|s| s.name == "api").unwrap();
        assert!(plan.warnings.iter().any(|x| x.field == "x-pyo.secrets"
            && x.message
                == "Secrets take their values from Pyo, not the file, so the file's API_TOKEN isn't used."));
        let mut fields: Vec<&str> = s.changes.iter().map(|c| c.field.as_str()).collect();
        fields.sort();
        assert_eq!(fields, ["image", "secret SIGNING_KEY"]);
        assert!(!serde_json::to_string(&plan.services).unwrap().contains(&value));

        let out = w.sync(&t.owner, &project, &plan.file, false).await.unwrap();
        assert_eq!(done(&out), pairs(&[("api", "changed")]));
        assert!(!out.to_string().contains(&value));
        let (id, spec, _, _) = w.get(&project, "api").await;
        assert_eq!(id, api);
        assert_eq!(spec["source"]["reference"], "1.3");
        let (after, opened, secret) = env(&spec, "API_TOKEN").await.unwrap();
        assert!(secret, "still a secret");
        assert_eq!(opened, value, "not blanked, not the file's");
        assert_eq!(after, stored, "the stored value isn't written again");
        let (_, new, secret) = env(&spec, "SIGNING_KEY").await.unwrap();
        assert!(secret && new.is_empty());
        // It's deployed with the value it has in Pyo.
        let dpl = out["services"][0]["deploymentId"].as_str().unwrap();
        let deployed: Value = sqlx::query_scalar("SELECT spec FROM pyo.deployments WHERE id = $1")
            .bind(dpl)
            .fetch_one(&w.db)
            .await
            .unwrap();
        assert_eq!(env(&deployed, "API_TOKEN").await.unwrap().1, value);

        // The file stops naming it a secret and keeps its own value for
        // it, with another variable changed: in Pyo it's still the secret.
        w.settle(&t.id).await;
        let again = next
            .replace("LOG_LEVEL: info", "LOG_LEVEL: warn")
            .replace("secrets: [API_TOKEN, SIGNING_KEY]", "secrets: [SIGNING_KEY]");
        let plan = w.plan(&t.owner, &t.id, None, &again).await.unwrap();
        let s = plan.services.iter().find(|s| s.name == "api").unwrap();
        assert!(
            s.notes.iter().any(|n| n.field == "env API_TOKEN"
                && n.message == "It's a secret in Pyo, so its value stays.")
        );
        let fields: Vec<&str> = s.changes.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(fields, ["env LOG_LEVEL"]);
        let out = w.sync(&t.owner, &project, &plan.file, false).await.unwrap();
        assert!(!out.to_string().contains(&value));
        let (_, spec, _, _) = w.get(&project, "api").await;
        assert_eq!(env(&spec, "LOG_LEVEL").await.unwrap().1, "warn");
        let (after, opened, secret) = env(&spec, "API_TOKEN").await.unwrap();
        assert!(secret);
        assert_eq!(opened, value);
        assert_eq!(after, stored);
    });
}

/// Without `--project` the file goes to the project made from a file of
/// its name. With it, to the project named and no other, also when that
/// one has nothing of the file yet.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_sync_goes_to_the_project_named() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let cloud = w.import(&t, &text).await;
        let other = w.project(&t, "other").await;

        let plan = w.plan(&t.owner, &t.id, None, &text).await.unwrap();
        assert_eq!(plan.project.unwrap().id, cloud);
        let e = w
            .plan(&t.owner, &t.id, Some("prj_nosuchproject"), &text)
            .await
            .unwrap_err();
        assert_eq!(status(&e), StatusCode::NOT_FOUND);

        // Named: the other project gets the services, the first isn't written.
        let rows = |snap: &Value, project: &str| -> Vec<Value> {
            snap["services"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["project_id"] == json!(project))
                .cloned()
                .collect()
        };
        let before = w.snapshot(&t.id).await;
        let text_b = file(&format!("{}b", w.run)).replace("api:1.2", "api:9.9");
        let plan = w.plan(&t.owner, &t.id, Some(&other), &text_b).await.unwrap();
        assert_eq!(plan.project.as_ref().unwrap().id, other);
        assert_eq!(statuses(&plan), [("api", "added"), ("worker", "added")]);
        let out = w.sync(&t.owner, &other, &plan.file, true).await.unwrap();
        assert_eq!(out["projectId"], json!(other));
        assert_eq!(w.names(&other).await, ["api", "worker"]);
        assert_eq!(w.get(&other, "api").await.1["source"]["reference"], "9.9");
        let after = w.snapshot(&t.id).await;
        assert_eq!(rows(&after, &cloud), rows(&before, &cloud));
        assert_eq!(w.get(&cloud, "api").await.1["source"]["reference"], "1.2");

        // Both have services of a compose.yaml now: without a name, still
        // the one in the file's own namespace.
        let plan = w.plan(&t.owner, &t.id, None, &text).await.unwrap();
        assert_eq!(plan.project.unwrap().id, cloud);
    });
}

/// A team's sync stays in the team: another team's project isn't found by
/// name or by id, its cluster isn't one to deploy to, a hostname it holds
/// isn't taken, and none of its rows is written.
async fn teams_are_apart(db: PgPool) {
    let w = World::new(db).await;
    let (a, b) = (w.team("a").await, w.team("b").await);
    let text_a = file(&w.run);
    let text_b = file(&format!("{}b", w.run));
    let theirs = w.import(&a, &text_a).await;
    let ours = w.import(&b, &text_b).await;
    let (before_a, before_b) = (w.snapshot(&a.id).await, w.snapshot(&b.id).await);
    let bob = &b.owner;

    // Both teams have a project "cloud" made from compose.yaml: each finds its own.
    let plan = w.plan(bob, &b.id, None, &text_b).await.unwrap();
    assert_eq!(plan.project.as_ref().unwrap().id, ours);
    let file_b = plan.file;
    let plan = w.plan(&a.owner, &a.id, None, &text_a).await.unwrap();
    assert_eq!(plan.project.unwrap().id, theirs);

    // Their team, or their project under our team: not there.
    for (team, project) in [(&a.id, None), (&a.id, Some(theirs.as_str())), (&b.id, Some(theirs.as_str()))] {
        let e = w.plan(bob, team, project, &text_b).await.unwrap_err();
        assert_eq!(status(&e), StatusCode::NOT_FOUND, "{}", e.message);
    }
    for prune in [false, true] {
        let e = w.sync(bob, &theirs, &file_b, prune).await.unwrap_err();
        assert_eq!(status(&e), StatusCode::NOT_FOUND, "{}", e.message);
    }
    // A file of ours with one service more, sent to their cluster.
    let more = with_service(&text_b, "  cache:\n    image: redis:7\n");
    let plan = w.plan(bob, &b.id, None, &more).await.unwrap();
    let e = crate::ops::run_json(
        &w.s,
        bob,
        json!({ "op": "syncCompose", "projectId": ours, "clusterId": a.cluster, "plan": plan.file }),
    )
    .await
    .unwrap_err();
    assert_eq!(status(&e), StatusCode::NOT_FOUND, "{}", e.message);
    // A hostname their service has: refused, and their service isn't named.
    let taken = text_b.replace(
        &format!("api-{}b.example.com", w.run),
        &format!("api-{0}b.example.com, api-{0}.example.com", w.run),
    );
    let e = w.deploy(bob, &b.id, None, &taken, false).await.unwrap_err();
    assert_eq!(status(&e), StatusCode::CONFLICT);
    assert_eq!(
        e.message,
        format!("api-{}.example.com is already used by another service.", w.run)
    );
    assert_eq!(w.snapshot(&a.id).await, before_a);
    assert_eq!(w.snapshot(&b.id).await, before_b);

    // What our team does to its own project leaves theirs as it was.
    let out = w
        .deploy(bob, &b.id, None, &without_worker(&text_b).replace("api:1.2", "api:2.0"), true)
        .await
        .unwrap();
    assert_eq!(done(&out), pairs(&[("api", "changed"), ("worker", "deleted")]));
    assert_eq!(w.names(&ours).await, ["api"]);
    assert_eq!(w.snapshot(&a.id).await, before_a);
    assert_eq!(w.names(&theirs).await, ["api", "worker"]);
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_team_cant_sync_into_another_teams_project() {
    run_big(async {
        teams_are_apart(installation_db().await).await;
    });
}

/// The same on Pyo Cloud's login: not a superuser, held by row security.
/// In a database of its own, so with PYO_SECRET_KEY set it runs alone:
/// the process has one vault, and it keeps its data keys in the database
/// of the first test that started.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_team_cant_sync_into_another_teams_project_on_a_login_without_superuser() {
    run_big(async {
        let (db, name) = cloud_db().await;
        teams_are_apart(db.clone()).await;
        drop_db(db, &name).await;
    });
}

/// Who may sync: the team's developers, admins and owners. A viewer only
/// sees the plan. The installation's admin has no say in a team they're
/// not in. An API key needs full access, in a team it's for.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn roles_and_api_keys_decide_who_syncs() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let (a, b) = (w.team("a").await, w.team("b").await);
        let text = file(&w.run);
        let project = w.import(&a, &text).await;
        let before = w.snapshot(&a.id).await;
        let next = text.replace("replicas: 2", "replicas: 3");
        let file_next = w.plan(&a.owner, &a.id, None, &next).await.unwrap().file;

        // A viewer: the plan, not the sync.
        let viewer = w.member(&a.id, "viewer", "viewer").await;
        let plan = w.plan(&viewer, &a.id, None, &next).await.unwrap();
        assert_eq!(statuses(&plan), [("api", "changed"), ("worker", "unchanged")]);
        let e = w.sync(&viewer, &project, &file_next, false).await.unwrap_err();
        assert_eq!(status(&e), StatusCode::FORBIDDEN);
        assert_eq!(e.message, "Viewers can't make changes.");

        // The installation's admin, and an account in no team.
        let mut admin = w.account("admin").await;
        admin.is_admin = true;
        sqlx::query("UPDATE pyo.users SET is_admin = true WHERE id = $1")
            .bind(&admin.id)
            .execute(&w.db)
            .await
            .unwrap();
        let nobody = w.account("nobody").await;
        for user in [&admin, &nobody, &b.owner] {
            let e = w.plan(user, &a.id, None, &next).await.unwrap_err();
            assert_eq!(status(&e), StatusCode::NOT_FOUND, "{}", user.name);
            let e = w.sync(user, &project, &file_next, true).await.unwrap_err();
            assert_eq!(status(&e), StatusCode::NOT_FOUND, "{}", user.name);
        }

        // API keys of the owner.
        let key = |access: KeyAccess, teams: Option<Vec<String>>| {
            let mut u = a.owner.clone();
            u.key = Some(KeyScope { access, teams });
            u
        };
        let read = key(KeyAccess::Read, None);
        w.plan(&read, &a.id, None, &next).await.unwrap();
        for (user, want) in [
            (read, StatusCode::FORBIDDEN),
            (key(KeyAccess::Deploy, None), StatusCode::FORBIDDEN),
            (key(KeyAccess::Sandboxes, None), StatusCode::FORBIDDEN),
            (key(KeyAccess::Full, Some(vec![b.id.clone()])), StatusCode::NOT_FOUND),
        ] {
            let e = w.sync(&user, &project, &file_next, true).await.unwrap_err();
            assert_eq!(status(&e), want, "{}", e.message);
        }
        // A deploy key deploys what's there. A sync changes settings, and
        // so does the first apply (an import): both need full access.
        let deploy = key(KeyAccess::Deploy, None);
        let e = w.sync(&deploy, &project, &file_next, false).await.unwrap_err();
        assert_eq!(
            e.message,
            "This API key can only deploy, roll back, restart, stop and scale services."
        );
        let e = crate::ops::run_json(
            &w.s,
            &deploy,
            json!({ "op": "importCompose", "teamId": a.id, "plan": file_next }),
        )
        .await
        .unwrap_err();
        assert_eq!(status(&e), StatusCode::FORBIDDEN);
        w.plan(&deploy, &a.id, None, &next).await.unwrap();
        assert_eq!(w.snapshot(&a.id).await, before, "none of them changed a thing");

        // A full key for this team, then a developer and an admin.
        let full = key(KeyAccess::Full, Some(vec![a.id.clone()]));
        let out = w.sync(&full, &project, &file_next, false).await.unwrap();
        assert_eq!(done(&out), pairs(&[("api", "changed")]));
        assert_eq!(w.get(&project, "api").await.1["replicas"], 3);
        for (name, role, replicas) in [("dev", "developer", 4), ("adm", "admin", 5)] {
            w.settle(&a.id).await;
            let user = w.member(&a.id, name, role).await;
            let text = text.replace("replicas: 2", &format!("replicas: {replicas}"));
            let out = w.deploy(&user, &a.id, None, &text, false).await.unwrap();
            assert_eq!(done(&out), pairs(&[("api", "changed")]), "{role}");
            assert_eq!(w.get(&project, "api").await.1["replicas"], replicas);
        }
    });
}

/// `--dry-run` writes nothing, and what it lists is what the sync then
/// does: the same plan comes back with the sync, each service it lists as
/// added, changed or deleted is, and the others aren't written.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn the_dry_run_plan_is_what_the_sync_does() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let project = w.import(&t, &text).await;
        let manual = w.service(&t, &project, "manual", None).await;
        let (api, ..) = w.get(&project, "api").await;
        let (worker, ..) = w.get(&project, "worker").await;

        let next = format!(
            "{}  cache:\n    image: redis:7\n",
            without_worker(&text)
                .replace("api:1.2", "api:1.3")
                .replace("replicas: 2", "replicas: 4")
        );
        let before = w.snapshot(&t.id).await;
        let plan = w.plan(&t.owner, &t.id, None, &next).await.unwrap();
        assert_eq!(w.snapshot(&t.id).await, before, "a plan writes nothing");
        assert_eq!(
            statuses(&plan),
            [("api", "changed"), ("cache", "added"), ("worker", "notInFile"), ("manual", "notInFile")]
        );
        let api_plan = &plan.services[0];
        assert_eq!(
            api_plan.changes,
            [
                SyncChange {
                    field: "image".into(),
                    from: Some("ghcr.io/acme/api:1.2".into()),
                    to: Some("ghcr.io/acme/api:1.3".into()),
                },
                SyncChange {
                    field: "replicas".into(),
                    from: Some("2".into()),
                    to: Some("4".into()),
                },
            ]
        );

        let out = w.sync(&t.owner, &project, &plan.file, true).await.unwrap();
        // The sync worked out the same plan.
        assert_eq!(out["plan"]["services"], serde_json::to_value(&plan.services).unwrap());
        // And did what it lists, no more.
        let expected: Vec<(String, String)> = plan
            .services
            .iter()
            .filter_map(|s| {
                let did = match s.status.as_str() {
                    "added" => "added",
                    "changed" => "changed",
                    "notInFile" if s.from_file => "deleted",
                    _ => return None,
                };
                Some((s.name.clone(), did.to_owned()))
            })
            .collect();
        assert_eq!(done(&out), expected);
        assert_eq!(
            expected,
            pairs(&[("api", "changed"), ("cache", "added"), ("worker", "deleted")])
        );
        let (id, spec, _, _) = w.get(&project, "api").await;
        assert_eq!(id, api);
        assert_eq!(spec["source"]["reference"], "1.3");
        assert_eq!(spec["replicas"], 4);
        // Each service the plan deploys got one deployment.
        for s in plan.services.iter().filter(|s| s.deploy) {
            let d = out["services"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == json!(s.name))
                .unwrap();
            let service: String =
                sqlx::query_scalar("SELECT service_id FROM pyo.deployments WHERE id = $1")
                    .bind(d["deploymentId"].as_str().unwrap())
                    .fetch_one(&w.db)
                    .await
                    .unwrap();
            assert_eq!(json!(service), d["id"], "{}", s.name);
        }
        assert_eq!(w.names(&project).await, ["api", "cache", "manual"]);
        assert!(w.get(&project, "worker").await.3, "{worker} is being deleted");
        let after = w.snapshot(&t.id).await;
        let row = |snap: &Value| {
            snap["services"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == json!(manual))
                .cloned()
        };
        assert_eq!(row(&after), row(&before));

        // Planned again, there's nothing left to do.
        w.settle(&t.id).await;
        let plan = w.plan(&t.owner, &t.id, None, &next).await.unwrap();
        assert_eq!(
            statuses(&plan),
            [("api", "unchanged"), ("cache", "unchanged"), ("manual", "notInFile")]
        );
    });
}

/// A sync is one transaction: when a service of the file is refused, the
/// services before it that were changed, added or deleted are as they
/// were, with no deployment and no activity. The plan doesn't run those
/// checks, so a dry run of the same file lists it as a change.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_refused_service_leaves_the_project_as_it_was() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let t = w.team("a").await;
        let text = file(&w.run);
        let project = w.import(&t, &text).await;
        let legacy = with_service(&text, "  legacy:\n    image: ghcr.io/acme/legacy:1\n");
        w.deploy(&t.owner, &t.id, None, &legacy, false)
            .await
            .unwrap();
        w.settle(&t.id).await;
        assert_eq!(w.names(&project).await, ["api", "legacy", "worker"]);
        let before = w.snapshot(&t.id).await;

        // api changes, cache is new, legacy goes, and the worker (last, as
        // it depends on the others) asks for a server the cluster hasn't.
        let good = with_service(
            &text.replace("api:1.2", "api:1.3").replace(
                "    volumes: [\"work:/data\"]",
                "    volumes: [\"work:/data\"]\n    depends_on: [api, cache]\n    x-pyo:\n      placement: { servers: [one] }",
            ),
            "  cache:\n    image: redis:7\n",
        );
        let bad = good.replace("servers: [one]", "servers: [nowhere]");
        let plan = w.plan(&t.owner, &t.id, None, &bad).await.unwrap();
        let order: Vec<&str> = plan.file.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(order, ["api", "cache", "worker"]);
        assert_eq!(
            statuses(&plan),
            [("api", "changed"), ("cache", "added"), ("worker", "changed"), ("legacy", "notInFile")]
        );
        let e = w.sync(&t.owner, &project, &plan.file, true).await.unwrap_err();
        assert_eq!(status(&e), StatusCode::BAD_REQUEST);
        assert_eq!(e.message, "nowhere isn't a server of this cluster.");
        assert_eq!(w.snapshot(&t.id).await, before, "nothing of it stays");

        // A hostname another service of the team has: refused as a whole too.
        let taken = good.replace(
            &format!("domains: [api-{}.example.com]", w.run),
            &format!("domains: [api-{0}.example.com, taken-{0}.example.com]", w.run),
        );
        let side = w.project(&t, "side").await;
        let holder = format!(
            "services:\n  holder:\n    image: nginx:1\n    ports: [\"80:80\"]\n    x-pyo:\n      domains: [taken-{}.example.com]\n",
            w.run
        );
        w.deploy(&t.owner, &t.id, Some(&side), &holder, false).await.unwrap();
        w.settle(&t.id).await;
        let before = w.snapshot(&t.id).await;
        let e = w.deploy(&t.owner, &t.id, None, &taken, true).await.unwrap_err();
        assert_eq!(status(&e), StatusCode::CONFLICT);
        assert_eq!(
            e.message,
            format!("taken-{}.example.com is already used by holder.", w.run)
        );
        assert_eq!(w.snapshot(&t.id).await, before);

        // The file put right goes through whole.
        let out = w.deploy(&t.owner, &t.id, None, &good, true).await.unwrap();
        assert_eq!(
            done(&out),
            pairs(&[("api", "changed"), ("cache", "added"), ("worker", "changed"), ("legacy", "deleted")])
        );
        assert_eq!(w.names(&project).await, ["api", "cache", "worker"]);
        assert_eq!(
            w.get(&project, "worker").await.1["placement"],
            json!({ "servers": ["one"], "tags": [] })
        );
    });
}

/// As it is today: the plan compares the file with the project and runs
/// none of the checks the apply does. A server the cluster hasn't, a
/// hostname another team's service has, and a very large replica change
/// are each listed as a plain change, so `--dry-run` reads clean, and the
/// sync of that same plan is refused (and changes nothing).
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_dry_run_doesnt_run_the_applys_checks() {
    run_big(async {
        let w = World::new(installation_db().await).await;
        let (a, b) = (w.team("a").await, w.team("b").await);
        let text = file(&w.run);
        let project = w.import(&a, &text).await;
        w.import(&b, &file(&format!("{}b", w.run))).await;
        let before = w.snapshot(&a.id).await;

        let nowhere =
            text.replace("replicas: 2", "replicas: 2\n      placement: { servers: [nowhere] }");
        let taken = text.replace(
            &format!("domains: [api-{}.example.com]", w.run),
            &format!("domains: [api-{0}.example.com, api-{0}b.example.com]", w.run),
        );
        let huge = text.replace("replicas: 2", "replicas: 500");
        let theirs = format!("domain api-{}b.example.com", w.run);
        for (next, field, want, message) in [
            (
                &nowhere,
                "placement",
                StatusCode::BAD_REQUEST,
                "nowhere isn't a server of this cluster.".to_owned(),
            ),
            (
                &taken,
                theirs.as_str(),
                StatusCode::CONFLICT,
                format!("api-{}b.example.com is already used by another service.", w.run),
            ),
            (&huge, "replicas", StatusCode::CONFLICT, String::new()),
        ] {
            let plan = w.plan(&a.owner, &a.id, None, next).await.unwrap();
            let api = plan.services.iter().find(|s| s.name == "api").unwrap();
            assert_eq!(api.status, "changed", "{field}");
            let fields: Vec<&str> = api.changes.iter().map(|c| c.field.as_str()).collect();
            assert_eq!(fields, [field]);
            assert!(api.notes.is_empty() && api.manual.is_empty(), "{field}: no hint");

            let e = w.sync(&a.owner, &project, &plan.file, false).await.unwrap_err();
            assert_eq!(status(&e), want, "{field}: {}", e.message);
            if message.is_empty() {
                assert!(e.needs_confirm, "{field}: {}", e.message);
            } else {
                assert_eq!(e.message, message);
            }
            assert_eq!(w.snapshot(&a.id).await, before, "{field}");
        }
    });
}

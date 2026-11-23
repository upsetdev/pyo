//! Previews: the rules on their own, then against a real Postgres
//! (DATABASE_URL, a throwaway one): a pull request's life (open, push,
//! close, reopen, merge), limits, forks, labels, idle ones, removal by
//! hand, and what the bot says (a fake provider records it).

use std::sync::Mutex;

use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::teamsec_tests::run_big;

fn pull(state: &str) -> PullRow {
    PullRow {
        id: "pr_1".into(),
        team_id: "team_1".into(),
        provider: "github".into(),
        installation_id: Some(7),
        repo: "acme/shop".into(),
        number: 42,
        head_ref: "search".into(),
        head_sha: "a".repeat(40),
        state: state.into(),
        pushed_ms: 1_000_000,
        ..Default::default()
    }
}

fn on() -> Settings {
    Settings {
        enabled: true,
        ..Default::default()
    }
}

// ── The rules ──────────────────────────────────────────────────────────────

#[test]
fn settings_are_checked_and_filled_in() {
    let git =
        json!({ "type": "git", "repoUrl": "https://github.com/acme/shop", "installationId": 7 });
    let mut spec = json!({ "source": git, "previews": { "enabled": true } });
    check_settings(&mut spec).unwrap();
    let s = Settings::of(&spec);
    assert_eq!(
        (
            s.limit,
            s.pulls.as_str(),
            s.forks.as_str(),
            s.access.as_str()
        ),
        (5, "all", "off", "public")
    );
    assert!(!s.copy_secrets);
    // Off is fine anywhere; on needs a repository Pyo reads pull requests of.
    let mut image = json!({ "source": { "type": "image", "image": "nginx" }, "previews": { "enabled": false } });
    check_settings(&mut image).unwrap();
    let mut image =
        json!({ "source": { "type": "image", "image": "nginx" }, "previews": { "enabled": true } });
    assert!(check_settings(&mut image).is_err());
    let mut plain = json!({ "source": { "type": "git", "repoUrl": "https://github.com/acme/shop" }, "previews": { "enabled": true } });
    assert!(
        check_settings(&mut plain).is_err(),
        "a public repository without the app"
    );
    let mut gitlab = json!({ "source": { "type": "git", "repoUrl": "https://gitlab.com/a/b", "gitConnectionId": "gitc_1" }, "previews": { "enabled": true } });
    check_settings(&mut gitlab).unwrap();
    let mut cron =
        json!({ "source": git, "workloadKind": "CronJob", "previews": { "enabled": true } });
    assert!(check_settings(&mut cron).is_err());
    for bad in [
        json!({ "limit": 0 }),
        json!({ "limit": 51 }),
        json!({ "idleDays": 91 }),
        json!({ "pulls": "some" }),
        json!({ "pulls": "label", "label": " " }),
        json!({ "access": "secret" }),
        json!({ "forks": "yes" }),
        json!({ "env": [{ "key": "1X", "value": "" }] }),
        json!({ "env": [{ "key": "A", "value": "" }, { "key": "A", "value": "" }] }),
    ] {
        let mut spec = json!({ "source": git, "previews": bad });
        assert!(check_settings(&mut spec).is_err(), "{bad}");
    }
    // Null clears them.
    let mut spec = json!({ "source": git, "previews": null });
    check_settings(&mut spec).unwrap();
    assert!(spec.get("previews").is_none());
    // New variables get an id.
    let mut spec =
        json!({ "source": git, "previews": { "env": [{ "key": " API_URL ", "value": "x" }] } });
    check_settings(&mut spec).unwrap();
    assert_eq!(spec["previews"]["env"][0]["key"], "API_URL");
    assert!(
        spec["previews"]["env"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("env_")
    );
}

fn reason(d: Decision) -> &'static str {
    match d {
        Decision::Remove { reason, .. } => reason,
        Decision::Run { .. } => "run",
    }
}

#[test]
fn what_runs_and_what_goes() {
    let spec = json!({});
    let run = |p: &PullRow, s: &Settings| decide(p, s, &spec, 2_000_000);
    assert_eq!(
        run(&pull("open"), &on()),
        Decision::Run {
            copy_secrets: false,
            secret_overrides: true
        }
    );
    assert_eq!(reason(run(&pull("closed"), &on())), "closed");
    assert_eq!(reason(run(&pull("merged"), &on())), "merged");
    assert_eq!(reason(run(&pull("open"), &Settings::default())), "settings");
    assert_eq!(
        reason(decide(
            &pull("open"),
            &on(),
            &json!({ "workloadKind": "CronJob" }),
            0
        )),
        "settings"
    );
    // Labels.
    let label = Settings {
        pulls: "label".into(),
        label: "Preview".into(),
        ..on()
    };
    assert_eq!(reason(run(&pull("open"), &label)), "label");
    let mut labelled = pull("open");
    labelled.labels = vec!["preview".into()];
    assert_eq!(reason(run(&labelled, &label)), "run");
    // Idle: no push for a day.
    let idle = Settings {
        idle_days: 1,
        ..on()
    };
    assert_eq!(
        reason(decide(
            &pull("open"),
            &idle,
            &spec,
            1_000_000 + 86_400_000 - 1
        )),
        "run"
    );
    assert_eq!(
        reason(decide(
            &pull("open"),
            &idle,
            &spec,
            1_000_000 + 86_400_000 + 1
        )),
        "idle"
    );
    // Secrets: only when copied, and never for a fork unless it says so.
    let secrets = Settings {
        copy_secrets: true,
        ..on()
    };
    assert_eq!(
        run(&pull("open"), &secrets),
        Decision::Run {
            copy_secrets: true,
            secret_overrides: true
        }
    );
    let mut fork = pull("open");
    fork.fork = true;
    assert_eq!(
        reason(run(&fork, &secrets)),
        "fork",
        "forks are off by default"
    );
    let no_secrets = Settings {
        forks: "no-secrets".into(),
        ..secrets.clone()
    };
    assert_eq!(
        run(&fork, &no_secrets),
        Decision::Run {
            copy_secrets: false,
            secret_overrides: false
        }
    );
    let with = Settings {
        forks: "secrets".into(),
        ..secrets
    };
    assert_eq!(
        run(&fork, &with),
        Decision::Run {
            copy_secrets: true,
            secret_overrides: true
        }
    );
}

#[test]
fn the_copy_is_the_service_from_the_head() {
    let src = json!({
        "name": "web", "replicas": 4, "description": "Shop",
        "source": { "type": "git", "repoUrl": "https://github.com/acme/shop", "branch": "main", "commit": "", "installationId": 7, "autoDeploy": true },
        "ports": [
            { "name": "http", "protocol": "HTTP", "containerPort": 3000, "exposure": "http" },
            { "name": "admin", "protocol": "HTTP", "containerPort": 3001, "exposure": "http" },
            { "name": "game", "protocol": "UDP", "containerPort": 7777, "exposure": "loadbalancer", "publicPort": 7777 },
        ],
        "domains": [{ "hostname": "shop.example.com", "portName": "http" }],
        "env": [
            { "id": "e1", "key": "MODE", "value": "production", "secret": false },
            { "id": "e2", "key": "DATABASE_URL", "value": "postgres://prod", "secret": true },
            { "id": "e3", "key": "API_URL", "value": "https://api.example.com", "secret": false },
        ],
        "autoscale": { "min": 2, "max": 9 }, "cronJobs": [{ "id": "c1" }], "secretImports": [{ "store": "s" }],
        "previews": { "enabled": true },
    });
    let s = Settings {
        env: vec![
            json!({ "id": "o1", "key": "API_URL", "value": "https://api-staging.example.com", "secret": false }),
            json!({ "id": "o2", "key": "STRIPE_KEY", "value": "sk_test", "secret": true }),
        ],
        ..on()
    };
    let d = domain::new_domain("web-pr-42.203.0.113.7.nip.io", "http");
    let c = copy_spec(&src, &s, "web-pr-42", Some(d), &pull("open"), false, true);
    assert_eq!(c["name"], "web-pr-42");
    assert_eq!(c["replicas"], 1);
    assert_eq!(c["source"]["commit"], "a".repeat(40));
    assert_eq!(c["source"]["branch"], "search");
    assert_eq!(c["source"]["autoDeploy"], false);
    for k in [
        "autoscale",
        "cronJobs",
        "secretImports",
        "previews",
        "description",
    ] {
        assert!(c.get(k).is_none(), "{k}");
    }
    assert_eq!(c["domains"].as_array().unwrap().len(), 1);
    assert_eq!(c["domains"][0]["hostname"], "web-pr-42.203.0.113.7.nip.io");
    // The public UDP port can't be had twice; one website port.
    let ports = c["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0]["exposure"], "http");
    assert_eq!(ports[1]["exposure"], "internal");
    let env = |c: &Value, k: &str| {
        c["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == k)
            .map(|e| e["value"].clone())
    };
    assert_eq!(env(&c, "MODE"), Some(json!("production")));
    assert_eq!(env(&c, "DATABASE_URL"), None, "secrets aren't copied");
    assert_eq!(
        env(&c, "API_URL"),
        Some(json!("https://api-staging.example.com")),
        "overridden"
    );
    assert_eq!(
        env(&c, "STRIPE_KEY"),
        Some(json!("sk_test")),
        "a secret of the previews' own"
    );
    assert_eq!(env(&c, "PYO_PREVIEW"), Some(json!("1")));
    assert_eq!(env(&c, "PYO_PULL_REQUEST"), Some(json!("42")));
    assert_eq!(
        env(&c, "PYO_PREVIEW_URL"),
        Some(json!("https://web-pr-42.203.0.113.7.nip.io"))
    );
    // With secrets; a fork without them, built from the pull request's ref.
    let c = copy_spec(&src, &s, "web-pr-42", None, &pull("open"), true, true);
    assert!(env(&c, "DATABASE_URL").is_some());
    assert!(c["secretImports"].is_array());
    assert_eq!(c["domains"], json!([]));
    let mut fork = pull("open");
    fork.fork = true;
    let c = copy_spec(&src, &s, "web-pr-42", None, &fork, false, false);
    assert_eq!(c["source"]["branch"], "refs/pull/42/head");
    assert!(env(&c, "STRIPE_KEY").is_none() && env(&c, "DATABASE_URL").is_none());
    fork.provider = "gitlab".into();
    assert_eq!(fork.build_ref(), "refs/merge-requests/42/head");
}

#[test]
fn the_copy_runs_where_the_service_does() {
    let p = pull("open");
    // Picked servers and tags come along as `placement`.
    let src = json!({ "name": "web", "placement": { "servers": ["server-1"], "tags": ["gpu"] } });
    let c = copy_spec(&src, &on(), "web-pr-42", None, &p, false, true);
    assert_eq!(c["placement"], json!({ "servers": ["server-1"], "tags": ["gpu"] }));
    // As saved before placement: the same, as `placement`.
    let old = json!({ "name": "web", "servers": ["server-2"], "serverTags": ["fast"] });
    let c = copy_spec(&old, &on(), "web-pr-42", None, &p, false, true);
    assert_eq!(c["placement"], json!({ "servers": ["server-2"], "tags": ["fast"] }));
    assert!(c.get("servers").is_none() && c.get("serverTags").is_none());
    // Any server: none.
    let any = json!({ "name": "web", "placement": { "servers": [], "tags": [] } });
    let c = copy_spec(&any, &on(), "web-pr-42", None, &p, false, true);
    assert!(c.get("placement").is_none());
}

// ── Against Postgres ───────────────────────────────────────────────────────

fn state(db: PgPool) -> AppState {
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
    AppState {
        db: db.clone(),
        config: std::sync::Arc::new(config),
        observability: crate::observability::Observability::from_env(db),
        sealer: std::sync::Arc::new(crate::secrets::Sealer::for_tests()),
        wakers: Default::default(),
        terminals: Default::default(),
        stopping,
    }
}

/// A team with a cluster (its preview domain set), a ready builder, a
/// project, the GitHub App's installation and a service of a repository
/// with previews on.
struct World {
    s: AppState,
    team: String,
    service: String,
    inst: i64,
    run: String,
}

async fn world(settings: Value) -> World {
    let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .await
        .unwrap();
    crate::migrate::run(&db).await.unwrap();
    crate::vault::init(crate::vault::Vault::new(
        db.clone(),
        crate::vault::Provider::from_env(),
    ));
    let run = crate::ids::new_id("t").replace('_', "-").to_lowercase();
    let (team, cluster, project, service) = (
        format!("team_{run}"),
        format!("cl_{run}"),
        format!("prj_{run}"),
        format!("svc_{run}"),
    );
    let inst = 800_000_000 + (pyo_proto::now_ms() % 100_000_000);
    let q = |sql: &'static str| sqlx::query(sql);
    q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at, preview_domain) VALUES ($1, $2, $1, $1, 'connected', now(), $3)")
        .bind(&cluster)
        .bind(&team)
        .bind(json!({ "domain": format!("{run}.preview.test") }))
        .execute(&db)
        .await
        .unwrap();
    q("INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, server, token_hash, status) VALUES ($1, $2, $1, 'cluster', $3, 's', $1, 'ready')")
        .bind(format!("bld_{run}"))
        .bind(&team)
        .bind(&cluster)
        .execute(&db)
        .await
        .unwrap();
    q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)")
        .bind(&project)
        .bind(&team)
        .bind(format!("shop-{run}"))
        .execute(&db)
        .await
        .unwrap();
    q("INSERT INTO pyo.github_installations (id, team_id, account, account_type) VALUES ($1, $2, 'acme', 'Organization')")
        .bind(inst)
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    let mut spec = json!({
        "name": "web", "replicas": 2, "resources": {}, "healthCheck": { "type": "none" },
        "source": { "type": "git", "repoUrl": format!("https://github.com/acme/shop-{run}"), "branch": "main", "commit": "", "installationId": inst },
        "ports": [{ "id": "port_1", "name": "http", "protocol": "HTTP", "containerPort": 3000, "exposure": "http" }],
        "env": [
            { "id": "e1", "key": "MODE", "value": "production", "secret": false },
            { "id": "e2", "key": "DATABASE_URL", "value": "postgres://prod", "secret": true },
        ],
        "domains": [],
        "previews": settings,
    });
    check_settings(&mut spec).unwrap();
    q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', $5, 'running')")
        .bind(&service)
        .bind(&team)
        .bind(&project)
        .bind(&cluster)
        .bind(&spec)
        .execute(&db)
        .await
        .unwrap();
    World {
        s: state(db),
        team,
        service,
        inst,
        run,
    }
}

impl World {
    fn pr(&self, number: i64, sha: char, state: &'static str) -> Pull {
        Pull {
            provider: "github",
            installation_id: Some(self.inst),
            repo: format!("acme/shop-{}", self.run),
            number,
            title: format!("Change {number}"),
            url: format!("https://github.com/acme/shop/pull/{number}"),
            author: "ana".into(),
            head_ref: format!("change-{number}"),
            head_sha: sha.to_string().repeat(40),
            base_ref: "main".into(),
            state,
            ..Default::default()
        }
    }

    async fn event(&self, p: &Pull) -> bool {
        pull_event(&self.s, &self.team, p).await.unwrap()
    }

    async fn preview(&self, number: i64) -> Option<PreviewRow> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {PREVIEW_COLS} FROM pyo.previews WHERE service_id = $1 AND pull_id = (SELECT id FROM pyo.pull_requests WHERE team_id = $2 AND number = $3)"
        )))
        .bind(&self.service)
        .bind(&self.team)
        .bind(number)
        .fetch_optional(&self.s.db)
        .await
        .unwrap()
    }

    async fn pull_id(&self, number: i64) -> String {
        sqlx::query_scalar("SELECT id FROM pyo.pull_requests WHERE team_id = $1 AND number = $2")
            .bind(&self.team)
            .bind(number)
            .fetch_one(&self.s.db)
            .await
            .unwrap()
    }

    /// The copy's deployments: (id, status, commit), newest first.
    async fn deployments(&self, copy: &str) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT id, status, data->'source'->>'commit' FROM pyo.deployments WHERE service_id = $1 ORDER BY created_at DESC")
            .bind(copy)
            .fetch_all(&self.s.db)
            .await
            .unwrap()
    }

    /// What the agent reports at the end, then what follows it.
    async fn settle(&self, dep: &str, status: &str) {
        sqlx::query("UPDATE pyo.deployments SET status = $2, finished_at = now() WHERE id = $1")
            .bind(dep)
            .bind(status)
            .execute(&self.s.db)
            .await
            .unwrap();
        deployment_changed(&self.s, dep).await.unwrap();
    }

    async fn jobs(&self, kind: &str, pull: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pyo.jobs WHERE kind = $1 AND payload->>'pullId' = $2",
        )
        .bind(kind)
        .bind(pull)
        .fetch_one(&self.s.db)
        .await
        .unwrap()
    }

    async fn copy(&self, id: &str) -> (String, Value, bool, Option<String>) {
        sqlx::query_as("SELECT name, spec, deleting, preview_of FROM pyo.services WHERE id = $1")
            .bind(id)
            .fetch_one(&self.s.db)
            .await
            .unwrap()
    }

    async fn delete_commands(&self, copy: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pyo.commands WHERE command->>'kind' = 'delete_service' AND command->>'serviceId' = $1")
            .bind(copy)
            .fetch_one(&self.s.db)
            .await
            .unwrap()
    }

    /// The agent removed the copies being deleted.
    async fn copies_gone(&self) {
        sqlx::query("DELETE FROM pyo.services WHERE preview_of = $1 AND deleting")
            .bind(&self.service)
            .execute(&self.s.db)
            .await
            .unwrap();
    }

    async fn set_settings(&self, settings: Value) {
        let mut spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(&self.service)
            .fetch_one(&self.s.db)
            .await
            .unwrap();
        spec["previews"] = settings;
        check_settings(&mut spec).unwrap();
        sqlx::query("UPDATE pyo.services SET spec = $2 WHERE id = $1")
            .bind(&self.service)
            .bind(&spec)
            .execute(&self.s.db)
            .await
            .unwrap();
    }

    async fn member(&self) -> crate::auth::User {
        let user = crate::auth::User {
            id: format!("usr_{}", self.run),
            name: "T".into(),
            email: format!("{}@t.test", self.run),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&self.s.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')")
            .bind(format!("mem_{}", self.run))
            .bind(&self.team)
            .bind(&user.id)
            .bind(&user.email)
            .execute(&self.s.db)
            .await
            .unwrap();
        user
    }
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_pull_requests_life() {
    run_big(async {
        let w = world(json!({ "enabled": true })).await;
        // Another repository's pull request: not followed.
        let mut other = w.pr(1, 'f', "open");
        other.repo = "acme/elsewhere".into();
        assert!(!w.event(&other).await);

        // Opened: a copy, deployed from the head.
        assert!(w.event(&w.pr(42, 'a', "open")).await);
        let p = w.preview(42).await.expect("a preview");
        assert_eq!((p.state.as_str(), p.name.as_str()), ("active", "web-pr-42"));
        assert_eq!(
            p.hostname.as_deref(),
            Some(format!("web-pr-42.{}.preview.test", w.run).as_str())
        );
        let copy = p.preview_service_id.clone().unwrap();
        let (name, spec, deleting, of) = w.copy(&copy).await;
        assert_eq!(
            (name.as_str(), deleting, of.as_deref()),
            ("web-pr-42", false, Some(w.service.as_str()))
        );
        assert_eq!(spec["replicas"], 1);
        assert_eq!(spec["source"]["commit"], "a".repeat(40));
        assert_eq!(spec["domains"][0]["hostname"], json!(p.hostname));
        let keys: Vec<&str> = spec["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["key"].as_str())
            .collect();
        assert!(
            keys.contains(&"MODE") && keys.contains(&"PYO_PREVIEW_URL"),
            "{keys:?}"
        );
        assert!(!keys.contains(&"DATABASE_URL"), "no secrets by default");
        let deps = w.deployments(&copy).await;
        assert_eq!(deps.len(), 1);
        assert_eq!(
            (deps[0].1.as_str(), deps[0].2.as_str()),
            ("queued", "a".repeat(40).as_str())
        );
        assert_eq!(p.deployment_id.as_deref(), Some(deps[0].0.as_str()));
        let commit: String = sqlx::query_scalar(
            "SELECT job->>'commit' FROM pyo.build_jobs WHERE deployment_id = $1",
        )
        .bind(&deps[0].0)
        .fetch_one(&w.s.db)
        .await
        .unwrap();
        assert_eq!(commit, "a".repeat(40));
        let pull = w.pull_id(42).await;
        assert!(
            w.jobs("preview.report", &pull).await >= 1,
            "the bot says so"
        );
        // The same event again changes nothing.
        w.event(&w.pr(42, 'a', "open")).await;
        assert_eq!(w.deployments(&copy).await.len(), 1);

        // A push while it builds: the old build stops first...
        w.event(&w.pr(42, 'b', "open")).await;
        assert_eq!(w.deployments(&copy).await.len(), 1);
        let cancels: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.commands WHERE command->>'kind' = 'cancel' AND coalesce(command->>'deploymentId', command->>'deployment_id') = $1")
            .bind(&deps[0].0)
            .fetch_one(&w.s.db)
            .await
            .unwrap();
        assert_eq!(cancels, 1);
        // ...and once it settled, the new head deploys, on the same copy and URL.
        w.settle(&deps[0].0, "cancelled").await;
        let deps = w.deployments(&copy).await;
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].2, "b".repeat(40));
        let p2 = w.preview(42).await.unwrap();
        assert_eq!(p2.sha, "b".repeat(40));
        assert_eq!(p2.hostname, p.hostname);
        assert_eq!(p2.preview_service_id.as_deref(), Some(copy.as_str()));
        w.settle(&deps[0].0, "succeeded").await;

        // Closed: the copy goes.
        w.event(&w.pr(42, 'b', "closed")).await;
        let p3 = w.preview(42).await.unwrap();
        assert_eq!(
            (p3.state.as_str(), p3.removed_reason.as_deref()),
            ("removed", Some("closed"))
        );
        assert!(w.copy(&copy).await.2, "deleting");
        assert_eq!(w.delete_commands(&copy).await, 1);

        // Reopened: back, on a new copy (the old one is still going, so it
        // waits for it first).
        w.event(&w.pr(42, 'b', "open")).await;
        assert_eq!(w.preview(42).await.unwrap().state, "waiting");
        w.copies_gone().await;
        reconcile(&w.s, &pull).await.unwrap();
        let p4 = w.preview(42).await.unwrap();
        assert_eq!(p4.state, "active");
        assert_ne!(p4.preview_service_id.as_deref(), Some(copy.as_str()));
        assert_eq!(p4.hostname, p.hostname, "the same URL");

        // Merged: removed, with why.
        w.event(&w.pr(42, 'b', "merged")).await;
        let p5 = w.preview(42).await.unwrap();
        assert_eq!(
            (p5.state.as_str(), p5.removed_reason.as_deref()),
            ("removed", Some("merged"))
        );

        // The service deleted: its other previews go too.
        w.event(&w.pr(43, 'c', "open")).await;
        let p6 = w.preview(43).await.unwrap();
        let mut tx = w.s.db.begin().await.unwrap();
        let mut wake = vec![];
        service_deleted(&mut tx, &w.service, &mut wake)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(w.preview(43).await.unwrap().state, "removed");
        assert_eq!(
            w.delete_commands(p6.preview_service_id.as_deref().unwrap())
                .await,
            1
        );
    });
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn limits_forks_labels_idle_and_removal_by_hand() {
    run_big(async {
        let w = world(json!({ "enabled": true, "limit": 1 })).await;
        w.event(&w.pr(1, 'a', "open")).await;
        w.event(&w.pr(2, 'b', "open")).await;
        assert_eq!(w.preview(1).await.unwrap().state, "active");
        let waiting = w.preview(2).await.unwrap();
        assert_eq!(waiting.state, "waiting");
        assert!(waiting.message.unwrap().contains("limit"));
        // The first one closes: the second starts.
        w.event(&w.pr(1, 'a', "closed")).await;
        reconcile(&w.s, &w.pull_id(2).await).await.unwrap();
        assert_eq!(w.preview(2).await.unwrap().state, "active");

        // Forks: not by default; without secrets when allowed.
        let mut fork = w.pr(3, 'c', "open");
        fork.fork = true;
        w.set_settings(json!({ "enabled": true, "limit": 5, "copySecrets": true }))
            .await;
        w.event(&fork).await;
        assert!(w.preview(3).await.is_none(), "not deployed");
        w.set_settings(
            json!({ "enabled": true, "limit": 5, "copySecrets": true, "forks": "no-secrets" }),
        )
        .await;
        w.event(&fork).await;
        let p = w.preview(3).await.unwrap();
        let (_, spec, _, _) = w.copy(p.preview_service_id.as_deref().unwrap()).await;
        assert_eq!(spec["source"]["branch"], "refs/pull/3/head");
        assert!(
            !spec["env"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["key"] == "DATABASE_URL")
        );
        // A pull request of the repository's own: secrets copied.
        w.event(&w.pr(2, 'd', "open")).await;
        let p2 = w.preview(2).await.unwrap();
        let (_, spec, _, _) = w.copy(p2.preview_service_id.as_deref().unwrap()).await;
        assert!(
            spec["env"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["key"] == "DATABASE_URL")
        );

        // Labels: without it the preview goes; with it, it's back.
        w.set_settings(
            json!({ "enabled": true, "pulls": "label", "label": "preview", "forks": "no-secrets" }),
        )
        .await;
        reconcile(&w.s, &w.pull_id(2).await).await.unwrap();
        let p = w.preview(2).await.unwrap();
        assert_eq!(
            (p.state.as_str(), p.removed_reason.as_deref()),
            ("removed", Some("label"))
        );
        let mut labelled = w.pr(2, 'd', "open");
        labelled.labels = vec!["preview".into()];
        w.copies_gone().await;
        w.event(&labelled).await;
        assert_eq!(w.preview(2).await.unwrap().state, "active");

        // Removed by hand: stays removed until a push.
        w.set_settings(json!({ "enabled": true })).await;
        let id = w.preview(2).await.unwrap().id;
        let user = w.member().await;
        let mut tx = w.s.db.begin().await.unwrap();
        let mut wake = vec![];
        delete(&mut tx, &user, &id, &mut wake).await.unwrap();
        tx.commit().await.unwrap();
        w.copies_gone().await;
        w.event(&labelled).await;
        let p = w.preview(2).await.unwrap();
        assert_eq!(
            (p.state.as_str(), p.removed_reason.as_deref()),
            ("removed", Some("user"))
        );
        w.event(&w.pr(2, 'e', "open")).await;
        assert_eq!(
            w.preview(2).await.unwrap().state,
            "active",
            "a push brings it back"
        );

        // Idle: no push for longer than the setting.
        w.set_settings(json!({ "enabled": true, "idleDays": 1 }))
            .await;
        sqlx::query(
            "UPDATE pyo.pull_requests SET pushed_at = now() - interval '2 days' WHERE id = $1",
        )
        .bind(w.pull_id(2).await)
        .execute(&w.s.db)
        .await
        .unwrap();
        reconcile(&w.s, &w.pull_id(2).await).await.unwrap();
        let p = w.preview(2).await.unwrap();
        assert_eq!(
            (p.state.as_str(), p.removed_reason.as_deref()),
            ("removed", Some("idle"))
        );

        // Previews turned off: every one goes.
        w.event(&w.pr(5, 'f', "open")).await;
        assert_eq!(w.preview(5).await.unwrap().state, "active");
        w.set_settings(json!({ "enabled": false })).await;
        reconcile(&w.s, &w.pull_id(5).await).await.unwrap();
        let p = w.preview(5).await.unwrap();
        assert_eq!(
            (p.state.as_str(), p.removed_reason.as_deref()),
            ("removed", Some("settings"))
        );
    });
}

/// What a fake provider was asked.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    fail_comments: bool,
}

impl Forge for Recorder {
    async fn comment(&self, id: Option<i64>, body: &str) -> anyhow::Result<i64> {
        if self.fail_comments {
            anyhow::bail!("GitHub refused to comment (403)");
        }
        let first = body.lines().nth(1).unwrap_or("").to_owned();
        self.calls
            .lock()
            .unwrap()
            .push(format!("comment {id:?} {first}"));
        Ok(id.unwrap_or(9))
    }
    async fn check(
        &self,
        id: Option<i64>,
        sha: &str,
        c: &bot::Check,
        _: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        self.calls.lock().unwrap().push(format!(
            "check {id:?} {} {} {:?}",
            &sha[..1],
            c.status,
            c.conclusion
        ));
        Ok(Some(id.unwrap_or(5)))
    }
    async fn deployment(&self, d: &mut Remote, shown: bot::Shown) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!(
            "deployment {} {} {}",
            d.name,
            &d.sha[..1],
            shown.key()
        ));
        d.deployment_id = Some(1);
        d.deployment_sha = Some(d.sha.clone());
        Ok(())
    }
}

impl Recorder {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

async fn say_now(w: &World, f: &Recorder, pull: &str) -> anyhow::Result<()> {
    let mut tx = w.s.db.begin().await.unwrap();
    let p = load_pull(&mut tx, pull, true).await.unwrap().unwrap();
    let v = views(&mut tx, pull).await.unwrap();
    let out = say(&mut tx, f, &p, &v, "https://pyo.example.com/acme").await;
    tx.commit().await.unwrap();
    out
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn the_bot_says_only_what_changed() {
    run_big(async {
        let w = world(json!({ "enabled": true })).await;
        w.event(&w.pr(7, 'a', "open")).await;
        let pull = w.pull_id(7).await;
        let f = Recorder::default();
        say_now(&w, &f, &pull).await.unwrap();
        assert_eq!(
            f.take(),
            vec![
                "comment None **Preview**",
                "check None a in_progress None",
                "deployment web-pr-7 a building"
            ]
        );
        // Nothing new: nothing said.
        say_now(&w, &f, &pull).await.unwrap();
        assert!(f.take().is_empty());
        // Ready: the comment, check and deployment move on.
        let dep = w.preview(7).await.unwrap().deployment_id.unwrap();
        w.settle(&dep, "succeeded").await;
        say_now(&w, &f, &pull).await.unwrap();
        assert_eq!(
            f.take(),
            vec![
                "comment Some(9) **Preview**",
                "check Some(5) a completed Some(\"success\")",
                "deployment web-pr-7 a ready"
            ]
        );
        // A push: a new check on the new head.
        w.event(&w.pr(7, 'b', "open")).await;
        say_now(&w, &f, &pull).await.unwrap();
        let calls = f.take();
        assert!(
            calls.contains(&"check None b in_progress None".to_owned()),
            "{calls:?}"
        );
        assert!(
            calls.contains(&"deployment web-pr-7 b building".to_owned()),
            "{calls:?}"
        );
        // A failed build fails the check.
        let dep = w.preview(7).await.unwrap().deployment_id.unwrap();
        w.settle(&dep, "failed").await;
        say_now(&w, &f, &pull).await.unwrap();
        let calls = f.take();
        assert!(
            calls.contains(&"check Some(5) b completed Some(\"failure\")".to_owned()),
            "{calls:?}"
        );
        // Closed: "Preview removed", and the deployment inactive.
        w.event(&w.pr(7, 'b', "closed")).await;
        say_now(&w, &f, &pull).await.unwrap();
        let calls = f.take();
        assert!(
            calls.contains(&"comment Some(9) **Preview removed**".to_owned()),
            "{calls:?}"
        );
        assert!(
            calls.contains(&"deployment web-pr-7 b removing".to_owned()),
            "{calls:?}"
        );
        // A provider that refuses: what worked is kept, and it's an error
        // (the job tries again).
        w.event(&w.pr(8, 'c', "open")).await;
        let pull8 = w.pull_id(8).await;
        let refusing = Recorder {
            fail_comments: true,
            ..Default::default()
        };
        assert!(say_now(&w, &refusing, &pull8).await.is_err());
        let calls = refusing.take();
        assert!(
            calls.iter().any(|c| c.starts_with("check None c")),
            "{calls:?}"
        );
        say_now(&w, &f, &pull8).await.unwrap();
        assert_eq!(
            f.take(),
            vec!["comment None **Preview**"],
            "only the comment is left to say"
        );
    });
}

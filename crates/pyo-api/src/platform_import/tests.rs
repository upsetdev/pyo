//! Mapping tests against recorded answers shaped as each platform's API
//! answers (tests/fixtures/platforms; see coolify.rs and dokploy.rs for the
//! documents and source files each field comes from).

use super::*;

fn fixture(name: &str) -> Responses {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/platforms/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn service<'a>(p: &'a ImportProject, key: &str) -> &'a ImportService {
    p.services
        .iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("no {key}"))
}

fn database<'a>(p: &'a ImportProject, key: &str) -> &'a ImportDatabase {
    p.databases
        .iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("no {key}"))
}

fn env<'a>(s: &'a PlanService, key: &str) -> &'a PlanEnv {
    s.env
        .iter()
        .find(|e| e.key == key)
        .unwrap_or_else(|| panic!("no {key}"))
}

fn has(warnings: &[String], part: &str) -> bool {
    warnings.iter().any(|w| w.contains(part))
}

#[tokio::test]
async fn coolify_walk_asks_for_every_recorded_answer() {
    let recorded = fixture("coolify");
    let got = coolify::gather(&Fetch::Recorded(recorded.clone()))
        .await
        .unwrap();
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        recorded.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn dokploy_walk_asks_for_every_recorded_answer() {
    let recorded = fixture("dokploy");
    let got = dokploy::gather(&Fetch::Recorded(recorded.clone()))
        .await
        .unwrap();
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        recorded.keys().collect::<Vec<_>>()
    );
}

#[test]
fn coolify_projects_and_environments() {
    let plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    assert_eq!(plan.platform, "coolify");
    let names: Vec<&str> = plan.projects.iter().map(|p| p.name.as_str()).collect();
    // Production keeps the project's name; other environments add theirs.
    assert_eq!(names, ["Shop", "Shop (staging)"]);
    assert_eq!(plan.projects[0].key, "p1shop/e1prod");
}

#[test]
fn coolify_applications() {
    let plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    let prod = &plan.projects[0];

    // owner/name on GitHub, nixpacks: zero config.
    let web = service(prod, "app:a1web");
    assert!(web.importable);
    assert_eq!(web.builder, "railpack");
    assert_eq!(web.service.source.kind, "git");
    assert_eq!(web.service.source.repo_url, "https://github.com/acme/shop");
    assert_eq!(web.service.source.branch, "main");
    assert_eq!(web.service.source.build_context, "");
    assert_eq!(web.service.ports.len(), 1);
    assert_eq!(web.service.ports[0].container_port, 3000);
    assert!(web.service.ports[0].public);
    let hosts: Vec<&str> = web.domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(hosts, ["shop.example.com", "www.shop.example.com"]);
    assert!(has(&web.warnings, "sslip.io is a free domain Coolify made"));
    assert_eq!(web.service.health_check["type"], "http");
    assert_eq!(web.service.health_check["path"], "/health");
    assert_eq!(web.service.resources.memory_limit, 1024);
    // Runtime variables; a build-only one is a build argument; preview ones are left out.
    assert_eq!(env(&web.service, "NODE_ENV").value, "production");
    assert_eq!(
        web.service
            .env
            .iter()
            .filter(|e| e.key == "NODE_ENV")
            .count(),
        1
    );
    assert!(env(&web.service, "DATABASE_URL").secret);
    assert!(!env(&web.service, "NODE_ENV").secret);
    // Locked in Coolify: secret, empty, and said so.
    let stripe = env(&web.service, "STRIPE_KEY");
    assert!(stripe.secret && stripe.value.is_empty());
    assert!(has(&web.warnings, "STRIPE_KEY is locked in Coolify"));
    assert_eq!(web.service.source.build_args.len(), 1);
    assert_eq!(web.service.source.build_args[0].key, "NEXT_PUBLIC_SITE");
    // `real_value`, Coolify's shared variables resolved.
    assert_eq!(
        web.service.source.build_args[0].value,
        "https://shop.example.com"
    );
    // DATABASE_URL still points at the Coolify database's container.
    assert!(has(&web.warnings, "DATABASE_URL points at shop-db there"));
    // Volumes start empty; a host directory too; a file isn't carried.
    assert_eq!(web.service.volumes.len(), 2);
    assert!(web.service.volumes[1].from_bind);
    assert!(has(
        &web.warnings,
        "File /app/public/robots.txt isn't carried over"
    ));

    // A URL elsewhere, a Dockerfile, a domain on another port with a path.
    let api = service(prod, "app:a2api");
    assert_eq!(api.builder, "dockerfile");
    assert_eq!(api.service.source.repo_url, "https://gitlab.com/acme/api");
    assert_eq!(api.service.source.branch, "release");
    assert_eq!(api.service.source.build_context, "backend");
    assert_eq!(api.service.source.dockerfile_path, "Dockerfile.prod");
    assert_eq!(
        api.domains,
        [ImportDomain {
            service: String::new(),
            hostname: "api.example.com".into(),
            port_name: "http".into()
        }]
    );
    assert!(has(&api.warnings, "served only /v1 there"));
    assert!(has(&api.warnings, "start command isn't carried over"));
    let tcp: Vec<(u16, Option<u16>)> = api
        .service
        .ports
        .iter()
        .filter(|p| p.protocol == "TCP")
        .map(|p| (p.container_port, p.public_port))
        .collect();
    assert_eq!(tcp, [(5000, Some(5000))]);
    assert!(has(&api.warnings, "Port 22/tcp can't be public in Pyo"));
    assert_eq!(api.service.resources.cpu_limit, 1500);
    assert_eq!(api.service.health_check["type"], "tcp");

    // From an image, with a name made DNS-safe.
    let admin = service(prod, "app:a3img");
    assert_eq!(admin.service.name, "admin-panel");
    assert_eq!(admin.service.source.kind, "image");
    assert_eq!(admin.service.source.image, "ghcr.io/acme/admin");
    assert_eq!(admin.service.source.reference, "1.4.2");

    // A Dockerfile pasted into Coolify: reported, not imported.
    let pasted = service(prod, "app:a4paste");
    assert!(!pasted.importable);
    assert!(has(&pasted.warnings, "pasted into Coolify"));

    // Staging: an SSH URL, a domain production already has, a deploy key,
    // a value the token couldn't read.
    let staging = &plan.projects[1];
    let web2 = service(staging, "app:a5stage");
    assert_eq!(web2.service.source.repo_url, "https://github.com/acme/shop");
    let hosts: Vec<&str> = web2.domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(hosts, ["staging.shop.example.com"]);
    assert!(has(
        &web2.warnings,
        "shop.example.com is on another service in this import"
    ));
    assert!(has(&web2.warnings, "deploy key"));
    assert!(has(&plan.warnings, "read:sensitive"));
}

#[test]
fn coolify_databases() {
    let plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    let prod = &plan.projects[0];
    let pg = database(prod, "db:d1pg");
    assert_eq!(
        (pg.target.as_deref(), pg.action.as_str()),
        (Some("postgres"), "import")
    );
    assert_eq!(
        pg.url.as_deref(),
        Some("postgres://postgres:s3cret@203.0.113.7:5433/postgres")
    );
    let my = database(prod, "db:d2my");
    assert_eq!(
        (my.target.as_deref(), my.action.as_str()),
        (Some("mysql"), "create")
    );
    assert!(my.url.is_none());
    assert!(has(&my.warnings, "make it public in Coolify"));
    let mongo = database(prod, "db:d3mongo");
    assert_eq!(
        (mongo.target.as_deref(), mongo.action.as_str()),
        (None, "manual")
    );
    let redis = database(prod, "db:d4redis");
    assert_eq!(
        (redis.target.as_deref(), redis.action.as_str()),
        (Some("valkey"), "create")
    );
    // KeyDB isn't in the environment's answer: placed by its environment id.
    let staging = &plan.projects[1];
    let keydb = database(staging, "db:d5kdb");
    assert_eq!(keydb.target.as_deref(), Some("valkey"));
    // One whose environment isn't readable: said so.
    assert!(has(&plan.warnings, "analytics (clickhouse)"));
}

#[test]
fn coolify_service_stack() {
    let plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    let st = &plan.projects[0].stacks[0];
    assert_eq!(st.key, "compose:s1umami");
    assert!(st.importable, "{:?}", st.warnings);
    assert_eq!(st.plan.project.name, "Shop umami");
    let mut names: Vec<&str> = st
        .plan
        .services
        .iter()
        .map(|s| s.compose_name.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["postgresql", "umami"]);
    // Its variables fill the file's ${VAR}s.
    let umami = st
        .plan
        .services
        .iter()
        .find(|s| s.compose_name == "umami")
        .unwrap();
    assert_eq!(env(umami, "APP_SECRET").value, "appsecret");
    assert!(
        env(umami, "DATABASE_URL")
            .value
            .contains("umami:pgpass@postgresql:5432")
    );
    // The container's domain, on its port.
    assert_eq!(st.domains.len(), 1);
    assert_eq!(st.domains[0].service, "umami");
    assert_eq!(st.domains[0].hostname, "stats.example.com");
    let port = umami
        .ports
        .iter()
        .find(|p| p.name == st.domains[0].port_name)
        .unwrap();
    assert_eq!(port.container_port, 3000);
    assert!(port.public);
}

#[test]
fn dokploy_plan() {
    let plan = dokploy::plan(&fixture("dokploy"), "dokploy.example.com");
    assert_eq!(plan.platform, "dokploy");
    assert_eq!(plan.projects.len(), 1);
    let p = &plan.projects[0];
    assert_eq!(p.name, "Blog");
    assert_eq!(p.key, "prjBlog01/envProd01");

    let web = service(p, "app:appGh01");
    assert!(web.importable);
    assert_eq!(
        web.service.source.repo_url,
        "https://github.com/acme/blog-web"
    );
    assert_eq!(web.service.source.build_context, "apps/web");
    assert_eq!(web.builder, "railpack");
    assert_eq!(web.service.replicas, 2);
    // Shared variables resolved as Dokploy does.
    assert_eq!(
        env(&web.service, "SITE_URL").value,
        "https://blog.example.com"
    );
    assert_eq!(
        env(&web.service, "API_BASE").value,
        "https://api.blog.example.com"
    );
    assert!(env(&web.service, "NEXTAUTH_SECRET").secret);
    assert_eq!(env(&web.service, "MISSING").value, "${{project.NOPE}}");
    assert!(has(&web.warnings, "${{project.NOPE}} can't be resolved"));
    assert_eq!(
        web.service.source.build_args[0].value,
        "https://blog.example.com"
    );
    assert!(has(&web.warnings, "DATABASE_URL points at blog-db there"));
    // A preview domain is left out; traefik.me is Dokploy's own.
    let hosts: Vec<&str> = web.domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(hosts, ["blog.example.com"]);
    assert!(has(
        &web.warnings,
        "traefik.me is a free domain Dokploy made"
    ));
    assert_eq!(web.service.ports[0].container_port, 3000);
    assert_eq!(web.service.volumes.len(), 1);
    assert_eq!(web.service.volumes[0].name, "blog-media");
    assert!(has(
        &web.warnings,
        "File /app/config.json isn't carried over"
    ));

    let worker = service(p, "app:appImg01");
    assert_eq!(worker.service.source.kind, "image");
    assert_eq!(
        worker.service.source.image,
        "registry.example.com/acme/worker"
    );
    assert_eq!(worker.service.source.reference, "2.1");
    assert!(has(&worker.warnings, "registry login"));
    assert_eq!(worker.service.ports[0].public_port, Some(2525));
    assert_eq!(worker.service.ports[0].container_port, 25);
    assert!(worker.service.volumes[0].from_bind);

    let legacy = service(p, "app:appDrop01");
    assert!(!legacy.importable);
    assert!(has(&legacy.warnings, "uploaded folder"));

    // A raw stack is imported; one in a repository is reported.
    let ghost = p
        .stacks
        .iter()
        .find(|s| s.key == "compose:cmpRaw01")
        .unwrap();
    assert!(ghost.importable, "{:?}", ghost.warnings);
    assert_eq!(ghost.plan.project.name, "Blog ghost");
    let g = ghost
        .plan
        .services
        .iter()
        .find(|s| s.compose_name == "ghost")
        .unwrap();
    assert_eq!(env(g, "url").value, "https://blog.example.com");
    assert_eq!(ghost.domains.len(), 1);
    assert_eq!(ghost.domains[0].hostname, "ghost.example.com");
    assert!(has(&ghost.warnings, "old.example.com is for nginx"));
    let mon = p
        .stacks
        .iter()
        .find(|s| s.key == "compose:cmpGit01")
        .unwrap();
    assert!(!mon.importable);
    assert!(has(&mon.warnings, "monitoring/docker-compose.yml"));

    // Databases: an external port means Pyo can copy it.
    let pg = database(p, "db:pgMain01");
    assert_eq!(pg.action, "import");
    assert_eq!(
        pg.url.as_deref(),
        Some("postgres://blog:p%40ss%2Fword@dokploy.example.com:5432/blog")
    );
    assert!(has(&pg.warnings, "Dokploy's own address"));
    let my = database(p, "db:myOld01");
    assert_eq!(my.action, "create");
    let maria = database(p, "db:maria01");
    assert_eq!(maria.action, "manual");
    let redis = database(p, "db:redis01");
    assert_eq!(
        (redis.target.as_deref(), redis.action.as_str()),
        (Some("valkey"), "create")
    );
}

#[test]
fn previews_have_no_values() {
    let mut plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    redact(&mut plan);
    let text = serde_json::to_string(&plan).unwrap();
    for secret in ["s3cret", "t0k", "pgpass", "appsecret", "\"production\""] {
        assert!(!text.contains(secret), "{secret} in the preview");
    }
    // Names stay.
    assert!(text.contains("STRIPE_KEY"));
    let mut plan = dokploy::plan(&fixture("dokploy"), "dokploy.example.com");
    redact(&mut plan);
    let text = serde_json::to_string(&plan).unwrap();
    for secret in ["p%40ss", "wppw", "s3cr3t", "ghostpw"] {
        assert!(!text.contains(secret), "{secret} in the preview");
    }
}

#[test]
fn git_urls() {
    for (given, want) in [
        (
            "https://github.com/acme/shop.git",
            Some("https://github.com/acme/shop"),
        ),
        (
            "git@github.com:acme/shop.git",
            Some("https://github.com/acme/shop"),
        ),
        (
            "ssh://git@gitlab.example.com:2222/team/app.git",
            Some("https://gitlab.example.com/team/app"),
        ),
        (
            "http://gitea.lan:3000/me/app/",
            Some("https://gitea.lan:3000/me/app"),
        ),
        ("ftp://example.com/x", None),
        ("not a url", None),
    ] {
        assert_eq!(https_repo(given).as_deref(), want, "{given}");
    }
}

/// The platform's address never reaches this machine, the cluster or a
/// metadata service, nor plain http on the internet.
#[tokio::test]
async fn addresses_are_guarded() {
    for bad in [
        "http://127.0.0.1:8000",
        "https://127.0.0.1",
        "https://169.254.169.254/latest",
        "https://10.43.0.1",
        "https://[::1]:3000",
        "https://user:pass@203.0.113.9",
        "http://203.0.113.9:8000",
        "https://203.0.113.9/?x=1",
        "file:///etc/passwd",
    ] {
        assert!(api_base(Platform::Coolify, bad).await.is_err(), "{bad}");
    }
    let ok = api_base(Platform::Coolify, "https://203.0.113.9/api/v1/")
        .await
        .unwrap();
    assert_eq!(ok.as_str(), "https://203.0.113.9/api/v1");
    let ok = api_base(Platform::Dokploy, "203.0.113.9:3000")
        .await
        .unwrap();
    assert_eq!(ok.as_str(), "https://203.0.113.9:3000/api");
}

/// Needs Postgres: DATABASE_URL=… [PYO_SECRET_KEY=<64 hex>] cargo test -p pyo-api -- --ignored platform_import
#[tokio::test]
#[ignore]
async fn importing_twice_makes_nothing_twice() {
    use crate::auth::User;
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let db = sqlx::PgPool::connect(&url).await.unwrap();
    crate::migrate::run(&db).await.unwrap();
    crate::vault::init(crate::vault::Vault::new(
        db.clone(),
        crate::vault::Provider::from_env(),
    ));
    let config = crate::Config {
        agent_api_url: String::new(),
        agent_image: String::new(),
        waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
        geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
        cookie_secure: false,
        allow_signup: false,
        builder_image: String::new(),
        tailnet_api_url: String::new(),
        relay_addr: None,
    };
    let (_stop, stopping) = tokio::sync::watch::channel(false);
    let s = AppState {
        db: db.clone(),
        config: std::sync::Arc::new(config),
        observability: crate::observability::Observability::from_env(db.clone()),
        sealer: std::sync::Arc::new(crate::secrets::Sealer::from_env()),
        wakers: Default::default(),
        terminals: Default::default(),
        stopping,
    };
    let run = crate::ids::new_id("t");
    let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
    let user = User {
        id: format!("usr_{run}"),
        name: "T".into(),
        email: format!("{run}@t.test"),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')")
        .bind(&user.id)
        .bind(&user.email)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
    sqlx::query("UPDATE pyo.teams SET default_cluster_id = $2 WHERE id = $1")
        .bind(&team)
        .bind(&cluster)
        .execute(&db)
        .await
        .unwrap();

    // Each run is its own Coolify, with its own domains.
    let mut plan = coolify::plan(&fixture("coolify"), &format!("{run}.example.com"));
    let host = run.replace('_', "-");
    for p in &mut plan.projects {
        for svc in &mut p.services {
            for d in &mut svc.domains {
                d.hostname = format!("{host}.{}", d.hostname);
            }
        }
        for st in &mut p.stacks {
            for d in &mut st.domains {
                d.hostname = format!("{host}.{}", d.hostname);
            }
        }
    }
    let items: Vec<String> = [
        "app:a1web",
        "app:a2api",
        "app:a3img",
        "app:a4paste",
        "app:a5stage",
        "compose:s1umami",
    ]
    .map(String::from)
    .to_vec();

    let first = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
        .await
        .unwrap();
    let by = |k: &str| first.results.iter().find(|r| r.key == k).unwrap();
    assert_eq!(by("app:a1web").state, "created", "{:?}", first.results);
    assert_eq!(by("app:a3img").state, "created", "{:?}", first.results);
    assert_eq!(by("app:a4paste").state, "skipped");
    assert_eq!(
        by("compose:s1umami").state,
        "created",
        "{:?}",
        first.results
    );
    // Saved, not deployed; secret values sealed; where it came from recorded.
    let web = by("app:a1web").id.clone().unwrap();
    let (status, spec): (String, Value) =
        sqlx::query_as("SELECT status, spec FROM pyo.services WHERE id = $1")
            .bind(&web)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "stopped");
    assert_eq!(spec["source"]["importedFrom"]["id"], "app:a1web");
    assert_eq!(spec["source"]["importedFrom"]["platform"], "coolify");
    assert_eq!(spec["source"]["builder"], "railpack");
    let deployments: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pyo.deployments WHERE service_id = $1")
            .bind(&web)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(deployments, 0);
    let dburl = spec["env"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "DATABASE_URL")
        .unwrap();
    if std::env::var("PYO_SECRET_KEY").is_ok() {
        assert!(
            crate::vault::Vault::is_sealed(dburl["value"].as_str().unwrap()),
            "sealed"
        );
    }
    // Production's services share one project.
    let web_project = by("app:a1web").project_id.clone();
    assert_eq!(web_project, by("app:a2api").project_id);

    let count = |table: &'static str| {
        let (db, team) = (db.clone(), team.clone());
        async move {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM pyo.{table} WHERE team_id = $1"
            )))
            .bind(&team)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let (p1, s1) = (count("projects").await, count("services").await);

    // Again: nothing new.
    let second = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
        .await
        .unwrap();
    for r in &second.results {
        assert!(matches!(r.state.as_str(), "exists" | "skipped"), "{r:?}");
    }
    assert_eq!((count("projects").await, count("services").await), (p1, s1));

    // A service new there joins the project the first import made.
    let mut more = plan.clone();
    let mut extra = more.projects[0].services[2].clone();
    extra.key = "app:a9new".into();
    extra.service.name = "admin-two".into();
    extra.domains.clear();
    more.projects[0].services.push(extra);
    let third = create(
        &s,
        &user,
        &team,
        None,
        more,
        Some(&["app:a9new".to_owned()]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(third.results[0].state, "created", "{:?}", third.results);
    assert_eq!(third.results[0].project_id, web_project);
    assert_eq!(count("projects").await, p1);

    sqlx::query("DELETE FROM pyo.services WHERE team_id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.users WHERE id = $1")
        .bind(&user.id)
        .execute(&db)
        .await
        .unwrap();
}

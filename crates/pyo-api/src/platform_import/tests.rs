//! Mapping tests against recorded answers shaped as each platform's API
//! answers (tests/fixtures/platforms; see coolify.rs, dokploy.rs,
//! railway.rs and vercel.rs for the documents and source files each field
//! comes from).

use super::*;
use crate::compose::PlanEnv;

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

fn connect(database: &str, env: &[(&str, &str)]) -> ImportConnect {
    ImportConnect {
        database: database.to_owned(),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

// ── Variables that point at the import's databases ───────────────────────────

/// Coolify sends `real_value` with `{{environment.X}}` resolved: read by
/// what it holds. Its URL inside Coolify (the container's uuid) or from
/// outside, a host with its port, login and database next to it. A URL to
/// a container Pyo can't match, or to a database it doesn't import, holds
/// the deploy. One outside Coolify is left alone.
#[test]
fn coolify_database_links() {
    let mut r = fixture("coolify");
    let var =
        |k: &str, v: &str| json!({ "key": k, "value": v, "real_value": v, "is_runtime": true });
    r.insert(
        "/applications/a2api/envs".into(),
        json!([
            // `{{environment.DATABASE_URL}}` there.
            json!({ "key": "DB", "value": "{{environment.DATABASE_URL}}", "real_value": "postgresql://postgres:s3cret@d1pg:5432/postgres", "is_runtime": true }),
            var("DB_PUBLIC", "postgres://postgres:s3cret@203.0.113.7:5433/postgres"),
            var("PGHOST", "d1pg"),
            var("PGPORT", "5432"),
            var("PGUSER", "postgres"),
            var("PGPASSWORD", "s3cret"),
            var("PGDATABASE", "postgres"),
            var("CACHE", "d4redis:6379"),
            var("EVENTS_URL", "mongodb://u:p@d3mongo:27017/events"),
            var("OLD_URL", "postgres://u:p@x9gone:5432/app"),
            var("NEON_URL", "postgres://u:p@ep-1.eu-central-1.aws.neon.tech/app"),
            var("LOG_LEVEL", "info"),
        ]),
    );
    let plan = coolify::plan(&r, "coolify.example.com");
    let api = service(&plan.projects[0], "app:a2api");
    assert_eq!(
        api.connects,
        [
            connect(
                "db:d1pg",
                &[
                    ("DB", "DATABASE_URL"),
                    ("DB_PUBLIC", "DATABASE_URL"),
                    ("PGDATABASE", "POSTGRES_DATABASE"),
                    ("PGHOST", "POSTGRES_HOST"),
                    ("PGPASSWORD", "POSTGRES_PASSWORD"),
                    ("PGPORT", "POSTGRES_PORT"),
                    ("PGUSER", "POSTGRES_USER"),
                ]
            ),
            connect("db:d4redis", &[("CACHE", "REDIS_HOST:REDIS_PORT")]),
        ]
    );
    assert_eq!(
        api.holds,
        [
            "OLD_URL points at a database Pyo couldn't match. Connect one, then deploy.",
            "EVENTS_URL points at events, which Pyo doesn't import. Connect a database, then deploy.",
        ]
    );
}

/// Dokploy's `${{environment.X}}` comes resolved: the database's
/// `appName` inside Dokploy, or its external port on the server.
#[test]
fn dokploy_database_links() {
    let mut r = fixture("dokploy");
    let app = r.get_mut("/application.one?applicationId=appGh01").unwrap();
    app["env"] = json!(
        "DATABASE_URL=${{environment.DB}}\nREDIS_URL=redis://default:rpw@blog-sessions-r4t5y6:6379\nFORUM_URL=mysql://forum:fpw@blog-forum-m1n2b3:3306/forum\nOTHER_URL=redis://10.0.0.9:6379\nPUBLIC=postgres://blog:x@dokploy.example.com:5432/blog"
    );
    let env = r
        .get_mut("/environment.one?environmentId=envProd01")
        .unwrap();
    env["env"] = json!("DB=postgres://blog:pw@blog-db-a1b2c3:5432/blog");
    let plan = dokploy::plan(&r, "dokploy.example.com");
    let web = service(&plan.projects[0], "app:appGh01");
    assert_eq!(
        web.connects,
        [
            connect(
                "db:pgMain01",
                &[("DATABASE_URL", "DATABASE_URL"), ("PUBLIC", "DATABASE_URL")]
            ),
            connect("db:redis01", &[("REDIS_URL", "REDIS_URL")]),
        ]
    );
    assert_eq!(
        web.holds,
        [
            "OTHER_URL points at a database Pyo couldn't match. Connect one, then deploy.",
            "FORUM_URL points at forum-db, which Pyo doesn't import. Connect a database, then deploy.",
        ]
    );
}

/// Railway says it: `${{Postgres.DATABASE_URL}}` is the Postgres
/// service's URL, `${{Postgres.PGHOST}}` its host. A URL made of its
/// parts is read as it resolves. A reference to MongoDB, which Pyo
/// doesn't import, and a `.railway.internal` name of no database hold.
#[test]
fn railway_database_links() {
    let mut r = fixture("railway");
    let d = &mut r.get_mut("details:prj-shop").unwrap()["data"]["v_0_2"];
    *d = json!({
        "QUEUE": "default",
        "PGHOST": "${{Postgres.PGHOST}}",
        "PGPASSWORD": "${{Postgres.PGPASSWORD}}",
        "BUILT": "postgresql://${{Postgres.PGUSER}}:${{Postgres.PGPASSWORD}}@${{Postgres.RAILWAY_PRIVATE_DOMAIN}}:5432/${{Postgres.PGDATABASE}}",
        "PUBLIC": "${{Postgres.DATABASE_PUBLIC_URL}}",
        "MYSQL": "${{MySQL.MYSQL_URL}}",
        "MONGO": "${{MongoDB.MONGO_URL}}",
        "GONE": "redis://default:x@cache.railway.internal:6379",
    });
    let plan = railway::plan(&r);
    let worker = service(&plan.projects[0], "app:svc-worker@env-prod");
    assert_eq!(
        worker.connects,
        [
            connect(
                "db:svc-pg@env-prod",
                &[
                    ("BUILT", "DATABASE_URL"),
                    ("PGHOST", "POSTGRES_HOST"),
                    ("PGPASSWORD", "POSTGRES_PASSWORD"),
                    ("PUBLIC", "DATABASE_URL"),
                ]
            ),
            connect("db:svc-mysql@env-prod", &[("MYSQL", "DATABASE_URL")]),
        ]
    );
    assert_eq!(
        worker.holds,
        [
            "GONE points at a database Pyo couldn't match. Connect one, then deploy.",
            "MONGO points at MongoDB, which Pyo doesn't import. Connect a database, then deploy.",
        ]
    );
    // The value stays as written: the connection replaces it.
    assert_eq!(
        env(&worker.service, "MONGO").value,
        "${{MongoDB.MONGO_URL}}"
    );
}

/// Vercel says which store a variable is (`contentHint`), even when its
/// value is secret. A URL to a store's host is read as it. A KV store
/// whose REST API is used is made with it.
#[test]
fn vercel_database_links() {
    let mut r = fixture("vercel");
    let list = r.get_mut("/v10/projects/prj_docs/env").unwrap();
    let envs = list["envs"].as_array_mut().unwrap();
    envs.push(json!({ "id": "envd3", "key": "PGHOST", "type": "sensitive", "target": ["production"], "contentHint": { "type": "postgres-host", "storeId": "store_pg1" } }));
    envs.push(json!({ "id": "envd4", "key": "COPY", "type": "plain", "target": ["production"], "value": "postgres://u:p@ep-cool-1.us-east-1.aws.neon.tech:5432/neondb" }));
    envs.push(json!({ "id": "envd5", "key": "ELSEWHERE", "type": "plain", "target": ["production"], "value": "postgres://u:p@db.example.org:5432/app" }));
    let plan = vercel::plan(&r);
    let docs = plan
        .projects
        .iter()
        .flat_map(|p| &p.services)
        .find(|s| s.key == "app:prj_docs")
        .unwrap();
    assert_eq!(
        docs.connects,
        [connect(
            "db:store_pg1",
            &[
                ("COPY", "DATABASE_URL"),
                ("PGHOST", "POSTGRES_HOST"),
                ("POSTGRES_URL", "DATABASE_URL")
            ]
        )]
    );
    assert!(docs.holds.is_empty(), "{:?}", docs.holds);
    let kv = plan
        .projects
        .iter()
        .flat_map(|p| &p.databases)
        .find(|d| d.key == "db:store_kv1")
        .unwrap();
    assert!(kv.rest);
}

/// A database the import has but the user didn't pick: the service is held.
#[test]
fn a_database_not_brought_holds() {
    let plan = coolify::plan(&fixture("coolify"), "coolify.example.com");
    let db = database(&plan.projects[0], "db:d1pg");
    assert_eq!(
        links::not_brought(&["DATABASE_URL".into()], db),
        "DATABASE_URL points at shop-db, which this import doesn't bring. Import it, or connect a database, then deploy."
    );
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
fn free_domain_keeps_its_port() {
    // Only a free platform domain: the hostname is left out, the port it
    // served stays (private, for a free Pyo domain).
    let mut web = Web::default();
    let mut seen = HashSet::new();
    web.domain(
        Platform::Dokploy,
        "demo.10.97.0.169.sslip.io",
        Some(3000),
        "/",
        &mut seen,
    );
    assert!(web.domains.is_empty());
    assert_eq!(web.ports.len(), 1);
    assert_eq!(web.ports[0].container_port, 3000);
    assert!(!web.ports[0].public);
    assert!(has(&web.warnings, "sslip.io is a free domain Dokploy made"));
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
    // DATABASE_URL points at the Coolify database's container: Pyo
    // connects the new one in its place.
    assert_eq!(
        web.connects,
        [connect("db:d1pg", &[("DATABASE_URL", "DATABASE_URL")])]
    );
    assert!(web.holds.is_empty());
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
    assert_eq!(
        web.connects,
        [connect("db:pgMain01", &[("DATABASE_URL", "DATABASE_URL")])]
    );
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

#[tokio::test]
async fn railway_walk_asks_for_every_recorded_answer() {
    for name in ["railway", "railway-project-token"] {
        let recorded = fixture(name);
        let got = railway::gather(&Fetch::Recorded(recorded.clone()))
            .await
            .unwrap();
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            recorded.keys().collect::<Vec<_>>(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn vercel_walk_asks_for_every_recorded_answer() {
    for name in ["vercel", "vercel-empty"] {
        let recorded = fixture(name);
        let got = vercel::gather(&Fetch::Recorded(recorded.clone()))
            .await
            .unwrap();
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            recorded.keys().collect::<Vec<_>>(),
            "{name}"
        );
    }
}

#[test]
fn railway_projects_and_environments() {
    let plan = railway::plan(&fixture("railway"));
    assert_eq!(
        (plan.platform.as_str(), plan.host.as_str()),
        ("railway", "railway.com")
    );
    let names: Vec<&str> = plan.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Shop", "Shop (staging)"]);
    assert_eq!(plan.projects[0].key, "prj-shop/env-prod");
    // The same service in two environments: two keys.
    assert_eq!(plan.projects[1].services[0].key, "app:svc-web@env-stage");
    assert_eq!(
        plan.projects[1].services[0].service.source.branch,
        "develop"
    );

    // A project token reads its own environment only.
    let plan = railway::plan(&fixture("railway-project-token"));
    let names: Vec<&str> = plan.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Blog"]);
    assert_eq!(plan.projects[0].services.len(), 1);
}

#[test]
fn railway_services() {
    let plan = railway::plan(&fixture("railway"));
    let prod = &plan.projects[0];

    // A repository built with Railpack, its commands as Railpack's.
    let web = service(prod, "app:svc-web@env-prod");
    assert!(web.importable);
    assert_eq!(web.builder, "railpack");
    assert_eq!(web.service.source.repo_url, "https://github.com/acme/shop");
    assert_eq!(web.service.source.branch, "main");
    assert_eq!(web.service.source.build_context, "apps/web");
    let args: Vec<(&str, &str)> = web
        .service
        .source
        .build_args
        .iter()
        .map(|a| (a.key.as_str(), a.value.as_str()))
        .collect();
    assert_eq!(
        args,
        [
            ("RAILPACK_BUILD_CMD", "pnpm build"),
            ("RAILPACK_START_CMD", "pnpm start")
        ]
    );
    assert_eq!(web.service.replicas, 2);
    assert_eq!(web.service.health_check["path"], "/health");
    // PORT picks the port; custom domains on it; Railway's own is dropped.
    assert_eq!(web.service.ports[0].container_port, 3000);
    let hosts: Vec<&str> = web.domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(hosts, ["shop.example.com", "www.shop.example.com"]);
    assert!(has(
        &web.warnings,
        "up.railway.app is a free domain Railway made"
    ));

    // References: shared, another service's, Railway's own, and missing.
    assert_eq!(env(&web.service, "SESSION_SECRET").value, "s3ss10n");
    assert!(env(&web.service, "SESSION_SECRET").secret);
    assert_eq!(env(&web.service, "CONTACT").value, "help@shop.example.com");
    assert_eq!(env(&web.service, "API_URL").value, "http://api:8000");
    assert_eq!(
        env(&web.service, "SITE_URL").value,
        "https://shop.example.com"
    );
    assert_eq!(env(&web.service, "MISSING").value, "${{shared.NOPE}}");
    assert!(has(&web.warnings, "${{shared.NOPE}} can't be resolved"));
    // A database's stays, with a note to connect the new one.
    assert_eq!(
        env(&web.service, "DATABASE_URL").value,
        "${{Postgres.DATABASE_URL}}"
    );
    assert_eq!(
        web.connects,
        [connect(
            "db:svc-pg@env-prod",
            &[("DATABASE_URL", "DATABASE_URL")]
        )]
    );
    // Sealed: by name, secret and empty.
    let stripe = env(&web.service, "STRIPE_KEY");
    assert!(stripe.secret && stripe.value.is_empty());
    assert!(has(&web.warnings, "STRIPE_KEY is sealed in Railway"));

    // The config file's Dockerfile, a target port, a TCP proxy, a volume.
    let api = service(prod, "app:svc-api@env-prod");
    assert_eq!(api.builder, "dockerfile");
    assert_eq!(api.service.source.dockerfile_path, "Dockerfile.prod");
    assert_eq!(api.service.source.build_context, "backend");
    assert_eq!(api.service.source.branch, "release");
    assert_eq!(api.service.replicas, 3);
    assert!(api.service.source.build_args.is_empty());
    assert!(has(&api.warnings, "start command isn't carried over"));
    assert!(has(&api.warnings, "pre-deploy command"));
    assert_eq!(api.domains[0].hostname, "api.example.com");
    let http = api
        .service
        .ports
        .iter()
        .find(|p| p.name == api.domains[0].port_name)
        .unwrap();
    assert_eq!(http.container_port, 8000);
    let tcp: Vec<(u16, Option<u16>)> = api
        .service
        .ports
        .iter()
        .filter(|p| p.protocol == "TCP")
        .map(|p| (p.container_port, p.public_port))
        .collect();
    assert_eq!(tcp, [(5000, Some(5000))]);
    assert!(has(&api.warnings, "shuttle.proxy.rlwy.net:15140"));
    assert_eq!(api.service.volumes.len(), 1);
    assert_eq!(api.service.volumes[0].mount_path, "/app/uploads");
    assert_eq!(api.service.volumes[0].size_gi, 5);
    assert_eq!(
        api.connects,
        [connect(
            "db:svc-redis@env-prod",
            &[("REDIS_URL", "REDIS_URL")]
        )]
    );

    // An image, on a schedule, with no domain; a name made DNS-safe.
    let worker = service(prod, "app:svc-worker@env-prod");
    assert_eq!(worker.service.name, "queue-worker");
    assert_eq!(worker.service.source.kind, "image");
    assert_eq!(worker.service.source.image, "ghcr.io/acme/worker");
    assert_eq!(worker.service.source.reference, "1.4.2");
    assert!(has(&worker.warnings, "on a schedule (*/15 * * * *)"));
    // No domain: Pyo's default HTTP port, private, and no check, since a
    // worker may not listen.
    assert!(has(&worker.warnings, "gets HTTP port 8080"));
    let ports: Vec<(u16, &str, bool)> = worker
        .service
        .ports
        .iter()
        .map(|p| (p.container_port, p.protocol.as_str(), p.public))
        .collect();
    assert_eq!(ports, [(8080, "HTTP", false)]);
    assert_eq!(worker.service.health_check["type"], "none");
    assert!(has(&worker.warnings, "put it to sleep"));

    // Uploaded with Railway's CLI: reported, not imported.
    let legacy = service(prod, "app:svc-upload@env-prod");
    assert!(!legacy.importable);
    assert!(has(&legacy.warnings, "uploaded with Railway's CLI"));
    // Databases aren't services.
    assert_eq!(prod.services.len(), 4);
}

#[test]
fn railway_databases() {
    let plan = railway::plan(&fixture("railway"));
    let prod = &plan.projects[0];
    let pg = database(prod, "db:svc-pg@env-prod");
    assert_eq!(
        (pg.target.as_deref(), pg.action.as_str()),
        (Some("postgres"), "import")
    );
    assert_eq!(
        pg.url.as_deref(),
        Some("postgresql://postgres:pgS3cret@roundhouse.proxy.rlwy.net:11105/railway")
    );
    assert_eq!(pg.name, "postgres");
    let my = database(prod, "db:svc-mysql@env-prod");
    assert_eq!(
        (my.target.as_deref(), my.action.as_str()),
        (Some("mysql"), "create")
    );
    assert!(has(&my.warnings, "make it public in Railway"));
    let redis = database(prod, "db:svc-redis@env-prod");
    assert_eq!(
        (redis.target.as_deref(), redis.action.as_str()),
        (Some("valkey"), "create")
    );
    let mongo = database(prod, "db:svc-mongo@env-prod");
    assert_eq!(
        (mongo.target.as_deref(), mongo.action.as_str()),
        (None, "manual")
    );
}

/// Without a public URL, a TCP proxy and the database's own variables
/// still reach it.
#[test]
fn railway_database_from_its_proxy() {
    let mut r = fixture("railway");
    let d = r.get_mut("details:prj-shop").unwrap();
    d["data"]["v_0_4"]
        .as_object_mut()
        .unwrap()
        .remove("DATABASE_PUBLIC_URL");
    d["data"]["v_0_4"]["PGPASSWORD"] = json!("p@ss/word");
    let plan = railway::plan(&r);
    let pg = database(&plan.projects[0], "db:svc-pg@env-prod");
    assert_eq!(
        pg.url.as_deref(),
        Some("postgres://postgres:p%40ss%2Fword@roundhouse.proxy.rlwy.net:11105/railway")
    );
}

/// One question per project: database variables rendered (for their URL),
/// the others as written, IDs as variables.
#[test]
fn railway_details_query() {
    let r = fixture("railway");
    let p = &r["projects:ws-acme"]["data"]["projects"]["edges"][0]["node"];
    let (q, vars) = railway::details_query(p, None);
    assert!(q.contains(
        "v_0_0: variables(projectId: $projectId, environmentId: $e0, serviceId: $s0_0, unrendered: true)"
    ));
    assert!(q.contains("v_0_4: variables(projectId: $projectId, environmentId: $e0, serviceId: $s0_4, unrendered: false)"));
    assert!(q.contains("t_1_0: tcpProxies(environmentId: $e1, serviceId: $s1_0)"));
    assert!(!q.contains("svc-"), "IDs go in variables");
    assert_eq!(vars["s0_4"], "svc-pg");
    assert_eq!(vars["e1"], "env-stage");
}

#[test]
fn vercel_plan() {
    let plan = vercel::plan(&fixture("vercel"));
    assert_eq!(
        (plan.platform.as_str(), plan.host.as_str()),
        ("vercel", "vercel.com")
    );
    assert!(has(&plan.warnings, "Preview deployments"));
    let names: Vec<&str> = plan.projects.iter().map(|p| p.name.as_str()).collect();
    // Both pages.
    assert_eq!(names, ["shop", "docs", "cli-app"]);

    let shop = service(&plan.projects[0], "app:prj_shop");
    assert!(shop.importable);
    assert_eq!(shop.builder, "railpack");
    assert_eq!(shop.service.source.repo_url, "https://github.com/acme/shop");
    assert_eq!(shop.service.source.branch, "main");
    assert_eq!(shop.service.source.build_context, "apps/web");
    assert_eq!(shop.service.ports[0].container_port, 3000);
    let args: Vec<(&str, &str)> = shop
        .service
        .source
        .build_args
        .iter()
        .map(|a| (a.key.as_str(), a.value.as_str()))
        .collect();
    assert_eq!(
        args,
        [
            ("RAILPACK_INSTALL_CMD", "pnpm install --frozen-lockfile"),
            ("RAILPACK_NODE_VERSION", "22"),
            ("NEXT_PUBLIC_SITE_URL", "https://shop.example.com"),
        ]
    );
    // Production values: plain, and encrypted ones read one by one.
    assert_eq!(
        env(&shop.service, "NEXT_PUBLIC_SITE_URL").value,
        "https://shop.example.com"
    );
    assert_eq!(
        env(&shop.service, "SENTRY_DSN").value,
        "https://k3y@o1.ingest.sentry.io/1"
    );
    assert!(env(&shop.service, "KV_REST_API_TOKEN").secret);
    // Preview, branch and system variables aren't carried; one key once.
    for k in ["FEATURE_X", "API_BASE", "VERCEL_URL"] {
        assert!(shop.service.env.iter().all(|e| e.key != k), "{k}");
    }
    assert_eq!(
        shop.service
            .env
            .iter()
            .filter(|e| e.key == "NEXT_PUBLIC_SITE_URL")
            .count(),
        1
    );
    // Write-only: by name, secret and empty, listed to set again.
    for k in ["STRIPE_SECRET_KEY", "AUTH_SECRET"] {
        let e = env(&shop.service, k);
        assert!(e.secret && e.value.is_empty(), "{k}");
    }
    assert!(has(
        &shop.warnings,
        "STRIPE_SECRET_KEY, AUTH_SECRET are secret in Vercel"
    ));
    // Domains: custom only; a redirect, a branch's and vercel.app's are left out.
    let hosts: Vec<&str> = shop.domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(hosts, ["shop.example.com"]);
    assert!(has(&shop.warnings, "www.shop.example.com redirected"));
    assert!(has(&shop.warnings, "served the develop branch"));
    assert!(has(
        &shop.warnings,
        "vercel.app is a free domain Vercel made"
    ));
    // What doesn't map.
    assert!(has(&shop.warnings, "Next.js runs as one server"));
    assert!(has(&shop.warnings, "/api/cron/digest (0 8 * * *)"));
    assert!(has(&shop.warnings, "Vercel Blob"));
    assert!(has(&shop.warnings, "VERCEL_URL"));
    assert!(has(&shop.warnings, "files from the rest of the repository"));
    assert_eq!(
        shop.connects,
        [
            connect(
                "db:store_pg1",
                &[
                    ("POSTGRES_URL", "DATABASE_URL"),
                    ("POSTGRES_URL_NON_POOLING", "DATABASE_URL_DIRECT")
                ]
            ),
            connect(
                "db:store_kv1",
                &[
                    ("KV_REST_API_TOKEN", "KV_REST_API_TOKEN"),
                    ("KV_REST_API_URL", "KV_REST_API_URL")
                ]
            ),
        ]
    );

    // A static framework: its output directory for Railpack, functions noted.
    let docs = service(&plan.projects[1], "app:prj_docs");
    assert_eq!(docs.service.source.branch, "trunk");
    let args: Vec<&str> = docs
        .service
        .source
        .build_args
        .iter()
        .map(|a| a.key.as_str())
        .collect();
    assert_eq!(
        args,
        [
            "RAILPACK_BUILD_CMD",
            "RAILPACK_NODE_VERSION",
            "RAILPACK_SPA_OUTPUT_DIR",
            "VITE_API"
        ]
    );
    assert!(has(&docs.warnings, "Vercel Functions"));
    assert!(has(&docs.warnings, "password protection"));
    assert!(has(
        &docs.warnings,
        "shop.example.com is on another service in this import"
    ));
    // Its store is the shop's: listed once, but noted here too.
    assert!(plan.projects[1].databases.is_empty());
    assert_eq!(
        docs.connects,
        [connect("db:store_pg1", &[("POSTGRES_URL", "DATABASE_URL")])]
    );

    // Deployed with the CLI: reported.
    let cli = service(&plan.projects[2], "app:prj_cli");
    assert!(!cli.importable);
    assert!(has(&cli.warnings, "Vercel's CLI"));
}

#[test]
fn vercel_storage() {
    let plan = vercel::plan(&fixture("vercel"));
    let shop = &plan.projects[0];
    // Postgres copied from its direct URL, not the pooled one.
    let pg = database(shop, "db:store_pg1");
    assert_eq!(
        (pg.target.as_deref(), pg.action.as_str()),
        (Some("postgres"), "import")
    );
    assert_eq!(pg.name, "shop-db");
    assert!(pg.url.as_deref().unwrap().contains("ep-cool-1.us-east-1"));
    // The key-value store: an empty KV database.
    let kv = database(shop, "db:store_kv1");
    assert_eq!(
        (kv.target.as_deref(), kv.action.as_str()),
        (Some("valkey"), "create")
    );
    assert_eq!(kv.source_name, "key-value store");
    assert!(kv.url.is_none());
    // Blob stores aren't databases.
    assert_eq!(shop.databases.len(), 2);
}

#[test]
fn vercel_without_projects_names_the_teams() {
    let plan = vercel::plan(&fixture("vercel-empty"));
    assert!(plan.projects.is_empty());
    assert!(has(&plan.warnings, "acme, side-project"));

    // A team with no projects (no other teams to name): still a reason.
    let mut r = fixture("vercel-empty");
    r.remove("/v2/teams?limit=20");
    let plan = vercel::plan(&r);
    assert!(plan.projects.is_empty());
    assert!(has(
        &plan.warnings,
        "Vercel has no projects here, or the token"
    ));
}

#[test]
fn hosted_previews_have_no_values() {
    let mut plan = railway::plan(&fixture("railway"));
    redact(&mut plan);
    let text = serde_json::to_string(&plan).unwrap();
    for secret in ["s3ss10n", "t0k3n", "pgS3cret", "pnpm start"] {
        assert!(!text.contains(secret), "{secret} in the preview");
    }
    assert!(text.contains("STRIPE_KEY"));
    let mut plan = vercel::plan(&fixture("vercel"));
    redact(&mut plan);
    let text = serde_json::to_string(&plan).unwrap();
    for secret in ["npg_S3cr3t", "kvT0kenValue", "k3y@", "vercel_blob_rw"] {
        assert!(!text.contains(secret), "{secret} in the preview");
    }
    assert!(text.contains("STRIPE_SECRET_KEY"));
}

/// Railway and Vercel are read only from their own API: no address of the
/// user's is ever contacted.
#[tokio::test]
async fn hosted_platforms_use_only_their_api() {
    for (p, api) in [
        (
            Platform::Railway,
            "https://backboard.railway.com/graphql/v2",
        ),
        (Platform::Vercel, "https://api.vercel.com/"),
    ] {
        assert_eq!(api_base(p, "").await.unwrap().as_str(), api);
        assert_eq!(api_base(p, "  ").await.unwrap().as_str(), api);
        let host = reqwest::Url::parse(api)
            .unwrap()
            .host_str()
            .unwrap()
            .to_owned();
        assert_eq!(api_base(p, &host).await.unwrap().as_str(), api);
        assert_eq!(
            api_base(p, &format!("https://{host}/x"))
                .await
                .unwrap()
                .as_str(),
            api
        );
        for bad in [
            "https://evil.example.com",
            "https://127.0.0.1",
            "http://169.254.169.254",
            &format!("http://{host}"),
            &format!("https://user:pass@{host}"),
            &format!("https://{host}:8443"),
            &format!("https://{host}.evil.example.com"),
        ] {
            assert!(api_base(p, bad).await.is_err(), "{p:?} {bad}");
        }
    }
    // Teams are Vercel's only, and plain ids or slugs.
    assert_eq!(
        team_scope(Platform::Vercel, Some("team_abc123")).unwrap(),
        [("teamId".to_owned(), "team_abc123".to_owned())]
    );
    assert_eq!(
        team_scope(Platform::Vercel, Some("acme")).unwrap(),
        [("slug".to_owned(), "acme".to_owned())]
    );
    assert!(team_scope(Platform::Vercel, Some("acme&x=1")).is_err());
    assert!(team_scope(Platform::Railway, Some("acme")).is_err());
    assert!(team_scope(Platform::Railway, None).unwrap().is_empty());
}

/// A team with an owner and a connected cluster in the Postgres at
/// DATABASE_URL, for the ignored tests below.
async fn test_team() -> (AppState, crate::auth::User, String, sqlx::PgPool) {
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
    let (stop, stopping) = tokio::sync::watch::channel(false);
    // Kept open for the test's whole run.
    std::mem::forget(stop);
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

    (s, user, team, db)
}

/// Removes what `test_team` and the import made.
async fn drop_team(db: &sqlx::PgPool, team: &str, user: &crate::auth::User) {
    sqlx::query("DELETE FROM pyo.services WHERE team_id = $1")
        .bind(team)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
        .bind(team)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.users WHERE id = $1")
        .bind(&user.id)
        .execute(db)
        .await
        .unwrap();
}

/// Needs Postgres: DATABASE_URL=… [PYO_SECRET_KEY=<64 hex>] cargo test -p pyo-api -- --ignored platform_import
#[tokio::test]
#[ignore]
async fn importing_twice_makes_nothing_twice() {
    let (s, user, team, db) = test_team().await;
    let run = team.trim_start_matches("team_").to_owned();
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

    let first = create(
        &s,
        &user,
        &team,
        None,
        plan.clone(),
        Some(&items),
        false,
        None,
    )
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
    let second = create(
        &s,
        &user,
        &team,
        None,
        plan.clone(),
        Some(&items),
        false,
        None,
    )
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
        None,
    )
    .await
    .unwrap();
    assert_eq!(third.results[0].state, "created", "{:?}", third.results);
    assert_eq!(third.results[0].project_id, web_project);
    assert_eq!(count("projects").await, p1);

    drop_team(&db, &team, &user).await;
}

/// Railway and Vercel: a second import makes nothing new, and the source
/// is recorded with the platform's own host.
/// Needs Postgres, as above.
#[tokio::test]
#[ignore]
async fn hosted_imports_make_nothing_twice() {
    let (s, user, team, db) = test_team().await;
    let host = team.trim_start_matches("team_").replace('_', "-");
    for (platform, mut plan) in [
        ("railway", railway::plan(&fixture("railway"))),
        ("vercel", vercel::plan(&fixture("vercel"))),
    ] {
        for p in &mut plan.projects {
            for svc in &mut p.services {
                for d in &mut svc.domains {
                    d.hostname = format!("{platform}-{host}.{}", d.hostname);
                }
            }
        }
        let items: Vec<String> = plan
            .projects
            .iter()
            .flat_map(|p| p.services.iter().map(|s| s.key.clone()))
            .collect();
        let first = create(
            &s,
            &user,
            &team,
            None,
            plan.clone(),
            Some(&items),
            false,
            None,
        )
        .await
        .unwrap();
        let made: Vec<&ImportResult> = first
            .results
            .iter()
            .filter(|r| r.state == "created")
            .collect();
        assert!(!made.is_empty(), "{platform}: {:?}", first.results);
        for r in &first.results {
            assert!(
                matches!(r.state.as_str(), "created" | "skipped"),
                "{platform}: {r:?}"
            );
        }
        let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(made[0].id.as_deref().unwrap())
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(spec["source"]["importedFrom"]["platform"], platform);
        assert_eq!(
            spec["source"]["importedFrom"]["host"],
            format!("{platform}.com")
        );
        assert!(spec["source"]["builder"].as_str().is_some());
        let second = create(
            &s,
            &user,
            &team,
            None,
            plan.clone(),
            Some(&items),
            false,
            None,
        )
        .await
        .unwrap();
        for r in &second.results {
            assert!(
                matches!(r.state.as_str(), "exists" | "skipped"),
                "{platform}: {r:?}"
            );
        }
    }
    drop_team(&db, &team, &user).await;
}

/// Databases: a second import finds the one it made by where it came from,
/// not by its name. One the team had already, with the same name, isn't
/// taken for it: the new one gets the platform in front (`shop-2` would
/// meet `shop`'s pods). Every platform.
/// Needs Postgres, as above.
#[tokio::test]
#[ignore]
async fn databases_match_by_where_they_came_from() {
    let (s, user, team, db) = test_team().await;
    let cluster = format!("cl_{}", team.trim_start_matches("team_"));
    for (platform, mut plan) in [
        (
            "coolify",
            coolify::plan(&fixture("coolify"), "coolify.example.com"),
        ),
        (
            "dokploy",
            dokploy::plan(&fixture("dokploy"), "dokploy.example.com"),
        ),
        ("railway", railway::plan(&fixture("railway"))),
        ("vercel", vercel::plan(&fixture("vercel"))),
    ] {
        // Made empty here: copying data needs a cluster to reach the source.
        for d in plan.projects.iter_mut().flat_map(|p| &mut p.databases) {
            if d.action == "import" {
                d.action = "create".into();
                d.url = None;
            }
        }
        let wanted: Vec<ImportDatabase> = plan
            .projects
            .iter()
            .flat_map(|p| &p.databases)
            .filter(|d| d.action == "create")
            .cloned()
            .collect();
        assert!(!wanted.is_empty(), "{platform}: no database to make");
        let items: Vec<String> = wanted.iter().map(|d| d.key.clone()).collect();
        // The team's own database, with the first one's name, made by hand.
        let mine = crate::ids::new_id("mdb");
        let name = wanted[0].name.clone();
        sqlx::query("INSERT INTO pyo.databases (id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, namespace, password) VALUES ($1, $2, $3, $4, 'postgres', '18', 'small', 5, 1, 'pyo-db-t', '')")
            .bind(&mine).bind(&team).bind(&cluster).bind(&name).execute(&db).await.unwrap();

        let first = create(
            &s,
            &user,
            &team,
            None,
            plan.clone(),
            Some(&items),
            false,
            None,
        )
        .await
        .unwrap();
        for r in &first.results {
            assert_eq!(r.state, "created", "{platform}: {r:?}");
            assert_ne!(r.id.as_deref(), Some(mine.as_str()), "{platform}");
        }
        let made = &first.results[0];
        assert_eq!(made.name, format!("{platform}-{name}"), "{platform}");
        let from: Value =
            sqlx::query_scalar("SELECT imported_from FROM pyo.databases WHERE id = $1")
                .bind(made.id.as_deref().unwrap())
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(from["platform"], platform);
        assert_eq!(from["id"], wanted[0].key);

        let second = create(
            &s,
            &user,
            &team,
            None,
            plan.clone(),
            Some(&items),
            false,
            None,
        )
        .await
        .unwrap();
        for (a, b) in first.results.iter().zip(&second.results) {
            assert_eq!(b.state, "exists", "{platform}: {b:?}");
            assert_eq!(a.id, b.id, "{platform}");
            assert_eq!(a.name, b.name, "{platform}");
        }
        sqlx::query("DELETE FROM pyo.databases WHERE team_id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
    }
    drop_team(&db, &team, &user).await;
}

/// A Railway plan whose databases are made empty (copying data needs a
/// cluster to reach the source), with the keys of what can be imported.
fn railway_plan_to_make(host: &str) -> (ImportPlan, Vec<String>) {
    let mut plan = railway::plan(&fixture("railway"));
    for p in &mut plan.projects {
        for svc in &mut p.services {
            for d in &mut svc.domains {
                d.hostname = format!("{host}.{}", d.hostname);
            }
        }
        for d in &mut p.databases {
            if d.action == "import" {
                d.action = "create".into();
                d.url = None;
            }
        }
    }
    let keys = plan
        .projects
        .iter()
        .flat_map(|p| {
            p.services
                .iter()
                .filter(|x| x.importable)
                .map(|x| x.key.clone())
                .chain(
                    p.databases
                        .iter()
                        .filter(|d| d.action == "create")
                        .map(|d| d.key.clone()),
                )
        })
        .collect();
    (plan, keys)
}

/// Waits for an import to end (at most 30 s).
async fn ended(s: &AppState, id: &str) -> ImportRun {
    for _ in 0..300 {
        let run = load_run(&s.db, id).await.unwrap().unwrap();
        if !matches!(run.status.as_str(), "reading" | "importing") {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("import {id} didn't end");
}

/// The client goes away once the services are made (a closed tab, a
/// dropped connection): its request's future is dropped. The import still
/// makes the databases and records every result.
/// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored platform_import
#[tokio::test]
#[ignore]
async fn an_import_finishes_when_its_client_goes_away() {
    let (s, user, team, db) = test_team().await;
    let host = team.trim_start_matches("team_").replace('_', "-");
    let (plan, items) = railway_plan_to_make(&host);
    let databases: Vec<String> = items
        .iter()
        .filter(|k| k.starts_with("db:"))
        .cloned()
        .collect();
    assert!(!databases.is_empty());

    let id = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    // As apply runs it, then waits as a client would until it's dropped.
    let request = {
        let (s, user, team, id, items) = (
            s.clone(),
            user.clone(),
            team.clone(),
            id.clone(),
            items.clone(),
        );
        async move {
            let run = new_run(&s, &user, &team, Platform::Railway, false)
                .await
                .unwrap();
            *id.lock().unwrap() = Some(run.clone());
            spawn_run(
                &s,
                &user,
                &run,
                &team,
                None,
                Some(items),
                false,
                async move { Ok(plan) },
            );
            std::future::pending::<()>().await;
        }
    };
    let services_made = async {
        loop {
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&db)
                .await
                .unwrap();
            if n > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        _ = request => unreachable!(),
        _ = tokio::time::timeout(Duration::from_secs(30), services_made) => {}
    }
    // The request is gone. The import isn't.
    let id = id.lock().unwrap().clone().unwrap();
    let run = ended(&s, &id).await;
    assert_eq!(run.status, "done", "{run:?}");
    assert_eq!(run.items.len(), items.len());
    for key in &items {
        let r = run.results.iter().find(|r| &r.key == key);
        assert!(
            r.is_some_and(|r| r.state == "created"),
            "{key}: {:?}",
            run.results
        );
    }
    for key in &databases {
        let made: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.databases WHERE team_id = $1 AND imported_from->>'id' = $2",
        )
        .bind(&team)
        .bind(key)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(made, 1, "{key}");
    }

    // Again, as after a crash: nothing twice.
    let (plan, _) = railway_plan_to_make(&host);
    let again = new_run(&s, &user, &team, Platform::Railway, false)
        .await
        .unwrap();
    spawn_run(
        &s,
        &user,
        &again,
        &team,
        None,
        Some(items.clone()),
        false,
        async move { Ok(plan) },
    );
    let run = ended(&s, &again).await;
    assert_eq!(run.status, "done");
    for r in &run.results {
        assert_eq!(r.state, "exists", "{r:?}");
    }
    sqlx::query("DELETE FROM pyo.databases WHERE team_id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    drop_team(&db, &team, &user).await;
}

/// A platform that can't be read fails the import with its reason. One
/// whose process stopped (no heartbeat) shows as interrupted.
/// Needs Postgres, as above.
#[tokio::test]
#[ignore]
async fn imports_say_how_they_ended() {
    let (s, user, team, db) = test_team().await;
    let id = new_run(&s, &user, &team, Platform::Coolify, false)
        .await
        .unwrap();
    spawn_run(&s, &user, &id, &team, None, None, false, async {
        Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Coolify refused the token.",
        ))
    });
    let run = ended(&s, &id).await;
    assert_eq!(run.status, "failed");
    assert_eq!(run.error, "Coolify refused the token.");
    assert!(run.finished_at.is_some());

    let cut = new_run(&s, &user, &team, Platform::Coolify, false)
        .await
        .unwrap();
    sqlx::query("UPDATE pyo.platform_imports SET status = 'importing', updated_at = now() - interval '5 minutes' WHERE id = $1")
        .bind(&cut)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        load_run(&db, &cut).await.unwrap().unwrap().status,
        "interrupted"
    );
    let fresh = new_run(&s, &user, &team, Platform::Coolify, false)
        .await
        .unwrap();
    assert_eq!(
        load_run(&db, &fresh).await.unwrap().unwrap().status,
        "reading"
    );
    drop_team(&db, &team, &user).await;
}

/// With deploy on: a service whose DATABASE_URL points at a database of
/// the import is connected to the new one in place of it, then deployed
/// once the database is ready. One pointing at a database Pyo can't match,
/// or at one the import doesn't bring, is held. Importing again connects
/// nothing twice and deploys nothing again.
/// Needs Postgres, as above.
#[tokio::test]
#[ignore]
async fn imports_connect_databases_before_the_first_deploy() {
    let (s, user, team, db) = test_team().await;
    let run = team.trim_start_matches("team_").to_owned();
    let host = run.replace('_', "-");
    let mut r = fixture("coolify");
    let var =
        |k: &str, v: &str| json!({ "key": k, "value": v, "real_value": v, "is_runtime": true });
    r.insert(
        "/applications/a2api/envs".into(),
        json!([var("OLD_URL", "postgres://u:p@x9gone:5432/app")]),
    );
    // The image's service (a Git one needs a builder to deploy).
    r.insert(
        "/applications/a3img/envs".into(),
        json!([
            var(
                "DATABASE_URL",
                "postgres://postgres:s3cret@d1pg:5432/postgres"
            ),
            var("LOG_LEVEL", "info")
        ]),
    );
    r.insert(
        "/applications/a1web/envs".into(),
        json!([var("CACHE_URL", "redis://default:x@d4redis:6379")]),
    );
    let plan = || {
        let mut plan = coolify::plan(&r, &format!("{run}.example.com"));
        for p in &mut plan.projects {
            for svc in &mut p.services {
                for d in &mut svc.domains {
                    d.hostname = format!("{host}.{}", d.hostname);
                }
            }
            // Made empty here: copying data needs a cluster.
            for d in &mut p.databases {
                if d.action == "import" {
                    d.action = "create".into();
                    d.url = None;
                }
            }
        }
        plan
    };
    // d4redis isn't picked.
    let items: Vec<String> = ["app:a1web", "app:a2api", "app:a3img", "db:d1pg"]
        .map(String::from)
        .to_vec();
    // The cluster says the database is ready once it's there.
    let ready = {
        let (db, team) = (db.clone(), team.clone());
        tokio::spawn(async move {
            loop {
                let _ = sqlx::query("UPDATE pyo.databases SET status = '{\"phase\": \"Ready\"}' WHERE team_id = $1 AND status = '{}'")
                    .bind(&team)
                    .execute(&db)
                    .await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let go = |plan: ImportPlan| {
        let (s, user, team, items) = (s.clone(), user.clone(), team.clone(), items.clone());
        async move {
            let id = new_run(&s, &user, &team, Platform::Coolify, true)
                .await
                .unwrap();
            spawn_run(&s, &user, &id, &team, None, Some(items), true, async move {
                Ok(plan)
            });
            ended(&s, &id).await
        }
    };

    let first = go(plan()).await;
    assert_eq!(first.status, "done", "{first:?}");
    let by = |run: &ImportRun, k: &str| run.results.iter().find(|r| r.key == k).cloned().unwrap();
    let pg = by(&first, "db:d1pg");
    assert_eq!(pg.state, "created", "{pg:?}");
    let pg_id = pg.id.clone().unwrap();

    // Connected, in place of the old URL, and deployed.
    let web = by(&first, "app:a3img");
    assert_eq!(web.state, "created", "{web:?}");
    assert_eq!(
        web.connected,
        [ImportConnected {
            database: pg.name.clone(),
            database_id: pg_id.clone(),
            variables: vec!["DATABASE_URL".into(), "DATABASE_URL_DIRECT".into()],
        }]
    );
    let web_id = web.id.clone().unwrap();
    let env_of = |id: String| {
        let db = db.clone();
        async move {
            let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
                .bind(&id)
                .fetch_one(&db)
                .await
                .unwrap();
            let mut keys: Vec<(String, bool)> = spec["env"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["key"].as_str().unwrap().to_owned(),
                        e["secret"].as_bool().unwrap_or(false),
                    )
                })
                .collect();
            keys.sort();
            keys
        }
    };
    let deployments = |id: String| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pyo.deployments WHERE service_id = $1",
            )
            .bind(&id)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let env = env_of(web_id.clone()).await;
    assert_eq!(
        env.iter()
            .filter(|(k, _)| k.starts_with("DATABASE_URL"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            ("DATABASE_URL".to_owned(), true),
            ("DATABASE_URL_DIRECT".to_owned(), true)
        ]
    );
    let links: Vec<String> =
        sqlx::query_scalar("SELECT database_id FROM pyo.database_links WHERE service_id = $1")
            .bind(&web_id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(links.as_slice(), std::slice::from_ref(&pg_id));
    assert_eq!(deployments(web_id.clone()).await, 1);

    // Held, saying why, and not deployed.
    let api = by(&first, "app:a2api");
    assert_eq!(api.state, "held", "{api:?}");
    assert_eq!(
        api.message,
        "OLD_URL points at a database Pyo couldn't match. Connect one, then deploy."
    );
    assert_eq!(deployments(api.id.clone().unwrap()).await, 0);
    let img = by(&first, "app:a1web");
    assert_eq!(img.state, "held", "{img:?}");
    assert!(
        img.message.contains("which this import doesn't bring"),
        "{img:?}"
    );
    assert_eq!(deployments(img.id.clone().unwrap()).await, 0);

    // Again: nothing twice.
    let second = go(plan()).await;
    assert_eq!(second.status, "done", "{second:?}");
    for r in &second.results {
        assert_eq!(r.state, "exists", "{r:?}");
        assert!(r.connected.is_empty(), "{r:?}");
    }
    assert_eq!(env_of(web_id.clone()).await, env);
    assert_eq!(deployments(web_id.clone()).await, 1);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pyo.database_links WHERE service_id = $1")
            .bind(&web_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(n, 1);

    ready.abort();
    sqlx::query("DELETE FROM pyo.deployments WHERE team_id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.databases WHERE team_id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    drop_team(&db, &team, &user).await;
}

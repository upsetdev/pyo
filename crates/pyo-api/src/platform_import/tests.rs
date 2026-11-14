//! Mapping tests against recorded answers shaped as each platform's API
//! answers (tests/fixtures/platforms; see coolify.rs, dokploy.rs,
//! railway.rs and vercel.rs for the documents and source files each field
//! comes from).

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
    assert!(has(&web.warnings, "DATABASE_URL points at Postgres there"));
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
    assert!(has(&api.warnings, "REDIS_URL points at Redis there"));

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
    assert!(has(
        &shop.warnings,
        "POSTGRES_URL, POSTGRES_URL_NON_POOLING point at Postgres database there"
    ));

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
    assert!(has(
        &docs.warnings,
        "POSTGRES_URL points at Postgres database there"
    ));

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
        let first = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
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
        let second = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
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

        let first = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
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

        let second = create(&s, &user, &team, None, plan.clone(), Some(&items), false)
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

//! Pyo's built-in sandbox templates ([`ALL`]), from upsetdev/pyo-sandbox-images
//! ([`super::images`]):
//! - `base`: Ubuntu with the tools coding agents expect. A sandbox created
//!   without a template starts from it.
//! - `desktop`: `base` with a desktop, voxl on port 6080 and a browser with
//!   CDP on port 9222, started on first use. Its sandboxes get 2 CPUs and
//!   2 GiB unless the create says otherwise.
//!
//! They're the installation's, so every team has them. For each:
//! - Its image, in this order: its env var (`PYO_SANDBOX_BASE_REF`,
//!   `PYO_SANDBOX_DESKTOP_REF`) when that's pinned by digest, for dev
//!   builds and pinning. Else the newest of the images.json Pyo last read
//!   and the one it shipped with ([`super::images::current`]).
//! - Seeding: when a worker starts, in every sandbox round and when Pyo
//!   reads a newer images.json, Pyo makes sure it exists and points at that
//!   image. It's idempotent. It's ready to use from the start, with or
//!   without servers. Lists show it even before it's added ([`listed`]).
//! - On first use: servers don't make it when they join. The first sandbox
//!   that needs it on a server has the server make it, then starts
//!   ([`super::firstuse`]). Later ones there start from that copy.
//! - On a new image: it gets a new revision (the digest's start). Servers
//!   that have an older one make it beside it in the background, and
//!   [`super::templates`] moves new sandboxes over once it's ready and
//!   deletes the old one once no sandbox uses it. Servers that never used it
//!   stay as they are. It only moves forward: an older Pyo still running
//!   during an update, or an older build of the images, never moves it back
//!   ([`decide`]).
//! - Nobody deletes it, and no other template may take its name.

use std::sync::atomic::{AtomicU8, Ordering};

use pyo_proto::{pinned_image, version_newer};
use serde_json::{Value, json};

use super::images::Images;
use crate::AppState;

/// One of Pyo's built-in templates.
#[derive(Debug)]
pub struct Builtin {
    /// Its name, which sandboxes ask for, and its image's in images.json.
    pub name: &'static str,
    /// Its id, the same on every installation.
    pub id: &'static str,
    /// What it runs, when its image comes from its env var.
    pub os: &'static str,
    /// The env var that overrides its image.
    pub env: &'static str,
    /// The size its sandboxes get when the create gives none.
    pub cpus: u32,
    pub memory_mib: u32,
}

pub const BASE: Builtin = Builtin {
    name: "base",
    id: "stp_base",
    os: "Ubuntu 24.04",
    env: pyo_proto::SANDBOX_BASE_ENV,
    cpus: pyo_proto::sandbox::DEFAULT_CPUS,
    memory_mib: pyo_proto::sandbox::DEFAULT_MEMORY_MIB,
};

pub const DESKTOP: Builtin = Builtin {
    name: "desktop",
    id: "stp_desktop",
    os: "Ubuntu 24.04",
    env: pyo_proto::SANDBOX_DESKTOP_ENV,
    // The browser and the screen need room.
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

/// The image to seed a built-in template from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    pub image: String,
    /// The Pyo that seeds it.
    pub release: String,
    /// What it runs, like "Ubuntu 24.04".
    pub os: String,
    /// The images' build date (YYYYMMDD) when they come from images.json.
    pub build: Option<String>,
}

/// `b`'s image from its env var, when that's pinned by digest. None
/// otherwise (scripts/pyo-up.sh leaves it empty unless it built one).
pub fn from_env(b: &Builtin) -> Option<Wanted> {
    let image = std::env::var(b.env).ok()?;
    let image = image.trim();
    pinned_image(image).then(|| Wanted {
        image: image.to_owned(),
        release: crate::updates::current().to_owned(),
        os: b.os.to_owned(),
        build: None,
    })
}

/// `b`'s image in images.json.
pub fn from_images(b: &Builtin, i: &Images) -> Option<Wanted> {
    let image = i.image(b.name)?;
    pinned_image(image).then(|| Wanted {
        image: image.to_owned(),
        release: crate::updates::current().to_owned(),
        os: i.os(),
        build: Some(i.date.clone()),
    })
}

/// The revision servers keep an image under: the first 12 hex digits of
/// its digest.
pub fn revision(image: &str) -> Option<String> {
    let (_, digest) = image.split_once("@sha256:")?;
    (digest.len() >= 12).then(|| digest[..12].to_owned())
}

/// The template's source for an image.
pub fn source(w: &Wanted) -> Value {
    let mut s = json!({ "kind": "image", "image": w.image, "os": w.os, "release": w.release });
    if let Some(build) = &w.build {
        s["build"] = json!(build);
    }
    s
}

/// Why `b` can't be used yet, when Pyo has no image for it.
pub fn unavailable(b: &Builtin) -> String {
    format!(
        "Pyo has no image for {}. Set {} to an image pinned by digest, or update Pyo.",
        b.name, b.env
    )
}

/// `b` as a list shows it before it's added: as it will be, from the image
/// it would have. Unavailable, with why, when there's none.
pub fn view(b: &Builtin, want: Option<&Wanted>) -> Value {
    let (source, state, error) = match want {
        Some(w) => (source(w), "ready", None),
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

/// Forward only: a newer Pyo's choice stays (an older Pyo still runs
/// during an update), and so does a newer build of the images. A release
/// candidate's choice compares as its release's does.
pub fn decide(have: Option<&Value>, want: &Wanted) -> Seed {
    let Some(src) = have else {
        return Seed::Insert;
    };
    if src["image"].as_str() == Some(want.image.as_str()) {
        return Seed::Keep;
    }
    if src["release"]
        .as_str()
        .is_some_and(|theirs| version_newer(theirs, &want.release))
    {
        return Seed::Keep;
    }
    if let (Some(a), Some(b)) = (src["build"].as_str(), want.build.as_deref())
        && a > b
    {
        return Seed::Keep;
    }
    Seed::Move
}

/// Which image `b` takes: `env`'s (its env var, pinned) over `images`'s.
pub fn choose(b: &Builtin, env: Option<Wanted>, images: Option<&Images>) -> Option<Wanted> {
    env.or_else(|| from_images(b, images?))
}

/// The image `b` should have now: its env var's, else images.json's.
pub async fn wanted(db: &sqlx::PgPool, b: &Builtin) -> Option<Wanted> {
    if let Some(w) = from_env(b) {
        return Some(w);
    }
    choose(b, None, super::images::current(db).await.as_ref())
}

/// Which templates' missing image was said (a bit each, by place in
/// [`ALL`]), once per process, so it isn't repeated every round.
static SAID_MISSING: AtomicU8 = AtomicU8::new(0);

/// Makes sure `b` exists and has the image it should. Tells the servers at
/// once when that changed. False when there's no image for it.
pub async fn ensure(s: &AppState, b: &'static Builtin) -> anyhow::Result<bool> {
    let Some(want) = wanted(&s.db, b).await else {
        let bit = 1 << ALL.iter().position(|x| x.id == b.id).unwrap_or(0);
        if SAID_MISSING.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            tracing::warn!("sandbox template {}: {}", b.name, unavailable(b));
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
            .bind(source(&want))
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
            .bind(source(&want))
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

/// On a worker's start: reads the latest images.json once, then
/// [`ensure_all`], retried while the database is busy. Sandbox rounds and
/// [`super::images::runner`] keep them in step after that.
pub async fn seed(s: AppState) {
    if let Err(e) = super::images::refresh(&s.db).await {
        tracing::warn!(error = %format!("{e:#}"), "sandbox images: couldn't keep the latest");
    }
    let mut attempt = 0u64;
    while attempt < 10 && !s.stopping() {
        match ensure_all(&s).await {
            Ok(_) => return,
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

    const A: &str = "ghcr.io/upsetdev/pyo-sandbox-images/base@sha256:aaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const B: &str = "ghcr.io/upsetdev/pyo-sandbox-images/base@sha256:cccccccccccc0000000000000000000000000000000000000000000000000000";

    fn want(image: &str, release: &str) -> Wanted {
        Wanted {
            image: image.into(),
            release: release.into(),
            os: "Ubuntu 24.04".into(),
            build: None,
        }
    }

    fn built(image: &str, release: &str, build: &str) -> Wanted {
        Wanted {
            build: Some(build.into()),
            ..want(image, release)
        }
    }

    fn images() -> &'static Images {
        super::super::images::shipped().unwrap()
    }

    #[test]
    fn seeding_is_idempotent() {
        let w = want(A, "0.4.1");
        assert_eq!(decide(None, &w), Seed::Insert);
        let src = source(&w);
        assert_eq!(decide(Some(&src), &w), Seed::Keep, "the same image again");
        assert_eq!(src["os"], "Ubuntu 24.04");
        assert_eq!(src["kind"], "image");
        assert!(src.get("build").is_none());
        let src = source(&built(A, "0.4.1", "20261116"));
        assert_eq!(src["build"], "20261116");
    }

    #[test]
    fn a_new_release_moves_it_forward_only() {
        let old = source(&want(A, "0.4.1"));
        let new = want(B, "0.4.2");
        assert_eq!(decide(Some(&old), &new), Seed::Move);
        // The older Pyo, still running during the update, leaves it.
        assert_eq!(decide(Some(&source(&new)), &want(A, "0.4.1")), Seed::Keep);
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

    /// A release candidate decides as its release does: it leaves a newer
    /// Pyo's choice and moves an older one's. Its release moves its choice.
    #[test]
    fn a_release_candidate_moves_it_as_its_release_does() {
        for theirs in ["0.51.0", "0.52.1", "99.0.1", ""] {
            let have = if theirs.is_empty() {
                json!({ "kind": "image", "image": A })
            } else {
                source(&want(A, theirs))
            };
            assert_eq!(
                decide(Some(&have), &want(B, "0.52.0-rc.1")),
                decide(Some(&have), &want(B, "0.52.0")),
                "{theirs}"
            );
        }
        assert_eq!(decide(Some(&source(&want(A, "99.0.1"))), &want(B, "0.52.0-rc.1")), Seed::Keep);
        assert_eq!(decide(Some(&source(&want(A, "0.51.0"))), &want(B, "0.52.0-rc.1")), Seed::Move);
        // The release after its candidate moves it, and the candidate,
        // still running during that update, leaves the release's.
        let rc = want(A, "0.52.0-rc.1");
        let release = want(B, "0.52.0");
        assert_eq!(decide(Some(&source(&rc)), &release), Seed::Move);
        assert_eq!(decide(Some(&source(&release)), &rc), Seed::Keep);
    }

    #[test]
    fn a_newer_build_of_the_images_moves_it_forward_only() {
        let old = built(A, "0.5.0", "20261116");
        let new = built(B, "0.5.0", "20261123");
        assert_eq!(decide(Some(&source(&old)), &new), Seed::Move);
        // A replica that hasn't read the newer build yet leaves it.
        assert_eq!(decide(Some(&source(&new)), &old), Seed::Keep);
        // A rebuild the same day moves it.
        assert_eq!(
            decide(Some(&source(&old)), &built(B, "0.5.0", "20261116")),
            Seed::Move
        );
        // A newer Pyo with an older build (it couldn't reach GitHub) leaves
        // it too.
        assert_eq!(
            decide(Some(&source(&new)), &built(A, "0.5.1", "20261116")),
            Seed::Keep
        );
        // An older Pyo (0.4.x, Debian from release.json) never moves it
        // back, and the images move it from there.
        let debian = json!({ "kind": "image", "os": "Debian 13", "release": "0.4.1",
            "image": "ghcr.io/upsetdev/pyo-sandbox-base:0.4.1@sha256:aaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" });
        assert_eq!(decide(Some(&debian), &new), Seed::Move);
        let older_pyo = Wanted {
            image: debian["image"].as_str().unwrap().into(),
            release: "0.4.1".into(),
            os: "Debian 13".into(),
            build: None,
        };
        assert_eq!(decide(Some(&source(&new)), &older_pyo), Seed::Keep);
        // A pinned image (env var) of the same Pyo moves it, either way.
        assert_eq!(decide(Some(&source(&new)), &want(A, "0.5.0")), Seed::Move);
    }

    #[test]
    fn the_env_var_wins_over_images_json() {
        let i = images();
        let from_json = choose(&BASE, None, Some(i)).unwrap();
        assert_eq!(from_json.image, i.image("base").unwrap());
        assert_eq!(from_json.os, i.os());
        assert_eq!(from_json.build.as_deref(), Some(i.date.as_str()));
        assert_eq!(
            choose(&DESKTOP, None, Some(i)).unwrap().image,
            i.image("desktop").unwrap()
        );
        let pinned = want(A, "0.5.0");
        assert_eq!(
            choose(&BASE, Some(pinned.clone()), Some(i)),
            Some(pinned.clone())
        );
        assert_eq!(choose(&BASE, Some(pinned.clone()), None), Some(pinned));
        assert_eq!(choose(&BASE, None, None), None);
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
            "Pyo has no image for base. Set PYO_SANDBOX_BASE_REF to an image pinned by digest, or update Pyo."
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
        // desktop's sandboxes get room for the browser.
        const { assert!(DESKTOP.cpus >= 2 && DESKTOP.memory_mib >= 2048) };
        for b in ALL {
            assert!(pyo_proto::sandbox::is_template_name(b.name));
            assert!(
                !b.env.ends_with("_IMAGE"),
                "older Pyos refuse new *_IMAGE vars"
            );
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
        assert_eq!(
            seed_from(&s, &BASE, &old).await.unwrap(),
            Seed::Keep,
            "idempotent"
        );
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
        assert_eq!(views[0]["source"]["os"], "Ubuntu 24.04");
        // desktop right after, listed before it's added. A team's template
        // named before both still comes after them.
        sqlx::raw_sql(
            "INSERT INTO pyo.teams (id, name, slug) VALUES ('tm_any', 'any', 'any');
             INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by)
             VALUES ('stp_alpine', 'tm_any', 'alpine', '{\"kind\": \"image\", \"image\": \"alpine\"}', 'ready', 'x')",
        )
        .execute(&db)
        .await
        .unwrap();
        let views = super::super::template_views(&db, &["tm_any".into()], None)
            .await
            .unwrap();
        let names: Vec<&str> = views.iter().filter_map(|v| v["name"].as_str()).collect();
        assert_eq!(names, ["base", "desktop", "alpine"]);
        assert_eq!(views[1]["id"], DESKTOP.id);
        assert_eq!(views[1]["builtin"], true);
        sqlx::query("DELETE FROM pyo.sandbox_templates WHERE id = 'stp_alpine'")
            .execute(&db)
            .await
            .unwrap();

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

//! Pyo's built-in sandbox template `base`: Debian 13 with the common tools
//! (deploy/images/sandbox-base.Dockerfile). It's the installation's, so
//! every team has it, and a sandbox created without a template starts
//! from it.
//!
//! - Seeding: when a worker starts, and in every sandbox round, Pyo makes
//!   sure `base` exists and points at its release's image
//!   (`PYO_SANDBOX_BASE_REF`, pinned by digest in release.json). It's
//!   idempotent.
//! - On update: a new release has a new image, so `base` gets a new
//!   revision (the digest's start). Servers make it beside the old one in
//!   the background, and [`super::templates`] moves new sandboxes over
//!   once it's ready and deletes the old one once no sandbox uses it.
//!   An older Pyo still running during the update never moves it back.
//! - Dev builds have no image of their own: they use the latest release
//!   Pyo found when it checked for updates, or wait for one.
//! - Nobody deletes it, and no other template may be named `base`.

use std::sync::atomic::{AtomicBool, Ordering};

use pyo_proto::{parse_version, pinned_image};
use serde_json::{Value, json};

use crate::AppState;

/// Its name, which sandboxes ask for.
pub const NAME: &str = "base";
/// Its id, the same on every installation.
pub const ID: &str = "stp_base";
/// What it runs, shown beside its image.
pub const OS: &str = "Debian 13";

/// The image to seed `base` from, and the release it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub image: String,
    pub release: String,
}

/// This release's image, from `PYO_SANDBOX_BASE_REF`. None for a build
/// that isn't a release (scripts/pyo-up.sh leaves it empty).
pub fn from_env() -> Option<Wanted> {
    let image = std::env::var(pyo_proto::SANDBOX_BASE_ENV).ok()?;
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
pub fn source(w: &Wanted) -> Value {
    json!({ "kind": "image", "image": w.image, "os": OS, "release": w.release })
}

/// What seeding does, given the template's source now (None: there's no
/// `base` yet).
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

/// The image `base` should have now: this release's, else (a dev build)
/// the latest release found by the update check.
async fn wanted(s: &AppState) -> Option<Wanted> {
    if let Some(w) = from_env() {
        return Some(w);
    }
    let found = crate::updates::checked(&s.db).await.ok()?.found?;
    let image = found.release.images.sandbox_base;
    let pinned = pinned_image(&image);
    pinned.then_some(Wanted {
        image,
        release: found.release.version,
    })
}

/// Said once per process, so a dev build doesn't repeat it every round.
static SAID_WAITING: AtomicBool = AtomicBool::new(false);

/// Makes sure `base` exists and has the image it should. Tells the servers
/// at once when that changed.
pub async fn ensure(s: &AppState) -> anyhow::Result<()> {
    let Some(want) = wanted(s).await else {
        if !SAID_WAITING.swap(true, Ordering::Relaxed) {
            tracing::info!(
                "sandbox template base: this build has no image for it, and no release was found yet, so it's added later"
            );
        }
        return Ok(());
    };
    seed_from(s, &want).await.map(|_| ())
}

/// Seeds `base` from `want`, and says what it did.
pub async fn seed_from(s: &AppState, want: &Wanted) -> anyhow::Result<Seed> {
    let want = want.clone();
    let rev = revision(&want.image);
    let mut tx = s.db.begin().await?;
    let have: Option<Value> =
        sqlx::query_scalar("SELECT source FROM pyo.sandbox_templates WHERE id = $1 FOR UPDATE")
            .bind(ID)
            .fetch_optional(&mut *tx)
            .await?;
    let seed = decide(have.as_ref(), &want);
    match seed {
        Seed::Keep => return Ok(Seed::Keep),
        Seed::Insert => {
            let res = sqlx::query(
                "INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by, builtin, revision)
                 VALUES ($1, NULL, $2, $3, 'pending', 'Pyo', true, $4)
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(ID)
            .bind(NAME)
            .bind(source(&want))
            .bind(&rev)
            .execute(&mut *tx)
            .await;
            if let Err(e) = res {
                if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
                    tracing::warn!(
                        "sandbox template base: the installation has another template named base, so Pyo's isn't added"
                    );
                    return Ok(Seed::Keep);
                }
                return Err(e.into());
            }
            tracing::info!(image = %want.image, "sandbox template base added");
        }
        Seed::Move => {
            sqlx::query(
                "UPDATE pyo.sandbox_templates
                    SET source = $2, revision = $3, builtin = true, error = NULL, updated_at = now()
                  WHERE id = $1",
            )
            .bind(ID)
            .bind(source(&want))
            .bind(&rev)
            .execute(&mut *tx)
            .await?;
            tracing::info!(image = %want.image, "sandbox template base moved to a new image");
        }
    }
    tx.commit().await?;
    tokio::spawn(super::templates::push(s.clone(), ID.to_owned()));
    Ok(seed)
}

/// On a worker's start: [`ensure`], retried while the database is busy.
pub async fn seed(s: AppState) {
    for attempt in 1..=10u64 {
        match ensure(&s).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(attempt, error = %format!("{e:#}"), "sandbox template base: couldn't add it, retrying");
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
        let src = source(&w);
        assert_eq!(decide(Some(&src), &w), Seed::Keep, "the same image again");
        assert_eq!(src["os"], "Debian 13");
        assert_eq!(src["kind"], "image");
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

    #[test]
    fn revisions_come_from_the_digest() {
        assert_eq!(revision(A).as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(revision(B).as_deref(), Some("cccccccccccc"));
        assert_eq!(revision("debian:13"), None);
        let name = super::super::templates::server_name(ID, revision(A).as_deref());
        assert_eq!(name, "stp_base.aaaaaaaaaaaa");
        assert!(pyo_proto::sandbox::is_template_name(&name));
    }

    #[test]
    fn only_a_pinned_image_from_the_env_counts() {
        // SAFETY: tests in this module don't read it concurrently.
        unsafe { std::env::set_var("PYO_SANDBOX_BASE_REF", "") };
        assert_eq!(from_env(), None, "a dev build (pyo-up.sh)");
        unsafe {
            std::env::set_var(
                "PYO_SANDBOX_BASE_REF",
                "127.0.0.1:5005/pyo/sandbox-base:dev",
            )
        };
        assert_eq!(from_env(), None, "not pinned");
        unsafe { std::env::set_var("PYO_SANDBOX_BASE_REF", A) };
        assert_eq!(from_env(), Some(want(A, crate::updates::current())));
        unsafe { std::env::remove_var("PYO_SANDBOX_BASE_REF") };
        assert_eq!(from_env(), None);
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
            .bind(ID)
            .fetch_one(&db)
            .await
            .unwrap()
        };

        let old = want(A, "0.4.1");
        assert_eq!(seed_from(&s, &old).await.unwrap(), Seed::Insert);
        assert_eq!(seed_from(&s, &old).await.unwrap(), Seed::Keep, "idempotent");
        let (n, state, src, rev, builtin, team) = row().await;
        assert_eq!(
            (n.as_str(), state.as_str(), rev.as_deref(), builtin, team),
            ("base", "pending", Some("aaaaaaaaaaaa"), true, None)
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
        assert_eq!(seed_from(&s, &new).await.unwrap(), Seed::Move);
        assert_eq!(seed_from(&s, &old).await.unwrap(), Seed::Keep);
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
        let e = super::super::delete_template(&s, &user, ID)
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

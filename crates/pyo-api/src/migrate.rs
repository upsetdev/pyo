//! Pyo's database migrations.

use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// Pyo's migrations.
pub fn migrator() -> Migrator {
    let mut m = sqlx::migrate!("./migrations");
    // A release has to start on a database a newer one already migrated (an
    // old pod restarting mid-rollout, or a rollback).
    m.set_ignore_missing(true);
    m
}

/// Brings `db` up to date. Safe with several processes starting at once:
/// sqlx holds a lock while migrating.
pub async fn run(db: &PgPool) -> anyhow::Result<()> {
    migrator().run(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real Postgres (DATABASE_URL, a throwaway one): a new
    /// database gets every migration, and starting again changes nothing.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_new_database_runs_every_migration() {
        use sqlx::postgres::PgConnectOptions;
        let opts: PgConnectOptions = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        let admin = PgPool::connect_with(opts.clone()).await.unwrap();
        let name = format!("pyo_migrate_{}", std::process::id());
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name}"
        )))
        .execute(&admin)
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPool::connect_with(opts.database(&name)).await.unwrap();

        let expected: Vec<i64> = migrator().iter().map(|m| m.version).collect();
        for _ in 0..2 {
            run(&pool).await.unwrap();
            let history: Vec<i64> = sqlx::query_scalar(
                "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(history, expected);
        }

        pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE {name}")))
            .execute(&admin)
            .await
            .unwrap();
    }
}

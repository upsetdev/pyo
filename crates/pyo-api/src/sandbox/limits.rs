//! Per-team sandbox limits, checked when a sandbox is created: how many run
//! at once, their CPUs, memory and disk together, and the longest lifetime.
//! The installation's admin sets them per team (`setSandboxLimits`).
//! Without a row a team gets [`Limits::default`].

use pyo_proto::sandbox::SandboxSpec;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// Sandboxes that may exist at once, paused ones included.
    pub max_running: i32,
    /// CPUs of those sandboxes together.
    pub max_cpus: i32,
    /// Memory of those sandboxes together, in MiB.
    pub max_memory_mib: i32,
    /// Disk of those sandboxes together, in MiB.
    pub max_disk_mib: i32,
    /// The longest lifetime one sandbox may have, in seconds, up to 365
    /// days. Null: no limit, and sandboxes may run until they're deleted.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub max_ttl_secs: Option<i32>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_running: 10,
            max_cpus: 16,
            max_memory_mib: 16 * 1024,
            max_disk_mib: 100 * 1024,
            max_ttl_secs: Some(24 * 3600),
        }
    }
}

impl Limits {
    /// Within what any single sandbox can have.
    pub fn validate(&self) -> Result<(), String> {
        let fields = [
            ("Running sandboxes", self.max_running),
            ("CPUs", self.max_cpus),
            ("Memory", self.max_memory_mib),
            ("Disk", self.max_disk_mib),
            ("Lifetime", self.max_ttl_secs.unwrap_or(0)),
        ];
        for (name, v) in fields {
            if v < 0 {
                return Err(format!("{name} can't be below 0."));
            }
        }
        if self
            .max_ttl_secs
            .is_some_and(|t| t as u64 > pyo_proto::sandbox::MAX_TTL_SECS)
        {
            return Err("The lifetime can be 365 days at most.".into());
        }
        Ok(())
    }
}

/// What a team's sandboxes take now: those that aren't stopped or failed.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, sqlx::FromRow, utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct InUse {
    pub sandboxes: i64,
    pub cpus: i64,
    pub memory_mib: i64,
    pub disk_mib: i64,
}

/// Whether one more sandbox like `spec` fits. The message is for people.
pub fn check(limits: &Limits, used: &InUse, spec: &SandboxSpec) -> Result<(), String> {
    let l = limits;
    if used.sandboxes + 1 > l.max_running as i64 {
        return Err(format!(
            "Your team has {} sandboxes, which is its limit. Delete one first.",
            used.sandboxes
        ));
    }
    if used.cpus + spec.cpus as i64 > l.max_cpus as i64 {
        return Err(format!(
            "This would take your team's sandboxes past their {} CPUs.",
            l.max_cpus
        ));
    }
    if used.memory_mib + spec.memory_mib as i64 > l.max_memory_mib as i64 {
        return Err(format!(
            "This would take your team's sandboxes past their {} MiB of memory.",
            l.max_memory_mib
        ));
    }
    if used.disk_mib + spec.disk_mib as i64 > l.max_disk_mib as i64 {
        return Err(format!(
            "This would take your team's sandboxes past their {} MiB of disk.",
            l.max_disk_mib
        ));
    }
    check_ttl(l, spec.ttl_secs)
}

/// Whether a lifetime is within the team's longest. `None` is no
/// lifetime, which only a team without a limit may have.
pub fn check_ttl(limits: &Limits, ttl_secs: Option<u64>) -> Result<(), String> {
    let Some(max) = limits.max_ttl_secs.map(|m| m.max(0) as u64) else {
        return Ok(());
    };
    if ttl_secs.is_none_or(|t| t > max) {
        return Err(format!(
            "Your team's sandboxes can live {} at most.",
            super::format_ttl(max)
        ));
    }
    Ok(())
}

/// A team's limits.
pub async fn load(db: &mut PgConnection, team_id: &str) -> ApiResult<Limits> {
    let row: Option<Limits> = sqlx::query_as(
        "SELECT max_running, max_cpus, max_memory_mib, max_disk_mib, max_ttl_secs FROM pyo.sandbox_limits WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_optional(db)
    .await?;
    Ok(row.unwrap_or_default())
}

/// What a team's sandboxes take now.
pub async fn in_use(db: &mut PgConnection, team_id: &str) -> ApiResult<InUse> {
    Ok(sqlx::query_as(
        "SELECT count(*) AS sandboxes,
                coalesce(sum((spec->>'cpus')::bigint), 0)::bigint AS cpus,
                coalesce(sum((spec->>'memoryMib')::bigint), 0)::bigint AS memory_mib,
                coalesce(sum((spec->>'diskMib')::bigint), 0)::bigint AS disk_mib
           FROM pyo.sandboxes WHERE team_id = $1 AND state NOT IN ('stopped', 'failed')",
    )
    .bind(team_id)
    .fetch_one(db)
    .await?)
}

/// Saves a team's limits.
pub async fn save(db: &mut PgConnection, team_id: &str, l: &Limits) -> ApiResult<()> {
    l.validate().map_err(ApiError::bad_request)?;
    sqlx::query(
        "INSERT INTO pyo.sandbox_limits (team_id, max_running, max_cpus, max_memory_mib, max_disk_mib, max_ttl_secs)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (team_id) DO UPDATE SET max_running = $2, max_cpus = $3, max_memory_mib = $4,
           max_disk_mib = $5, max_ttl_secs = $6, updated_at = now()",
    )
    .bind(team_id)
    .bind(l.max_running)
    .bind(l.max_cpus)
    .bind(l.max_memory_mib)
    .bind(l.max_disk_mib)
    .bind(l.max_ttl_secs)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cpus: u32, memory_mib: u32, ttl_secs: u64) -> SandboxSpec {
        spec_for(cpus, memory_mib, Some(ttl_secs))
    }

    fn spec_for(cpus: u32, memory_mib: u32, ttl_secs: Option<u64>) -> SandboxSpec {
        let mut s = SandboxSpec::new("sbx_abcdefghijklmnop", "tm_one", "node-22");
        s.cpus = cpus;
        s.memory_mib = memory_mib;
        s.disk_mib = 1024;
        s.ttl_secs = ttl_secs;
        s
    }

    #[test]
    fn an_empty_team_fits_a_default_sandbox() {
        let l = Limits::default();
        l.validate().unwrap();
        check(&l, &InUse::default(), &spec(1, 512, 300)).unwrap();
    }

    #[test]
    fn each_limit_is_enforced() {
        let l = Limits {
            max_running: 2,
            max_cpus: 4,
            max_memory_mib: 2048,
            max_disk_mib: 4096,
            max_ttl_secs: Some(3600),
        };
        let used = InUse {
            sandboxes: 1,
            cpus: 2,
            memory_mib: 1024,
            disk_mib: 1024,
        };
        check(&l, &used, &spec(2, 1024, 3600)).unwrap();
        let full = InUse {
            sandboxes: 2,
            ..used
        };
        assert!(
            check(&l, &full, &spec(1, 128, 60))
                .unwrap_err()
                .contains("2 sandboxes")
        );
        assert!(
            check(&l, &used, &spec(3, 128, 60))
                .unwrap_err()
                .contains("4 CPUs")
        );
        assert!(
            check(&l, &used, &spec(1, 1025, 60))
                .unwrap_err()
                .contains("2048 MiB of memory")
        );
        let mut big = spec(1, 128, 60);
        big.disk_mib = 3073;
        assert!(
            check(&l, &used, &big)
                .unwrap_err()
                .contains("4096 MiB of disk")
        );
        assert_eq!(
            check(&l, &used, &spec(1, 128, 3601)).unwrap_err(),
            "Your team's sandboxes can live 1h at most."
        );
        check_ttl(&l, Some(3600)).unwrap();
        assert_eq!(
            check(&l, &used, &spec_for(1, 128, None)).unwrap_err(),
            "Your team's sandboxes can live 1h at most."
        );
    }

    #[test]
    fn a_zero_limit_turns_sandboxes_off() {
        let l = Limits {
            max_running: 0,
            ..Limits::default()
        };
        assert!(check(&l, &InUse::default(), &spec(1, 128, 60)).is_err());
    }

    #[test]
    fn limits_are_checked_before_saving() {
        let l = Limits {
            max_cpus: -1,
            ..Limits::default()
        };
        assert!(l.validate().is_err());
        let year = pyo_proto::sandbox::MAX_TTL_SECS as i32;
        for ok in [Some(2 * 24 * 3600), Some(year), None] {
            Limits {
                max_ttl_secs: ok,
                ..Limits::default()
            }
            .validate()
            .unwrap();
        }
        let l = Limits {
            max_ttl_secs: Some(year + 1),
            ..Limits::default()
        };
        assert!(l.validate().unwrap_err().contains("365 days"));
        let l = Limits {
            max_ttl_secs: Some(-1),
            ..Limits::default()
        };
        assert!(l.validate().is_err());
    }

    #[test]
    fn new_teams_keep_a_day_at_most() {
        let l = Limits::default();
        assert_eq!(l.max_ttl_secs, Some(86400));
        check_ttl(&l, Some(86400)).unwrap();
        assert!(check_ttl(&l, Some(86401)).is_err());
        assert_eq!(
            check_ttl(&l, None).unwrap_err(),
            "Your team's sandboxes can live 1d at most."
        );
    }

    #[test]
    fn a_team_without_a_limit_takes_any_lifetime() {
        let l = Limits {
            max_ttl_secs: None,
            ..Limits::default()
        };
        check_ttl(&l, None).unwrap();
        check_ttl(&l, Some(pyo_proto::sandbox::MAX_TTL_SECS)).unwrap();
        check(&l, &InUse::default(), &spec_for(1, 128, None)).unwrap();
        let year = Limits {
            max_ttl_secs: Some(pyo_proto::sandbox::MAX_TTL_SECS as i32),
            ..Limits::default()
        };
        check_ttl(&year, Some(pyo_proto::sandbox::MAX_TTL_SECS)).unwrap();
        assert!(check_ttl(&year, None).is_err());
    }

    #[test]
    fn no_limit_is_written_as_null() {
        let l: Limits = serde_json::from_value(serde_json::json!({
            "maxRunning": 1, "maxCpus": 1, "maxMemoryMib": 1, "maxDiskMib": 1, "maxTtlSecs": null
        }))
        .unwrap();
        assert_eq!(l.max_ttl_secs, None);
        assert_eq!(
            serde_json::to_value(l).unwrap()["maxTtlSecs"],
            serde_json::Value::Null
        );
        // Left out isn't no limit: it's a mistake.
        assert!(
            serde_json::from_value::<Limits>(serde_json::json!({
                "maxRunning": 1, "maxCpus": 1, "maxMemoryMib": 1, "maxDiskMib": 1
            }))
            .is_err()
        );
    }
}

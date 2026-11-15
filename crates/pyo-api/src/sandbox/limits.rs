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
    /// The longest lifetime one sandbox may have, in seconds.
    pub max_ttl_secs: i32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_running: 10,
            max_cpus: 16,
            max_memory_mib: 16 * 1024,
            max_disk_mib: 100 * 1024,
            max_ttl_secs: 24 * 3600,
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
            ("Lifetime", self.max_ttl_secs),
        ];
        for (name, v) in fields {
            if v < 0 {
                return Err(format!("{name} can't be below 0."));
            }
        }
        if self.max_ttl_secs as u64 > pyo_proto::sandbox::MAX_TTL_SECS {
            return Err("The lifetime can be 24 hours at most.".into());
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

/// Whether a lifetime is within the team's longest.
pub fn check_ttl(limits: &Limits, ttl_secs: u64) -> Result<(), String> {
    if ttl_secs > limits.max_ttl_secs as u64 {
        return Err(format!(
            "Your team's sandboxes can live {} at most.",
            super::format_ttl(limits.max_ttl_secs as u64)
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
            max_ttl_secs: 3600,
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
        check_ttl(&l, 3600).unwrap();
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
        let l = Limits {
            max_ttl_secs: 2 * 24 * 3600,
            ..Limits::default()
        };
        assert!(l.validate().unwrap_err().contains("24 hours"));
    }
}

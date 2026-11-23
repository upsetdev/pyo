//! Timeline history files in a Postgres database's backups.
//!
//! Every failover or switchover starts a new timeline, and Postgres writes
//! `<timeline>.history` for it. A restore needs that file from the backups:
//! without it the first WAL file of the timeline is refused ("unexpected
//! timeline ID") and the restore fails.
//!
//! Postgres hands the file to the archiver once, when the timeline starts.
//! While backups are off, CloudNativePG's archiver takes it and stores
//! nothing, so backups turned on later never get it, and a base backup
//! that starts in that WAL file can't be restored.
//!
//! So when backups are on and the database is past its first timeline, a
//! Job puts the missing history files in the backups: it asks the primary
//! for each (`TIMELINE_HISTORY`, over a replication connection with the
//! cluster's own replication certificate) and archives it with
//! barman-cloud, as the archiver would have. Files already there are left
//! alone. It runs once per timeline and destination: the Cluster's
//! `pyo.dev/timeline-history` annotation records what's done. Backups
//! taken before the Job ran are restorable once it's done.

use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api, Resource, ResourceExt,
    api::{DynamicObject, ListParams, Patch, PatchParams, PostParams},
};
use serde_json::{Value, json};

use crate::{
    controller::{Kube, labels},
    crd::Destination,
    postgres::destination_path,
};

/// On the Cluster: the timeline and destination whose history files are in
/// the backups ([`fingerprint`]).
pub const ANNOTATION: &str = "pyo.dev/timeline-history";
/// The Job's pods (`pyo.dev/component`), as the backups' other objects.
const COMPONENT: &str = "backup";

/// What the Job did, for a timeline and a destination: another timeline
/// (a failover) or another destination, and it runs again.
pub fn fingerprint(timeline: i64, d: &Destination, namespace: &str, server: &str) -> String {
    format!(
        "{timeline} {}/{server} {}",
        destination_path(d, namespace),
        d.endpoint_url
    )
}

/// The Job for one fingerprint: `<name>-history-<hash>`.
pub fn job_name(db: &str, fingerprint: &str) -> String {
    let hash = fingerprint
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
    format!("{db}-history-{:08x}", hash >> 32)
}

/// Whether the history files of this timeline may be missing from the
/// backups: past the first timeline, and not recorded as done.
pub fn needed(timeline: i64, recorded: Option<&str>, fingerprint: &str) -> bool {
    timeline > 1 && recorded != Some(fingerprint)
}

/// For each timeline from 2 to the current one: leave the history file
/// when the backups have it, else read it from the primary and archive it.
/// barman-cloud-wal-restore exits 1 when the file isn't there, and with
/// something else when the storage can't be reached or refuses.
///
/// An older timeline's file the primary no longer has is skipped: the
/// current one's lists every timeline before it. The current one's must
/// get there.
pub const SCRIPT: &str = r#"set -uo pipefail
fail() { echo "$1" | tee /dev/termination-log >&2; exit 1; }
# The last thing a tool said, without barman's time and level.
why() { grep -v '^$' /work/err | tail -n 1 | sed 's/^.* ERROR: //'; }
conn="host=$PGHOST_RW port=5432 dbname=postgres user=streaming_replica replication=true sslmode=verify-ca sslcert=/certs/tls.crt sslkey=/certs/tls.key sslrootcert=/ca/ca.crt connect_timeout=10"
cd /work || fail "no work directory"
added=""
for ((t = 2; t <= TIMELINE; t++)); do
  f=$(printf '%08X.history' "$t")
  barman-cloud-wal-restore --endpoint-url "$ENDPOINT" "$DESTINATION" "$SERVER" "$f" "/work/have-$f" 2>/work/err
  rc=$?
  if [ "$rc" = 0 ]; then echo "pyo-history: $f is in the backups"; continue; fi
  if [ "$rc" != 1 ]; then fail "can't read the backups: $(why)"; fi
  if ! psql "$conn" -XAtc "TIMELINE_HISTORY $t" >/work/raw 2>/work/err; then
    if [ "$t" -lt "$TIMELINE" ]; then echo "pyo-history: the primary has no $f, skipped"; continue; fi
    fail "can't read $f from the primary: $(why)"
  fi
  # psql prints "<file name>|<content>" and one newline of its own.
  sed '1s/^[^|]*|//' /work/raw | head -c -1 >"/work/$f"
  [ -s "/work/$f" ] || fail "the primary sent an empty $f"
  barman-cloud-wal-archive --gzip --endpoint-url "$ENDPOINT" "$DESTINATION" "$SERVER" "/work/$f" 2>/work/err \
    || fail "can't write $f to the backups: $(why)"
  echo "pyo-history: archived $f"
  added="$added $f"
done
# What was added, for the event.
if [ -n "$added" ]; then echo "added$added" >/dev/termination-log; fi
"#;

/// The annotation's value on a Cluster.
fn recorded(cluster: &DynamicObject) -> Option<&str> {
    cluster.annotations().get(ANNOTATION).map(String::as_str)
}

impl Kube<'_> {
    /// The Job that puts the history files up to `timeline` in the backups
    /// at `d`.
    pub fn history_job(&self, d: &Destination, timeline: i64, name: &str) -> Value {
        let db = self.name();
        let key = |k: &str| json!({ "secretKeyRef": { "name": d.credentials_secret, "key": k } });
        let plain = |k: &str, v: String| json!({ "name": k, "value": v });
        let mut env = vec![
            plain("PGHOST_RW", self.svc_host(&format!("{db}-rw"))),
            plain("TIMELINE", timeline.to_string()),
            plain("ENDPOINT", d.endpoint_url.clone()),
            plain("DESTINATION", destination_path(d, &self.ns())),
            plain("SERVER", db.clone()),
            json!({ "name": "AWS_ACCESS_KEY_ID", "valueFrom": key("accessKeyId") }),
            json!({ "name": "AWS_SECRET_ACCESS_KEY", "valueFrom": key("secretAccessKey") }),
        ];
        if !d.region.is_empty() {
            env.push(plain("AWS_DEFAULT_REGION", d.region.clone()));
        }
        let lbl = labels(self.db, COMPONENT);
        // The key is root's with the group's read bit: what libpq accepts.
        let secret = |n: String| json!({ "secretName": n, "defaultMode": 0o640 });
        json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": name, "namespace": self.ns(), "labels": lbl, "ownerReferences": [self.db.controller_owner_ref(&())] },
            "spec": {
                "backoffLimit": 2,
                "activeDeadlineSeconds": 600,
                // A failed one is made again once it's gone.
                "ttlSecondsAfterFinished": 600,
                "template": { "metadata": { "labels": lbl }, "spec": {
                    "restartPolicy": "Never",
                    "automountServiceAccountToken": false,
                    // CloudNativePG's images run Postgres as uid 26.
                    "securityContext": { "runAsNonRoot": true, "runAsUser": 26, "runAsGroup": 26, "fsGroup": 26, "seccompProfile": { "type": "RuntimeDefault" } },
                    "containers": [{
                        "name": "history",
                        "image": self.pg_image(),
                        "command": ["bash", "-c", SCRIPT],
                        "env": env,
                        "volumeMounts": [
                            { "name": "certs", "mountPath": "/certs", "readOnly": true },
                            { "name": "ca", "mountPath": "/ca", "readOnly": true },
                            { "name": "work", "mountPath": "/work" },
                        ],
                        "terminationMessagePolicy": "FallbackToLogsOnError",
                        "resources": { "requests": { "cpu": "20m", "memory": "64Mi" }, "limits": { "memory": "256Mi" } },
                        "securityContext": { "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } },
                    }],
                    "volumes": [
                        { "name": "certs", "secret": secret(format!("{db}-replication")) },
                        { "name": "ca", "secret": secret(format!("{db}-ca")) },
                        { "name": "work", "emptyDir": {} },
                    ],
                } },
            },
        })
    }

    /// Sees to the history files of a database with backups on. `cluster`
    /// is its CloudNativePG Cluster, `serving` whether its primary takes
    /// connections. Returns what's wrong, for the backup status: empty
    /// when the files are there or on their way.
    pub async fn timeline_history(
        &self,
        d: &Destination,
        cluster: &DynamicObject,
        serving: bool,
    ) -> anyhow::Result<String> {
        let timeline = cluster.data["status"]["timelineID"].as_i64().unwrap_or(0);
        let want = fingerprint(timeline, d, &self.ns(), &self.name());
        if !needed(timeline, recorded(cluster), &want) || !serving {
            return Ok(String::new());
        }
        let name = job_name(&self.name(), &want);
        let api: Api<Job> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let Some(job) = api.get_opt(&name).await? else {
            let job: Job = serde_json::from_value(self.history_job(d, timeline, &name))?;
            match api.create(&PostParams::default(), &job).await {
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                r => {
                    r?;
                }
            }
            return Ok(String::new());
        };
        let done = |t: &str| {
            job.status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|c| c.iter().any(|c| c.type_ == t && c.status == "True"))
        };
        if done("Complete") {
            let patch = json!({ "metadata": { "annotations": { ANNOTATION: want } } });
            let ar = kube::api::ApiResource::from_gvk(&kube::api::GroupVersionKind::gvk(
                "postgresql.cnpg.io",
                "v1",
                "Cluster",
            ));
            Api::<DynamicObject>::namespaced_with(self.ctx.client.clone(), &self.ns(), &ar)
                .patch(&self.name(), &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
            // Only when it had something to add.
            if self.job_said(&name).await.starts_with("added") {
                self.event(
                    false,
                    "BackupHistoryAdded",
                    "Added the failover history to the backups".into(),
                )
                .await;
            }
            return Ok(String::new());
        }
        if !done("Failed") {
            return Ok(String::new());
        }
        // The Job goes after 10 minutes, and the next pass makes it again.
        Ok(failure_message(&self.job_said(&name).await))
    }

    /// What a Job's last pod left as its termination message.
    async fn job_said(&self, job: &str) -> String {
        let pods: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let Ok(list) = pods
            .list(&ListParams::default().labels(&format!("job-name={job}")))
            .await
        else {
            return String::new();
        };
        let mut pods = list.items;
        pods.sort_by_key(|p| p.metadata.creation_timestamp.clone());
        pods.iter()
            .rev()
            .find_map(|p| {
                p.status
                    .as_ref()?
                    .container_statuses
                    .iter()
                    .flatten()
                    .find_map(|c| c.state.as_ref()?.terminated.as_ref()?.message.clone())
            })
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

/// The backup status message while the history files can't be added.
pub fn failure_message(why: &str) -> String {
    let last = why.lines().last().unwrap_or("").trim();
    if last.is_empty() {
        "Backups can't be restored yet. Pyo is trying again".to_owned()
    } else {
        format!("Backups can't be restored yet: {last}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest() -> Destination {
        serde_json::from_value(json!({
            "endpointURL": "http://s3:3900", "bucket": "b", "path": "team", "region": "garage", "credentialsSecret": "s3",
        }))
        .unwrap()
    }

    #[test]
    fn only_past_the_first_timeline_and_once() {
        let fp = fingerprint(2, &dest(), "ns", "shop");
        assert_eq!(fp, "2 s3://b/team/ns/shop http://s3:3900");
        // A database that never failed over has no history file.
        assert!(!needed(1, None, &fingerprint(1, &dest(), "ns", "shop")));
        assert!(!needed(0, None, &fingerprint(0, &dest(), "ns", "shop")));
        // After a failover, until it's recorded.
        assert!(needed(2, None, &fp));
        assert!(!needed(2, Some(&fp), &fp));
        // Another failover, or another destination: again.
        assert!(needed(3, Some(&fp), &fingerprint(3, &dest(), "ns", "shop")));
        let mut other = dest();
        other.bucket = "c".into();
        assert!(needed(2, Some(&fp), &fingerprint(2, &other, "ns", "shop")));
    }

    #[test]
    fn job_names_follow_the_fingerprint() {
        let a = job_name("shop", &fingerprint(2, &dest(), "ns", "shop"));
        let b = job_name("shop", &fingerprint(3, &dest(), "ns", "shop"));
        assert!(a.starts_with("shop-history-") && a.len() == "shop-history-".len() + 8);
        assert_ne!(a, b);
        assert_eq!(a, job_name("shop", &fingerprint(2, &dest(), "ns", "shop")));
    }

    #[test]
    fn the_script_archives_only_what_is_missing() {
        // Syntax, with bash when it's there.
        if let Ok(out) = std::process::Command::new("bash").args(["-n", "-c", SCRIPT]).output() {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        // Not found (1) is the only answer that leads to an upload.
        assert!(SCRIPT.contains(r#"if [ "$rc" != 1 ]; then fail"#));
        assert!(SCRIPT.contains("TIMELINE_HISTORY $t"));
        assert!(SCRIPT.contains("replication=true"));
        assert!(SCRIPT.contains("sslmode=verify-ca"));
    }

    /// The script against stand-ins for psql and barman-cloud: what's in
    /// the backups stays, what's missing is written byte for byte, and a
    /// storage error stops it.
    #[test]
    fn the_script_run_with_stand_ins() {
        let Ok(bash) = std::process::Command::new("bash").arg("--version").output() else {
            return;
        };
        if !bash.status.success() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pyo-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["bin", "work", "store"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let write = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join("bin").join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // The store is a directory; STORE_RC makes reads fail another way.
        write(
            "barman-cloud-wal-restore",
            "#!/bin/bash\n[ -n \"${STORE_RC:-}\" ] && { echo '2026-01-01 00:00:00,000 [8] ERROR: boom' >&2; echo >&2; exit \"$STORE_RC\"; }\nf=\"${@: -2:1}\"; [ -f \"$STORE/$f\" ] || exit 1; cp \"$STORE/$f\" \"${@: -1}\"\n",
        );
        write(
            "barman-cloud-wal-archive",
            "#!/bin/bash\nf=\"${@: -1}\"; cp \"$f\" \"$STORE/$(basename \"$f\")\"\n",
        );
        // psql -At: the file's name, a bar, its content, and a newline.
        write(
            "psql",
            "#!/bin/bash\nt=\"${@: -1}\"; t=${t##* }\n[ \"$t\" = 3 ] && { echo 'ERROR: could not open file' >&2; exit 1; }\nprintf '%08X.history|1\\t0/6000128\\tno recovery target specified\\n\\n' \"$t\"\n",
        );
        let script = SCRIPT.replace("cd /work", &format!("cd {}", dir.join("work").display()))
            .replace("/work/", &format!("{}/", dir.join("work").display()))
            .replace("| tee /dev/termination-log", "")
            .replace(">/dev/termination-log", &format!(">{}/said", dir.join("work").display()));
        let run = |timeline: &str, store_rc: &str| {
            std::process::Command::new("bash")
                .args(["-c", &script])
                .env("PATH", format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap()))
                .env("STORE", dir.join("store"))
                .env("STORE_RC", store_rc)
                .env("TIMELINE", timeline)
                .envs([("PGHOST_RW", "pg"), ("ENDPOINT", "e"), ("DESTINATION", "d"), ("SERVER", "s")])
                .output()
                .unwrap()
        };
        let store = |f: &str| std::fs::read_to_string(dir.join("store").join(f)).ok();

        // Timeline 2's file is missing: written exactly as the primary has it.
        let out = run("2", "");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            store("00000002.history").as_deref(),
            Some("1\t0/6000128\tno recovery target specified\n")
        );
        // There already: left alone.
        std::fs::write(dir.join("store/00000002.history"), "kept\n").unwrap();
        let out = run("2", "");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("is in the backups"));
        assert_eq!(store("00000002.history").as_deref(), Some("kept\n"));
        // An older timeline the primary no longer has (3) is skipped, the
        // current one (4) is written.
        let out = run("4", "");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(store("00000003.history"), None);
        assert!(store("00000004.history").is_some());
        // The current one the primary can't give: a failure.
        std::fs::remove_file(dir.join("store/00000004.history")).unwrap();
        let out = run("3", "");
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("can't read 00000003.history from the primary"));
        // The storage can't be read: nothing is written.
        std::fs::remove_file(dir.join("store/00000002.history")).unwrap();
        let out = run("2", "2");
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("can't read the backups: boom"));
        assert_eq!(store("00000002.history"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_in_plain_words() {
        assert_eq!(
            failure_message("x\ncan't read the backups: denied"),
            "Backups can't be restored yet: can't read the backups: denied"
        );
        assert_eq!(
            failure_message(""),
            "Backups can't be restored yet. Pyo is trying again"
        );
    }
}

//! Cron job runs as the cluster has them: how each ended, and the end of
//! its output. Kubernetes keeps only the last few, and their pods' logs go
//! with them, so the output is read once a run finishes and sent while it's
//! recent: Pyo keeps it from there.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{Api, Client, ResourceExt, api::LogParams};
use pyo_proto::CronRunReport;

use crate::{
    cron::{CRON_LABEL, CRON_SERVICE_LABEL},
    observe::SERVICE_LABEL,
};

const JOB_LABEL: &str = "batch.kubernetes.io/job-name";
/// How long after finishing a run's output is still sent.
const RECENT_MS: i64 = 3 * 60_000;
const OUTPUT_LINES: i64 = 60;
const OUTPUT_BYTES: i64 = 8 * 1024;

/// Output already read, by Job uid: logs are read once per run.
fn outputs() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn time(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time) -> i64 {
    t.0.as_millisecond()
}

/// How a Job went, from it and its pod. Pure, so it can be tested.
pub fn report(job: &Job, pods: &[&Pod]) -> CronRunReport {
    let st = job.status.as_ref();
    let done = |n: Option<i32>| n.unwrap_or(0) > 0;
    let succeeded = if done(st.and_then(|s| s.succeeded)) {
        Some(true)
    } else if done(st.and_then(|s| s.failed)) {
        Some(false)
    } else {
        None
    };
    let pod = pods.iter().max_by_key(|p| {
        p.metadata
            .creation_timestamp
            .as_ref()
            .map(time)
            .unwrap_or(0)
    });
    let ended = pod
        .and_then(|p| p.status.as_ref())
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|c| c.first())
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.clone());
    let failed_condition = st
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.type_ == "Failed" && c.status == "True"));
    let finished_at = st
        .and_then(|s| s.completion_time.as_ref())
        .map(time)
        .or_else(|| {
            failed_condition
                .and_then(|c| c.last_transition_time.as_ref())
                .map(time)
        })
        .or_else(|| {
            ended
                .as_ref()
                .and_then(|e| e.finished_at.as_ref())
                .map(time)
        });
    let reason = ended
        .as_ref()
        .and_then(|e| e.reason.clone())
        .filter(|r| r != "Completed" && r != "Error")
        .or_else(|| {
            failed_condition
                .and_then(|c| c.reason.clone())
                .filter(|r| r != "BackoffLimitExceeded")
        });
    CronRunReport {
        job: job.name_any(),
        cron_id: job.labels().get(CRON_LABEL).cloned(),
        started_at: st
            .and_then(|s| s.start_time.as_ref())
            .map(time)
            .or_else(|| job.metadata.creation_timestamp.as_ref().map(time))
            .unwrap_or(0),
        finished_at: if succeeded.is_some() {
            finished_at
        } else {
            None
        },
        succeeded,
        exit_code: ended.map(|e| e.exit_code),
        reason,
        output: None,
    }
}

/// Every service's recent runs, with output for the ones that just finished.
pub async fn collect(
    client: &Client,
    jobs: &[Job],
    pods: &[Pod],
    now: i64,
) -> HashMap<String, Vec<CronRunReport>> {
    let mut out: HashMap<String, Vec<CronRunReport>> = HashMap::new();
    let mut seen = vec![];
    for job in jobs {
        let labels = job.labels();
        let Some(service) = labels
            .get(CRON_SERVICE_LABEL)
            .or_else(|| labels.get(SERVICE_LABEL))
            .cloned()
        else {
            continue;
        };
        let ns = job.namespace().unwrap_or_default();
        let name = job.name_any();
        let own: Vec<&Pod> = pods
            .iter()
            .filter(|p| {
                p.namespace().as_deref() == Some(ns.as_str())
                    && p.labels().get(JOB_LABEL) == Some(&name)
            })
            .collect();
        let mut run = report(job, &own);
        let uid = job.metadata.uid.clone().unwrap_or_default();
        seen.push(uid.clone());
        if run.finished_at.is_some_and(|t| now - t < RECENT_MS) {
            let cached = outputs().lock().unwrap().get(&uid).cloned();
            run.output = match cached {
                Some(o) => o,
                None => {
                    let o = read_output(client, &ns, &own).await;
                    outputs().lock().unwrap().insert(uid, o.clone());
                    o
                }
            };
        }
        out.entry(service).or_default().push(run);
    }
    outputs()
        .lock()
        .unwrap()
        .retain(|uid, _| seen.contains(uid));
    out
}

async fn read_output(client: &Client, ns: &str, pods: &[&Pod]) -> Option<String> {
    let pod = pods.iter().max_by_key(|p| {
        p.metadata
            .creation_timestamp
            .as_ref()
            .map(time)
            .unwrap_or(0)
    })?;
    let lp = LogParams {
        tail_lines: Some(OUTPUT_LINES),
        limit_bytes: Some(OUTPUT_BYTES),
        ..Default::default()
    };
    Api::<Pod>::namespaced(client.clone(), ns)
        .logs(&pod.name_any(), &lp)
        .await
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(status: serde_json::Value) -> Job {
        serde_json::from_value(json!({
            "metadata": { "name": "web-abc-1", "labels": { CRON_LABEL: "crn_abc", CRON_SERVICE_LABEL: "svc_web" }, "creationTimestamp": "2026-11-03T03:00:00Z" },
            "status": status,
        }))
        .unwrap()
    }

    fn pod(terminated: serde_json::Value) -> Pod {
        serde_json::from_value(json!({
            "metadata": { "name": "web-abc-1-x", "labels": { JOB_LABEL: "web-abc-1" } },
            "status": { "containerStatuses": [{ "name": "app", "image": "x", "imageID": "", "ready": false, "restartCount": 0, "state": { "terminated": terminated } }] },
        }))
        .unwrap()
    }

    #[test]
    fn a_success() {
        let j = job(
            json!({ "startTime": "2026-11-03T03:00:01Z", "completionTime": "2026-11-03T03:00:09Z", "succeeded": 1 }),
        );
        let p = pod(json!({ "exitCode": 0, "reason": "Completed" }));
        let r = report(&j, &[&p]);
        assert_eq!(r.succeeded, Some(true));
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.reason, None);
        assert_eq!(r.finished_at.unwrap() - r.started_at, 8000);
        assert_eq!(r.cron_id.as_deref(), Some("crn_abc"));
    }

    #[test]
    fn a_failure_keeps_its_exit_code_and_why() {
        let j = job(
            json!({ "startTime": "2026-11-03T03:00:01Z", "failed": 1, "conditions": [{ "type": "Failed", "status": "True", "reason": "BackoffLimitExceeded", "lastTransitionTime": "2026-11-03T03:00:05Z" }] }),
        );
        let p = pod(json!({ "exitCode": 137, "reason": "OOMKilled" }));
        let r = report(&j, &[&p]);
        assert_eq!(
            (r.succeeded, r.exit_code, r.reason.as_deref()),
            (Some(false), Some(137), Some("OOMKilled"))
        );
        assert!(r.finished_at.is_some());
    }

    #[test]
    fn a_run_in_progress_has_no_end() {
        let r = report(
            &job(json!({ "startTime": "2026-11-03T03:00:01Z", "active": 1 })),
            &[],
        );
        assert_eq!(
            (r.succeeded, r.finished_at, r.exit_code),
            (None, None, None)
        );
    }
}

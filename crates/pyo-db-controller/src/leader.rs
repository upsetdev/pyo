//! One active controller among replicas: whoever holds the Lease runs, the
//! others wait to take over when it stops renewing (its server died).

use std::time::Duration;

use k8s_openapi::{
    api::coordination::v1::{Lease, LeaseSpec},
    apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta},
    jiff::Timestamp,
};
use kube::{Api, Client, api::PostParams};

pub const NAME: &str = "pyo-db-controller";
/// A holder that hasn't renewed for this long is gone.
const TTL: i32 = 10;
const RENEW: Duration = Duration::from_secs(2);

pub struct Leader {
    api: Api<Lease>,
    me: String,
}

impl Leader {
    pub fn new(client: Client, ns: &str, me: String) -> Leader {
        Leader {
            api: Api::namespaced(client, ns),
            me,
        }
    }

    /// Waits until this replica holds the lease.
    pub async fn acquire(&self) {
        loop {
            match self.try_hold().await {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "lease"),
            }
            tokio::time::sleep(RENEW).await;
        }
    }

    /// Keeps renewing; returns when the lease is lost (another replica took
    /// it, or renewing failed for longer than the lease lasts).
    pub async fn hold(&self) {
        let mut last_ok = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(RENEW).await;
            match self.try_hold().await {
                Ok(true) => last_ok = tokio::time::Instant::now(),
                Ok(false) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "renewing the lease");
                    // Stop before another replica may take over.
                    if last_ok.elapsed() > Duration::from_secs((TTL - 2) as u64) {
                        return;
                    }
                }
            }
        }
    }

    /// Takes or renews the lease; Ok(false) while someone else holds it.
    async fn try_hold(&self) -> kube::Result<bool> {
        let now = MicroTime(Timestamp::now());
        let Some(mut lease) = self.api.get_opt(NAME).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(NAME.into()),
                    ..Default::default()
                },
                spec: Some(self.spec(now.clone(), now, 0)),
            };
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(s)) if s.code == 409 => Ok(false),
                Err(e) => Err(e),
            };
        };
        let spec = lease.spec.clone().unwrap_or_default();
        let holder = spec.holder_identity.as_deref().unwrap_or("");
        let expired = spec.renew_time.as_ref().is_none_or(|t| {
            let ttl = spec.lease_duration_seconds.unwrap_or(TTL) as i64;
            Timestamp::now().as_second() - t.0.as_second() > ttl
        });
        if holder != self.me && !expired && !holder.is_empty() {
            return Ok(false);
        }
        let (acquired, transitions) = if holder == self.me {
            (
                spec.acquire_time.clone().unwrap_or(now.clone()),
                spec.lease_transitions.unwrap_or(0),
            )
        } else {
            tracing::info!(from = holder, "taking over");
            (now.clone(), spec.lease_transitions.unwrap_or(0) + 1)
        };
        lease.spec = Some(self.spec(acquired, now, transitions));
        // The resourceVersion makes this fail if someone else wrote first.
        match self.api.replace(NAME, &PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(s)) if s.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn spec(&self, acquired: MicroTime, renewed: MicroTime, transitions: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some(self.me.clone()),
            lease_duration_seconds: Some(TTL),
            acquire_time: Some(acquired),
            renew_time: Some(renewed),
            lease_transitions: Some(transitions),
            ..Default::default()
        }
    }
}

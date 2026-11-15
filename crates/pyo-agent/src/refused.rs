//! Writes the cluster's install refuses: its admission policy or its roles
//! are older than Pyo's (right after an update of Pyo, until the cluster's
//! connect command runs). Trying such a write again changes nothing until
//! the install does, so the agent's loops wait for that (or a long
//! backoff) instead of trying, and logging, every few seconds.

use std::time::Duration;

use kube::{
    Api, Client, ResourceExt,
    api::DynamicObject,
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::rbac;

/// How often the install is looked at while waiting.
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// The first wait after a refusal; it doubles up to [`MAX_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(5 * 60);
const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// Whether Kubernetes refused the write for the agent's install: its
/// admission policy denied it, or its roles don't allow it.
pub fn is_refusal(e: &kube::Error) -> bool {
    match e {
        kube::Error::Api(s) => {
            s.message.contains("ValidatingAdmissionPolicy")
                || (s.code == 403 && !s.message.contains("because it is being terminated"))
        }
        _ => false,
    }
}

/// [`is_refusal`] anywhere in an error's chain.
pub fn in_chain(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<kube::Error>())
        .any(is_refusal)
}

/// How long to wait after the `refusals`th refusal in a row before trying
/// anyway (the install may have changed in a way its stamp doesn't show).
pub fn wait_after(refusals: u32) -> Duration {
    FIRST_WAIT
        .saturating_mul(1 << refusals.saturating_sub(1).min(8))
        .min(MAX_WAIT)
}

/// The install the cluster has now: the stamp on the agent's admission
/// policy (the connect command writes a new one). Empty when it can't be read.
pub async fn install_stamp(client: &Client) -> String {
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(
            "admissionregistration.k8s.io",
            "v1",
            "ValidatingAdmissionPolicy",
        ),
        "validatingadmissionpolicies",
    );
    Api::<DynamicObject>::all_with(client.clone(), &ar)
        .get_opt(rbac::ADMISSION_POLICY)
        .await
        .ok()
        .flatten()
        .and_then(|p| p.annotations().get(rbac::INSTALL_ANNOTATION).cloned())
        .unwrap_or_default()
}

/// Whether to try again: the install changed since the refusal, or the
/// wait is over.
pub fn try_again(refused_on: &str, now: &str, waited: Duration, wait: Duration) -> bool {
    refused_on != now || waited >= wait
}

/// After the `refusals`th refusal in a row: back once the cluster's
/// install changes, or after [`wait_after`].
pub async fn wait_for_install(client: &Client, refusals: u32) {
    let refused_on = install_stamp(client).await;
    let wait = wait_after(refusals);
    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        waited += LOOK_EVERY;
        if try_again(&refused_on, &install_stamp(client).await, waited, wait) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::Status;

    fn api(code: u16, reason: &str, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status::failure(message, reason).with_code(code)))
    }

    /// What the agent logged every 30 s on a v0.2 install after Pyo's update.
    const DENIED: &str = "envoyproxies.gateway.envoyproxy.io \"pyo\" is forbidden: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request: failed expression: gatewayAllowed";

    #[test]
    fn refusals_by_the_install_are_told_apart() {
        assert!(is_refusal(&api(403, "Forbidden", DENIED)));
        // The policy's refusal as a 422.
        assert!(is_refusal(&api(
            422,
            "Invalid",
            "ValidatingAdmissionPolicy 'pyo-agent' denied request"
        )));
        // Its roles.
        assert!(is_refusal(&api(
            403,
            "Forbidden",
            "services \"pyo-tailnet-web\" is forbidden: User \"system:serviceaccount:pyo-system:pyo-agent\" cannot patch resource \"services\""
        )));
        // Not the install's doing.
        assert!(!is_refusal(&api(
            404,
            "NotFound",
            "the server could not find the requested resource"
        )));
        assert!(!is_refusal(&api(
            403,
            "Forbidden",
            "unable to create new content in namespace pyo-waf because it is being terminated"
        )));
        assert!(!is_refusal(&api(500, "InternalError", "etcd timeout")));
        let e = anyhow::Error::from(api(403, "Forbidden", DENIED)).context("the gateway");
        assert!(in_chain(&e));
        assert!(!in_chain(&anyhow::anyhow!("no CRDs yet")));
    }

    #[test]
    fn refused_writes_back_off_up_to_an_hour() {
        assert_eq!(wait_after(1), Duration::from_secs(300));
        assert_eq!(wait_after(2), Duration::from_secs(600));
        assert_eq!(wait_after(4), Duration::from_secs(2400));
        assert_eq!(wait_after(5), MAX_WAIT);
        assert_eq!(wait_after(1000), MAX_WAIT);
        // Never more often than the old 30 s retry.
        assert!(wait_after(0) >= Duration::from_secs(300));
    }

    #[test]
    fn a_new_install_is_tried_at_once() {
        let wait = wait_after(1);
        assert!(!try_again("h1", "h1", Duration::from_secs(30), wait));
        // The connect command ran.
        assert!(try_again("h1", "h2", Duration::from_secs(30), wait));
        // Long enough.
        assert!(try_again("h1", "h1", wait, wait));
    }
}

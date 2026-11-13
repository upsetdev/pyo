//! Bringing this cluster's install up to Pyo's (pyo-proto's rbac.rs,
//! `RefreshInstall`), with the optional self-update role. Pyo sends it when
//! the agent's heartbeat reports an older install (after Pyo was updated).
//!
//! 1. Its roles must already be as Pyo's: the agent reads each one (it may
//!    never write them); if any differs, only the cluster's connect command
//!    can bring them up to date, and nothing is changed.
//! 2. The admission policy is replaced as a whole (its binding only if that
//!    differs), then a dry run of the Deployment change below goes through
//!    it. If the new policy refuses that, the old one is put back.
//! 3. Last, after reporting: the Deployment's image pins and its stamp.
//!    When the pins change, the agent restarts on them.
//!
//! Every step finds what's already done, so a refresh that stops halfway
//! is simply sent again. The agent's token, Pyo's address, its image and
//! everything else of its Deployment are never touched here.

use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, Patch, PatchParams, PostParams},
};
use pyo_proto::rbac::{self, InstallParts};
use serde_json::{Value, json};

use crate::scope::{api_for, same};

const DEPLOYMENT: &str = "pyo-agent";
/// What a refresh can't do by itself.
const RERUN: &str = "run the cluster's connect command to bring them up to date";

/// The install stamped on the agent's Deployment, if any.
pub async fn deployment_stamp(client: &Client) -> Option<String> {
    let d = Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .get_opt(DEPLOYMENT)
        .await
        .ok()
        .flatten()?;
    d.annotations().get(rbac::INSTALL_ANNOTATION).cloned()
}

/// What Pyo sent is a refresh of this install and nothing else: the
/// admission policy and its binding by name, still holding the agent (Deny,
/// failing closed, matching its account), the known pins, and a hash that
/// matches all of it.
pub fn check(p: &InstallParts) -> Result<(), String> {
    let [policy, binding] = p.admission.as_slice() else {
        return Err("expected the admission policy and its binding".into());
    };
    let named = |o: &Value, kind: &str| {
        o["kind"] == kind
            && o["metadata"]["name"] == rbac::ADMISSION_POLICY
            && o["metadata"]["namespace"].is_null()
    };
    if !named(policy, "ValidatingAdmissionPolicy")
        || !named(binding, "ValidatingAdmissionPolicyBinding")
    {
        return Err("expected the admission policy and its binding".into());
    }
    if policy["metadata"]["annotations"][rbac::INSTALL_ANNOTATION] != json!(p.hash) {
        return Err("the admission policy isn't stamped with this install".into());
    }
    let spec = &policy["spec"];
    let agent = json!([{ "name": "pyo-agent", "expression": format!("request.userInfo.username == '{}'", rbac::AGENT_USER) }]);
    if spec["failurePolicy"] != "Fail"
        || spec["matchConditions"] != agent
        || !spec["validations"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    {
        return Err("the admission policy wouldn't hold the agent".into());
    }
    if binding["spec"]
        != json!({ "policyName": rbac::ADMISSION_POLICY, "validationActions": ["Deny"] })
    {
        return Err("the admission policy's binding wouldn't deny".into());
    }
    let keys: Vec<&str> = p.pins.keys().map(String::as_str).collect();
    let mut want = rbac::PINNED_ENV.to_vec();
    want.sort_unstable();
    if keys != want {
        return Err("unexpected image pins".into());
    }
    if p.pins.values().any(|v| {
        v.is_empty()
            || v.chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    }) {
        return Err("an image pin isn't an image".into());
    }
    // The hash covers the policy as it was before stamping.
    let mut unstamped = p.admission.clone();
    if let Some(m) = unstamped[0]["metadata"].as_object_mut() {
        m.remove("annotations");
    }
    if rbac::install_hash(&p.access, &unstamped, &p.pins) != p.hash {
        return Err("its hash doesn't match its content".into());
    }
    Ok(())
}

/// The Deployment change: its pins (env by name, the rest kept) and stamp.
fn deployment_patch(p: &InstallParts) -> Value {
    let env: Vec<Value> = p
        .pins
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();
    json!({
        "metadata": { "annotations": { rbac::INSTALL_ANNOTATION: p.hash } },
        "spec": { "template": { "spec": { "containers": [{ "name": "agent", "env": env }] } } },
    })
}

/// Steps 1 and 2. Ok once the admission policy is this install's and lets
/// the Deployment change through; `finish` makes that change.
pub async fn refresh(client: &Client, p: &InstallParts) -> anyhow::Result<()> {
    check(p).map_err(|e| anyhow::anyhow!("Pyo sent an install this agent won't apply: {e}"))?;
    // 1. Its roles, as Pyo's are.
    for o in &p.access {
        let kind = o["kind"].as_str().unwrap_or("");
        // Nothing in these to compare: there by now.
        if matches!(kind, "Namespace" | "ServiceAccount") {
            continue;
        }
        let (api, name) = api_for(client, o);
        let have = api
            .get_opt(&name)
            .await
            .map_err(|e| anyhow::anyhow!("couldn't read its {kind} {name} ({e}); {RERUN}"))?;
        match have {
            None => anyhow::bail!(
                "this agent's roles are older than Pyo's ({kind} {name} is missing); {RERUN}"
            ),
            Some(h) if !same(o, &serde_json::to_value(&h)?) => anyhow::bail!(
                "this agent's roles are older than Pyo's ({kind} {name} differs); {RERUN}"
            ),
            Some(_) => {}
        }
    }
    // 2. The admission policy, then its binding.
    let mut replaced: Vec<(Api<DynamicObject>, DynamicObject)> = vec![];
    for o in &p.admission {
        let (api, name) = api_for(client, o);
        // Changed under it (another process of this agent, on its way out,
        // making the same refresh): read it again.
        for attempt in 1.. {
            let Some(old) = api.get_opt(&name).await? else {
                anyhow::bail!("this cluster has no admission policy for the agent; {RERUN}")
            };
            let stamp = |d: &DynamicObject| d.annotations().get(rbac::INSTALL_ANNOTATION).cloned();
            let old_value = serde_json::to_value(&old)?;
            if same(o, &old_value)
                && stamp(&old).as_deref()
                    == o["metadata"]["annotations"][rbac::INSTALL_ANNOTATION].as_str()
            {
                break;
            }
            let mut new: DynamicObject = serde_json::from_value(o.clone())?;
            new.metadata.resource_version = old.resource_version();
            // Other labels and annotations stay (kubectl's own, say).
            let mut labels = old.labels().clone();
            labels.extend(new.labels().clone());
            new.metadata.labels = Some(labels);
            let mut annotations = old.annotations().clone();
            annotations.extend(new.annotations().clone());
            new.metadata.annotations = Some(annotations);
            match api.replace(&name, &PostParams::default(), &new).await {
                Ok(_) => {
                    replaced.push((api, old));
                    break;
                }
                Err(kube::Error::Api(e)) if e.code == 409 && attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => anyhow::bail!(
                    "couldn't update {} {name}: {e}",
                    o["kind"].as_str().unwrap_or("")
                ),
            }
        }
    }
    if !replaced.is_empty() {
        // The API server takes a moment to enforce it.
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let deployments = Api::<Deployment>::namespaced(client.clone(), rbac::NS);
    let probe = deployments
        .patch(
            DEPLOYMENT,
            &PatchParams {
                dry_run: true,
                ..Default::default()
            },
            &Patch::Strategic(deployment_patch(p)),
        )
        .await;
    if let Err(e) = probe {
        let mut back = String::new();
        for (api, mut old) in replaced.into_iter().rev() {
            let name = old.name_any();
            old.metadata.managed_fields = None;
            old.metadata.resource_version = api
                .get(&name)
                .await
                .ok()
                .and_then(|cur| cur.resource_version());
            if let Err(e) = api.replace(&name, &PostParams::default(), &old).await {
                back = format!(" Putting back the old one failed too: {e}.");
            }
        }
        anyhow::bail!(
            "the new admission policy refused the agent's own update ({e}), so the old one stays.{back}"
        );
    }
    Ok(())
}

/// Step 3: the Deployment's pins and stamp. When the pins change, this
/// agent restarts.
pub async fn finish(client: &Client, p: &InstallParts) -> anyhow::Result<()> {
    Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .patch(
            DEPLOYMENT,
            &PatchParams::default(),
            &Patch::Strategic(deployment_patch(p)),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> InstallParts {
        let db = pyo_proto::system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        InstallParts::new(
            false,
            true,
            "127.0.0.1:5005/pyo/waf:dev",
            "127.0.0.1:5005/pyo/geoip:dev",
            &db,
        )
    }

    #[test]
    fn takes_pyos_install_as_rendered() {
        assert_eq!(check(&parts()), Ok(()));
        // As it travels: a command, through JSON.
        let cmd = pyo_proto::Command::RefreshInstall(Box::new(parts()));
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["kind"], "refresh_install");
        let Ok(pyo_proto::Command::RefreshInstall(back)) =
            serde_json::from_value::<pyo_proto::Command>(v)
        else {
            panic!()
        };
        assert_eq!(check(&back), Ok(()));
    }

    #[test]
    fn refuses_anything_else() {
        let tamper = |f: &dyn Fn(&mut InstallParts)| {
            let mut p = parts();
            f(&mut p);
            check(&p)
        };
        // Loosening the policy, by its binding or by what it matches.
        assert!(
            tamper(&|p| p.admission[1]["spec"]["validationActions"] = json!(["Warn"])).is_err()
        );
        assert!(tamper(&|p| p.admission[0]["spec"]["failurePolicy"] = json!("Ignore")).is_err());
        assert!(
            tamper(&|p| p.admission[0]["spec"]["matchConditions"] =
                json!([{ "name": "pyo-agent", "expression": "false" }]))
            .is_err()
        );
        assert!(tamper(&|p| p.admission[0]["spec"]["validations"] = json!([])).is_err());
        // Another object, or another name.
        assert!(tamper(&|p| p.admission[0]["metadata"]["name"] = json!("other")).is_err());
        assert!(tamper(&|p| p.admission.push(json!({ "kind": "ClusterRoleBinding" }))).is_err());
        // Pins: only the known ones, as images.
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_API_URL".into(), "http://evil".into());
            })
            .is_err()
        );
        assert!(
            tamper(&|p| {
                p.pins.remove("PYO_WAF_IMAGE");
            })
            .is_err()
        );
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_WAF_IMAGE".into(), "x\" y".into());
            })
            .is_err()
        );
        // Content that doesn't match its hash.
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_WAF_IMAGE".into(), "other/waf:1".into());
            })
            .is_err()
        );
        assert!(
            tamper(&|p| p.admission[0]["spec"]["validations"][0]["expression"] = json!("true"))
                .is_err()
        );
        assert!(tamper(&|p| p.hash = "0000000000000000".into()).is_err());
    }

    #[test]
    fn the_deployment_change_is_its_pins_and_stamp() {
        let p = parts();
        let patch = deployment_patch(&p);
        assert_eq!(
            patch["metadata"]["annotations"][rbac::INSTALL_ANNOTATION],
            json!(p.hash)
        );
        let c = &patch["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["name"], "agent");
        assert!(
            c.as_object()
                .unwrap()
                .keys()
                .all(|k| k == "name" || k == "env"),
            "no image, command or anything else"
        );
        assert_eq!(c["env"].as_array().unwrap().len(), rbac::PINNED_ENV.len());
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount: its admission policy holds what it runs to Pod
    /// Security's baseline, and it can't loosen a namespace's level.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored pod_security_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn pod_security_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest,
            core::v1::{Namespace, Pod, ServiceAccount},
        };
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let p = parts();
        let pp = PatchParams::apply("pyo-test").force();
        for o in p
            .access
            .iter()
            .chain(p.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let (api, name) = api_for(&admin, o);
            api.patch(&name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // The policy has to compile: no type-checking warnings.
        let vaps = api_for(&admin, &p.admission[0]).0;
        for _ in 0..30 {
            let v = vaps.get(rbac::ADMISSION_POLICY).await.unwrap();
            if v.data["status"]["observedGeneration"].as_i64().is_some() {
                assert!(
                    v.data["status"]["typeChecking"]["expressionWarnings"]
                        .as_array()
                        .is_none_or(|w| w.is_empty()),
                    "{}",
                    v.data["status"]
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();

        // A project's namespace: baseline, as rollout.rs makes it.
        let namespaces = Api::<Namespace>::all(sa.clone());
        let project = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-psa-test", "labels": {
            "app.kubernetes.io/managed-by": "pyo", "pyo/project-id": "prj_test", rbac::PSA_ENFORCE: "baseline" } } });
        let _ = Api::<Namespace>::all(admin.clone())
            .delete("pyo-psa-test", &Default::default())
            .await;
        while Api::<Namespace>::all(admin.clone())
            .get_opt("pyo-psa-test")
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::apply("pyo").force(),
                &Patch::Apply(&project),
            )
            .await
            .unwrap();
        crate::scope::ensure_managed(&sa, "pyo-psa-test")
            .await
            .unwrap();
        // Not loosened, by anyone's patch through the agent.
        let loosen = namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "privileged" } } }),
                ),
            )
            .await;
        assert!(
            loosen.is_err_and(|e| e.to_string().contains("Pod Security")),
            "loosened"
        );
        let drop = namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: null } } })),
            )
            .await;
        assert!(drop.is_err(), "label dropped");
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "restricted" } } }),
                ),
            )
            .await
            .expect("stricter is fine");
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "baseline" } } }),
                ),
            )
            .await
            .expect_err("and stays");
        // A project namespace without a level, or a databases' one: refused; as managed_namespace makes it: fine.
        let bare = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-psa-bare", "labels": { "pyo/project-id": "prj_x" } } });
        assert!(
            namespaces
                .patch(
                    "pyo-psa-bare",
                    &PatchParams::apply("pyo"),
                    &Patch::Apply(&bare)
                )
                .await
                .is_err()
        );
        let db_bare = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-db-psatest", "labels": { rbac::MANAGED_LABEL: "true" } } });
        assert!(
            namespaces
                .patch(
                    "pyo-db-psatest",
                    &PatchParams {
                        dry_run: true,
                        ..PatchParams::apply("pyo")
                    },
                    &Patch::Apply(&db_bare)
                )
                .await
                .is_err()
        );
        namespaces
            .patch(
                "pyo-db-psatest",
                &PatchParams {
                    dry_run: true,
                    ..PatchParams::apply("pyo")
                },
                &Patch::Apply(&rbac::managed_namespace("pyo-db-psatest")),
            )
            .await
            .expect("databases' namespace as Pyo makes it");

        // In pyo-system, which has no Pod Security level of its own: the
        // agent's policy alone.
        let pods = Api::<Pod>::namespaced(sa.clone(), rbac::NS);
        let dry = PostParams {
            dry_run: true,
            ..Default::default()
        };
        let try_pod = |name: &str, spec: Value, annotations: Value| {
            let pods = pods.clone();
            let dry = dry.clone();
            let pod: Pod = serde_json::from_value(
                json!({ "metadata": { "name": name, "annotations": annotations }, "spec": spec }),
            )
            .unwrap();
            async move { pods.create(&dry, &pod).await }
        };
        let c = |sc: Value| json!({ "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "securityContext": sc }] });
        let r = try_pod("fine", c(json!({ "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] }, "seccompProfile": { "type": "RuntimeDefault" } })), json!({})).await;
        assert!(r.is_ok(), "a plain pod: {r:?}");
        let refused = |r: Result<Pod, kube::Error>, what: &str| {
            assert!(
                r.as_ref()
                    .is_err_and(|e| e.to_string().contains("Pyo's agent")),
                "{what}: {r:?}"
            )
        };
        refused(
            try_pod(
                "caps",
                c(json!({ "capabilities": { "add": ["SYS_ADMIN"] } })),
                json!({}),
            )
            .await,
            "capabilities.add",
        );
        refused(
            try_pod(
                "caps-net",
                c(json!({ "capabilities": { "add": ["NET_BIND_SERVICE"] } })),
                json!({}),
            )
            .await,
            "any added capability",
        );
        refused(
            try_pod(
                "seccomp",
                c(json!({ "seccompProfile": { "type": "Unconfined" } })),
                json!({}),
            )
            .await,
            "seccomp",
        );
        refused(
            try_pod(
                "apparmor",
                c(json!({ "appArmorProfile": { "type": "Unconfined" } })),
                json!({}),
            )
            .await,
            "AppArmor",
        );
        refused(
            try_pod(
                "apparmor-old",
                c(json!({})),
                json!({ "container.apparmor.security.beta.kubernetes.io/x": "unconfined" }),
            )
            .await,
            "AppArmor annotation",
        );
        refused(try_pod("podseccomp", json!({ "securityContext": { "seccompProfile": { "type": "Unconfined" } }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await, "pod seccomp");
        refused(try_pod("sysctl", json!({ "securityContext": { "sysctls": [{ "name": "kernel.msgmax", "value": "1" }] }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await, "sysctl");
        try_pod("safe-sysctl", json!({ "securityContext": { "sysctls": [{ "name": "net.ipv4.tcp_syncookies", "value": "1" }] }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await.expect("a safe sysctl");
        refused(try_pod("hostport", json!({ "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "ports": [{ "containerPort": 80, "hostPort": 80 }] }] }), json!({})).await, "hostPort");
        // Pyo's own exceptions don't cover these either: a builder adding capabilities.
        let builder: Deployment = serde_json::from_value(json!({ "metadata": { "name": "pyo-builder-test" }, "spec": { "selector": { "matchLabels": { "a": "b" } }, "template": { "metadata": { "labels": { "a": "b" } },
            "spec": { "containers": [{ "name": "builder", "image": "registry.k8s.io/pause:3.10", "securityContext": { "capabilities": { "add": ["SYS_MODULE"] } } }] } } } })).unwrap();
        let r = Api::<Deployment>::namespaced(sa.clone(), rbac::NS)
            .create(&dry, &builder)
            .await;
        assert!(r.is_err_and(|e| e.to_string().contains("capabilities")));

        Api::<Namespace>::all(admin.clone())
            .delete("pyo-psa-test", &Default::default())
            .await
            .unwrap();
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount with the self-update role: an install with older
    /// image pins is refreshed to Pyo's; one whose roles differ isn't.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored refresh_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn refresh_against_a_cluster() {
        use k8s_openapi::api::{authentication::v1::TokenRequest, core::v1::ServiceAccount};
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let db = pyo_proto::system::db_images_from_env();
        let new = InstallParts::new(
            false,
            true,
            &pyo_proto::system::waf_image_from_env(),
            &pyo_proto::system::geoip_image_from_env(),
            &db,
        );
        let old = InstallParts::new(
            false,
            true,
            "127.0.0.1:5005/pyo/waf:old",
            "127.0.0.1:5005/pyo/geoip:old",
            &db,
        );
        assert_ne!(old.hash, new.hash);
        let pp = PatchParams::apply("pyo-test").force();
        let apply = |o: Value| {
            let admin = admin.clone();
            async move {
                let (api, name) = api_for(&admin, &o);
                api.patch(&name, &pp_of(), &Patch::Apply(&o))
                    .await
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        };
        fn pp_of() -> PatchParams {
            PatchParams::apply("pyo-test").force()
        }
        // What connect.sh would have installed: the old install, with the
        // self-update role bound, and a stand-in for the agent's Deployment.
        for o in old.access.iter().chain(old.admission.iter()) {
            apply(o.clone()).await;
        }
        let env: Vec<Value> = old
            .pins
            .iter()
            .map(|(k, v)| json!({ "name": k, "value": v }))
            .collect();
        let deployments = Api::<Deployment>::namespaced(admin.clone(), rbac::NS);
        let _ = deployments.delete(DEPLOYMENT, &Default::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let d = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": { "name": DEPLOYMENT, "namespace": rbac::NS, "annotations": { rbac::INSTALL_ANNOTATION: old.hash } },
            "spec": { "replicas": 0, "selector": { "matchLabels": { "app": "pyo-agent-test" } }, "template": { "metadata": { "labels": { "app": "pyo-agent-test" } }, "spec": {
                "serviceAccountName": "pyo-agent", "containers": [{ "name": "agent", "image": "registry.k8s.io/pause:3.10", "env": env }] } } },
        });
        deployments
            .patch(DEPLOYMENT, &pp, &Patch::Apply(&d))
            .await
            .unwrap();

        // From here on, only the agent's ServiceAccount.
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();
        let detect = || crate::scope::detect(&sa, Default::default());
        let before = detect().await;
        assert_eq!(
            (
                before.admission.as_str(),
                before.install.as_str(),
                before.self_update
            ),
            ("enforced", old.hash.as_str(), true)
        );

        // The self-update role reaches only the agent's own policy.
        let vaps = api_for(&sa, &new.admission[0]).0;
        assert!(
            vaps.get("moco-delete-validator").await.is_err()
                || vaps
                    .replace(
                        "moco-delete-validator",
                        &PostParams::default(),
                        &vaps.get("moco-delete-validator").await.unwrap()
                    )
                    .await
                    .is_err()
        );

        // A role that differs: nothing changes, and it says to run the connect command.
        let roles = api_for(
            &admin,
            &new.access
                .iter()
                .find(|o| o["metadata"]["name"] == rbac::STATS_ROLE)
                .unwrap()
                .clone(),
        )
        .0;
        roles.patch(rbac::STATS_ROLE, &pp, &Patch::Apply(json!({ "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": { "name": rbac::STATS_ROLE }, "rules": [] }))).await.unwrap();
        let err = refresh(&sa, &new).await.expect_err("its roles differ");
        assert!(format!("{err:#}").contains("connect command"), "{err:#}");
        assert_eq!(detect().await.install, old.hash, "nothing changed");
        apply(
            new.access
                .iter()
                .find(|o| o["metadata"]["name"] == rbac::STATS_ROLE)
                .unwrap()
                .clone(),
        )
        .await;

        // Its roles as Pyo's: the policy is refreshed, then the pins.
        refresh(&sa, &new).await.unwrap();
        assert_eq!(
            vaps.get(rbac::ADMISSION_POLICY)
                .await
                .unwrap()
                .annotations()[rbac::INSTALL_ANNOTATION],
            new.hash
        );
        assert_eq!(
            detect().await.install,
            "",
            "the Deployment isn't stamped yet"
        );
        finish(&sa, &new).await.unwrap();
        let after = detect().await;
        assert_eq!(
            (after.admission.as_str(), after.install.as_str()),
            ("enforced", new.hash.as_str())
        );
        let d = deployments.get(DEPLOYMENT).await.unwrap();
        let c = &d.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(
            c.image.as_deref(),
            Some("registry.k8s.io/pause:3.10"),
            "its image stays"
        );
        let env: std::collections::BTreeMap<String, String> = c
            .env
            .clone()
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.value.unwrap_or_default()))
            .collect();
        assert_eq!(env, new.pins);
        // Sent again: nothing to do.
        refresh(&sa, &new).await.unwrap();
        // Still enforced: a privileged pod is refused.
        let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(json!({ "metadata": { "name": "pyo-test-privileged" }, "spec": { "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "securityContext": { "privileged": true } }] } })).unwrap();
        let refused = Api::<k8s_openapi::api::core::v1::Pod>::namespaced(sa.clone(), rbac::NS)
            .create(
                &PostParams {
                    dry_run: true,
                    ..Default::default()
                },
                &pod,
            )
            .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("privileged")));

        // The cluster's owner deletes the binding: the agent says it can't
        // refresh any more (Pyo then shows the connect command), and can't.
        let binding = rbac::self_update_objects()
            .into_iter()
            .find(|o| o["kind"] == "ClusterRoleBinding")
            .unwrap();
        let (api, name) = api_for(&admin, &binding);
        api.delete(&name, &Default::default()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!detect().await.self_update);
        assert!(
            refresh(&sa, &old).await.is_err(),
            "without the binding it can't read its roles or write the policy"
        );

        // As it was: no stand-in, no self-update role.
        deployments
            .delete(DEPLOYMENT, &Default::default())
            .await
            .unwrap();
        for o in rbac::self_update_objects().iter().rev() {
            let (api, name) = api_for(&admin, o);
            let _ = api.delete(&name, &Default::default()).await;
        }
    }
}

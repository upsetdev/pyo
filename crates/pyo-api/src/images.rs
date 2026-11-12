//! The team's container images: every image its services have run, grouped by
//! repository, with the versions (digests) deployed and the services using
//! them. Images Pyo built live in its registry, which also gives their size.

use crate::error::ApiResult;
use crate::ops::{Role, require_role};
use crate::{AppState, auth::SessionUser, deploy};
use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ImagesQuery {
    /// The team's id.
    team: String,
}

/// One version of an image that was deployed.
#[derive(Serialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Version {
    /// What the deployment asked for (a tag, or a digest).
    image: String,
    /// The tag, commit or image reference shown for it.
    label: String,
    digest: Option<String>,
    /// In milliseconds since 1970.
    first_deployed: i64,
    last_deployed: i64,
    deployments: u32,
    /// The services that deployed it.
    service_ids: Vec<String>,
    /// A service runs it now.
    live: bool,
    /// Removed from Pyo's registry by retention.
    expired: bool,
    /// In bytes, for images Pyo built.
    size: Option<u64>,
}

/// An image repository a team deployed from.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Image {
    /// Repository without tag or digest, e.g. "docker.io/library/nginx".
    repository: String,
    /// Built by Pyo from Git, kept in its registry.
    built: bool,
    /// Pushed by the team to Pyo registry (`<team>/<project>/<service>`).
    pushed: bool,
    versions: Vec<Version>,
}

/// The team's images.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Images {
    images: Vec<Image>,
}

/// A deployment's image: reference, pinned id, revision, service, created at,
/// live, expired.
type DeploymentImage = (String, Option<String>, String, String, i64, bool, bool);

/// Images
///
/// Every image a team's services deployed, by repository, with each
/// version's deployments, the services that used it and whether one runs it
/// now.
#[utoipa::path(get, path = "/images", operation_id = "listImages", tag = "Images",
    params(ImagesQuery),
    responses(
        (status = 200, description = "The images.", body = Images),
        (status = 400, description = "No `team`.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<ImagesQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    let rows: Vec<DeploymentImage> = sqlx::query_as(
        "SELECT d.spec->>'image', nullif(coalesce(d.data->>'pinnedImage', d.data->>'imageId', ''), ''), coalesce(d.data->>'revisionLabel', ''), d.service_id,
                pyo.ms(d.created_at), coalesce(d.id = a.current_deployment_id, false), coalesce((d.data->>'imageExpired')::bool, false)
           FROM pyo.deployments d JOIN pyo.services a ON a.id = d.service_id
          WHERE d.team_id = $1 AND d.status = 'succeeded' AND coalesce(d.spec->>'image', '') <> ''
          ORDER BY d.created_at",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let ours = format!("{}/pyo/services/", deploy::registry().pull);
    let theirs = format!("{}/", deploy::registry().pull);
    let ours_root = format!("{}/pyo/", deploy::registry().pull);
    let team_repos: Vec<String> =
        sqlx::query_scalar("SELECT repo FROM pyo.registry_repos WHERE team_id = $1 ORDER BY repo")
            .bind(&q.team)
            .fetch_all(&s.db)
            .await?;
    let mut images: BTreeMap<String, Image> = BTreeMap::new();
    for (image, pinned, label, service_id, at, live, expired) in rows {
        let digest = pinned
            .as_deref()
            .or(Some(image.as_str()))
            .and_then(|r| r.split_once('@'))
            .map(|(_, d)| d.to_owned());
        let repository = repository(&image);
        let entry = images.entry(repository.clone()).or_insert_with(|| Image {
            built: image.starts_with(&ours),
            pushed: image.starts_with(&theirs) && !image.starts_with(&ours_root),
            repository,
            versions: vec![],
        });
        // One version per digest (or per reference, when there's none).
        let key = digest.clone().unwrap_or_else(|| image.clone());
        let v = match entry
            .versions
            .iter()
            .position(|v| v.digest.as_deref().unwrap_or(&v.image) == key)
        {
            Some(i) => &mut entry.versions[i],
            None => {
                entry.versions.push(Version {
                    image: image.clone(),
                    digest: digest.clone(),
                    first_deployed: at,
                    ..Default::default()
                });
                entry.versions.last_mut().expect("just pushed")
            }
        };
        v.label = if label.is_empty() { tag(&image) } else { label };
        v.last_deployed = at;
        v.deployments += 1;
        v.live |= live;
        // Expired only when every deployment of it is.
        v.expired = if v.deployments == 1 {
            expired
        } else {
            v.expired && expired
        };
        if !v.service_ids.contains(&service_id) {
            v.service_ids.push(service_id);
        }
    }

    // Sizes of the images Pyo built (still in its registry).
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(anyhow::Error::from)?;
    let push = deploy::registry().internal.clone();
    let pull = deploy::registry().pull.clone();
    // The team's pushed images, deployed or not: each tag a version.
    for (repo, tags) in pushed(&http, &push, team_repos).await {
        let repository = format!("{pull}/{repo}");
        let entry = images.entry(repository.clone()).or_insert_with(|| Image {
            built: false,
            pushed: true,
            repository: repository.clone(),
            versions: vec![],
        });
        for t in tags {
            let image = format!("{repository}:{t}");
            if !entry
                .versions
                .iter()
                .any(|v| v.image == image || v.label == t)
            {
                entry.versions.push(Version {
                    image,
                    label: t,
                    ..Default::default()
                });
            }
        }
    }
    let wanted: Vec<(String, String)> = images
        .values()
        .filter(|i| i.built)
        .flat_map(|i| {
            i.versions
                .iter()
                .filter(|v| !v.expired)
                .filter_map(|v| v.digest.clone().map(|d| (i.repository.clone(), d)))
        })
        .collect();
    let sizes = futures::future::join_all(wanted.iter().map(|(repo, digest)| {
        let path = repo
            .strip_prefix(&format!("{pull}/"))
            .unwrap_or(repo)
            .to_owned();
        size(&http, &push, path, digest.clone())
    }))
    .await;
    let sizes: BTreeMap<&str, u64> = wanted
        .iter()
        .zip(sizes)
        .filter_map(|((_, d), s)| s.map(|s| (d.as_str(), s)))
        .collect();
    for v in images.values_mut().flat_map(|i| i.versions.iter_mut()) {
        v.size = v.digest.as_deref().and_then(|d| sizes.get(d).copied());
        v.service_ids.sort();
    }
    let mut out: Vec<Image> = images.into_values().collect();
    for i in &mut out {
        i.versions
            .sort_by_key(|a| std::cmp::Reverse(a.last_deployed));
    }
    Ok(Json(
        serde_json::to_value(Images { images: out }).map_err(anyhow::Error::from)?,
    ))
}

/// "docker.io/library/nginx:1.27" → "docker.io/library/nginx".
fn repository(image: &str) -> String {
    let no_digest = image.split('@').next().unwrap_or(image);
    match no_digest.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo.to_owned(),
        _ => no_digest.to_owned(),
    }
}

/// The tag of a reference ("latest" when it names none), or a short digest.
fn tag(image: &str) -> String {
    if let Some((_, d)) = image.split_once('@') {
        return d.trim_start_matches("sha256:").chars().take(12).collect();
    }
    match image.rsplit_once(':') {
        Some((_, t)) if !t.contains('/') => t.to_owned(),
        _ => "latest".into(),
    }
}

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

/// The compressed size of an image in Pyo's registry: its config and
/// layers (of the first real platform, for a multi-platform index).
async fn size(http: &reqwest::Client, registry: &str, repo: String, digest: String) -> Option<u64> {
    Some(
        blobs(http, registry, &repo, &digest)
            .await?
            .iter()
            .map(|(_, n)| n)
            .sum(),
    )
}

/// The team's pushed repositories (`<team>/<project>/<service>`, as they
/// were named when first pushed) and their tags.
async fn pushed(
    http: &reqwest::Client,
    registry: &str,
    repos: Vec<String>,
) -> Vec<(String, Vec<String>)> {
    let tags = futures::future::join_all(repos.iter().map(|repo| async move {
        let v: Value = http
            .get(format!("http://{registry}/v2/{repo}/tags/list"))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(
            v["tags"]
                .as_array()?
                .iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        )
    }))
    .await;
    repos
        .into_iter()
        .zip(tags)
        .filter_map(|(r, t)| t.filter(|t| !t.is_empty()).map(|t| (r, t)))
        .collect()
}

/// The blobs (config and layers, with sizes) of an image in Pyo's registry,
/// of the first real platform for a multi-platform index.
async fn blobs(
    http: &reqwest::Client,
    registry: &str,
    repo: &str,
    reference: &str,
) -> Option<Vec<(String, u64)>> {
    let mut m = manifest(http, registry, repo, reference).await?;
    if let Some(list) = m["manifests"].as_array() {
        let first = list
            .iter()
            .find(|x| x["platform"]["os"] != json!("unknown"))?["digest"]
            .as_str()?
            .to_owned();
        m = manifest(http, registry, repo, &first).await?;
    }
    let mut out: Vec<(String, u64)> = m["layers"]
        .as_array()?
        .iter()
        .filter_map(|l| Some((l["digest"].as_str()?.to_owned(), l["size"].as_u64()?)))
        .collect();
    if let (Some(d), Some(n)) = (m["config"]["digest"].as_str(), m["config"]["size"].as_u64()) {
        out.push((d.to_owned(), n));
    }
    Some(out)
}

async fn manifest(
    http: &reqwest::Client,
    registry: &str,
    repo: &str,
    reference: &str,
) -> Option<Value> {
    http.get(format!("http://{registry}/v2/{repo}/manifests/{reference}"))
        .header("accept", ACCEPT)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RegistryQuery {
    /// The team's id.
    team: String,
}

/// Pyo registry
///
/// Pyo's registry, where the images it builds from Git and the team's own
/// pushes are kept, as one team sees it: its address, where it runs (when the team has that
/// cluster), whether Pyo reaches it, the space the team's images take, the
/// team's repositories (tags, size, last build) and its cleanup: how many
/// deployments' images are kept per service, and when images were last
/// removed (within the last week). Read only.
#[utoipa::path(get, path = "/registry", operation_id = "getRegistry", tag = "Images",
    params(RegistryQuery),
    responses(
        (status = 200, description = "`address` (in the cluster only, where servers pull from), `location` (`clusterId` and `server`, or null), `reachable`, `usedBytes`, `repositories`, `retainDeployments`, `lastCleanupAt`, `pushAddress`, `teamPath` and `tokens`. Each of `repositories` has `repository` (as servers pull it), `tailnetRepository`, `kind` (\"built\" or \"pushed\"), `serviceId`, `tags`, `bytes` and `lastPushAt`. `tailnetRepository` is a pushed repository's name on the team's tailnet, `<pushAddress>/<team>/<project>/<service>`. It's null for one Pyo built, or without a tailnet. `pushAddress` is the registry's address on the team's tailnet, where the team pushes and pulls its own images, or null without a tailnet. Each of `tokens` has `id`, `name`, `scope`, `createdBy`, `createdAt`, `expiresAt` and `lastUsedAt`, but never the token itself.", body = Object),
        (status = 400, description = "No `team`.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn registry(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<RegistryQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    let pull = deploy::registry().pull.clone();
    // Pyo's own calls go to the registry itself, in the cluster.
    let push = deploy::registry().internal.clone();
    let ours = format!("{pull}/pyo/services/");
    // The team's services, with their last build.
    let services: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT a.id, (SELECT pyo.ms(max(d.created_at)) FROM pyo.deployments d WHERE d.service_id = a.id AND d.status = 'succeeded' AND d.spec->>'image' LIKE $2 || '%')
           FROM pyo.services a WHERE a.team_id = $1",
    )
    .bind(&q.team)
    .bind(&ours)
    .fetch_all(&mut *tx)
    .await?;
    let pushed_repos: Vec<(String, i64)> = sqlx::query_as(
        "SELECT repo, pyo.ms(last_push_at) FROM pyo.registry_repos WHERE team_id = $1",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    let (retain, slug): (i64, String) = sqlx::query_as("SELECT coalesce((preferences->>'retainDeployments')::bigint, 20), slug FROM pyo.teams WHERE id = $1").bind(&q.team).fetch_one(&mut *tx).await?;
    let tokens: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'name', name, 'scope', scope, 'createdBy', created_by, 'createdAt', pyo.ms(created_at),
                                  'expiresAt', pyo.ms(expires_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.registry_tokens WHERE team_id = $1 ORDER BY created_at DESC",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    let repos: Vec<String> = services
        .iter()
        .map(|(id, _)| format!("pyo/services/{id}"))
        .collect();
    let last_cleanup: Option<i64> = sqlx::query_scalar("SELECT pyo.ms(max(finished_at)) FROM pyo.jobs WHERE kind = 'registry.delete' AND status = 'done' AND payload->>'repo' = ANY($1)")
        .bind(&repos)
        .fetch_one(&mut *tx)
        .await?;
    // Where it runs: Pyo's own cluster, when it's this team's.
    let location: Option<(String, Value)> = sqlx::query_as(
        "SELECT id, workloads FROM pyo.clusters WHERE team_id = $1 AND same_cluster LIMIT 1",
    )
    .bind(&q.team)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let location = location.map(|(cluster_id, workloads)| {
        let server = workloads
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["namespace"] == "pyo-system" && w["name"] == "registry")
            .and_then(|w| w["servers"][0].as_str().map(str::to_owned));
        json!({ "clusterId": cluster_id, "server": server })
    });

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(anyhow::Error::from)?;
    let reachable = http
        .get(format!("http://{push}/v2/"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    let mut repositories = vec![];
    let mut all: BTreeMap<String, u64> = BTreeMap::new();
    if reachable {
        let found = futures::future::join_all(services.iter().map(|(id, last)| {
            let (http, push) = (http.clone(), push.clone());
            async move {
                let repo = format!("pyo/services/{id}");
                let tags: Vec<String> = http
                    .get(format!("http://{push}/v2/{repo}/tags/list"))
                    .send()
                    .await
                    .ok()?
                    .json::<Value>()
                    .await
                    .ok()?["tags"]
                    .as_array()?
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect();
                let mut own: BTreeMap<String, u64> = BTreeMap::new();
                for b in
                    futures::future::join_all(tags.iter().map(|t| blobs(&http, &push, &repo, t)))
                        .await
                        .into_iter()
                        .flatten()
                        .flatten()
                {
                    own.insert(b.0, b.1);
                }
                Some((id.clone(), *last, tags.len(), own))
            }
        }))
        .await;
        for (id, last, tags, own) in found.into_iter().flatten() {
            if tags == 0 {
                continue;
            }
            repositories.push(json!({ "repository": format!("{ours}{id}"), "tailnetRepository": null, "kind": "built", "serviceId": id, "tags": tags, "bytes": own.values().sum::<u64>(), "lastPushAt": last }));
            all.extend(own);
        }
        // The team's own pushes.
        let last_push: BTreeMap<String, i64> = pushed_repos.iter().cloned().collect();
        for (repo, tags) in pushed(
            &http,
            &push,
            pushed_repos.into_iter().map(|(r, _)| r).collect(),
        )
        .await
        {
            let mut own: BTreeMap<String, u64> = BTreeMap::new();
            for b in futures::future::join_all(tags.iter().map(|t| blobs(&http, &push, &repo, t)))
                .await
                .into_iter()
                .flatten()
                .flatten()
            {
                own.insert(b.0, b.1);
            }
            repositories.push(json!({ "repository": format!("{pull}/{repo}"), "tailnetRepository": crate::registrydoor::as_named(&repo), "kind": "pushed", "serviceId": null, "tags": tags.len(), "bytes": own.values().sum::<u64>(), "lastPushAt": last_push.get(&repo) }));
            all.extend(own);
        }
    }
    repositories.sort_by(|a, b| b["lastPushAt"].as_i64().cmp(&a["lastPushAt"].as_i64()));
    Ok(Json(json!({
        // The in-cluster address servers pull from; users reach the
        // registry at `pushAddress` (over the tailnet) instead.
        "address": pull,
        "location": location,
        "reachable": reachable,
        "usedBytes": if reachable { Some(all.values().sum::<u64>()) } else { None },
        "repositories": repositories,
        "retainDeployments": retain,
        "lastCleanupAt": last_cleanup,
        // Pushing the team's own: where (over the tailnet; null without one),
        // under which path, and its tokens (never the tokens themselves).
        "pushAddress": crate::registrydoor::address(),
        "teamPath": crate::registrydoor::team_path(&slug),
        "tokens": tokens,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_references() {
        assert_eq!(
            repository("docker.io/library/nginx:1.27"),
            "docker.io/library/nginx"
        );
        assert_eq!(
            repository("127.0.0.1:5005/pyo/services/svc_x@sha256:ab"),
            "127.0.0.1:5005/pyo/services/svc_x"
        );
        assert_eq!(
            repository("127.0.0.1:5005/pyo/services/svc_x"),
            "127.0.0.1:5005/pyo/services/svc_x"
        );
        assert_eq!(tag("redis"), "latest");
        assert_eq!(tag("ghcr.io/a/b:v1"), "v1");
        assert_eq!(tag("x@sha256:0123456789abcdef"), "0123456789ab");
    }
}

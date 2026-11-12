//! External secret stores: a team connects Vault, OpenBao, Infisical, AWS
//! Secrets Manager, GCP Secret Manager, Azure Key Vault or Doppler, and a
//! service imports paths from it. Values are read when a deployment is
//! created and go the way of its other variables: sealed with the team's key,
//! into a Secret on the cluster. So a running service never depends on the
//! store, and a rollback doesn't need it.
//!
//! The address comes from a team admin and is called from Pyo's own network,
//! so it's checked before every use, redirects aren't followed, and a store's
//! responses are never shown: only their status.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use pyo_proto::EnvVar;
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

use crate::{
    error::{ApiError, ApiResult},
    outbound::{self, Reach, Refused},
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn unavailable(msg: impl Into<String>) -> ApiError {
    ApiError::new(axum::http::StatusCode::BAD_GATEWAY, msg)
}

/// A store as saved, with its credential opened.
pub struct Store {
    pub name: String,
    pub kind: String,
    pub addr: String,
    pub options: Value,
    pub credential: String,
}

fn parse_address(addr: &str) -> ApiResult<Url> {
    let url = Url::parse(addr.trim()).map_err(|_| {
        ApiError::bad_request("Enter the store's address, like https://vault.example.com.")
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ApiError::bad_request(
            "The address has to start with https://.",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError::bad_request("Don't put a login in the address."));
    }
    Ok(url)
}

/// Checks the address against what it resolves to now, not only what it
/// says ([`crate::outbound`]: a team's address, so a private network only
/// when the installation allows it, plain http only there). The client
/// checks it again as it connects.
pub async fn check_address(addr: &str) -> ApiResult<Url> {
    check_address_for(addr, Reach::team()).await
}

async fn check_address_for(addr: &str, reach: Reach) -> ApiResult<Url> {
    let url = parse_address(addr)?;
    outbound::check(&url, reach).await.map_err(|e| match e {
        Refused::Unknown => unavailable(e.to_string()),
        e => ApiError::bad_request(e.to_string()),
    })?;
    Ok(url)
}

fn client() -> ApiResult<reqwest::Client> {
    outbound::client(Reach::team(), TIMEOUT).map_err(|e| anyhow::Error::from(e).into())
}

fn endpoint(base: &Url, segments: &[&str]) -> ApiResult<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| ApiError::bad_request("That address can't be used."))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// A path's parts, refusing anything that could step outside it.
fn path_segments(path: &str) -> ApiResult<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Err(ApiError::bad_request("That path isn't valid."));
    }
    Ok(parts)
}

fn describe(store: &str, status: StatusCode, path: &str) -> ApiError {
    let what = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            format!("{store} refused access to {path}. Check its credential and permissions.")
        }
        StatusCode::NOT_FOUND => format!("{store} has nothing at {path}."),
        s => format!("{store} answered {path} with an error ({}).", s.as_u16()),
    };
    unavailable(what)
}

/// `(status, body)`, without ever passing the body on in an error.
async fn fetch(store: &str, req: reqwest::RequestBuilder) -> ApiResult<(StatusCode, Value)> {
    let res = req
        .send()
        .await
        .map_err(|_| unavailable(format!("Pyo can't reach {store}.")))?;
    let status = res.status();
    if status.is_redirection() {
        return Err(unavailable(format!(
            "{store} answered with a redirect, which Pyo doesn't follow. Use its final address."
        )));
    }
    Ok((status, res.json().await.unwrap_or(Value::Null)))
}

async fn infisical_token(s: &Store, base: &Url) -> ApiResult<String> {
    let login: Value = serde_json::from_str(&s.credential).unwrap_or(Value::Null);
    let url = endpoint(base, &["api", "v1", "auth", "universal-auth", "login"])?;
    let (status, body) = fetch(
        &s.name,
        client()?
            .post(url)
            .json(&json!({ "clientId": login["clientId"], "clientSecret": login["clientSecret"] })),
    )
    .await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, "its login"));
    }
    body["accessToken"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| unavailable(format!("{} didn't return a token.", s.name)))
}

async fn infisical_read(s: &Store, base: &Url, path: &str) -> ApiResult<Vec<(String, String)>> {
    let token = infisical_token(s, base).await?;
    let mut url = endpoint(base, &["api", "v3", "secrets", "raw"])?;
    let folder = format!("/{}", path_segments(path)?.join("/"));
    url.query_pairs_mut()
        .append_pair("workspaceId", s.options["projectId"].as_str().unwrap_or(""))
        .append_pair(
            "environment",
            s.options["environment"].as_str().unwrap_or(""),
        )
        .append_pair("secretPath", &folder);
    let (status, body) = fetch(&s.name, client()?.get(url).bearer_auth(token)).await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, path));
    }
    let secrets = body["secrets"]
        .as_array()
        .ok_or_else(|| unavailable(format!("{} sent something Pyo can't read.", s.name)))?;
    Ok(secrets
        .iter()
        .filter_map(|x| {
            Some((
                x["secretKey"].as_str()?.to_owned(),
                x["secretValue"].as_str()?.to_owned(),
            ))
        })
        .collect())
}

fn vault_request(s: &Store, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let req = req.header("X-Vault-Token", &s.credential);
    match s.options["namespace"].as_str().filter(|n| !n.is_empty()) {
        Some(ns) => req.header("X-Vault-Namespace", ns),
        None => req,
    }
}

async fn vault_read(s: &Store, base: &Url, path: &str) -> ApiResult<Vec<(String, String)>> {
    let mount = s.options["mount"]
        .as_str()
        .filter(|m| !m.is_empty())
        .unwrap_or("secret");
    let mut segments = vec!["v1"];
    segments.extend(path_segments(mount)?);
    segments.push("data");
    let parts = path_segments(path)?;
    if parts.is_empty() {
        return Err(ApiError::bad_request("Enter a path, like app/production."));
    }
    segments.extend(parts);
    let (status, body) = fetch(
        &s.name,
        vault_request(s, client()?.get(endpoint(base, &segments)?)),
    )
    .await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, path));
    }
    let data = body["data"]["data"]
        .as_object()
        .ok_or_else(|| unavailable(format!("{} has no KV v2 secret at {path}.", s.name)))?;
    Ok(data
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .collect())
}

/// `a=1&b=2`, for the OAuth token endpoints.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://form.invalid/").expect("a valid URL");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or("").to_owned()
}

fn credential_json(s: &Store) -> ApiResult<Value> {
    serde_json::from_str(&s.credential)
        .map_err(|_| ApiError::bad_request(format!("{}'s credential isn't valid JSON.", s.name)))
}

/// What a secret holds: a JSON object is one variable per field, anything else
/// is one variable named after the secret.
fn expand(name: &str, raw: &str) -> Vec<(String, String)> {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) {
        return map
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string()),
                )
            })
            .collect();
    }
    let last = name.trim_matches('/').rsplit('/').next().unwrap_or(name);
    vec![(
        last.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect(),
        raw.to_owned(),
    )]
}

// ── AWS ──────────────────────────────────────────────────────────────────

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

struct SigV4<'a> {
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
    service: &'a str,
    /// 20150830T123600Z
    time: &'a str,
}

impl SigV4<'_> {
    /// The Authorization header. `headers` are lowercase and sorted by name.
    fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> String {
        let canonical: String = headers
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect();
        let signed = headers
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(";");
        let request = format!(
            "{method}\n{path}\n{query}\n{canonical}\n{signed}\n{}",
            hex::encode(Sha256::digest(body))
        );
        let date = &self.time[..8];
        let scope = format!("{date}/{}/{}/aws4_request", self.region, self.service);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
            self.time,
            hex::encode(Sha256::digest(request))
        );
        let key = [self.region, self.service, "aws4_request"].iter().fold(
            hmac(format!("AWS4{}", self.secret_key).as_bytes(), date),
            |k, part| hmac(&k, part),
        );
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={}",
            self.access_key,
            hex::encode(hmac(&key, &to_sign))
        )
    }
}

fn aws_time(at: cookie::time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// A signed POST. `target` is the JSON API's action; without one it's a form.
async fn aws_post(
    s: &Store,
    url: Url,
    service: &str,
    target: Option<&str>,
    body: String,
) -> ApiResult<(StatusCode, Value)> {
    let login = credential_json(s)?;
    let access_key = login["accessKeyId"].as_str().unwrap_or("");
    let secret_key = login["secretAccessKey"].as_str().unwrap_or("");
    let token = login["sessionToken"].as_str().filter(|t| !t.is_empty());
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").to_owned(),
    };
    let time = aws_time(cookie::time::OffsetDateTime::now_utc());
    let content_type = if target.is_some() {
        "application/x-amz-json-1.1"
    } else {
        "application/x-www-form-urlencoded; charset=utf-8"
    };
    let mut headers = vec![
        ("content-type", content_type),
        ("host", host.as_str()),
        ("x-amz-date", time.as_str()),
    ];
    if let Some(t) = token {
        headers.push(("x-amz-security-token", t));
    }
    if let Some(t) = target {
        headers.push(("x-amz-target", t));
    }
    let sig = SigV4 {
        access_key,
        secret_key,
        region: s.options["region"].as_str().unwrap_or(""),
        service,
        time: &time,
    };
    let auth = sig.authorization("POST", "/", "", &headers, &body);
    let mut req = client()?.post(url).header("authorization", auth).body(body);
    for (k, v) in headers.iter().filter(|(k, _)| *k != "host") {
        req = req.header(*k, *v);
    }
    fetch(&s.name, req).await
}

async fn aws_read(s: &Store, base: &Url, path: &str) -> ApiResult<Vec<(String, String)>> {
    let id = path.trim();
    if id.is_empty() {
        return Err(ApiError::bad_request(
            "Enter the secret's name, like app/production.",
        ));
    }
    let (status, body) = aws_post(
        s,
        base.clone(),
        "secretsmanager",
        Some("secretsmanager.GetSecretValue"),
        json!({ "SecretId": id }).to_string(),
    )
    .await?;
    if !status.is_success() {
        let kind = body["__type"].as_str().unwrap_or("");
        return Err(describe(
            &s.name,
            if kind.contains("ResourceNotFound") {
                StatusCode::NOT_FOUND
            } else if kind.contains("AccessDenied") {
                StatusCode::FORBIDDEN
            } else {
                status
            },
            id,
        ));
    }
    let raw = body["SecretString"].as_str().ok_or_else(|| {
        unavailable(format!(
            "{id} is a binary secret. Pyo can only import text."
        ))
    })?;
    Ok(expand(id, raw))
}

/// Who these credentials are: STS answers for any valid key, whatever it may read.
async fn aws_check(s: &Store, base: &Url) -> ApiResult<()> {
    let region = s.options["region"].as_str().unwrap_or("");
    let custom = !base.host_str().unwrap_or("").ends_with(".amazonaws.com");
    let url = if custom {
        base.clone()
    } else {
        Url::parse(&format!("https://sts.{region}.amazonaws.com/"))
            .map_err(|_| ApiError::bad_request("That region isn't valid."))?
    };
    let (status, _) = aws_post(
        s,
        url,
        "sts",
        None,
        "Action=GetCallerIdentity&Version=2011-06-15".into(),
    )
    .await?;
    if status.is_success() {
        Ok(())
    } else {
        Err(describe(&s.name, status, "its access key"))
    }
}

// ── GCP ──────────────────────────────────────────────────────────────────

const GCP_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const GCP_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

async fn gcp_token(s: &Store) -> ApiResult<String> {
    let key = credential_json(s)?;
    let (Some(email), Some(pem)) = (key["client_email"].as_str(), key["private_key"].as_str())
    else {
        return Err(ApiError::bad_request(
            "Paste the service account's whole JSON key.",
        ));
    };
    let token_uri = key["token_uri"].as_str().unwrap_or(GCP_TOKEN_URI);
    let url = check_address(token_uri).await?;
    let now = pyo_proto::now_ms() / 1000;
    let claims = json!({ "iss": email, "scope": GCP_SCOPE, "aud": token_uri, "iat": now, "exp": now + 3600 });
    let signing = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|_| ApiError::bad_request("That JSON key's private key isn't valid."))?;
    let assertion = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &signing,
    )
    .map_err(anyhow::Error::from)?;
    let body = form(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", &assertion),
    ]);
    let (status, reply) = fetch(
        &s.name,
        client()?
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body),
    )
    .await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, "its service account"));
    }
    reply["access_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| unavailable(format!("{} didn't return a token.", s.name)))
}

fn gcp_project(s: &Store) -> ApiResult<String> {
    let key = credential_json(s)?;
    s.options["project"]
        .as_str()
        .filter(|p| !p.is_empty())
        .or(key["project_id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::bad_request("Enter the project."))
}

async fn gcp_read(s: &Store, base: &Url, path: &str) -> ApiResult<Vec<(String, String)>> {
    let parts = path_segments(path)?;
    let [name] = parts.as_slice() else {
        return Err(ApiError::bad_request(
            "Enter the secret's name, like app-production.",
        ));
    };
    let token = gcp_token(s).await?;
    let project = gcp_project(s)?;
    let url = endpoint(
        base,
        &[
            "v1",
            "projects",
            &project,
            "secrets",
            name,
            "versions",
            "latest:access",
        ],
    )?;
    let (status, body) = fetch(&s.name, client()?.get(url).bearer_auth(token)).await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, path));
    }
    let data = body["payload"]["data"]
        .as_str()
        .and_then(|d| STANDARD.decode(d).ok())
        .and_then(|d| String::from_utf8(d).ok());
    let raw =
        data.ok_or_else(|| unavailable(format!("{name} isn't text. Pyo can only import text.")))?;
    Ok(expand(name, &raw))
}

// ── Azure ────────────────────────────────────────────────────────────────

const AZURE_LOGIN: &str = "https://login.microsoftonline.com";
const AZURE_SCOPE: &str = "https://vault.azure.net/.default";

async fn azure_token(s: &Store) -> ApiResult<String> {
    let login = credential_json(s)?;
    let (Some(tenant), Some(id), Some(secret)) = (
        login["tenantId"].as_str(),
        login["clientId"].as_str(),
        login["clientSecret"].as_str(),
    ) else {
        return Err(ApiError::bad_request(
            "Enter the tenant ID, client ID and client secret.",
        ));
    };
    // Sovereign clouds log in elsewhere.
    let base = check_address(
        s.options["loginUrl"]
            .as_str()
            .filter(|u| !u.is_empty())
            .unwrap_or(AZURE_LOGIN),
    )
    .await?;
    let url = endpoint(&base, &[tenant, "oauth2", "v2.0", "token"])?;
    let body = form(&[
        ("grant_type", "client_credentials"),
        ("client_id", id),
        ("client_secret", secret),
        ("scope", AZURE_SCOPE),
    ]);
    let (status, reply) = fetch(
        &s.name,
        client()?
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body),
    )
    .await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, "its login"));
    }
    reply["access_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| unavailable(format!("{} didn't return a token.", s.name)))
}

async fn azure_read(s: &Store, base: &Url, path: &str) -> ApiResult<Vec<(String, String)>> {
    let parts = path_segments(path)?;
    let [name] = parts.as_slice() else {
        return Err(ApiError::bad_request(
            "Enter the secret's name, like app-production.",
        ));
    };
    let token = azure_token(s).await?;
    let mut url = endpoint(base, &["secrets", name])?;
    url.query_pairs_mut().append_pair("api-version", "7.4");
    let (status, body) = fetch(&s.name, client()?.get(url).bearer_auth(token)).await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, path));
    }
    let raw = body["value"]
        .as_str()
        .ok_or_else(|| unavailable(format!("{} sent something Pyo can't read.", s.name)))?;
    Ok(expand(name, raw))
}

// ── Doppler ──────────────────────────────────────────────────────────────

/// What Doppler adds to every config to describe itself.
const DOPPLER_OWN: [&str; 3] = ["DOPPLER_PROJECT", "DOPPLER_CONFIG", "DOPPLER_ENVIRONMENT"];

/// A service token belongs to one config, so there's no path to choose.
async fn doppler_read(s: &Store, base: &Url) -> ApiResult<Vec<(String, String)>> {
    let mut url = endpoint(base, &["v3", "configs", "config", "secrets", "download"])?;
    url.query_pairs_mut().append_pair("format", "json");
    for option in ["project", "config"] {
        if let Some(v) = s.options[option].as_str().filter(|v| !v.is_empty()) {
            url.query_pairs_mut().append_pair(option, v);
        }
    }
    let (status, body) = fetch(&s.name, client()?.get(url).bearer_auth(&s.credential)).await?;
    if !status.is_success() {
        return Err(describe(&s.name, status, "its config"));
    }
    let map = body
        .as_object()
        .ok_or_else(|| unavailable(format!("{} sent something Pyo can't read.", s.name)))?;
    Ok(map
        .iter()
        .filter(|(k, _)| !DOPPLER_OWN.contains(&k.as_str()))
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string()),
            )
        })
        .collect())
}

/// Where a provider with one public address lives. Others need one entered.
pub fn default_address(kind: &str, options: &Value) -> ApiResult<Option<String>> {
    Ok(match kind {
        "aws" => {
            let region = options["region"].as_str().unwrap_or("");
            if region.is_empty()
                || region.len() > 32
                || !region
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(ApiError::bad_request(
                    "Enter the AWS region, like us-east-1.",
                ));
            }
            Some(format!("https://secretsmanager.{region}.amazonaws.com"))
        }
        "gcp" => Some("https://secretmanager.googleapis.com".into()),
        "doppler" => Some("https://api.doppler.com".into()),
        _ => None,
    })
}

/// The key-value pairs at a path.
pub async fn read(s: &Store, path: &str) -> ApiResult<Vec<(String, String)>> {
    let base = check_address(&s.addr).await?;
    match s.kind.as_str() {
        "infisical" => infisical_read(s, &base, path).await,
        "aws" => aws_read(s, &base, path).await,
        "gcp" => gcp_read(s, &base, path).await,
        "azure" => azure_read(s, &base, path).await,
        "doppler" => doppler_read(s, &base).await,
        _ => vault_read(s, &base, path).await,
    }
}

/// Whether the store accepts this credential.
pub async fn check_connection(s: &Store) -> ApiResult<()> {
    let base = check_address(&s.addr).await?;
    match s.kind.as_str() {
        "infisical" => return infisical_token(s, &base).await.map(|_| ()),
        "aws" => return aws_check(s, &base).await,
        "gcp" => return gcp_token(s).await.and(gcp_project(s)).map(|_| ()),
        "azure" => return azure_token(s).await.map(|_| ()),
        "doppler" => return doppler_read(s, &base).await.map(|_| ()),
        _ => {}
    }
    let url = endpoint(&base, &["v1", "auth", "token", "lookup-self"])?;
    let (status, _) = fetch(&s.name, vault_request(s, client()?.get(url))).await?;
    if status.is_success() {
        Ok(())
    } else {
        Err(describe(&s.name, status, "its token"))
    }
}

/// Load a team's store. Scoped to the team, so one can't use another's.
pub async fn load(tx: &mut Transaction<'_, Postgres>, team_id: &str, id: &str) -> ApiResult<Store> {
    let row: Option<(String, String, String, Value, String)> =
        sqlx::query_as("SELECT name, kind, addr, options, credential FROM pyo.secret_stores WHERE id = $1 AND team_id = $2").bind(id).bind(team_id).fetch_optional(&mut **tx).await?;
    let (name, kind, addr, options, sealed) =
        row.ok_or_else(|| ApiError::not_found("Secret store"))?;
    let credential = crate::vault::get().open(&sealed).await?;
    Ok(Store {
        name,
        kind,
        addr,
        options,
        credential,
    })
}

pub fn env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Add what a service imports to its variables. Its own variables win, then
/// imports in order. If a store can't answer, the deploy fails: starting
/// without secrets would be worse.
pub async fn import_env(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    spec: &Value,
    env: &mut Vec<EnvVar>,
) -> ApiResult<usize> {
    let mut added = 0;
    for import in spec["secretImports"].as_array().into_iter().flatten() {
        let store = load(tx, team_id, import["storeId"].as_str().unwrap_or("")).await?;
        for (key, value) in read(&store, import["path"].as_str().unwrap_or("")).await? {
            if env_key(&key) && !env.iter().any(|e| e.key == key) {
                env.push(EnvVar { key, value });
                added += 1;
            }
        }
    }
    Ok(added)
}

/// A service can only import from its own team's stores.
pub async fn imports_ok(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    for import in spec["secretImports"].as_array().into_iter().flatten() {
        let id = import["storeId"].as_str().unwrap_or("");
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.secret_stores WHERE id = $1 AND team_id = $2)",
        )
        .bind(id)
        .bind(team_id)
        .fetch_one(&mut **tx)
        .await?;
        if !found {
            return Err(ApiError::not_found("Secret store"));
        }
        let path = import["path"].as_str().unwrap_or("");
        if path.len() > 512 {
            return Err(ApiError::bad_request("That path is too long."));
        }
        path_segments(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never this machine, metadata or the cluster's network; a private
    /// network only when the installation allows it.
    #[tokio::test]
    async fn stores_are_checked_where_they_point() {
        let (lan, cloud) = (Reach { private: true }, Reach { private: false });
        for a in [
            "https://127.0.0.1:8200",
            "https://169.254.169.254",
            "http://10.43.0.10:5432",
            "https://[::1]:8200",
            "https://localhost:8200",
        ] {
            assert!(check_address_for(a, lan).await.is_err(), "{a}");
        }
        assert!(
            check_address_for("http://192.168.1.2:8200", lan)
                .await
                .is_ok(),
            "private, allowed"
        );
        assert!(
            check_address_for("http://192.168.1.2:8200", cloud)
                .await
                .is_err(),
            "private, not allowed"
        );
        assert!(
            check_address_for("http://1.1.1.1", lan).await.is_err(),
            "plain http to the internet"
        );
        assert!(check_address_for("https://1.1.1.1", cloud).await.is_ok());
    }

    #[test]
    fn addresses_are_parsed_strictly() {
        assert!(parse_address("https://vault.example.com").is_ok());
        for a in [
            "vault.example.com",
            "ftp://vault.example.com",
            "https://user:pw@vault.example.com",
            "",
        ] {
            assert!(parse_address(a).is_err(), "{a}");
        }
    }

    #[test]
    fn paths_cant_step_outside() {
        assert_eq!(
            path_segments("/app/production/").unwrap(),
            ["app", "production"]
        );
        assert!(path_segments("app/../../sys/seal").is_err());
        assert!(path_segments("./app").is_err());
    }

    #[test]
    fn urls_escape_each_segment() {
        let base = Url::parse("https://vault.example.com/").unwrap();
        assert_eq!(
            endpoint(&base, &["v1", "secret", "data", "a b?c"])
                .unwrap()
                .as_str(),
            "https://vault.example.com/v1/secret/data/a%20b%3Fc"
        );
    }

    /// AWS's own worked example of Signature Version 4.
    #[test]
    fn sigv4_matches_aws_published_example() {
        let sig = SigV4 {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "iam",
            time: "20150830T123600Z",
        };
        let headers = [
            (
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            ),
            ("host", "iam.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        assert_eq!(
            sig.authorization(
                "GET",
                "/",
                "Action=ListUsers&Version=2010-05-08",
                &headers,
                ""
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn a_json_secret_is_one_variable_per_field() {
        let mut got = expand("app/production", r#"{"A":"1","PORT":5432}"#);
        got.sort();
        assert_eq!(
            got,
            [
                ("A".to_owned(), "1".to_owned()),
                ("PORT".to_owned(), "5432".to_owned())
            ]
        );
    }

    #[test]
    fn anything_else_is_named_after_the_secret() {
        assert_eq!(
            expand("prod/stripe-key", "sk_live_1"),
            [("STRIPE_KEY".to_owned(), "sk_live_1".to_owned())]
        );
        assert_eq!(
            expand("token", "[1,2]"),
            [("TOKEN".to_owned(), "[1,2]".to_owned())]
        );
    }

    #[test]
    fn providers_with_one_address_dont_need_one_entered() {
        assert_eq!(
            default_address("aws", &json!({ "region": "eu-west-1" }))
                .unwrap()
                .unwrap(),
            "https://secretsmanager.eu-west-1.amazonaws.com"
        );
        assert!(default_address("aws", &json!({ "region": "x.evil.com/" })).is_err());
        assert!(default_address("vault", &json!({})).unwrap().is_none());
    }

    #[test]
    fn only_valid_names_become_variables() {
        assert!(env_key("DATABASE_URL") && env_key("_x1"));
        assert!(!env_key("1BAD") && !env_key("has-dash") && !env_key(""));
    }
}

//! The agent's side of the traffic store: it brings the store Pyo's context,
//! sends the store's finished totals on to Pyo as its own signed requests,
//! and answers Pyo's queries for kept rows (QueryTraffic) from the store.

use std::{sync::OnceLock, time::Duration};

use pyo_proto::traffic::{TotalsBatch, TrafficAnswer, TrafficQuery};
use serde_json::{Value, json};

use crate::Agent;

/// The store's token, from this agent's own (`token_for`).
pub fn token() -> &'static str {
    static T: OnceLock<String> = OnceLock::new();
    T.get_or_init(|| super::token_for(&std::env::var("PYO_AGENT_TOKEN").unwrap_or_default()))
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

async fn call(method: reqwest::Method, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> anyhow::Result<Value> {
    let mut req = http()
        .request(method, format!("{}{path}", super::url()))
        .bearer_auth(token())
        .timeout(timeout);
    if let Some(b) = body {
        req = req.header("content-type", "application/json").body(b);
    }
    let res = req.send().await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "the traffic store answered {status}");
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Pyo's query for kept rows, answered by the store.
pub async fn query(q: &TrafficQuery) -> anyhow::Result<TrafficAnswer> {
    let v = call(reqwest::Method::POST, "/query", Some(serde_json::to_vec(q)?), Duration::from_secs(20))
        .await
        .map_err(|e| anyhow::anyhow!("couldn't read the kept requests: {e:#}"))?;
    Ok(serde_json::from_value(v)?)
}

/// Every 15 seconds: the store's finished minutes to Pyo; every minute (and
/// at the start), Pyo's context to the store. Quiet while there's no store
/// (observability off, or it's starting).
pub async fn run(agent: Agent) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: u64 = 0;
    let mut warned = false;
    loop {
        tick.tick().await;
        // No store (observability is off, or it's starting): nothing to do.
        if call(reqwest::Method::GET, "/status", None, Duration::from_secs(5)).await.is_err() {
            continue;
        }
        if n.is_multiple_of(4)
            && let Err(e) = push_context(&agent).await
        {
            tracing::debug!(error = %format!("{e:#}"), "traffic store's context not updated");
        }
        n += 1;
        match send_totals(&agent).await {
            Ok(_) => warned = false,
            Err(e) if !warned => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't send traffic totals; retrying");
                warned = true;
            }
            Err(_) => {}
        }
    }
}

async fn push_context(agent: &Agent) -> anyhow::Result<()> {
    let mut ctx = agent.api.traffic_context().await?;
    // Room for what the team keeps: the store's volume grows, or the store
    // keeps less on one that can't.
    if ctx.max_mb > 0 {
        match crate::observability::fit_traffic_volume(&agent.kube, ctx.max_mb).await {
            Ok(mb) => ctx.max_mb = mb,
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "kept requests' volume not checked"),
        }
    }
    call(reqwest::Method::PUT, "/context", Some(serde_json::to_vec(&ctx)?), Duration::from_secs(10)).await?;
    Ok(())
}

/// Sends what the store has finished, oldest first, acknowledging each
/// batch Pyo took. Returns how many went.
async fn send_totals(agent: &Agent) -> anyhow::Result<usize> {
    let v = call(reqwest::Method::GET, "/totals", None, Duration::from_secs(10)).await?;
    let mut sent = 0;
    for b in v["batches"].as_array().into_iter().flatten() {
        let seq = b["seq"].as_u64().unwrap_or(0);
        let batch: TotalsBatch = serde_json::from_value(b["batch"].clone())?;
        agent.api.traffic_totals(&batch).await?;
        call(
            reqwest::Method::POST,
            "/totals/ack",
            Some(json!({ "upto": seq }).to_string().into_bytes()),
            Duration::from_secs(10),
        )
        .await?;
        sent += 1;
    }
    Ok(sent)
}

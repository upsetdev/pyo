//! pyo-kv: the REST API in front of a Pyo KV database (Valkey).
//!
//! ```text
//! PYO_KV_ADDR            Valkey address (default 127.0.0.1:6379)
//! PYO_KV_PASSWORD        Valkey password
//! PYO_KV_TOKEN           the REST token (required with the REST API)
//! PYO_KV_REST            0: no REST API, only /_ready and /_health (default 1)
//! PYO_KV_REDIS_LISTEN    the Redis protocol's address (default 0.0.0.0:6379;
//!                        "off": none)
//! PYO_KV_READONLY_TOKEN  a token that can only read (optional)
//! PYO_KV_LISTEN          listen address (default 0.0.0.0:8080)
//! PYO_KV_METRICS_LISTEN  /metrics' address (default 0.0.0.0:9121; "off": none)
//! PYO_KV_MEMBERS         every Valkey instance by its own service, as
//!                        host:port,... (the host is named after the pod):
//!                        their INFO on /metrics
//! PYO_KV_MAX_CONNS       connections to Valkey at most (default 128)
//! PYO_KV_SENTINELS       sentinels as host:port,host:port,... (optional): follow
//!                        the primary they agree on instead of PYO_KV_ADDR
//! PYO_KV_SENTINEL_NAME   the name the sentinels watch it under
//! PYO_KV_READ_REPLICAS   read replicas as host:port,host:port,... (optional):
//!                        read-only requests go to them
//! PYO_KV_READ_MAX_LAG_MS a replica further behind gets no reads (default 1000)
//! PYO_KV_READ_TIMEOUT_MS a request on a replica that takes longer is answered
//!                        by the primary instead (default 1000)
//! PYO_KV_READ_AFTER_WRITE_MS  after any write, reads go to the primary this
//!                        long (default 0: off)
//! ```

use std::{sync::Arc, time::Duration};

use anyhow::Context;

fn env(k: &str, def: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| def.to_owned())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let token = env("PYO_KV_TOKEN", "");
    let rest = env("PYO_KV_REST", "1") != "0";
    anyhow::ensure!(!rest || !token.is_empty(), "PYO_KV_TOKEN is required");
    let max: usize = env("PYO_KV_MAX_CONNS", "128")
        .parse()
        .context("PYO_KV_MAX_CONNS")?;
    let addr = env("PYO_KV_ADDR", "127.0.0.1:6379");
    let password = env("PYO_KV_PASSWORD", "");
    let ms = |k: &str, def: &str| -> anyhow::Result<Duration> {
        Ok(Duration::from_millis(
            env(k, def).parse().with_context(|| k.to_owned())?,
        ))
    };
    let replicas: Vec<String> = env("PYO_KV_READ_REPLICAS", "")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    // Always watched: the Redis endpoint uses the primary's offsets for WAIT
    // and read-your-writes even without read replicas.
    let reads = Arc::new(pyo_kv::reads::Reads::new(
        &replicas,
        &password,
        max,
        ms("PYO_KV_READ_MAX_LAG_MS", "1000")?,
        ms("PYO_KV_READ_TIMEOUT_MS", "1000")?,
        ms("PYO_KV_READ_AFTER_WRITE_MS", "0")?,
    ));
    let pool = pyo_resp::Pool::new(addr.clone(), password.clone(), max);
    let pool3 = pyo_resp::Pool::with_proto(addr.clone(), password.clone(), max, 3);
    let (epoch_tx, epoch) = tokio::sync::watch::channel(0u64);
    let cfg = pyo_kv::Config {
        reads: (!replicas.is_empty()).then(|| reads.clone()),
        pool: pool.clone(),
        token,
        readonly_token: Some(env("PYO_KV_READONLY_TOKEN", "")).filter(|t| !t.is_empty()),
        timeout: Duration::from_secs(30),
        max_body: 10 << 20,
    };
    let sentinels: Vec<String> = env("PYO_KV_SENTINELS", "")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if !sentinels.is_empty() {
        let name = env("PYO_KV_SENTINEL_NAME", "");
        anyhow::ensure!(
            !name.is_empty(),
            "PYO_KV_SENTINEL_NAME is required with PYO_KV_SENTINELS"
        );
        tokio::spawn(follow(
            vec![pool.clone(), pool3.clone()],
            epoch_tx,
            sentinels,
            password.clone(),
            name,
        ));
    }
    if !replicas.is_empty() {
        tracing::info!(?replicas, "read replicas");
    }
    let allowed = pyo_kv::READS.iter().copied().collect();
    tokio::spawn(pyo_kv::reads::watch(
        reads.clone(),
        pool.clone(),
        password.clone(),
        allowed,
    ));

    let metrics = env("PYO_KV_METRICS_LISTEN", "0.0.0.0:9121");
    if metrics != "off" {
        let members: Vec<String> = env("PYO_KV_MEMBERS", "")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let l = tokio::net::TcpListener::bind(&metrics)
            .await
            .with_context(|| format!("listen on {metrics}"))?;
        tracing::info!(listen = %metrics, members = members.len(), "metrics");
        tokio::spawn(pyo_kv::metrics::serve(
            Arc::new(pyo_kv::metrics::Members::new(
                &members,
                &password,
                reads.clone(),
            )),
            l,
        ));
    }

    let redis = env("PYO_KV_REDIS_LISTEN", "0.0.0.0:6379");
    if redis != "off" {
        let l = tokio::net::TcpListener::bind(&redis)
            .await
            .with_context(|| format!("listen on {redis}"))?;
        tracing::info!(listen = %redis, "redis protocol");
        let proxy = Arc::new(pyo_kv::proxy::Proxy::new(
            password.clone(),
            pool.clone(),
            pool3,
            reads,
            epoch,
            Duration::from_secs(30),
        ));
        tokio::spawn(pyo_kv::proxy::serve(proxy, l));
    }
    let listen = env("PYO_KV_LISTEN", "0.0.0.0:8080");
    let l = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    tracing::info!(%listen, valkey = %addr, rest, "pyo-kv listening");
    let app = if rest {
        pyo_kv::router(cfg)
    } else {
        pyo_kv::health_router(cfg)
    };
    axum::serve(l, app)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        })
        .await?;
    Ok(())
}

/// Keeps the pools pointed at the primary the sentinels agree on. The sentinels' names are looked up once: their addresses
/// don't change, and a slow DNS server mustn't delay following a failover.
async fn follow(
    pools: Vec<pyo_resp::Pool>,
    epoch: tokio::sync::watch::Sender<u64>,
    sentinels: Vec<String>,
    password: String,
    name: String,
) {
    let pool = pools[0].clone();
    let mut addrs = Vec::new();
    for s in &sentinels {
        loop {
            match tokio::net::lookup_host(s.as_str())
                .await
                .ok()
                .and_then(|mut a| a.next())
            {
                Some(a) => break addrs.push(a.to_string()),
                None => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }
    tracing::info!(?addrs, "following the sentinels");
    // Four times a second: a failover reaches clients within a quarter
    // second of the sentinels agreeing on it.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Ok(primary) = pyo_resp::sentinel_primary(&addrs, &password, &name).await else {
            continue;
        };
        // Only an instance that says it's primary: the sentinels may still be
        // promoting it.
        if primary != pool.addr() && pyo_resp::is_primary(&primary, &password).await {
            for p in &pools {
                p.set_addr(&primary);
            }
            // Connections held to the old primary (transactions, blocking
            // commands, subscriptions) end now.
            epoch.send_modify(|e| *e += 1);
            tracing::info!(%primary, "the primary moved");
        }
    }
}

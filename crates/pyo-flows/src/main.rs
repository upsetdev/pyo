//! pyo-flows: connection analytics for a cluster's public TCP and UDP ports
//! (off by default). Runs on each server that takes public
//! traffic, in the server's network namespace with NET_ADMIN (to hear
//! conntrack's events), with no cluster credentials and no volumes.
//!
//! - Every minute it asks Pyo which ports are public (`/api/agent/v1/flows/ports`)
//!   and sends what it counted (`/api/agent/v1/flows`), with a token that
//!   only works for those two endpoints, and only while it is on.
//! - It records connection metadata only: client address, port, protocol,
//!   duration, bytes and packets. Nothing inside a connection.

mod agg;
mod conntrack;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo_proto::flows::{FlowBatch, FlowPorts};
use tracing_subscriber::EnvFilter;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// This server's own addresses (the collector shares its network).
fn local_addrs() -> Vec<std::net::IpAddr> {
    let mut out = vec![];
    // SAFETY: getifaddrs/freeifaddrs as documented; addresses are copied out.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut p = ifap;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let s = &*(a as *const libc::sockaddr_in);
                        out.push(std::net::IpAddr::from(
                            u32::from_be(s.sin_addr.s_addr).to_be_bytes(),
                        ));
                    }
                    libc::AF_INET6 => {
                        let s = &*(a as *const libc::sockaddr_in6);
                        out.push(std::net::IpAddr::from(s.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// Byte counts need nf_conntrack_acct; turned on here when the server lets
/// the container (it usually doesn't: Pyo's server setup turns it on).
fn bytes_counted() -> bool {
    const ACCT: &str = "/proc/sys/net/netfilter/nf_conntrack_acct";
    if std::fs::read_to_string(ACCT).is_ok_and(|v| v.trim() == "1") {
        return true;
    }
    std::fs::write(ACCT, "1").is_ok()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "pyo_flows=info".into()),
        )
        .init();
    if std::env::args().nth(1).as_deref() == Some("version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let api = std::env::var("PYO_API_URL")?
        .trim_end_matches('/')
        .to_owned();
    let token = std::env::var("PYO_FLOWS_TOKEN")?;
    let node = std::env::var("PYO_NODE_NAME").unwrap_or_default();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let agg = Arc::new(Mutex::new(agg::Aggregator::new(agg::Limits::default())));
    let sock = conntrack::Socket::open().map_err(|e| {
        anyhow::anyhow!("can't listen to conntrack (needs NET_ADMIN and the server's network): {e}")
    })?;
    {
        let agg = agg.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                match sock.recv(&mut buf) {
                    Ok(events) => {
                        let t = now_ms();
                        let mut a = agg.lock().unwrap_or_else(|e| e.into_inner());
                        for e in &events {
                            a.add(e, t);
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                        agg.lock().unwrap_or_else(|e| e.into_inner()).overflow += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        tracing::error!(error = %e, "conntrack socket failed");
                        std::process::exit(1);
                    }
                }
            }
        });
    }
    tracing::info!(%node, "pyo-flows listening to conntrack");

    let mut pending: Vec<FlowBatch> = vec![];
    let mut tick = tokio::time::interval(Duration::from_secs(20));
    loop {
        tick.tick().await;
        // What to watch.
        match http
            .get(format!("{api}/api/agent/v1/flows/ports"))
            .bearer_auth(&token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                if let Ok(p) = r.json::<FlowPorts>().await {
                    let mut a = agg.lock().unwrap_or_else(|e| e.into_inner());
                    a.set_ports(&p.ports);
                    a.set_local(local_addrs());
                }
            }
            // Turned off, or the token was replaced: watch nothing.
            Ok(r) if r.status().as_u16() == 401 || r.status().as_u16() == 403 => {
                agg.lock().unwrap_or_else(|e| e.into_inner()).set_ports(&[])
            }
            Ok(r) => tracing::warn!(status = %r.status(), "couldn't get the public ports"),
            Err(e) => tracing::warn!(error = %e, "couldn't reach Pyo"),
        }
        // What was counted.
        let t = now_ms();
        let (records, overflow) = {
            let mut a = agg.lock().unwrap_or_else(|e| e.into_inner());
            a.expire(t, 24 * 3_600_000);
            let r = a.take_finished(t);
            (r, std::mem::take(&mut a.overflow))
        };
        if !records.is_empty() || overflow > 0 {
            pending.push(FlowBatch {
                node: node.clone(),
                records,
                bytes_counted: bytes_counted(),
                overflow,
            });
        }
        // Kept for a while if Pyo is away, then dropped (oldest first).
        while pending.len() > 30 {
            pending.remove(0);
        }
        while let Some(b) = pending.first() {
            match http
                .post(format!("{api}/api/agent/v1/flows"))
                .bearer_auth(&token)
                .json(b)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    pending.remove(0);
                }
                Ok(r) if r.status().as_u16() == 401 || r.status().as_u16() == 403 => {
                    pending.clear();
                }
                Ok(r) => {
                    tracing::warn!(status = %r.status(), "Pyo didn't take the connections");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't send the connections");
                    break;
                }
            }
        }
    }
}

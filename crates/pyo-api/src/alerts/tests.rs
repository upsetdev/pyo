use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
};
use serde_json::json;

use super::*;

fn alert(event: Event) -> Alert {
    Alert {
        event,
        key: "deploy:svc_1".into(),
        title: "Deploy failed: web".into(),
        team: "Acme".into(),
        team_slug: "acme".into(),
        facts: vec![("Project".into(), "shop".into())],
        path: "/deployments/dpl_1".into(),
        at: 1_700_000_000_000,
    }
}

const LINK: &str = "https://pyo.example.com/acme/deployments/dpl_1";

// ── Messages ──

#[test]
fn links_go_under_the_team_at_pyo_s_address() {
    let a = alert(Event::DeployFailed);
    assert_eq!(a.link("https://pyo.example.com/").as_deref(), Some(LINK));
    assert_eq!(a.link(""), None);
}

#[test]
fn plain_text_is_short_and_in_order() {
    let a = alert(Event::DeployFailed);
    assert_eq!(
        plain_text(&a, Some(LINK)),
        format!("Deploy failed: web\nTeam: Acme\nProject: shop\n{LINK}")
    );
    // No address: no link line.
    assert_eq!(
        plain_text(&a, None),
        "Deploy failed: web\nTeam: Acme\nProject: shop"
    );
}

#[test]
fn telegram_sends_plain_text_without_markup() {
    let b = telegram_body("-100123", &alert(Event::DeployFailed), Some(LINK));
    assert_eq!(b["chat_id"], "-100123");
    assert!(
        b["text"]
            .as_str()
            .unwrap()
            .starts_with("Deploy failed: web\n")
    );
    assert!(b.get("parse_mode").is_none());
    assert_eq!(b["link_preview_options"]["is_disabled"], true);
}

#[test]
fn slack_escapes_its_markup() {
    let mut a = alert(Event::DeployFailed);
    a.team = "R&D <!channel>".into();
    let text = slack_body(&a, Some(LINK))["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.starts_with("*Deploy failed: web*\n"), "{text}");
    assert!(text.contains("Team: R&amp;D &lt;!channel&gt;"), "{text}");
    assert!(text.ends_with(&format!("<{LINK}|Open in Pyo>")), "{text}");
}

#[test]
fn discord_mentions_no_one_and_escapes_markdown() {
    let mut a = alert(Event::DeployFailed);
    a.team = "@everyone *bold*".into();
    let b = discord_body(&a, Some(LINK));
    assert_eq!(b["allowed_mentions"], json!({ "parse": [] }));
    let content = b["content"].as_str().unwrap();
    assert!(content.contains(r"Team: \@everyone \*bold\*"), "{content}");
    assert!(content.starts_with("**Deploy failed: web**"), "{content}");
    // No preview card.
    assert!(content.ends_with(&format!("<{LINK}>")), "{content}");
}

#[test]
fn long_messages_are_cut_to_what_each_service_takes() {
    let mut a = alert(Event::DeployFailed);
    a.title = "x".repeat(5000);
    assert!(
        discord_body(&a, None)["content"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 2000
    );
    assert!(
        slack_body(&a, None)["text"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 3000
    );
    assert!(
        telegram_body("1", &a, None)["text"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 4000
    );
}

#[test]
fn a_webhook_gets_json_with_the_event_and_details() {
    let b = webhook_body(&alert(Event::ServerOnline), Some(LINK));
    assert_eq!(
        b,
        json!({
            "event": "server_online",
            "title": "Deploy failed: web",
            "team": { "name": "Acme", "slug": "acme" },
            "details": { "project": "shop" },
            "url": LINK,
            "at": 1_700_000_000_000_i64,
        })
    );
}

// ── Signing ──

#[test]
fn signatures_cover_the_timestamp_and_the_body() {
    let body = br#"{"event":"test"}"#;
    // printf '1700000000.{"event":"test"}' | openssl dgst -sha256 -hmac s3cret
    assert_eq!(
        sign("s3cret", 1_700_000_000, body),
        "sha256=1c5b24400e91c3c2a54a5fc594c52fc115eb8e1d8d6b200dc40d90fa6ef6e273"
    );
    // A recorded request sent again later doesn't match a new timestamp.
    assert_ne!(
        sign("s3cret", 1_700_000_001, body),
        sign("s3cret", 1_700_000_000, body)
    );
    assert_ne!(
        sign("other", 1_700_000_000, body),
        sign("s3cret", 1_700_000_000, body)
    );
}

#[test]
fn webhooks_are_signed_only_with_a_secret() {
    let a = alert(Event::DeployFailed);
    let header = |o: &Outgoing, k: &str| {
        o.headers
            .iter()
            .find(|(h, _)| *h == k)
            .map(|(_, v)| v.clone())
    };
    let signed = Target::Webhook {
        url: "https://hooks.example.com/pyo".into(),
        secret: Some("s3cret".into()),
    };
    let o = outgoing(&signed, &a, Some(LINK), TELEGRAM_API, 1_700_000_000);
    assert_eq!(header(&o, "x-pyo-timestamp").as_deref(), Some("1700000000"));
    assert_eq!(header(&o, "x-pyo-event").as_deref(), Some("deploy_failed"));
    assert_eq!(
        header(&o, "x-pyo-signature"),
        Some(sign("s3cret", 1_700_000_000, &o.body))
    );
    let unsigned = Target::Webhook {
        url: "https://hooks.example.com/pyo".into(),
        secret: None,
    };
    let o = outgoing(&unsigned, &a, None, TELEGRAM_API, 1_700_000_000);
    assert_eq!(header(&o, "x-pyo-signature"), None);
    assert_eq!(header(&o, "x-pyo-timestamp").as_deref(), Some("1700000000"));
}

#[test]
fn telegram_goes_to_the_bot_s_send_message() {
    let t = Target::Telegram {
        token: "123:abc".into(),
        chat_id: "42".into(),
    };
    let o = outgoing(&t, &alert(Event::Test), None, "http://127.0.0.1:9/", 0);
    assert_eq!(o.url, "http://127.0.0.1:9/bot123:abc/sendMessage");
    assert!(o.headers.iter().all(|(k, _)| !k.starts_with("x-pyo")));
}

// ── Where alerts may go ──

#[test]
fn telegram_tokens_and_chats_are_checked() {
    let token = format!("123456789:{}", "A".repeat(35));
    assert!(telegram_token_ok(&token));
    for bad in [
        "",
        "123456",
        "abc:defdefdefdefdefdefdefdefdefdefdef",
        "123:short",
        "123:has space in it and more and more and more",
    ] {
        assert!(!telegram_token_ok(bad), "{bad}");
    }
    for ok in ["42", "-1001234567890", "@pyo_alerts"] {
        assert!(telegram_chat_ok(ok), "{ok}");
    }
    for bad in [
        "",
        "-",
        "12a",
        "@abc",
        "@has-dash",
        "1234567890123456789012",
    ] {
        assert!(!telegram_chat_ok(bad), "{bad}");
    }
}

#[test]
fn discord_and_slack_only_take_their_own_webhooks() {
    for ok in [
        "https://discord.com/api/webhooks/123/abc",
        "https://discordapp.com/api/webhooks/123/abc",
        "https://ptb.discord.com/api/webhooks/123/abc",
    ] {
        assert!(url_form("discord", ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://discord.com/api/webhooks/123/abc",
        "https://discord.com:8443/api/webhooks/123/abc",
        "https://discord.com/channels/1/2",
        "https://discord.com.evil.example/api/webhooks/1/a",
        "https://user:pw@discord.com/api/webhooks/1/a",
        "https://169.254.169.254/api/webhooks/1/a",
    ] {
        assert!(url_form("discord", bad).is_err(), "{bad}");
    }
    assert!(url_form("slack", "https://hooks.slack.com/services/T0/B0/xyz").is_ok());
    for bad in [
        "http://hooks.slack.com/services/T0/B0/xyz",
        "https://hooks.slack.com/triggers/T0/1/abc",
        "https://slack.com/services/T0/B0/xyz",
        "https://localhost/services/T0/B0/xyz",
    ] {
        assert!(url_form("slack", bad).is_err(), "{bad}");
    }
}

fn webhook(url: &str) -> Target {
    Target::Webhook {
        url: url.into(),
        secret: None,
    }
}

#[tokio::test]
async fn webhooks_never_reach_pyo_metadata_or_the_cluster() {
    for url in [
        "https://169.254.169.254/latest/meta-data",
        "http://169.254.169.254/",
        "https://127.0.0.1/hook",
        "https://localhost/hook",
        "https://[::1]/hook",
        "https://10.43.0.1/",
        "https://10.42.0.7:8080/",
        "https://0.0.0.0/",
        "https://[fe80::1]/",
        "https://[::ffff:169.254.169.254]/",
    ] {
        for private in [true, false] {
            let r = check_target(&webhook(url), private).await;
            assert!(r.is_err(), "{url} (private networks {private})");
        }
    }
}

#[tokio::test]
async fn plain_http_only_to_private_networks_where_allowed() {
    // https on the internet: fine.
    assert_eq!(
        check_target(&webhook("https://93.184.215.14/hook"), false).await,
        Ok(())
    );
    // Plain http on the internet: never.
    let e = check_target(&webhook("http://93.184.215.14/hook"), true)
        .await
        .unwrap_err();
    assert!(e.contains("https://"), "{e}");
    // A LAN or tailnet address, plain or not, where the installation allows it.
    for url in [
        "http://192.168.1.20:8080/hook",
        "https://100.64.0.103/hook",
    ] {
        assert_eq!(check_target(&webhook(url), true).await, Ok(()), "{url}");
        assert!(check_target(&webhook(url), false).await.is_err(), "{url}");
    }
    // Not a URL Pyo can call.
    for url in ["ftp://example.com/x", "hooks.example.com", "https://"] {
        assert!(check_target(&webhook(url), true).await.is_err(), "{url}");
    }
}

#[test]
fn hints_never_show_a_token() {
    let token = format!("123456789:{}", "S".repeat(35));
    let t = Target::Telegram {
        token: token.clone(),
        chat_id: "-1001".into(),
    };
    assert_eq!(t.hint(), "Chat -1001");
    let d = Target::Discord {
        url: "https://discord.com/api/webhooks/123/secrettokenWXYZ".into(),
    };
    assert_eq!(d.hint(), "Webhook …WXYZ");
    let w = Target::Webhook {
        url: "https://hooks.example.com:8443/pyo/s3cr3t-path".into(),
        secret: Some("x".into()),
    };
    assert_eq!(w.hint(), "hooks.example.com:8443, signed");
}

#[test]
fn a_change_keeps_what_it_leaves_out() {
    let saved = Target::Telegram {
        token: "1:old".into(),
        chat_id: "42".into(),
    };
    let g = Given {
        chat_id: Some(" -7 ".into()),
        ..Default::default()
    };
    assert_eq!(
        target_from("telegram", &g, Some(&saved)),
        Ok(Target::Telegram {
            token: "1:old".into(),
            chat_id: "-7".into()
        })
    );
    let saved = Target::Webhook {
        url: "https://a.example.com/".into(),
        secret: Some("s".into()),
    };
    // A new URL keeps the secret; an empty secret removes it.
    let g = Given {
        webhook_url: Some("https://b.example.com/".into()),
        ..Default::default()
    };
    assert_eq!(
        target_from("webhook", &g, Some(&saved)),
        Ok(Target::Webhook {
            url: "https://b.example.com/".into(),
            secret: Some("s".into())
        })
    );
    let g = Given {
        secret: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(
        target_from("webhook", &g, Some(&saved)),
        Ok(Target::Webhook {
            url: "https://a.example.com/".into(),
            secret: None
        })
    );
    // A new one needs everything.
    assert!(target_from("telegram", &Given::default(), None).is_err());
    assert!(target_from("email", &Given::default(), None).is_err());
}

#[test]
fn every_event_is_on_unless_turned_off() {
    assert_eq!(muted_from(None).unwrap(), Vec::<String>::new());
    assert_eq!(
        muted_from(Some(&["deploy_failed".into(), "certificate".into()][..])).unwrap(),
        vec![
            "server_offline".to_owned(),
            "server_pressure".to_owned(),
            "backup_failed".to_owned()
        ]
    );
    assert!(muted_from(Some(&[][..])).is_err());
    assert!(muted_from(Some(&["everything".into()][..])).is_err());
}

// ── Which activity raises what ──

#[test]
fn activity_kinds_map_to_alerts() {
    let cert = json!({ "kind": "certificate" });
    let cluster = json!({ "kind": "cluster" });
    assert_eq!(
        classify("deployment.failed", &json!({})),
        Some(Event::DeployFailed)
    );
    assert_eq!(
        classify("cluster.unreachable", &cluster),
        Some(Event::ServerOffline)
    );
    assert_eq!(
        classify("cluster.reachable", &cluster),
        Some(Event::ServerOnline)
    );
    assert_eq!(
        classify("server.offline", &json!({})),
        Some(Event::ServerOffline)
    );
    assert_eq!(
        classify("server.pressure", &json!({})),
        Some(Event::ServerPressure)
    );
    assert_eq!(
        classify("server.pressure_ended", &json!({})),
        Some(Event::ServerPressureEnded)
    );
    assert_eq!(
        classify("backup.failed", &json!({})),
        Some(Event::BackupFailed)
    );
    assert_eq!(
        classify("domain.certificate_expiring", &cert),
        Some(Event::CertificateExpiring)
    );
    // The same kind for an mTLS change that failed isn't a certificate expiring.
    assert_eq!(classify("domain.certificate_expiring", &cluster), None);
    for quiet in [
        "deployment.succeeded",
        "service.created",
        "cluster.registered",
    ] {
        assert_eq!(classify(quiet, &json!({})), None, "{quiet}");
    }
    // Each lands under one of the settings.
    for e in [
        Event::DeployFailed,
        Event::ServerOffline,
        Event::ServerOnline,
        Event::ServerPressure,
        Event::ServerPressureEnded,
        Event::BackupFailed,
        Event::CertificateExpiring,
        Event::CertificateFailed,
    ] {
        assert!(TOPICS.contains(&e.topic()), "{e:?}");
    }
}

// ── One per subject per window ──

/// Runs events through `decide` as `raise` and `flush` keep the state,
/// `minutes` apart; returns what went out.
fn simulate(events: &[(i64, Event)]) -> Vec<(i64, Event)> {
    let mut last: Option<(i64, Event)> = None;
    let mut held: Option<Event> = None;
    let mut sent = vec![];
    let flush = |at: i64,
                 last: &mut Option<(i64, Event)>,
                 held: &mut Option<Event>,
                 sent: &mut Vec<(i64, Event)>| {
        if let (Some(h), Some((t, _))) = (*held, *last)
            && at - t >= WINDOW_SECS
        {
            sent.push((t + WINDOW_SECS, h));
            *last = Some((t + WINDOW_SECS, h));
            *held = None;
        }
    };
    for &(min, e) in events {
        let at = min * 60;
        flush(at, &mut last, &mut held, &mut sent);
        let state = last.map(|(t, ev)| Last {
            age: at - t,
            event: ev,
            held: held.is_some(),
        });
        match decide(state.as_ref(), e) {
            Decision::Send => {
                sent.push((at, e));
                last = Some((at, e));
                held = None;
            }
            Decision::Hold => held = Some(e),
            Decision::DropHeld => held = None,
            Decision::Drop => {}
        }
    }
    flush(i64::MAX / 2, &mut last, &mut held, &mut sent);
    sent.into_iter().map(|(t, e)| (t / 60, e)).collect()
}

#[test]
fn a_flapping_server_sends_offline_and_online_once_then_waits() {
    use Event::{ServerOffline as Off, ServerOnline as On};
    // Down and up every minute, ending up.
    let flaps: Vec<(i64, Event)> = (0..10)
        .map(|m| (m, if m % 2 == 0 { Off } else { On }))
        .collect();
    assert_eq!(simulate(&flaps), vec![(0, Off), (1, On)]);
    // Ending down: told once the window is over, since it still is.
    let mut down = flaps.clone();
    down.push((10, Off));
    assert_eq!(simulate(&down), vec![(0, Off), (1, On), (16, Off)]);
    // And back up later: that goes out too.
    down.push((30, On));
    assert_eq!(
        simulate(&down),
        vec![(0, Off), (1, On), (16, Off), (30, On)]
    );
}

#[test]
fn back_online_only_follows_an_offline_that_went_out() {
    use Event::{ServerOffline as Off, ServerOnline as On};
    assert_eq!(simulate(&[(0, On)]), vec![]);
    assert_eq!(
        simulate(&[(0, Off), (5, On), (6, On)]),
        vec![(0, Off), (5, On)]
    );
    // Offline again after the window: at once.
    assert_eq!(
        simulate(&[(0, Off), (5, On), (30, Off)]),
        vec![(0, Off), (5, On), (30, Off)]
    );
}

#[test]
fn pressure_that_comes_and_goes_alerts_once_per_window() {
    use Event::{ServerPressure as Low, ServerPressureEnded as Fine};
    let flaps: Vec<(i64, Event)> = (0..10)
        .map(|m| (m, if m % 2 == 0 { Low } else { Fine }))
        .collect();
    assert_eq!(simulate(&flaps), vec![(0, Low), (1, Fine)]);
    let mut low = flaps.clone();
    low.push((10, Low));
    assert_eq!(simulate(&low), vec![(0, Low), (1, Fine), (16, Low)]);
    // Fine only after a "low" that went out.
    assert_eq!(simulate(&[(0, Fine)]), vec![]);
}

#[test]
fn repeated_failures_go_once_per_window() {
    let d = Event::DeployFailed;
    assert_eq!(
        simulate(&[(0, d), (3, d), (14, d), (15, d), (16, d)]),
        vec![(0, d), (15, d)]
    );
}

// ── Sending and retries ──

#[derive(Clone)]
struct Srv {
    /// Status and body for each request in turn; 200 once they run out.
    replies: Arc<Mutex<VecDeque<(u16, &'static str)>>>,
    seen: Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>,
    delay: Duration,
}

async fn handle(
    State(s): State<Srv>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    s.seen
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), headers, body));
    tokio::time::sleep(s.delay).await;
    let (code, text) = s.replies.lock().unwrap().pop_front().unwrap_or((200, "ok"));
    (StatusCode::from_u16(code).unwrap(), text)
}

/// A local server that answers with `replies` in turn.
async fn serve(replies: &[(u16, &'static str)], delay: Duration) -> (String, Srv) {
    let srv = Srv {
        replies: Arc::new(Mutex::new(replies.iter().copied().collect())),
        seen: Arc::default(),
        delay,
    };
    let app = Router::new().fallback(handle).with_state(srv.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, srv)
}

/// Tests talk to a local server, so not through the guard.
fn plain_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder().timeout(timeout).build().unwrap()
}

const FAST: [Duration; 2] = [Duration::from_millis(5), Duration::from_millis(5)];

fn hook_to(base: &str) -> Outgoing {
    let t = webhook(&format!("{base}/hook"));
    outgoing(
        &t,
        &alert(Event::DeployFailed),
        Some(LINK),
        TELEGRAM_API,
        1_700_000_000,
    )
}

#[tokio::test]
async fn server_errors_are_tried_again() {
    let (base, srv) = serve(&[(500, ""), (503, "")], Duration::ZERO).await;
    let r = send(
        &plain_client(Duration::from_secs(5)),
        &hook_to(&base),
        &FAST,
    )
    .await;
    assert_eq!(r, Ok(()));
    assert_eq!(srv.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_failure_that_lasts_is_left_to_the_job() {
    let (base, srv) = serve(
        &[(502, ""), (502, ""), (502, ""), (502, "")],
        Duration::ZERO,
    )
    .await;
    let r = send(
        &plain_client(Duration::from_secs(5)),
        &hook_to(&base),
        &FAST,
    )
    .await;
    assert_eq!(r, Err(Failure::new(true, "502")));
    // Three tries in one run, then the job runs it again later.
    assert_eq!(srv.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn refusals_are_not_tried_again() {
    let (base, srv) = serve(&[(401, "nope")], Duration::ZERO).await;
    let r = send(
        &plain_client(Duration::from_secs(5)),
        &hook_to(&base),
        &FAST,
    )
    .await;
    // A webhook's own words aren't shown, only the status.
    assert_eq!(r, Err(Failure::new(false, "401")));
    assert_eq!(srv.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limits_are_waited_out() {
    let (base, srv) = serve(&[(429, "")], Duration::ZERO).await;
    let r = send(
        &plain_client(Duration::from_secs(5)),
        &hook_to(&base),
        &FAST,
    )
    .await;
    assert_eq!(r, Ok(()));
    assert_eq!(srv.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn timeouts_are_tried_again() {
    let (base, srv) = serve(&[], Duration::from_millis(400)).await;
    let r = send(
        &plain_client(Duration::from_millis(100)),
        &hook_to(&base),
        &FAST,
    )
    .await;
    assert_eq!(r, Err(Failure::new(true, "Timed out")));
    assert_eq!(srv.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn telegram_s_reason_is_shown() {
    let (base, srv) = serve(
        &[(
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
        )],
        Duration::ZERO,
    )
    .await;
    let t = Target::Telegram {
        token: "123:abc".into(),
        chat_id: "42".into(),
    };
    let out = outgoing(&t, &alert(Event::Test), None, &base, 0);
    let r = send(&plain_client(Duration::from_secs(5)), &out, &FAST).await;
    assert_eq!(r, Err(Failure::new(false, "400: chat not found")));
    let seen = srv.seen.lock().unwrap();
    assert_eq!(seen[0].0, "/bot123:abc/sendMessage");
    let body: Value = serde_json::from_slice(&seen[0].2).unwrap();
    assert_eq!(body["chat_id"], "42");
}

#[tokio::test]
async fn the_receiver_can_check_the_signature() {
    let (base, srv) = serve(&[], Duration::ZERO).await;
    let t = Target::Webhook {
        url: format!("{base}/hook"),
        secret: Some("s3cret".into()),
    };
    let out = outgoing(
        &t,
        &alert(Event::DeployFailed),
        Some(LINK),
        TELEGRAM_API,
        1_700_000_123,
    );
    send(&plain_client(Duration::from_secs(5)), &out, &FAST)
        .await
        .unwrap();
    let seen = srv.seen.lock().unwrap();
    let (_, headers, body) = &seen[0];
    let ts: i64 = headers["x-pyo-timestamp"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        headers["x-pyo-signature"].to_str().unwrap(),
        sign("s3cret", ts, body)
    );
    assert_eq!(headers["content-type"], "application/json");
    let v: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(v["event"], "deploy_failed");
}

#[tokio::test]
async fn the_guarded_client_refuses_this_machine() {
    let (base, srv) = serve(&[], Duration::ZERO).await;
    // By name, so the client's resolver checks it as it connects.
    let url = base.replace("127.0.0.1", "localhost");
    let client = outbound::client(Reach { private: true }, Duration::from_secs(5)).unwrap();
    let r = send(&client, &hook_to(&url), &FAST).await;
    let f = r.unwrap_err();
    assert!(!f.retry, "{f:?}");
    assert!(f.message.contains("isn't allowed"), "{f:?}");
    assert!(srv.seen.lock().unwrap().is_empty());
}

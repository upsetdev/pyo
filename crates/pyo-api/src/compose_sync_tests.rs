use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;
use crate::compose::{Input, plan};

const FILE: &str = r#"
name: cloud
services:
  cloud-api:
    image: ghcr.io/acme/cloud-api:1.2
    environment:
      LOG_LEVEL: info
      DATABASE_URL: postgres://db:5432/cloud
    ports: ["8080:8080"]
    x-pyo:
      domains: [cloud.example.com]
      behindCdn: cloudflare
      health: /readyz
      replicas: 3
      secrets: [PYO_SECRET_KEY]
  db:
    image: postgres:17
    volumes: ["db-data:/var/lib/postgresql/data"]
volumes:
  db-data: {}
"#;

fn file_plan(text: &str) -> ComposePlan {
    let mut p = plan(&Input {
        file: text,
        files: BTreeMap::new(),
        name_hint: "cloud",
        ..Default::default()
    })
    .unwrap();
    p.project.file = "compose.yaml".into();
    p
}

/// The project as an import of `text` leaves it.
fn imported(text: &str) -> Vec<Existing> {
    let p = file_plan(text);
    let n = std::cell::Cell::new(0);
    let new_id = |prefix: &str| {
        n.set(n.get() + 1);
        format!("{prefix}_{}", n.get())
    };
    p.services
        .iter()
        .map(|s| {
            let id = format!("svc_{}", s.name);
            let spec = compose::service_spec(
                s,
                Some(&p.project.file),
                &id,
                "team_a",
                "prj_a",
                "cl_a",
                1,
                &new_id,
            );
            Existing {
                id,
                name: s.name.clone(),
                spec,
                behind_cdn: s.behind_cdn.clone(),
                volumes: compose::volume_specs(s, &new_id),
            }
        })
        .collect()
}

fn project() -> Option<SyncProject> {
    Some(SyncProject {
        id: "prj_a".into(),
        name: "cloud".into(),
        namespace: "cloud".into(),
    })
}

fn sync(text: &str, existing: &[Existing]) -> SyncPlan {
    plan_of(project(), file_plan(text), existing)
}

fn service<'a>(p: &'a SyncPlan, name: &str) -> &'a SyncService {
    p.services.iter().find(|s| s.name == name).unwrap()
}

fn fields(s: &SyncService) -> Vec<&str> {
    s.changes.iter().map(|c| c.field.as_str()).collect()
}

#[test]
fn the_file_reads_its_x_pyo() {
    let p = file_plan(FILE);
    let api = p.services.iter().find(|s| s.name == "cloud-api").unwrap();
    assert_eq!(api.domains, vec!["cloud.example.com"]);
    assert_eq!(api.behind_cdn.as_deref(), Some("cloudflare"));
    assert_eq!(api.replicas, 3);
    assert_eq!(api.health_check["type"], "http");
    assert_eq!(api.health_check["path"], "/readyz");
    assert_eq!(api.health_check["port"], 8080);
    assert_eq!(api.secrets, vec!["PYO_SECRET_KEY"]);
    for m in [
        "source", "env", "ports", "domains", "behindCdn", "health", "replicas", "secrets",
    ] {
        assert!(api.manages(m), "{m}");
    }
    assert!(!api.manages("volumes") && !api.manages("resources.cpuLimit"));
    // Its domain replaces the free one.
    assert_eq!(api.ports[0].domain.as_deref(), Some("cloud.example.com"));
    assert!(p.warnings.iter().all(|w| !w.field.starts_with("x-pyo")));
}

#[test]
fn an_import_records_the_file_and_makes_what_x_pyo_says() {
    let ex = imported(FILE);
    let api = &ex[0].spec;
    assert_eq!(
        api["compose"],
        json!({ "file": "compose.yaml", "service": "cloud-api" })
    );
    assert_eq!(api["domains"][0]["hostname"], "cloud.example.com");
    assert_eq!(api["domains"].as_array().unwrap().len(), 1);
    let secret = api["env"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "PYO_SECRET_KEY")
        .unwrap();
    assert_eq!((secret["secret"].clone(), secret["value"].clone()), (json!(true), json!("")));
}

#[test]
fn the_same_file_again_changes_nothing() {
    let ex = imported(FILE);
    let p = sync(FILE, &ex);
    assert_eq!(p.services.len(), 2);
    for s in &p.services {
        assert_eq!(s.status, "unchanged", "{}: {:?}", s.name, s.changes);
        assert!(s.manual.is_empty() && !s.deploy, "{}", s.name);
    }
}

#[test]
fn every_kind_of_compose_file_reads_back_unchanged() {
    for f in [
        "plausible.yml",
        "immich.yml",
        "n8n.yml",
        "uptime-kuma.yml",
        "edge-cases.yml",
        "ac-wordpress-mysql.yml",
        "ac-prometheus-grafana.yml",
    ] {
        let text = std::fs::read_to_string(format!(
            "{}/tests/fixtures/compose/{f}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let ex = imported(&text);
        let p = sync(&text, &ex);
        for s in &p.services {
            assert_eq!(s.status, "unchanged", "{f} {}: {:?}", s.name, s.changes);
        }
    }
}

#[test]
fn only_what_the_file_says_differently_changes() {
    let ex = imported(FILE);
    let next = FILE
        .replace("cloud-api:1.2", "cloud-api:1.3")
        .replace("replicas: 3", "replicas: 4")
        .replace("LOG_LEVEL: info", "LOG_LEVEL: debug\n      NEW_ONE: x")
        .replace("[cloud.example.com]", "[cloud.example.com, api.example.com]")
        .replace("health: /readyz", "health: /livez");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(api.status, "changed");
    assert_eq!(
        fields(api),
        vec![
            "image",
            "env LOG_LEVEL",
            "env NEW_ONE",
            "domain api.example.com",
            "health check",
            "replicas"
        ]
    );
    assert_eq!(
        api.changes[0],
        SyncChange {
            field: "image".into(),
            from: Some("ghcr.io/acme/cloud-api:1.2".into()),
            to: Some("ghcr.io/acme/cloud-api:1.3".into()),
        }
    );
    // Values are never shown.
    assert!(api.changes[1].from.is_none() && api.changes[1].to.is_none());
    assert!(api.deploy);
    assert_eq!(service(&p, "db").status, "unchanged");

    // The new spec has them, and keeps the rest.
    let m = merge(&ex[0], &p.file.services[0], "compose.yaml", 2, &|p| format!("{p}_new"));
    assert_eq!(m.spec["source"]["reference"], "1.3");
    assert_eq!(m.spec["replicas"], 4);
    assert_eq!(m.spec["domains"].as_array().unwrap().len(), 2);
    assert_eq!(m.spec["domains"][1]["portName"], m.spec["ports"][0]["name"]);
    assert_eq!(m.spec["healthCheck"]["path"], "/livez");
}

#[test]
fn a_domain_alone_goes_live_without_a_deploy() {
    let ex = imported(FILE);
    let next = FILE.replace("[cloud.example.com]", "[cloud.example.com, b.example.com]");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(fields(api), vec!["domain b.example.com"]);
    assert!(!api.deploy);
    let next = FILE.replace("behindCdn: cloudflare", "behindCdn: none");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(fields(api), vec!["behind CDN"]);
    assert!(!api.deploy);
    let m = merge(&ex[0], &p.file.services[0], "compose.yaml", 2, &|p| p.to_owned());
    assert_eq!(m.behind_cdn, Some(None));
}

#[test]
fn settings_changed_in_pyo_stay_when_the_file_leaves_them_out() {
    let mut ex = imported(FILE);
    let db = &mut ex[1].spec;
    // By hand: more replicas, a bigger limit, a variable, a domain, a placement.
    db["replicas"] = json!(2);
    db["resources"]["memoryLimit"] = json!(4096);
    db["env"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "env_x", "key": "EXTRA", "value": "1", "secret": false }));
    db["placement"] = json!({ "servers": [], "tags": ["db"] });
    db["healthCheck"] = json!({ "type": "exec", "command": "pg_isready", "path": "", "port": 5432, "initialDelaySeconds": 5, "periodSeconds": 10 });
    let api = &mut ex[0].spec;
    api["env"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "env_y", "key": "BY_HAND", "value": "1", "secret": false }));
    api["domains"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "dom_y", "hostname": "extra.example.com", "portName": "http" }));
    let p = sync(FILE, &ex);
    for s in &p.services {
        assert_eq!(s.status, "unchanged", "{}: {:?}", s.name, s.changes);
    }
}

#[test]
fn a_secrets_value_is_never_touched() {
    let mut ex = imported(FILE);
    // The secret got a value in Pyo (sealed), and LOG_LEVEL became a secret.
    for e in ex[0].spec["env"].as_array_mut().unwrap() {
        if e["key"] == "PYO_SECRET_KEY" {
            e["value"] = json!("sealed:abc");
        }
        if e["key"] == "LOG_LEVEL" {
            e["secret"] = json!(true);
            e["value"] = json!("sealed:def");
        }
    }
    let next = FILE.replace("LOG_LEVEL: info", "LOG_LEVEL: debug");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(api.status, "unchanged", "{:?}", api.changes);
    assert!(api.notes.iter().any(|n| n.field == "env LOG_LEVEL"));
    let m = merge(&ex[0], &p.file.services[0], "compose.yaml", 2, &|p| p.to_owned());
    let value = |k: &str| {
        m.spec["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == k)
            .unwrap()["value"]
            .clone()
    };
    assert_eq!(value("PYO_SECRET_KEY"), "sealed:abc");
    assert_eq!(value("LOG_LEVEL"), "sealed:def");
    // And a value in the file for an x-pyo secret isn't used.
    let given = FILE.replace("LOG_LEVEL: info", "LOG_LEVEL: info\n      PYO_SECRET_KEY: oops");
    let fp = file_plan(&given);
    assert!(fp.warnings.iter().any(|w| w.field == "x-pyo.secrets"));
    assert!(!fp.services[0].env.iter().any(|e| e.key == "PYO_SECRET_KEY"));
}

#[test]
fn a_new_secret_comes_without_a_value() {
    let ex = imported(FILE);
    let next = FILE.replace("[PYO_SECRET_KEY]", "[PYO_SECRET_KEY, STRIPE_KEY]");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(fields(api), vec!["secret STRIPE_KEY"]);
    assert!(!api.deploy, "it waits for its value");
    assert!(api.notes.iter().any(|n| n.field == "secret STRIPE_KEY"));
    // A plain variable of that name is a manual step.
    let mut ex = imported(FILE);
    ex[0].spec["env"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": "env_s", "key": "STRIPE_KEY", "value": "x", "secret": false }));
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(api.status, "unchanged");
    assert_eq!(api.manual[0].field, "secret STRIPE_KEY");
}

#[test]
fn what_cant_change_in_place_is_a_manual_step() {
    let ex = imported(FILE);
    // The volume would have to move.
    let moved = FILE.replace("db-data:/var/lib/postgresql/data", "db-data:/data");
    let p = sync(&moved, &ex);
    let db = service(&p, "db");
    assert_eq!(db.status, "unchanged");
    assert_eq!(db.manual.len(), 1);
    assert!(db.manual[0].message.contains("/var/lib/postgresql/data"));
    let m = merge(&ex[1], &p.file.services[1], "compose.yaml", 2, &|p| p.to_owned());
    assert!(m.volumes.is_empty());
    // Another kind of source.
    let built = FILE.replace("image: postgres:17", "build: ./db");
    let p = sync(&built, &ex);
    let db = service(&p, "db");
    assert_eq!(db.manual[0].field, "source");
    assert_eq!(db.status, "unchanged");
    // A new volume is added.
    let more = FILE.replace(
        "\"db-data:/var/lib/postgresql/data\"",
        "\"db-data:/var/lib/postgresql/data\", \"wal:/wal\"",
    );
    let p = sync(&more, &ex);
    let db = service(&p, "db");
    assert_eq!(fields(db), vec!["volume db-wal"]);
    assert!(db.deploy);
}

#[test]
fn services_no_longer_in_the_file_are_only_listed() {
    let mut ex = imported(FILE);
    ex.push(Existing {
        id: "svc_hand".into(),
        name: "hand".into(),
        spec: json!({ "name": "hand" }),
        ..Default::default()
    });
    ex.push(Existing {
        id: "svc_other".into(),
        name: "other".into(),
        spec: json!({ "name": "other", "compose": { "file": "compose.db.yaml", "service": "other" } }),
        ..Default::default()
    });
    let only_api = r#"
services:
  cloud-api:
    image: ghcr.io/acme/cloud-api:1.2
"#;
    let p = sync(only_api, &ex);
    let names: Vec<(&str, &str, bool)> = p
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.status.as_str(), s.from_file))
        .collect();
    assert_eq!(
        names,
        vec![
            ("cloud-api", "unchanged", true),
            ("db", "notInFile", true),
            ("hand", "notInFile", false),
            ("other", "notInFile", false),
        ]
    );
}

#[test]
fn a_new_service_in_the_file_is_added() {
    let ex = imported(FILE);
    let more = format!("{FILE}\n");
    let more = more.replace(
        "volumes:\n  db-data: {}",
        "  worker:\n    image: ghcr.io/acme/worker:1\nvolumes:\n  db-data: {}",
    );
    let p = sync(&more, &ex);
    let w = service(&p, "worker");
    assert_eq!((w.status.as_str(), w.service_id.is_none(), w.deploy), ("added", true, true));
}

#[test]
fn autoscaling_keeps_its_replicas() {
    let mut ex = imported(FILE);
    ex[0].spec["autoscale"] = json!({ "min": 2, "max": 6, "cpuPercent": 70 });
    let next = FILE.replace("replicas: 3", "replicas: 5");
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(api.status, "unchanged");
    assert!(api.notes.iter().any(|n| n.field == "replicas"));
}

#[test]
fn placement_and_resources_follow_the_file() {
    let ex = imported(FILE);
    let next = FILE.replace(
        "      replicas: 3",
        "      replicas: 3\n      placement: { tags: [edge] }",
    );
    let next = next.replace(
        "    ports: [\"8080:8080\"]",
        "    ports: [\"8080:8080\"]\n    deploy: { resources: { limits: { memory: 2G } } }",
    );
    let p = sync(&next, &ex);
    let api = service(&p, "cloud-api");
    assert_eq!(fields(api), vec!["memory limit", "placement"]);
    assert_eq!(api.changes[1].to.as_deref(), Some("tag edge"));
    let m = merge(&ex[0], &p.file.services[0], "compose.yaml", 2, &|p| p.to_owned());
    assert_eq!(m.spec["placement"], json!({ "servers": [], "tags": ["edge"] }));
    assert_eq!(m.spec["resources"]["memoryLimit"], 2048);
    // The cpu limit isn't in the file: it stays.
    assert_eq!(m.spec["resources"]["cpuLimit"], ex[0].spec["resources"]["cpuLimit"]);
}

#[test]
fn a_linked_project_matches_by_name_and_starts_remembering() {
    // Made by hand, then synced: matched by name, nothing else changes.
    let mut ex = imported(FILE);
    for e in &mut ex {
        e.spec.as_object_mut().unwrap().remove("compose");
    }
    let p = sync(FILE, &ex);
    for s in &p.services {
        assert_eq!(s.status, "unchanged", "{}", s.name);
        assert!(s.service_id.is_some());
    }
    let m = merge(&ex[0], &p.file.services[0], "compose.yaml", 2, &|p| p.to_owned());
    assert!(m.record_only);
    assert_eq!(m.spec["compose"]["service"], "cloud-api");
}

#[test]
fn secrets_go_back_blank() {
    let mut spec = json!({ "env": [
        { "key": "A", "value": "sealed:x", "secret": true },
        { "key": "B", "value": "plain", "secret": false },
    ] });
    blank_secrets(&mut spec);
    assert_eq!(spec["env"][0]["value"], "");
    assert_eq!(spec["env"][1]["value"], "plain");
    assert_eq!(volume_label("db", "db-data"), "data");
    assert_eq!(volume_label("db", "uploads"), "uploads");
    let _ = Value::Null;
}

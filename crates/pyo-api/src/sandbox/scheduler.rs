//! Picks the sandbox server a new sandbox runs on, from the servers'
//! latest reports. Pure, so it's tested without servers.
//!
//! A server qualifies when it's ready, turned on, reported recently, runs
//! what the sandbox's `isolation` needs, has networking when the sandbox
//! wants the internet, has its template, and has the
//! memory: its headroom (available memory less the host reserve) less what
//! Pyo placed there since the report. Of those, in order:
//! 1. Firecracker before gVisor (`isolation: auto` tries Firecracker first);
//! 2. a snapshot of the template at this size, so it restores in
//!    milliseconds instead of booting;
//! 3. the most headroom left.

use pyo_proto::sandbox::{
    CapabilityReport, CapacityReport, Isolation, NetworkMode, Runtime, SandboxSpec, TemplateReport,
    TemplateState,
};

/// A report older than this doesn't count: the server may be gone.
pub const FRESH_SECS: i64 = 90;

/// A sandbox server as the scheduler sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub enabled: bool,
    pub ready: bool,
    /// Seconds since its last report.
    pub report_age_secs: i64,
    pub capability: CapabilityReport,
    pub capacity: CapacityReport,
    pub templates: Vec<TemplateReport>,
    /// Memory of sandboxes Pyo placed here since the report, in bytes.
    pub pending_bytes: u64,
}

impl Candidate {
    fn headroom(&self) -> u64 {
        self.capacity.headroom().saturating_sub(self.pending_bytes)
    }
}

/// Why no server fits, from the closest miss. Shown to people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoServer {
    /// The team has no sandbox server it can use.
    None,
    /// Servers exist but none is up.
    NoneUp,
    /// None runs Firecracker, and `isolation: vm` needs it.
    NoVm,
    /// None has networking, and the sandbox wants the internet.
    NoNetwork,
    /// None has the template yet.
    NoTemplate(String),
    /// None has the memory left.
    Full,
}

impl NoServer {
    pub fn message(&self) -> String {
        match self {
            NoServer::None => "Your team has no sandbox server yet. Add one first.".into(),
            NoServer::NoneUp => {
                "No sandbox server is up right now. Check your sandbox servers.".into()
            }
            NoServer::NoVm => {
                "No sandbox server has KVM, so isolation \"vm\" can't run. Use \"auto\".".into()
            }
            NoServer::NoNetwork => {
                "No sandbox server has networking set up. Create the sandbox with network set to none.".into()
            }
            NoServer::NoTemplate(t) => format!("No sandbox server has the template {t} yet."),
            NoServer::Full => {
                "Every sandbox server is full. Try again soon, or add a server.".into()
            }
        }
    }

    /// 503 for what passes on its own, 409 for what needs a change.
    pub fn status(&self) -> axum::http::StatusCode {
        match self {
            NoServer::Full | NoServer::NoneUp => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => axum::http::StatusCode::CONFLICT,
        }
    }
}

/// The best server for `spec`, whose template servers keep as
/// `spec.template`.
pub fn pick<'a>(servers: &'a [Candidate], spec: &SandboxSpec) -> Result<&'a Candidate, NoServer> {
    if servers.is_empty() {
        return Err(NoServer::None);
    }
    let up: Vec<&Candidate> = servers
        .iter()
        .filter(|c| {
            c.enabled
                && c.ready
                && c.report_age_secs <= FRESH_SECS
                && c.capability.runtime != Runtime::None
        })
        .collect();
    if up.is_empty() {
        return Err(NoServer::NoneUp);
    }
    let isolated: Vec<&Candidate> = up
        .into_iter()
        .filter(|c| c.capability.allows(spec.isolation))
        .collect();
    if isolated.is_empty() {
        return Err(if spec.isolation == Isolation::Vm {
            NoServer::NoVm
        } else {
            NoServer::NoneUp
        });
    }
    let networked: Vec<&Candidate> = isolated
        .into_iter()
        .filter(|c| spec.network == NetworkMode::None || c.capability.network)
        .collect();
    if networked.is_empty() {
        return Err(NoServer::NoNetwork);
    }
    let with_template: Vec<&Candidate> = networked
        .into_iter()
        .filter(|c| {
            c.templates
                .iter()
                .any(|t| t.name == spec.template && t.state == TemplateState::Ready)
        })
        .collect();
    if with_template.is_empty() {
        return Err(NoServer::NoTemplate(spec.template.clone()));
    }
    let need = (spec.memory_mib as u64) << 20;
    let snapshot = |c: &Candidate| {
        c.templates.iter().any(|t| {
            t.name == spec.template
                && t.has_snapshot(spec.network, spec.cpus, spec.memory_mib, spec.disk_mib)
        })
    };
    with_template
        .into_iter()
        .filter(|c| c.headroom() >= need)
        .max_by_key(|c| {
            (
                c.capability.runtime == Runtime::Firecracker,
                snapshot(c),
                c.headroom(),
                // Ties go to the same server every time.
                std::cmp::Reverse(c.id.clone()),
            )
        })
        .ok_or(NoServer::Full)
}

/// [`pick`] for a template that may have revisions (template `id`, kept
/// as `spec.template` now): a server with the current revision ready,
/// else one that still has an older one ready, while the new one is made.
/// Returns the server and the name of the revision it starts from.
pub fn pick_revision<'a>(
    servers: &'a [Candidate],
    spec: &SandboxSpec,
    id: &str,
) -> Result<(&'a Candidate, String), NoServer> {
    let first = match pick(servers, spec) {
        Ok(c) => return Ok((c, spec.template.clone())),
        Err(e) => e,
    };
    if !matches!(first, NoServer::NoTemplate(_)) {
        return Err(first);
    }
    let mut older: Vec<&str> = servers
        .iter()
        .flat_map(|c| super::templates::older_ready(&c.templates, id, &spec.template))
        .collect();
    older.sort_unstable();
    older.dedup();
    let mut why = first;
    for name in older {
        let mut s = spec.clone();
        s.template = name.to_owned();
        match pick(servers, &s) {
            Ok(c) => return Ok((c, s.template)),
            // Say why the older one can't run it either (full, say).
            Err(e) if !matches!(e, NoServer::NoTemplate(_)) => why = e,
            Err(_) => {}
        }
    }
    Err(why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::{Kvm, snapshot_size_tag};

    const GIB: u64 = 1 << 30;

    fn server(id: &str, runtime: Runtime, free_gib: u64) -> Candidate {
        Candidate {
            id: id.into(),
            enabled: true,
            ready: true,
            report_age_secs: 5,
            capability: CapabilityReport {
                runtime,
                kvm: if runtime == Runtime::Firecracker {
                    Kvm::Native
                } else {
                    Kvm::None
                },
                network: true,
                ..Default::default()
            },
            capacity: CapacityReport {
                memory_total: 32 * GIB,
                memory_available: free_gib * GIB + GIB,
                memory_reserved: GIB,
                ..Default::default()
            },
            templates: vec![TemplateReport {
                name: "stp_node".into(),
                ..Default::default()
            }],
            pending_bytes: 0,
        }
    }

    fn spec() -> SandboxSpec {
        let mut s = SandboxSpec::new("sbx_abcdefghijklmnop", "tm_one", "stp_node");
        s.memory_mib = 1024;
        s
    }

    fn pick_id(servers: &[Candidate], spec: &SandboxSpec) -> Result<String, NoServer> {
        pick(servers, spec).map(|c| c.id.clone())
    }

    #[test]
    fn firecracker_first_then_snapshots_then_room() {
        let s = vec![
            server("gv", Runtime::Gvisor, 30),
            server("fc-small", Runtime::Firecracker, 4),
            server("fc-big", Runtime::Firecracker, 8),
        ];
        assert_eq!(pick_id(&s, &spec()).unwrap(), "fc-big");
        let mut s2 = s.clone();
        s2[1].templates[0].snapshots = vec![format!(
            "fc1.18.0-none-net{}",
            snapshot_size_tag(1, 1024, 2048)
        )];
        assert_eq!(
            pick_id(&s2, &spec()).unwrap(),
            "fc-small",
            "a snapshot beats room"
        );
        // A snapshot of another size doesn't count.
        let mut other = spec();
        other.cpus = 2;
        assert_eq!(pick_id(&s2, &other).unwrap(), "fc-big");
        // Only gVisor left.
        let only_gv = vec![server("gv", Runtime::Gvisor, 30)];
        assert_eq!(pick_id(&only_gv, &spec()).unwrap(), "gv");
    }

    #[test]
    fn vm_isolation_needs_firecracker() {
        let mut s = spec();
        s.isolation = Isolation::Vm;
        let only_gv = vec![server("gv", Runtime::Gvisor, 30)];
        assert_eq!(pick_id(&only_gv, &s), Err(NoServer::NoVm));
        let both = vec![
            server("gv", Runtime::Gvisor, 30),
            server("fc", Runtime::Firecracker, 2),
        ];
        assert_eq!(pick_id(&both, &s).unwrap(), "fc");
    }

    #[test]
    fn headroom_counts_what_was_placed_since_the_report() {
        let mut s = vec![
            server("a", Runtime::Firecracker, 2),
            server("b", Runtime::Firecracker, 3),
        ];
        assert_eq!(pick_id(&s, &spec()).unwrap(), "b");
        s[1].pending_bytes = 2 * GIB + 1;
        assert_eq!(pick_id(&s, &spec()).unwrap(), "a");
        s[0].pending_bytes = 2 * GIB;
        assert_eq!(pick_id(&s, &spec()), Err(NoServer::Full));
        // The reserve is never handed out.
        let mut tight = server("t", Runtime::Firecracker, 0);
        tight.capacity.memory_available = GIB + (1023 << 20);
        assert_eq!(pick_id(&[tight], &spec()), Err(NoServer::Full));
    }

    #[test]
    fn servers_that_cant_take_it_are_skipped() {
        let mut off = server("off", Runtime::Firecracker, 30);
        off.enabled = false;
        let mut stale = server("stale", Runtime::Firecracker, 30);
        stale.report_age_secs = FRESH_SECS + 1;
        let mut joining = server("joining", Runtime::Firecracker, 30);
        joining.ready = false;
        let none = server("none", Runtime::None, 30);
        let all = vec![off, stale, joining, none];
        assert_eq!(pick_id(&all, &spec()), Err(NoServer::NoneUp));
        assert_eq!(pick_id(&[], &spec()), Err(NoServer::None));
        let mut s = all.clone();
        s.push(server("ok", Runtime::Firecracker, 2));
        assert_eq!(pick_id(&s, &spec()).unwrap(), "ok");
    }

    #[test]
    fn the_template_must_be_on_the_server() {
        let mut s = spec();
        s.template = "stp_python".into();
        let servers = vec![server("a", Runtime::Firecracker, 8)];
        assert_eq!(
            pick_id(&servers, &s),
            Err(NoServer::NoTemplate("stp_python".into()))
        );
    }

    #[test]
    fn only_a_ready_template_counts() {
        let mut building = server("building", Runtime::Firecracker, 16);
        building.templates[0].state = TemplateState::Building;
        let mut failed = server("failed", Runtime::Firecracker, 16);
        failed.templates[0].state = TemplateState::Failed;
        assert_eq!(
            pick_id(&[building.clone(), failed], &spec()),
            Err(NoServer::NoTemplate("stp_node".into()))
        );
        let ready = server("ready", Runtime::Firecracker, 2);
        assert_eq!(pick_id(&[building, ready], &spec()).unwrap(), "ready");
    }

    #[test]
    fn new_sandboxes_move_to_a_new_revision_once_it_is_ready() {
        let named = |id: &str, name: &str, state: TemplateState| {
            let mut c = server(id, Runtime::Firecracker, 8);
            c.templates[0].name = name.into();
            c.templates[0].state = state;
            c
        };
        let mut s = spec();
        s.template = "stp_base.new".into();
        // Only the old one is ready: it's used.
        let mut a = named("a", "stp_base.old", TemplateState::Ready);
        a.templates.push(TemplateReport {
            name: "stp_base.new".into(),
            state: TemplateState::Building,
            ..Default::default()
        });
        let (c, name) = pick_revision(std::slice::from_ref(&a), &s, "stp_base").unwrap();
        assert_eq!((c.id.as_str(), name.as_str()), ("a", "stp_base.old"));
        // Once a server has the new one ready, it's preferred.
        let b = named("b", "stp_base.new", TemplateState::Ready);
        let both = [a.clone(), b];
        let (c, name) = pick_revision(&both, &s, "stp_base").unwrap();
        assert_eq!((c.id.as_str(), name.as_str()), ("b", "stp_base.new"));
        // Another template's revisions don't count.
        let other = named("c", "stp_basement.x", TemplateState::Ready);
        assert_eq!(
            pick_revision(&[other], &s, "stp_base").map(|(c, _)| c.id.clone()),
            Err(NoServer::NoTemplate("stp_base.new".into()))
        );
        // The old one's server is full: that's the reason given.
        let mut full = named("a", "stp_base.old", TemplateState::Ready);
        full.capacity.memory_available = 0;
        assert_eq!(
            pick_revision(&[full], &s, "stp_base").map(|(c, _)| c.id.clone()),
            Err(NoServer::Full)
        );
    }

    #[test]
    fn internet_needs_a_server_with_networking() {
        let mut off = server("off", Runtime::Firecracker, 8);
        off.capability.network = false;
        assert_eq!(pick_id(&[off.clone()], &spec()), Err(NoServer::NoNetwork));
        let mut s = spec();
        s.network = NetworkMode::None;
        assert_eq!(pick_id(&[off.clone()], &s).unwrap(), "off");
        let on = server("on", Runtime::Firecracker, 2);
        assert_eq!(pick_id(&[off, on], &spec()).unwrap(), "on");
    }

    #[test]
    fn ties_are_stable() {
        let s = vec![
            server("b", Runtime::Firecracker, 4),
            server("a", Runtime::Firecracker, 4),
        ];
        assert_eq!(pick_id(&s, &spec()).unwrap(), "a");
    }

    #[test]
    fn messages_are_plain() {
        for n in [
            NoServer::None,
            NoServer::NoneUp,
            NoServer::NoVm,
            NoServer::NoNetwork,
            NoServer::NoTemplate("x".into()),
            NoServer::Full,
        ] {
            let m = n.message();
            assert!(
                m.ends_with('.') && !m.contains(';') && !m.contains("node"),
                "{m}"
            );
        }
    }
}

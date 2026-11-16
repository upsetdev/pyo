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
//!
//! A template made on first use (Pyo's `base`) may also go to a server
//! that doesn't have it yet, which makes it first ([`place`]). One already
//! making it comes first, so two creates share one build.

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
    pub name: String,
    /// None for the installation's, which every team may use.
    pub team_id: Option<String>,
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

    /// Why the one server asked for (`server`, by name) can't take a
    /// sandbox of `memory_mib`.
    pub fn message_on(&self, server: &str, memory_mib: u32) -> String {
        match self {
            NoServer::None | NoServer::NoneUp => {
                format!("{server} can't take sandboxes right now. Pick another server.")
            }
            NoServer::NoVm => {
                format!("{server} has no KVM, so isolation \"vm\" can't run there.")
            }
            NoServer::NoNetwork => {
                format!("{server} has no networking. Pick another server, or set network to none.")
            }
            NoServer::NoTemplate(t) => {
                format!("{server} doesn't have the template {t} yet. Pick another server.")
            }
            NoServer::Full => format!(
                "{server} has no room for {}. Pick another server or a smaller size.",
                size(memory_mib)
            ),
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

/// "512 MB", "2 GB", "1.5 GB".
fn size(mib: u32) -> String {
    if mib >= 1024 {
        let gb = (mib as f64 / 1024.0 * 10.0).round() / 10.0;
        format!("{gb} GB")
    } else {
        format!("{mib} MB")
    }
}

/// The servers up and able to run `spec`, whatever their templates.
fn usable<'a>(
    servers: &'a [Candidate],
    spec: &SandboxSpec,
) -> Result<Vec<&'a Candidate>, NoServer> {
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
    Ok(networked)
}

/// The best server for `spec`, whose template servers keep as
/// `spec.template`.
pub fn pick<'a>(servers: &'a [Candidate], spec: &SandboxSpec) -> Result<&'a Candidate, NoServer> {
    let with_template: Vec<&Candidate> = usable(servers, spec)?
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

/// Where a new sandbox goes, and what it starts from there.
#[derive(Debug, Clone)]
pub struct Placement<'a> {
    pub server: &'a Candidate,
    /// The name the server keeps the template under.
    pub template: String,
    /// The server makes the template first: it doesn't have it ready.
    pub build: bool,
}

/// [`pick_revision`], and for a template made on first use (`on_demand`)
/// that no server has ready: a server with room, which makes it first.
/// One already making it comes first, then Firecracker, then the most
/// headroom.
pub fn place<'a>(
    servers: &'a [Candidate],
    spec: &SandboxSpec,
    id: &str,
    on_demand: bool,
) -> Result<Placement<'a>, NoServer> {
    let why = match pick_revision(servers, spec, id) {
        Ok((server, template)) => {
            return Ok(Placement {
                server,
                template,
                build: false,
            });
        }
        Err(e) => e,
    };
    if !on_demand || !matches!(why, NoServer::NoTemplate(_)) {
        return Err(why);
    }
    let need = (spec.memory_mib as u64) << 20;
    let building = |c: &Candidate| {
        c.templates
            .iter()
            .any(|t| t.name == spec.template && t.state == TemplateState::Building)
    };
    usable(servers, spec)?
        .into_iter()
        .filter(|c| c.headroom() >= need)
        .max_by_key(|c| {
            (
                building(c),
                c.capability.runtime == Runtime::Firecracker,
                c.headroom(),
                std::cmp::Reverse(c.id.clone()),
            )
        })
        .map(|server| Placement {
            server,
            template: spec.template.clone(),
            build: true,
        })
        .ok_or(NoServer::Full)
}

/// [`place`] on one server the create asked for, by its id or name: one of
/// `team`'s or the installation's. The error says why it can't, plainly.
pub fn place_on<'a>(
    servers: &'a [Candidate],
    team: &str,
    asked: &str,
    spec: &SandboxSpec,
    id: &str,
    on_demand: bool,
) -> Result<Placement<'a>, (axum::http::StatusCode, String)> {
    let asked = asked.trim();
    let server = servers
        .iter()
        .filter(|c| c.team_id.as_deref().is_none_or(|t| t == team))
        .find(|c| c.id == asked || c.name == asked)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("No sandbox server {asked} in this team."),
            )
        })?;
    place(std::slice::from_ref(server), spec, id, on_demand)
        .map_err(|n| (n.status(), n.message_on(&server.name, spec.memory_mib)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::{Kvm, snapshot_size_tag};

    const GIB: u64 = 1 << 30;

    fn server(id: &str, runtime: Runtime, free_gib: u64) -> Candidate {
        Candidate {
            id: id.into(),
            name: format!("{id}-name"),
            team_id: None,
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
    fn base_goes_to_a_server_without_it_which_makes_it_first() {
        let mut s = spec();
        s.template = "stp_base.new".into();
        let servers = vec![
            server("small", Runtime::Firecracker, 2),
            server("big", Runtime::Firecracker, 8),
            server("gv", Runtime::Gvisor, 30),
        ];
        // A team's template isn't made on first use.
        assert_eq!(
            place(&servers, &s, "stp_base", false).map(|p| p.server.id.clone()),
            Err(NoServer::NoTemplate("stp_base.new".into()))
        );
        let p = place(&servers, &s, "stp_base", true).unwrap();
        assert_eq!(
            (p.server.id.as_str(), p.template.as_str(), p.build),
            ("big", "stp_base.new", true),
            "Firecracker, then room"
        );
        // A server that has it ready wins, with no build.
        let mut has = server("has", Runtime::Firecracker, 2);
        has.templates[0].name = "stp_base.new".into();
        let mut with = servers.clone();
        with.push(has);
        let p = place(&with, &s, "stp_base", true).unwrap();
        assert_eq!((p.server.id.as_str(), p.build), ("has", false));
        // So does one with an older revision, while the new one is made.
        let mut old = server("old", Runtime::Firecracker, 2);
        old.templates[0].name = "stp_base.old".into();
        let mut with = servers.clone();
        with.push(old);
        let p = place(&with, &s, "stp_base", true).unwrap();
        assert_eq!(
            (p.server.id.as_str(), p.template.as_str(), p.build),
            ("old", "stp_base.old", false)
        );
        // No room anywhere.
        let mut full = servers.clone();
        for c in &mut full {
            c.capacity.memory_available = 0;
        }
        assert_eq!(
            place(&full, &s, "stp_base", true).map(|p| p.server.id.clone()),
            Err(NoServer::Full)
        );
        // The usual reasons still count.
        let mut vm = s.clone();
        vm.isolation = Isolation::Vm;
        assert_eq!(
            place(&[server("gv", Runtime::Gvisor, 30)], &vm, "stp_base", true)
                .map(|p| p.server.id.clone()),
            Err(NoServer::NoVm)
        );
        assert_eq!(
            place(&[], &s, "stp_base", true).map(|p| p.server.id.clone()),
            Err(NoServer::None)
        );
    }

    #[test]
    fn two_creates_share_one_build() {
        let mut s = spec();
        s.template = "stp_base.new".into();
        // The first create set "small" making it. The second goes there
        // too, though "big" has more room.
        let mut small = server("small", Runtime::Firecracker, 3);
        small.templates[0] = TemplateReport {
            name: "stp_base.new".into(),
            state: TemplateState::Building,
            ..Default::default()
        };
        small.pending_bytes = 1 << 30;
        let mut servers = vec![small, server("big", Runtime::Firecracker, 8)];
        let p = place(&servers, &s, "stp_base", true).unwrap();
        assert_eq!((p.server.id.as_str(), p.build), ("small", true));
        // Unless it has no room left: then another server makes it.
        servers[0].pending_bytes = 3 << 30;
        let p = place(&servers, &s, "stp_base", true).unwrap();
        assert_eq!((p.server.id.as_str(), p.build), ("big", true));
        // A failed build is tried again.
        let mut failed = server("failed", Runtime::Firecracker, 8);
        failed.templates[0] = TemplateReport {
            name: "stp_base.new".into(),
            state: TemplateState::Failed,
            ..Default::default()
        };
        let p = place(std::slice::from_ref(&failed), &s, "stp_base", true).unwrap();
        assert_eq!((p.server.id.as_str(), p.build), ("failed", true));
    }

    #[test]
    fn a_server_asked_for_takes_it_or_says_why() {
        use axum::http::StatusCode;
        let mut s = spec();
        let mut other_team = server("theirs", Runtime::Firecracker, 30);
        other_team.team_id = Some("tm_two".into());
        let mut ours = server("ours", Runtime::Firecracker, 2);
        ours.team_id = Some("tm_one".into());
        let mut joining = server("joining", Runtime::Firecracker, 30);
        joining.ready = false;
        let installation = server("shared", Runtime::Gvisor, 30);
        let servers = vec![other_team, ours, joining, installation];
        let on = |asked: &str, s: &SandboxSpec, on_demand: bool| {
            place_on(&servers, "tm_one", asked, s, "stp_node", on_demand)
                .map(|p| (p.server.id.clone(), p.build))
        };
        // By id or name, the team's own or the installation's, even when
        // another has more room.
        assert_eq!(on("ours", &s, false), Ok(("ours".into(), false)));
        assert_eq!(on("ours-name", &s, false), Ok(("ours".into(), false)));
        assert_eq!(on(" shared ", &s, false), Ok(("shared".into(), false)));
        // Another team's is as good as missing.
        assert_eq!(
            on("theirs", &s, false),
            Err((
                StatusCode::NOT_FOUND,
                "No sandbox server theirs in this team.".into()
            ))
        );
        // Not ready.
        assert_eq!(
            on("joining-name", &s, false),
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "joining-name can't take sandboxes right now. Pick another server.".into()
            ))
        );
        // No room.
        s.memory_mib = 3072;
        assert_eq!(
            on("ours", &s, false),
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "ours-name has no room for 3 GB. Pick another server or a smaller size.".into()
            ))
        );
        s.memory_mib = 1024;
        // No KVM for "vm".
        let mut vm = s.clone();
        vm.isolation = Isolation::Vm;
        assert_eq!(
            on("shared", &vm, false).unwrap_err().1,
            "shared-name has no KVM, so isolation \"vm\" can't run there."
        );
        // A server without base makes it first. A team template it lacks
        // is refused.
        let mut base = s.clone();
        base.template = "stp_base.new".into();
        let p = place_on(&servers, "tm_one", "ours", &base, "stp_base", true).unwrap();
        assert_eq!((p.server.id.as_str(), p.build), ("ours", true));
        assert_eq!(
            place_on(&servers, "tm_one", "ours", &base, "stp_base", false)
                .unwrap_err()
                .1,
            "ours-name doesn't have the template stp_base.new yet. Pick another server."
        );
        assert_eq!(size(512), "512 MB");
        assert_eq!(size(1536), "1.5 GB");
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

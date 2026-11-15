//! Host memory, measured the way the spike showed is honest.
//!
//! PSS misses about two thirds of what an idle restored VM costs the host,
//! mostly kernel slab. A VM's cgroup `memory.current` counts kernel memory
//! too, so that's what the daemon reads. Under the jailer each VM has its
//! own cgroup. In dev mode there is none, so it falls back to the VM
//! process's RSS and says so.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
}

/// MemTotal and MemAvailable, in bytes.
pub fn parse_meminfo(text: &str) -> MemInfo {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    };
    MemInfo {
        total: field("MemTotal:"),
        available: field("MemAvailable:"),
    }
}

pub fn meminfo() -> MemInfo {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

/// Where a VM's memory is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySource {
    /// `<cgroup root>/<parent>/<id>`, made by the jailer.
    Cgroup(PathBuf),
    /// The VM process, in dev mode.
    Process(u32),
}

impl MemorySource {
    pub fn for_vm(jailed: bool, cgroup_parent: &str, id: &str, pid: u32) -> MemorySource {
        if jailed {
            MemorySource::Cgroup(Path::new("/sys/fs/cgroup").join(cgroup_parent).join(id))
        } else {
            MemorySource::Process(pid)
        }
    }

    /// Bytes in use now, if they can be read.
    pub fn read(&self) -> Option<u64> {
        match self {
            MemorySource::Cgroup(dir) => std::fs::read_to_string(dir.join("memory.current"))
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            MemorySource::Process(pid) => {
                parse_vm_rss(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
            }
        }
    }
}

/// VmRSS from /proc/<pid>/status, in bytes.
pub fn parse_vm_rss(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// What a new sandbox costs the host right away, before the guest touches
/// its memory. From the spike: about 7 MiB for a restore and about 60 MiB
/// for a cold boot. Rounded up.
pub fn start_cost(from_snapshot: bool) -> u64 {
    if from_snapshot { 16 << 20 } else { 96 << 20 }
}

/// Whether a sandbox asking for `want` bytes fits. Two rules:
/// - The memory sandboxes asked for, this one included, stays within what
///   the host has after the reserve, times the overcommit factor. Guest
///   memory only grows, so 1.0 means no sandbox can ever push the host
///   into its reserve.
/// - The host has room for the start right now without eating the reserve.
pub fn admits(
    mem: MemInfo,
    reserve: u64,
    overcommit: f64,
    committed: u64,
    want: u64,
    from_snapshot: bool,
) -> Result<(), String> {
    let budget = (mem.total.saturating_sub(reserve) as f64 * overcommit.max(0.0)) as u64;
    if committed + want > budget {
        return Err(format!(
            "This server has no room for another {} MiB sandbox. {} of {} MiB are taken.",
            want >> 20,
            committed >> 20,
            budget >> 20
        ));
    }
    if mem.available.saturating_sub(reserve) < start_cost(from_snapshot) {
        return Err("This server is low on free memory right now.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parsing() {
        let m = parse_meminfo(
            "MemTotal:       31000000 kB\nMemFree: 1 kB\nMemAvailable:   20000000 kB\n",
        );
        assert_eq!(m.total, 31_000_000 * 1024);
        assert_eq!(m.available, 20_000_000 * 1024);
        assert_eq!(parse_meminfo(""), MemInfo::default());
        assert!(meminfo().total > 0);
    }

    #[test]
    fn rss_parsing() {
        assert_eq!(
            parse_vm_rss("Name: x\nVmRSS:\t  7168 kB\n"),
            Some(7168 * 1024)
        );
        assert_eq!(parse_vm_rss("Name: x\n"), None);
        let me = MemorySource::Process(std::process::id());
        assert!(me.read().unwrap() > 0);
        let cg = MemorySource::for_vm(true, "pyo-sandbox", "sbx_a", 1);
        assert_eq!(
            cg,
            MemorySource::Cgroup(PathBuf::from("/sys/fs/cgroup/pyo-sandbox/sbx_a"))
        );
    }

    #[test]
    fn admission() {
        let gib = 1u64 << 30;
        let mem = MemInfo {
            total: 8 * gib,
            available: 6 * gib,
        };
        // 8 GiB minus a 1 GiB reserve leaves 7 GiB to hand out.
        assert!(admits(mem, gib, 1.0, 6 * gib, gib, true).is_ok());
        let e = admits(mem, gib, 1.0, 6 * gib + 1, gib, true).unwrap_err();
        assert!(e.contains("no room"), "{e}");
        assert!(!e.contains(';'));
        // Overcommit lets more through.
        assert!(admits(mem, gib, 2.0, 12 * gib, gib, true).is_ok());
        // Low free memory refuses even when the budget allows.
        let tight = MemInfo {
            total: 8 * gib,
            available: gib + (8 << 20),
        };
        assert!(admits(tight, gib, 1.0, 0, gib / 4, true).is_err());
    }
}

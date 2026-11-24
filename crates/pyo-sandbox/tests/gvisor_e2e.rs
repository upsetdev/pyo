//! Sandboxes under gVisor (runsc), end to end. Runs only with
//! `PYO_SANDBOX_GVISOR_E2E=1` as root, so plain `cargo test` skips it. It
//! mounts disks, makes network namespaces and the `pyo_sandbox` nftables
//! table, so run it in a throwaway VM or a CI runner, never on a laptop or a
//! real server:
//!
//! ```text
//! crates/pyo-sandbox/scripts/gvisor-assets.sh   # runsc, checked, into .cache/
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! cargo test -p pyo-sandbox --test gvisor_e2e --no-run
//! # as root:
//! PYO_SANDBOX_GVISOR_E2E=1 PYO_SANDBOX_RUNSC=... PYO_SANDBOX_ENVD=... ./gvisor_e2e-<hash> --nocapture
//! ```
//!
//! The template is the host's static busybox plus envd. Firecracker is
//! given a path that doesn't exist, so detection falls back to gVisor even
//! on a host with KVM, like a cloud server without it would.
//!
//! What it checks: detection picks gVisor and names the runsc release.
//! Exec, env, files, a port through envd, the disk cap and memory from the
//! sandbox cgroup. Internet with the host and cloud metadata blocked, and
//! "no network" with no interface. Pause and resume keep files and
//! processes. A daemon restart adopts running and paused sandboxes. The
//! lifetime ends a sandbox. Delete leaves no mount, namespace or runsc
//! state behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use pyo_proto::sandbox::wire::Cmd;
use pyo_proto::sandbox::{
    ExecRequest, ExecResult, NetworkMode, Runtime, SandboxSpec, SandboxState, sandbox_id_from,
};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::firecracker::Firecracker;
use pyo_sandbox::gvisor::{self, Gvisor};
use pyo_sandbox::manager::Manager;
use pyo_sandbox::network::{LinkKind, NETNS_DIR, Network, NetworkConfig, TABLE, TAP_PREFIX};
use pyo_sandbox::storage::{self, Layout};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TEAM: &str = "team-gvisor";

fn enabled() -> bool {
    if std::env::var("PYO_SANDBOX_GVISOR_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PYO_SANDBOX_GVISOR_E2E=1, as root in a throwaway VM");
        return false;
    }
    if !pyo_sandbox::config::is_root() {
        eprintln!("skipping: runsc needs root");
        return false;
    }
    true
}

fn env_path(var: &str) -> PathBuf {
    let p = PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} isn't set")));
    assert!(p.exists(), "{var}: {} is missing", p.display());
    p
}

fn sh(cmd: &str) -> (bool, String) {
    let out = Command::new("sh").args(["-c", cmd]).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn must(cmd: &str) -> String {
    let (ok, out) = sh(cmd);
    assert!(ok, "{cmd}: {out}");
    out
}

fn host_ip() -> String {
    let out = must("ip -4 -o route get 1.1.1.1");
    out.split_whitespace()
        .skip_while(|w| *w != "src")
        .nth(1)
        .expect("a src address")
        .to_string()
}

fn netns() -> Vec<String> {
    std::fs::read_dir(NETNS_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(TAP_PREFIX))
        .collect()
}

fn mounts_under(dir: &Path) -> Vec<PathBuf> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    gvisor::mounts_below(&gvisor::mount_points(&info), dir)
}

/// A template disk: the host's static busybox and envd.
async fn busybox_template(layout: &Layout, envd: &Path) {
    let dir = layout.root.join("template-src");
    let _ = std::fs::remove_dir_all(&dir);
    for d in [
        "bin", "sbin", "proc", "sys", "dev", "tmp", "run", "root", "mnt", "etc", "www",
    ] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::copy("/usr/bin/busybox", dir.join("bin/busybox")).unwrap();
    let list = Command::new("/usr/bin/busybox")
        .arg("--list")
        .output()
        .unwrap();
    for applet in String::from_utf8_lossy(&list.stdout).lines() {
        let p = dir.join("bin").join(applet);
        if applet != "busybox" && !p.exists() {
            std::os::unix::fs::symlink("busybox", p).unwrap();
        }
    }
    std::fs::copy(envd, dir.join("sbin/pyo-envd")).unwrap();
    std::fs::write(dir.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n").unwrap();
    std::fs::write(dir.join("www/index.html"), "hello from gvisor\n").unwrap();
    let out = layout.template_rootfs("busybox");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    storage::ext4_from_dir(&dir, &out, 64, "busybox")
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Ends this run's sandboxes and takes its mounts, namespaces and table
/// away, also when the test fails.
struct Cleanup {
    root: PathBuf,
    runsc: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let state = self.root.join("runsc");
        let _ = sh(&format!(
            "for c in $({r} --root={s} list --quiet 2>/dev/null); do {r} --root={s} delete --force $c; done",
            r = self.runsc.display(),
            s = state.display()
        ));
        // Its own thread: a failing test drops this inside the test's
        // runtime, where block_on panics.
        let root = self.root.clone();
        let _ = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(gvisor::unmount_below(&root));
        })
        .join();
        for ns in netns() {
            let _ = sh(&format!("ip netns del {ns}"));
        }
        let _ = sh(&format!(
            "for l in $(ls /sys/class/net | grep '^{TAP_PREFIX}'); do ip link del $l; done"
        ));
        let _ = sh(&format!("nft delete table inet {TABLE}"));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Env {
    cfg: Config,
    layout: Layout,
    fc: Firecracker,
    envd: PathBuf,
}

impl Env {
    /// What the daemon does on start: detect, open the network, adopt.
    async fn daemon(&self) -> Arc<Manager> {
        let mut cap = detect::detect(Inputs {
            fc: &self.fc,
            layout: &self.layout,
            envd: &self.envd,
            runsc: &self.cfg.runsc,
        })
        .await;
        assert_eq!(cap.runtime, Runtime::Gvisor, "{}", cap.summary());
        assert!(
            cap.gvisor_version.starts_with("release-"),
            "{}",
            cap.gvisor_version
        );
        let net = Network::open(self.cfg.network.clone(), LinkKind::Veth, 0, 0)
            .await
            .expect("networking opens");
        cap.network = false;
        let gv = Gvisor::new(
            self.cfg.runsc.clone(),
            self.cfg.root.join("runsc"),
            self.cfg.cgroup_parent.clone(),
        );
        let mgr = Manager::new(self.cfg.clone(), self.layout.clone(), self.fc.clone(), cap)
            .with_network(Some(net))
            .with_gvisor(Some(gv));
        mgr.adopt().await;
        assert!(mgr.capability().network, "networking is on after adopt");
        Arc::new(mgr)
    }
}

fn spec(n: u8, network: NetworkMode) -> SandboxSpec {
    let mut s = SandboxSpec::new(sandbox_id_from([n; 16]), TEAM, "busybox");
    s.memory_mib = 256;
    s.disk_mib = 128;
    s.network = network;
    s.env = BTreeMap::from([("GREETING".into(), "hi".into())]);
    s
}

async fn exec(mgr: &Manager, id: &str, cmd: &str) -> ExecResult {
    mgr.exec(
        id,
        &ExecRequest {
            cmd: Cmd::Shell(cmd.into()),
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
            timeout_ms: Some(30_000),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{cmd}: {}", e.message))
}

async fn ok(mgr: &Manager, id: &str, cmd: &str) -> String {
    let r = exec(mgr, id, cmd).await;
    assert_eq!(r.exit_code, Some(0), "{cmd}: {} {}", r.stdout, r.stderr);
    r.stdout
}

async fn connects(mgr: &Manager, id: &str, addr: &str, port: u16) -> bool {
    let r = exec(mgr, id, &format!("nc -w 3 {addr} {port} </dev/null")).await;
    r.exit_code == Some(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gvisor_end_to_end() {
    if !enabled() {
        return;
    }
    let runsc = env_path("PYO_SANDBOX_RUNSC");
    let envd = env_path("PYO_SANDBOX_ENVD");
    assert!(
        Path::new("/usr/bin/busybox").exists(),
        "the test needs busybox-static"
    );
    let root = std::env::temp_dir().join(format!("pyo-gvisor-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _cleanup = Cleanup {
        root: root.clone(),
        runsc: runsc.clone(),
    };
    let cfg = Config {
        server_id: "srv-gvisor".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        root: root.clone(),
        firecracker: "/nonexistent/firecracker".into(),
        jailer: None,
        kernel: "/nonexistent/vmlinux".into(),
        envd: envd.clone(),
        runsc: runsc.clone(),
        keys: vec![],
        host_reserve_mib: Some(512),
        memory_overcommit: 1.0,
        cpu_template: None,
        jail_uid: 0,
        jail_gid: 0,
        cgroup_parent: "pyo-sandbox-e2e".into(),
        network: NetworkConfig::default(),
        edge: None,
        dev: false,
        outbound: None,
        tailnet_https: None,
    };
    let layout = Layout::new(&root, false);
    layout.create().await.unwrap();
    let fc = Firecracker {
        firecracker: cfg.firecracker.clone(),
        kernel: cfg.kernel.clone(),
        jailer: None,
        cpu_template: None,
        serial: false,
    };
    let env = Env {
        cfg,
        layout: layout.clone(),
        fc,
        envd: envd.clone(),
    };
    busybox_template(&layout, &envd).await;
    let mgr = env.daemon().await;
    assert!(
        mgr.templates().iter().any(|t| t.name == "busybox"),
        "the template shows"
    );

    // ── No network: exec, env, files, the disk cap, memory ──
    let a = spec(1, NetworkMode::None);
    let st = mgr.create(a.clone()).await.unwrap();
    assert_eq!(st.runtime, Runtime::Gvisor);
    assert!(!st.from_snapshot, "gVisor starts cold");
    println!("gvisor cold start: {:?} ms", st.start_ms);
    assert_eq!(ok(&mgr, &a.id, "echo hello").await, "hello\n");
    assert_eq!(ok(&mgr, &a.id, "echo $GREETING").await, "hi\n");
    let r = exec(&mgr, &a.id, "echo oops >&2; exit 3").await;
    assert_eq!((r.exit_code, r.stderr.as_str()), (Some(3), "oops\n"));
    // gVisor's own kernel answers, not the host's.
    let dmesg = ok(&mgr, &a.id, "dmesg 2>/dev/null | head -3; true").await;
    println!("gvisor dmesg: {dmesg}");
    assert_eq!(
        ok(&mgr, &a.id, "ls /sys/class/net | grep -v '^lo$' | wc -l")
            .await
            .trim(),
        "0",
        "no network means no interface"
    );
    {
        let (_, mut c) = mgr.envd(&a.id).await.unwrap();
        c.write_file("/root/note.txt", b"kept", None).await.unwrap();
        assert_eq!(c.read_file("/root/note.txt", 1024).await.unwrap(), b"kept");
    }
    // The template's disk is shared and read-only. Writes land in the
    // sandbox's own layer.
    ok(&mgr, &a.id, "echo changed > /www/index.html").await;
    let tmpl_root = layout.template_dir("busybox").join("root");
    assert_eq!(
        std::fs::read_to_string(tmpl_root.join("www/index.html")).unwrap(),
        "hello from gvisor\n"
    );
    // The disk is 128 MiB, so 200 MiB can't fit.
    let r = exec(
        &mgr,
        &a.id,
        "dd if=/dev/zero of=/root/big bs=1M count=200 2>&1; echo rc=$?",
    )
    .await;
    assert!(
        !r.stdout.contains("rc=0"),
        "the disk cap holds: {}",
        r.stdout
    );
    ok(&mgr, &a.id, "rm -f /root/big").await;
    let used = mgr.status(&a.id).unwrap().memory_used;
    println!("gvisor memory.current: {used:?}");
    assert!(used.is_some_and(|m| m > 0), "memory comes from the cgroup");

    // ── A port, through envd like the edge proxy does it ──
    ok(&mgr, &a.id, "httpd -p 8080 -h /www").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let (_, c) = mgr.envd(&a.id).await.unwrap();
        let mut s = c.port_stream(8080).await.unwrap();
        s.write_all(b"GET /index.html HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut body = String::new();
        tokio::time::timeout(Duration::from_secs(10), s.read_to_string(&mut body))
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("changed"), "{body}");
    }

    // ── Pause and resume: files and processes stay ──
    // No nohup: Ubuntu's busybox-static doesn't have it.
    ok(&mgr, &a.id, "sleep 1000 >/dev/null 2>&1 &").await;
    let st = mgr.pause(&a.id).await.unwrap();
    assert_eq!(st.state, SandboxState::Paused);
    assert!(
        mgr.exec(
            &a.id,
            &ExecRequest {
                cmd: Cmd::Shell("true".into()),
                cwd: None,
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: None
            }
        )
        .await
        .is_err(),
        "a paused sandbox doesn't run commands"
    );
    let st = mgr.resume(&a.id).await.unwrap();
    assert_eq!(st.state, SandboxState::Running);
    println!("gvisor resume: {:?} ms", st.start_ms);
    assert_eq!(ok(&mgr, &a.id, "cat /root/note.txt").await, "kept");
    assert!(
        ok(&mgr, &a.id, "ps | grep -c '[s]leep 1000'").await.trim() != "0",
        "the process survived the pause"
    );

    // ── Internet, with the host and cloud metadata blocked ──
    let b = spec(2, NetworkMode::Internet);
    mgr.create(b.clone()).await.unwrap();
    assert_eq!(
        netns().len(),
        1,
        "one namespace for the one networked sandbox"
    );
    assert!(
        connects(&mgr, &b.id, "1.1.1.1", 53).await,
        "the internet answers"
    );
    let resolv = ok(&mgr, &b.id, "cat /etc/resolv.conf").await;
    assert!(resolv.contains("nameserver"), "{resolv}");
    assert!(
        !connects(&mgr, &b.id, &host_ip(), 22).await,
        "the host is blocked"
    );
    assert!(
        !connects(&mgr, &b.id, "169.254.169.254", 80).await,
        "cloud metadata is blocked"
    );

    // ── A daemon restart adopts the running sandboxes ──
    let paused_c = spec(3, NetworkMode::Internet);
    mgr.create(paused_c.clone()).await.unwrap();
    mgr.pause(&paused_c.id).await.unwrap();
    drop(mgr);
    let mgr = env.daemon().await;
    assert_eq!(ok(&mgr, &a.id, "cat /root/note.txt").await, "kept");
    assert!(connects(&mgr, &b.id, "1.1.1.1", 53).await);
    assert_eq!(
        mgr.status(&paused_c.id).unwrap().state,
        SandboxState::Paused
    );
    mgr.resume(&paused_c.id).await.unwrap();
    assert!(connects(&mgr, &paused_c.id, "1.1.1.1", 53).await);

    // ── The lifetime ends a sandbox ──
    mgr.set_ttl(&paused_c.id, Some(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    mgr.sweep().await;
    assert!(mgr.find(&paused_c.id).is_err(), "its lifetime is over");

    // ── Delete leaves nothing behind ──
    for x in [&a, &b] {
        mgr.delete(&x.id).await.unwrap();
    }
    let sandboxes = layout.sandboxes();
    assert!(
        mounts_under(&sandboxes).is_empty(),
        "{:?}",
        mounts_under(&sandboxes)
    );
    assert!(netns().is_empty(), "{:?}", netns());
    assert!(storage::dir_names(&sandboxes).is_empty());
    mgr.delete_template("busybox").await.unwrap();
    // runsc keeps a null-netns mount in its own state directory, for
    // every sandbox to come. It stays.
    let state = root.join("runsc");
    let left: Vec<_> = mounts_under(&root)
        .into_iter()
        .filter(|m| !m.starts_with(&state))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

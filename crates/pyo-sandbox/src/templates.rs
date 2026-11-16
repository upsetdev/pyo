//! Templates the server makes itself, from an OCI image Pyo names.
//!
//! A build pulls the image ([`crate::oci`]), unpacks its layers into a root
//! disk with `pyo-envd` at `/sbin/pyo-envd` ([`crate::rootfs`]), then boots
//! it once and snapshots it per network mode at the default size. gVisor
//! servers make the same disk and stop there: their sandboxes start cold
//! from it, mounted read-only (see `gvisor`). Pyo only
//! says which image and gives a registry login. It reads the progress from
//! the report:
//!
//! ```text
//! templates/<name>/template.json     state and build record (see TemplateBuild)
//! templates/<name>/build/            layers and the unpacked root, while building
//! templates/<name>/rootfs.ext4       the root disk, once unpacked
//! templates/<name>/snapshots/<key>/  as for any template
//! ```
//!
//! One build runs at a time, so a burst of new templates can't fill the
//! disk at once. A build cut off by a restart is removed on start, and Pyo
//! asks again since the template is gone from the report.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use anyhow::{Context, bail};
use pyo_proto::sandbox::{
    DEFAULT_CPUS, DEFAULT_DISK_MIB, DEFAULT_MEMORY_MIB, DEFAULT_TEMPLATE_MAX_MIB, DaemonError,
    ErrorCode, NetworkMode, RegistryAuth, Runtime, TemplateBuild, TemplateReport, TemplateState,
};
use serde::{Deserialize, Serialize};

use crate::manager::Manager;
use crate::oci::{self, ImageRef};
use crate::rootfs::{self, Unpacker};
use crate::storage;

const RECORD: &str = "template.json";
const BUILD_DIR: &str = "build";
/// Disk kept free on top of what a build is expected to take.
const DISK_MARGIN: u64 = 1 << 30;

type Result<T> = std::result::Result<T, DaemonError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> DaemonError {
    DaemonError::new(code, msg)
}

/// What `template.json` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub state: TemplateState,
    pub build: TemplateBuild,
}

/// A build in progress.
pub struct Build {
    record: StdMutex<TemplateBuild>,
    pulled: AtomicU64,
    cancel: Arc<AtomicBool>,
}

impl Build {
    fn snapshot(&self) -> TemplateBuild {
        let mut b = self
            .record
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        b.pulled = self.pulled.load(Ordering::Relaxed);
        b
    }

    fn set(&self, f: impl FnOnce(&mut TemplateBuild)) {
        f(&mut self.record.lock().unwrap_or_else(|p| p.into_inner()));
    }
}

/// Where a template's files are.
struct Paths {
    dir: PathBuf,
}

impl Paths {
    fn build(&self) -> PathBuf {
        self.dir.join(BUILD_DIR)
    }
    fn rootfs(&self) -> PathBuf {
        self.dir.join("rootfs.ext4")
    }
}

fn name_of(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

pub fn read_record(dir: &Path) -> Option<Record> {
    std::fs::read(dir.join(RECORD))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_record(dir: &Path, r: &Record) {
    let path = dir.join(RECORD);
    let tmp = dir.join(format!(".{RECORD}.tmp"));
    let res = std::fs::write(&tmp, serde_json::to_vec_pretty(r).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        tracing::warn!(error = %e, path = %path.display(), "can't save the template record");
    }
}

/// Bytes a directory takes on disk. Files seen before (hard links) count
/// once.
pub fn disk_bytes(dir: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir()
            && let Ok(rd) = std::fs::read_dir(&p)
        {
            stack.extend(rd.flatten().map(|e| e.path()));
        }
    }
    total
}

impl Manager {
    fn template_paths(&self, name: &str) -> Paths {
        Paths {
            dir: self.layout.template_dir(name),
        }
    }

    /// One template as the report shows it, or None when there's none by
    /// that name.
    pub fn template_report(&self, name: &str) -> Option<TemplateReport> {
        let p = self.template_paths(name);
        let snapshots = storage::dir_names(&p.dir.join("snapshots"));
        let bytes = disk_bytes(&p.dir, &mut Default::default());
        if let Some(b) = self.build_of(name) {
            return Some(TemplateReport {
                name: name.into(),
                snapshots,
                state: TemplateState::Building,
                bytes,
                build: Some(b.snapshot()),
            });
        }
        if let Some(r) = read_record(&p.dir) {
            return Some(TemplateReport {
                name: name.into(),
                // A failed build's snapshots don't count.
                snapshots: if r.state == TemplateState::Ready {
                    snapshots
                } else {
                    Vec::new()
                },
                state: r.state,
                bytes,
                build: Some(r.build),
            });
        }
        // Put there by hand.
        p.rootfs().exists().then(|| TemplateReport {
            name: name.into(),
            snapshots,
            state: TemplateState::Ready,
            bytes,
            build: None,
        })
    }

    fn build_of(&self, name: &str) -> Option<Arc<Build>> {
        self.builds
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    /// Starts making a template from an image. A template the server has
    /// already, ready, failed or building, is left as it is.
    pub fn prepare_template(
        self: &Arc<Self>,
        name: &str,
        image: &str,
        plain_http: bool,
        auth: Option<RegistryAuth>,
        max_mib: Option<u32>,
    ) -> Result<TemplateReport> {
        if !pyo_proto::sandbox::is_template_name(name) {
            return Err(err(
                ErrorCode::InvalidSpec,
                format!("{name:?} isn't a sandbox template name."),
            ));
        }
        let image_ref =
            ImageRef::parse(image).map_err(|e| err(ErrorCode::InvalidSpec, format!("{e:#}")))?;
        if self.capability().runtime == Runtime::None {
            return Err(err(
                ErrorCode::Unsupported,
                "This server can't run sandboxes, so it makes no templates.",
            ));
        }
        // Two asks at once (Pyo's round and a sandbox's first use) start
        // one build: the second finds the first's.
        let _starting = self.starting.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = self.template_report(name) {
            return Ok(r);
        }
        let p = self.template_paths(name);
        std::fs::create_dir_all(&p.dir).map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        let record = TemplateBuild {
            image: image.trim().into(),
            phase: Some("queued".into()),
            started_at: crate::now_ms(),
            ..Default::default()
        };
        write_record(
            &p.dir,
            &Record {
                state: TemplateState::Building,
                build: record.clone(),
            },
        );
        let build = Arc::new(Build {
            record: StdMutex::new(record),
            pulled: AtomicU64::new(0),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.builds
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.into(), build.clone());
        let max_bytes = (max_mib.unwrap_or(DEFAULT_TEMPLATE_MAX_MIB) as u64) << 20;
        let me = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            me.run_build(&name, build, image_ref, plain_http, auth, max_bytes)
                .await;
        });
        Ok(self.template_report(&name_of(&p.dir)).unwrap_or_default())
    }

    async fn run_build(
        self: Arc<Self>,
        name: &str,
        b: Arc<Build>,
        image: ImageRef,
        plain_http: bool,
        auth: Option<RegistryAuth>,
        max_bytes: u64,
    ) {
        let p = self.template_paths(name);
        let res = self
            .build_steps(name, &p, &b, image, plain_http, auth, max_bytes)
            .await;
        let _ = tokio::fs::remove_dir_all(p.build()).await;
        let cancelled = b.cancel.load(Ordering::Relaxed);
        b.set(|r| {
            r.phase = None;
            r.finished_at = Some(crate::now_ms());
        });
        let state = match &res {
            Ok(()) if !cancelled => TemplateState::Ready,
            _ => TemplateState::Failed,
        };
        if let Err(e) = &res {
            let msg = format!("{e:#}");
            tracing::warn!(template = name, error = %msg, "template build failed");
            b.set(|r| r.error = Some(msg));
            // What's left of it is no use: keep only the record of why.
            let _ = tokio::fs::remove_file(p.rootfs()).await;
            let _ = tokio::fs::remove_dir_all(p.dir.join("snapshots")).await;
        }
        if cancelled {
            let _ = tokio::fs::remove_dir_all(&p.dir).await;
        } else {
            write_record(
                &p.dir,
                &Record {
                    state,
                    build: b.snapshot(),
                },
            );
            if state == TemplateState::Ready {
                let r = b.snapshot();
                tracing::info!(
                    template = name,
                    image = %r.image,
                    pull_ms = r.pull_ms,
                    unpack_ms = r.unpack_ms,
                    snapshot_ms = r.snapshot_ms,
                    image_bytes = r.image_bytes,
                    rootfs_bytes = r.rootfs_bytes,
                    "template is ready"
                );
            }
        }
        self.builds
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_steps(
        &self,
        name: &str,
        p: &Paths,
        b: &Build,
        image: ImageRef,
        plain_http: bool,
        auth: Option<RegistryAuth>,
        max_bytes: u64,
    ) -> anyhow::Result<()> {
        let _turn = self.build_gate.acquire().await?;
        let stopped = || -> anyhow::Result<()> {
            if b.cancel.load(Ordering::Relaxed) {
                bail!("The template build was stopped.");
            }
            Ok(())
        };
        stopped()?;

        // Pull.
        b.set(|r| r.phase = Some("pulling".into()));
        let t0 = Instant::now();
        let mut client = oci::Client::new(image, plain_http, auth);
        let img = client.resolve().await?;
        // An image made for Pyo says which contract it was made for. One
        // this server doesn't run would start but not work.
        let labels = client.labels(&img).await?;
        if let Some(why) = labels
            .get(pyo_proto::sandbox::CONTRACT_LABEL)
            .and_then(|l| pyo_proto::sandbox::contract_refusal(l))
        {
            bail!(why);
        }
        let total: u64 = img.layers.iter().map(|l| l.size).sum();
        b.set(|r| {
            r.digest = Some(img.digest.clone());
            r.image_bytes = total;
        });
        if total > max_bytes {
            bail!(
                "The image's layers take {} MiB, more than the {} MiB a template may take.",
                total >> 20,
                max_bytes >> 20
            );
        }
        // Layers, the unpacked files and the disk made from them, roughly.
        let need = total.saturating_mul(6) + DISK_MARGIN;
        let free = storage::free_bytes(&self.layout.root);
        if free < need {
            bail!(
                "The server has {} MiB of disk free, and this image needs about {} MiB to unpack.",
                free >> 20,
                need >> 20
            );
        }
        let layers_dir = p.build().join("layers");
        tokio::fs::create_dir_all(&layers_dir).await?;
        let mut files = Vec::new();
        for (i, l) in img.layers.iter().enumerate() {
            stopped()?;
            let f = layers_dir.join(format!("{i}.blob"));
            client
                .blob(l, &f, &b.pulled)
                .await
                .with_context(|| format!("pulling layer {} of {}", i + 1, img.layers.len()))?;
            files.push((l.media_type.clone(), f));
        }
        b.set(|r| r.pull_ms = Some(t0.elapsed().as_millis() as u64));

        // Unpack into a root disk.
        stopped()?;
        b.set(|r| r.phase = Some("unpacking".into()));
        let t1 = Instant::now();
        let staging = p.build().join("root");
        let out = p.build().join("rootfs.ext4");
        let envd = self.cfg.envd.clone();
        let cancel = b.cancel.clone();
        let out2 = out.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut u = Unpacker::new(&staging, max_bytes, cancel)?;
            for (i, (media, f)) in files.iter().enumerate() {
                let file = std::fs::File::open(f)?;
                let r = rootfs::decompress(media, std::io::BufReader::new(file))?;
                u.apply(r)
                    .with_context(|| format!("unpacking layer {}", i + 1))?;
                let _ = std::fs::remove_file(f);
            }
            u.add_file("sbin/pyo-envd", &envd, 0o755)?;
            for d in rootfs::GUEST_DIRS {
                u.ensure_dir(d)?;
            }
            u.build_ext4(&out2, "pyo-template")?;
            std::fs::remove_dir_all(&staging)?;
            Ok(())
        })
        .await??;
        tokio::fs::rename(&out, p.rootfs()).await?;
        let rootfs_bytes = disk_bytes(&p.rootfs(), &mut Default::default());
        b.set(|r| {
            r.unpack_ms = Some(t1.elapsed().as_millis() as u64);
            r.rootfs_bytes = rootfs_bytes;
        });

        // gVisor starts every sandbox cold from the disk: nothing to snapshot.
        if self.capability().runtime == Runtime::Gvisor {
            return Ok(());
        }

        // Boot it once per network mode and snapshot it.
        stopped()?;
        b.set(|r| r.phase = Some("snapshotting".into()));
        let t2 = Instant::now();
        let mut modes = Vec::new();
        if self.has_network() {
            modes.push(NetworkMode::Internet);
        }
        modes.push(NetworkMode::None);
        let mut errors = Vec::new();
        for m in &modes {
            stopped()?;
            if let Err(e) = self
                .snapshot_template(
                    name,
                    DEFAULT_CPUS,
                    DEFAULT_MEMORY_MIB,
                    DEFAULT_DISK_MIB,
                    Some(*m),
                )
                .await
            {
                tracing::warn!(template = name, network = ?m, error = %e, "template snapshot failed");
                errors.push(e.message);
            }
        }
        b.set(|r| r.snapshot_ms = Some(t2.elapsed().as_millis() as u64));
        if errors.len() == modes.len() {
            bail!(
                "The template didn't boot: {}",
                errors.first().cloned().unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Deletes a template, or stops its build (which then removes it).
    /// Refused while sandboxes use it.
    pub async fn delete_template(&self, name: &str) -> Result<()> {
        if !pyo_proto::sandbox::is_template_name(name) {
            return Err(err(
                ErrorCode::InvalidSpec,
                format!("{name:?} isn't a sandbox template name."),
            ));
        }
        let users = self
            .list()
            .into_iter()
            .filter(|s| s.template == name && !s.state.is_final())
            .count();
        if users > 0 {
            return Err(err(
                ErrorCode::Conflict,
                format!("{users} sandboxes on this server still use the template {name}."),
            ));
        }
        if let Some(b) = self.build_of(name) {
            b.cancel.store(true, Ordering::Relaxed);
            tracing::info!(template = name, "template build stopped");
            return Ok(());
        }
        let dir = self.layout.template_dir(name);
        if !dir.exists() {
            return Err(err(
                ErrorCode::NotFound,
                format!("The sandbox template {name} isn't on this server."),
            ));
        }
        // gVisor servers have its disk mounted.
        crate::gvisor::unmount_below(&dir).await;
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        tracing::info!(template = name, "template deleted");
        Ok(())
    }

    /// On start: builds a previous daemon left halfway are removed, so Pyo
    /// asks for them again.
    pub async fn adopt_templates(&self) {
        for name in storage::dir_names(&self.layout.root.join("templates")) {
            let dir = self.layout.template_dir(&name);
            let Some(r) = read_record(&dir) else {
                continue;
            };
            if r.state == TemplateState::Building {
                tracing::info!(template = %name, "removing a template build cut off by a restart");
                let _ = tokio::fs::remove_dir_all(&dir).await;
            } else {
                let _ = tokio::fs::remove_dir_all(dir.join(BUILD_DIR)).await;
            }
        }
    }
}

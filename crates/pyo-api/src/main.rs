//! The self-hosted Pyo control plane: the library with no extensions.

fn main() -> anyhow::Result<()> {
    tune_malloc();
    // #[tokio::main], with roomier worker stacks: an operation's future is
    // large in a debug build, and 2 MiB isn't always enough for it. (Stacks
    // are reserved, not used: only the pages a thread touches take memory.)
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .thread_stack_size(8 << 20)
        .build()?
        .block_on(pyo_api::run(pyo_api::Extensions::default()))
}

/// The API waits on Postgres and the network far more than it computes, so
/// a few threads are plenty, whatever the server's CPU count (tokio's
/// default is one per CPU). `TOKIO_WORKER_THREADS` sets another number.
fn worker_threads() -> usize {
    std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(4)
        })
}

/// glibc keeps freed memory for reuse, and raises its mmap threshold (up to
/// 32 MiB) whenever a large block is freed: after that, large buffers (a
/// password hash's 19 MiB, a GeoIP download) come from per-thread arenas
/// and stay with the process. A fixed threshold returns them to the system
/// once freed, and two arenas keep the threads' leftovers few. The
/// `MALLOC_*` environment variables still win, being set explicitly.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_malloc() {
    let set = |var: &str, param: libc::c_int, value: libc::c_int| {
        if std::env::var_os(var).is_none() {
            // SAFETY: mallopt only changes malloc's tunables; it runs here
            // before any other thread exists.
            unsafe { libc::mallopt(param, value) };
        }
    };
    set("MALLOC_ARENA_MAX", libc::M_ARENA_MAX, 2);
    set("MALLOC_MMAP_THRESHOLD_", libc::M_MMAP_THRESHOLD, 256 << 10);
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_malloc() {}

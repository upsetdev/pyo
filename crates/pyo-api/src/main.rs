//! The self-hosted Pyo control plane: the library with no extensions.

fn main() -> anyhow::Result<()> {
    // #[tokio::main], with roomier worker stacks: an operation's future is
    // large in a debug build, and 2 MiB isn't always enough for it.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 << 20)
        .build()?
        .block_on(pyo_api::run(pyo_api::Extensions::default()))
}

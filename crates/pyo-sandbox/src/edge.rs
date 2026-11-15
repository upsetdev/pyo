//! TODO(edge): the public HTTPS edge proxy for sandbox ports. Comes after
//! the daemon API, since it needs the wildcard certificate (DNS-01, the
//! Cloudflare DNS work) and ports 80 and 443 as root.
//!
//! The design (PLAN.md "Dedicated sandbox servers"):
//! - Hosts look like `3000-sbx_abc.<domain>`. The proxy parses the port
//!   and the sandbox ID from the first label and checks the sandbox runs
//!   here.
//! - Each request opens a `portConnect` stream to envd and copies bytes
//!   both ways, so no network inside the sandbox is needed for previews.
//! - A port is closed until opened through the API (`ports` scope). An
//!   opened port is public by default or needs a token per request,
//!   chosen when it's opened.
//! - Only 80 and 443 are public on a sandbox server. The daemon API stays
//!   on the tailnet.

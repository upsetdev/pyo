//! Writes Pyo's API reference (OpenAPI 3.1) for the docs:
//!
//!   cargo run -p pyo-api --bin openapi [path]
//!
//! The default path is pyo-docs/openapi/pyo.json beside this repository;
//! `-` prints it instead.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let json = pyo_api::openapi::spec_json();
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../pyo-docs/openapi/pyo.json")
        });
    if path.as_os_str() == "-" {
        print!("{json}");
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, json)?;
    eprintln!("wrote {}", path.canonicalize().unwrap_or(path).display());
    Ok(())
}

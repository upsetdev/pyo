//! A folder sent with `pyo deploy` instead of a Git checkout: a gzipped tar
//! fetched from Pyo, checked against its SHA-256, and unpacked into the
//! build's workspace. Nothing in it may land outside the workspace.

use std::{io::Read, path::Path};

use anyhow::{Context, bail};

/// The most an archive may unpack to, and its most entries.
const MAX_UNPACKED: u64 = 8 << 30;
const MAX_ENTRIES: u64 = 500_000;

/// Unpack `archive` (a .tar.gz) into `dest`: files, folders and symbolic
/// links only, every path inside `dest`. Returns how many files.
pub fn extract(archive: &Path, dest: &Path) -> anyhow::Result<u64> {
    let file = std::fs::File::open(archive).context("couldn't open the archive")?;
    extract_from(file, dest)
}

fn extract_from(r: impl Read, dest: &Path) -> anyhow::Result<u64> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(r));
    tar.set_preserve_permissions(false);
    tar.set_preserve_mtime(false);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(true);
    let (mut files, mut total, mut entries) = (0u64, 0u64, 0u64);
    for entry in tar.entries().context("not a gzipped tar")? {
        let mut entry = entry.context("the archive is damaged")?;
        entries += 1;
        if entries > MAX_ENTRIES {
            bail!("the folder has more than {MAX_ENTRIES} files");
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            continue;
        }
        total += entry.header().size().unwrap_or(0);
        if total > MAX_UNPACKED {
            bail!("the folder unpacks to more than {} GB", MAX_UNPACKED >> 30);
        }
        let path = entry
            .path()
            .context("a file name isn't valid")?
            .into_owned();
        // unpack_in refuses absolute paths and "..", and never writes
        // through a link out of `dest`.
        if !entry
            .unpack_in(dest)
            .with_context(|| format!("couldn't unpack {}", path.display()))?
        {
            bail!("{} points outside the folder", path.display());
        }
        if kind.is_file() {
            files += 1;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(
        build: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>),
    ) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        build(&mut b);
        b.into_inner().unwrap().finish().unwrap()
    }

    fn file(b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>, path: &str, body: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        // Written by hand: the builder refuses "..", so bad names get past it.
        let name = h.as_old_mut().name.as_mut();
        name.fill(0);
        name[..path.len()].copy_from_slice(path.as_bytes());
        h.set_cksum();
        b.append(&h, body).unwrap();
    }

    #[test]
    fn unpacks_files_and_folders() {
        let dir = std::env::temp_dir().join(format!("pyo-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = archive(|b| {
            file(b, "Dockerfile", b"FROM scratch\n");
            file(b, "src/main.rs", b"fn main() {}\n");
        });
        assert_eq!(extract_from(&a[..], &dir).unwrap(), 2);
        assert_eq!(
            std::fs::read(dir.join("src/main.rs")).unwrap(),
            b"fn main() {}\n"
        );

        let bad = archive(|b| file(b, "../escape", b"x"));
        let out = dir.join("inner");
        std::fs::create_dir_all(&out).unwrap();
        assert!(extract_from(&bad[..], &out).is_err());
        assert!(!dir.join("escape").exists());

        assert!(extract_from(&b"not an archive"[..], &out).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

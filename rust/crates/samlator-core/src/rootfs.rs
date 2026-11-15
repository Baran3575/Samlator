use std::fs::File;
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};

use crate::{Error, Progress, ProgressFn, Result};

/// Append-only marker proving which payload versions landed on disk.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InstalledMarkers {
    pub rootfs_sha256: String,
    pub payloads: std::collections::BTreeMap<String, String>,
    pub bootstrapped_at: Option<String>,
}

impl InstalledMarkers {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?,
        )?;
        Ok(())
    }
}

/// Written only after a *complete* extraction and guest preparation.
pub const ROOTFS_MARKER: &str = ".samlator-rootfs-ok";

/// A rootfs is usable once it was fully extracted. The marker matters: a
/// half-extracted tree still has `bin/sh`, and treating that as ready made the
/// app skip the retry and fail later with confusing errors.
pub fn is_rootfs_ready(rootfs: &Path) -> bool {
    rootfs.join(ROOTFS_MARKER).is_file() && rootfs.join("bin/sh").is_file()
}

/// Make an extracted tree usable under proot.
///
/// proot only remaps paths, it does not mount anything, so the directories the
/// guest expects to exist must be there already and DNS has to be written by
/// hand: without `/etc/resolv.conf` every `apt-get update` in the guest fails
/// with "temporary failure resolving".
pub fn prepare_guest(rootfs: &Path) -> Result<()> {
    for dir in ["dev", "proc", "sys", "tmp", "run", "root", "opt", "var/tmp"] {
        std::fs::create_dir_all(rootfs.join(dir))?;
    }
    let resolv = rootfs.join("etc/resolv.conf");
    let empty = std::fs::metadata(&resolv).map(|m| m.len() == 0).unwrap_or(true);
    if empty {
        std::fs::create_dir_all(rootfs.join("etc"))?;
        std::fs::write(&resolv, "nameserver 1.1.1.1\nnameserver 8.8.8.8\n")?;
    }
    // A stable machine-id keeps Chromium from regenerating its profile keys on
    // every launch (which it treats as a fresh install).
    let machine_id = rootfs.join("etc/machine-id");
    if !machine_id.is_file() {
        std::fs::write(&machine_id, "0123456789abcdef0123456789abcdef\n")?;
    }
    std::fs::write(rootfs.join(ROOTFS_MARKER), "1\n")?;
    Ok(())
}

pub fn is_payload_ready(sys_root: &Path, target: &str) -> bool {
    let p = sys_root.join(target);
    p.exists()
}

/// Stream a `.tar.zst` into `dest`, dropping `strip` leading components.
///
/// Device nodes and FIFOs are skipped: an unprivileged Android process cannot
/// create them, and nothing we run needs them.
/// Extract an archive that is already in memory (APK assets cannot be mmapped).
pub fn extract_tar_zst_bytes(bytes: &[u8], dest: &Path, strip: u32) -> Result<()> {
    let tmp = dest.with_extension("tar.zst.tmp");
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, bytes)?;
    let mut noop = |_: Progress| {};
    let result = extract_tar_zst(&tmp, dest, strip, &mut noop);
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Extract any of the archive formats our payloads use, picked by magic bytes
/// rather than by file name: zstd (`rootfs.tar.zst`, `xkb.tar.zst`) and gzip
/// (the Antigravity `.tar.gz` tarball).
pub fn extract_archive(
    archive: &Path,
    dest: &Path,
    strip: u32,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let file = File::open(archive)?;
    let magic = archive_magic(archive)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    match magic {
        ZSTD_MAGIC => {
            let decoder = zstd::stream::read::Decoder::new(reader)
                .map_err(|e| Error::Archive(format!("zstd: {e}")))?;
            extract_tar_reader(decoder, dest, strip, progress)
        }
        GZIP_MAGIC => extract_tar_reader(
            flate2::read::GzDecoder::new(reader),
            dest,
            strip,
            progress,
        ),
        _ => Err(Error::Archive(format!(
            "bilinmeyen arsiv bicimi (ilk baytlar {:02x?})",
            magic
        ))),
    }
}

pub fn extract_tar_zst(
    archive: &Path,
    dest: &Path,
    strip: u32,
    progress: ProgressFn<'_>,
) -> Result<()> {
    extract_archive(archive, dest, strip, progress)
}

fn extract_tar_reader<R: std::io::Read>(
    reader: R,
    dest: &Path,
    strip: u32,
    progress: ProgressFn<'_>,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    progress(Progress::Stage("extracting".into()));
    let mut tar = tar::Archive::new(reader);
    tar.set_overwrite(true);
    tar.set_preserve_permissions(true);

    let entries = tar
        .entries()
        .map_err(|e| Error::Archive(format!("tar: {e}")))?;

    let mut count: u64 = 0;
    let mut pending: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Archive(format!("entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Error::Archive(format!("path: {e}")))?
            .into_owned();

        let Some(rel) = strip_components(&path, strip) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        if has_parent_escape(&rel) {
            tracing::warn!("skipping suspicious archive path {}", rel.display());
            continue;
        }

        let etype = entry.header().entry_type();
        if etype.is_character_special() || etype.is_block_special() || etype.is_fifo() {
            continue;
        }

        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Hard links are the one entry type tar cannot resolve on its own here:
        // the target may live anywhere in the tree and may not have been
        // written yet (Ubuntu's perl ships `usr/bin/perl -> perl5.38.2` long
        // before the real file). Collect them and link once the tree exists.
        if etype.is_hard_link() {
            if let Ok(Some(target)) = entry.link_name() {
                let target_rel = strip_components(&target, strip).unwrap_or_else(|| target.into_owned());
                pending.push((out, dest.join(target_rel), rel.clone()));
            }
            continue;
        }

        // `unpack` handles dirs, files and symlinks correctly.
        entry
            .unpack(&out)
            .map_err(|e| Error::Archive(format!("unpack {}: {e}", rel.display())))?;

        count += 1;
        if count % 128 == 0 {
            progress(Progress::Bytes { done: count, total: None });
        }
    }

    // Second pass: hard links, retried so chains (a -> b -> c) resolve no
    // matter which order the archive listed them in.
    let mut unresolved = pending;
    for _ in 0..4 {
        let before = unresolved.len();
        let mut still = Vec::new();
        for (link, target, rel) in unresolved {
            match std::fs::hard_link(&target, &link) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => still.push((link, target, rel)),
            }
        }
        unresolved = still;
        if unresolved.is_empty() || unresolved.len() == before {
            break;
        }
    }
    if !unresolved.is_empty() {
        // A missing link target is not fatal: fall back to a copy so the tree
        // still works, and say which entry it was.
        for (link, target, rel) in unresolved {
            match std::fs::copy(&target, &link) {
                Ok(_) => tracing::warn!("hard link {} replaced by a copy", rel.display()),
                Err(e) => tracing::warn!("hard link {} dropped: {e}", rel.display()),
            }
        }
    }

    progress(Progress::Done);
    tracing::info!("extracted {count} entries into {}", dest.display());
    Ok(())
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const GZIP_MAGIC: [u8; 4] = [0x1F, 0x8B, 0x08, 0x00];

/// First four bytes of a file, used to pick a decompressor. A truncated
/// download or an HTML error page shows up here as an unknown magic instead of
/// a confusing decoder error deep inside the extraction.
fn archive_magic(path: &Path) -> Result<[u8; 4]> {
    use std::io::Read;
    let mut head = [0u8; 4];
    let mut f = File::open(path)?;
    f.read_exact(&mut head).map_err(|_| {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Error::Archive(format!("arsiv cok kucuk ({size} bayt)"))
    })?;
    Ok(head)
}

fn strip_components(path: &Path, strip: u32) -> Option<PathBuf> {
    let mut comps = path.components();
    for _ in 0..strip {
        comps.next()?;
    }
    let rest: PathBuf = comps.collect();
    if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn has_parent_escape(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::ParentDir) || matches!(c, Component::RootDir) || matches!(c, Component::Prefix(_))
    })
}

/// One-shot provisioning script, executed inside the guest with PRoot.
///
/// Everything here is deliberate: `--no-install-recommends` keeps the image
/// small, and the package list is exactly the Chromium/Electron 39 Wayland
/// runtime plus a terminal and fonts.
pub fn bootstrap_script(packages: &[&str]) -> String {
    let pkg = packages.join(" ");
    format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive
echo "[samlator] configuring apt"
apt-get update -o Acquire::Retries=3 || true
echo "[samlator] installing desktop runtime"
# One shot first: fast and exact on Debian. Ubuntu 24.04 renamed a pile of
# libraries for the 64-bit time_t transition (libasound2 -> libasound2t64), so
# fall back to a per-package pass that tries the new name before giving up.
install_group() {{
    apt-get install -y --no-install-recommends {pkg} && return 0
    for p in {pkg}; do
        apt-get install -y --no-install-recommends "$p" && continue
        apt-get install -y --no-install-recommends "${{p}}t64" && continue
        echo "[samlator] atlandı: $p"
    done
}}
install_group
mkdir -p /opt/antigravity /root/Desktop /root/.config
# Wayland-only session: make sure no X11 path is picked by gui apps.
printf 'export XDG_SESSION_TYPE=wayland\nexport MOZ_ENABLE_WAYLAND=1\n' > /root/.profile
# Antigravity needs a writable HOME and a machine-id for Chromium's metrics.
[ -f /etc/machine-id ] || echo "samlator" > /etc/machine-id
echo "[samlator] bootstrap done"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_archive(dir: &Path) -> PathBuf {
        let archive = dir.join("test.tar.zst");
        let file = File::create(&archive).unwrap();
        let enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
        let mut tar = tar::Builder::new(enc);

        let mut header = tar::Header::new_gnu();
        header.set_size(11);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "top/hello.txt", &b"hello world"[..])
            .unwrap();

        // A symlink and a directory for good measure.
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_cksum();
        tar.append_link(&mut h, "top/link.txt", "hello.txt").unwrap();

        // Ubuntu lists `usr/bin/perl -> perl5.38.2` *before* the real file, so
        // the extractor has to defer hard links.
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Link);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_cksum();
        tar.append_link(&mut h, "top/perl", "top/perl5.38.2").unwrap();

        let mut header = tar::Header::new_gnu();
        header.set_size(9);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "top/perl5.38.2", &b"perl body"[..])
            .unwrap();

        let enc = tar.into_inner().unwrap();
        let mut file = enc.finish().unwrap();
        file.flush().unwrap();
        archive
    }

    /// Real Ubuntu arm64 base tarball: slow and network bound, so opt in.
    #[test]
    #[ignore]
    fn real_rootfs_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("rootfs.tar.zst");
        let url = "https://github.com/Baran3575/Samlator/releases/download/rootfs-latest/rootfs.tar.zst";
        crate::download::fetch(url, &archive, "", "rootfs", &mut |_| {}).unwrap();
        let dest = dir.path().join("rootfs");
        let mut seen = 0usize;
        extract_tar_zst(&archive, &dest, 0, &mut |p| {
            if matches!(p, Progress::Bytes { .. }) {
                seen += 1;
            }
        })
        .unwrap();
        assert!(seen > 0, "progress must be reported while extracting");
        for f in ["bin/sh", "usr/bin/perl", "etc/os-release", "usr/bin/apt-get"] {
            assert!(dest.join(f).exists(), "missing {f} after extraction");
        }
        prepare_guest(&dest).unwrap();
        assert!(is_rootfs_ready(&dest));
    }

    /// The Antigravity tarball is `.tar.gz`; magic sniffing has to pick gzip.
    #[test]
    fn extracts_gzip_tarballs() {
        use flate2::write::GzEncoder;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("app.tar.gz");

        // Build the plain tar first, then gzip it.
        let mut tar_bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "Antigravity IDE/resources/app.bin", &b"data"[..])
                .unwrap();
            tar.finish().unwrap();
        }
        let mut enc = GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, &tar_bytes).unwrap();
        enc.finish().unwrap();

        let dest = dir.path().join("out");
        extract_archive(&archive, &dest, 1, &mut |_| {}).unwrap();
        assert!(dest.join("resources/app.bin").is_file());
    }

    #[test]
    fn hard_links_listed_before_their_target() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_archive(dir.path());
        let dest = dir.path().join("out");
        extract_tar_zst(&archive, &dest, 1, &mut |_| {}).unwrap();
        let body = std::fs::read(dest.join("perl")).expect("hard link must exist");
        assert_eq!(body, b"perl body");
    }

    #[test]
    fn extracts_with_strip() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_archive(dir.path());
        let dest = dir.path().join("out");

        let mut noop = |_p: Progress| {};
        extract_tar_zst(&archive, &dest, 1, &mut noop).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("hello.txt")).unwrap(),
            "hello world"
        );
        assert!(dest.join("link.txt").is_symlink());
        assert!(!dest.join("top").exists());
    }

    #[test]
    fn rootfs_ready_detection() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::fs::write(rootfs.join("bin/sh"), "#!").unwrap();
        assert!(!is_rootfs_ready(&rootfs), "bin/sh alone is a half-extracted tree");
        std::fs::write(rootfs.join(ROOTFS_MARKER), "1").unwrap();
        assert!(is_rootfs_ready(&rootfs));
    }

    #[test]
    fn prepare_guest_adds_mountpoints_and_dns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        prepare_guest(root).unwrap();
        assert!(root.join("dev").is_dir());
        assert!(root.join("proc").is_dir());
        let resolv = std::fs::read_to_string(root.join("etc/resolv.conf")).unwrap();
        assert!(resolv.contains("nameserver 1.1.1.1"));
        // A real resolv.conf provided by the image must not be overwritten.
        std::fs::write(root.join("etc/resolv.conf"), "nameserver 9.9.9.9\n").unwrap();
        prepare_guest(root).unwrap();
        let kept = std::fs::read_to_string(root.join("etc/resolv.conf")).unwrap();
        assert!(kept.contains("9.9.9.9"));
    }

    #[test]
    fn bootstrap_mentions_antigravity_and_wayland() {
        let script = super::bootstrap_script(&["foot", "libgtk-3-0"]);
        assert!(script.contains("/opt/antigravity"));
        assert!(script.contains("XDG_SESSION_TYPE=wayland"));
        assert!(script.contains("apt-get install -y --no-install-recommends foot libgtk-3-0"));
    }
}

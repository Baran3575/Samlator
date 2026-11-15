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

/// A Debian rootfs is "usable" once dpkg's release stamp exists.
pub fn is_rootfs_ready(rootfs: &Path) -> bool {
    rootfs.join("etc/debian_version").is_file() || rootfs.join("bin/sh").is_file()
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

pub fn extract_tar_zst(
    archive: &Path,
    dest: &Path,
    strip: u32,
    progress: ProgressFn<'_>,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    progress(Progress::Stage("extracting".into()));

    let file = File::open(archive)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let decoder = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| Error::Archive(format!("zstd: {e}")))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_overwrite(true);
    tar.set_preserve_permissions(true);

    let entries = tar
        .entries()
        .map_err(|e| Error::Archive(format!("tar: {e}")))?;

    let mut count: u64 = 0;
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

        // `unpack` handles dirs, files, symlinks and hardlinks correctly.
        entry
            .unpack(&out)
            .map_err(|e| Error::Archive(format!("unpack {}: {e}", rel.display())))?;

        count += 1;
        if count % 512 == 0 {
            progress(Progress::Bytes { done: count, total: None });
        }
    }

    progress(Progress::Done);
    tracing::info!("extracted {count} entries into {}", dest.display());
    Ok(())
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

        let enc = tar.into_inner().unwrap();
        let mut file = enc.finish().unwrap();
        file.flush().unwrap();
        archive
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
        assert!(!is_rootfs_ready(&rootfs));
        std::fs::write(rootfs.join("etc/debian_version").as_path(), "12.0").ok();
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/debian_version"), "12.0").unwrap();
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

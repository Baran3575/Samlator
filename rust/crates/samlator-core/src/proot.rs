use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::Paths;

/// Describes one guest invocation.
///
/// Samlator never needs root: PRoot intercepts path-bearing syscalls with
/// `ptrace` and rewrites them, which is exactly enough to fake a `chroot` with
/// ordinary app permissions. No kernel modules, no image files, no VM.
/// Which PRoot implementation the guest command line is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProotKind {
    /// Classic PRoot (Termux lineage): it can load a glibc binary on Android
    /// because it ships its own loader, and it wants `-r path` style options.
    Classic,
    /// The Rust rewrite, whose CLI only knows `--rootfs/--cwd/--bind`.
    Rs,
}

#[derive(Debug, Clone)]
pub struct ProotSpec {
    pub kind: ProotKind,
    pub rootfs: PathBuf,
    pub binds: Vec<(PathBuf, String)>,
    pub cwd: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl ProotSpec {
    pub fn new(paths: &Paths, rootfs: &Path) -> Self {
        // Classic PRoot can actually run a glibc userland on Android 15, so it
        // wins whenever the payload installed it.
        let kind = if paths.classic_proot().is_file() {
            ProotKind::Classic
        } else {
            ProotKind::Rs
        };
        let mut spec = Self {
            kind,
            rootfs: rootfs.to_path_buf(),
            binds: Vec::new(),
            cwd: "/root".to_string(),
            command: Vec::new(),
            env: BTreeMap::new(),
        };
        // Fake /dev, /proc, /sys: the guest gets the app's view of them.
        for d in ["/dev", "/proc", "/sys", "/dev/urandom", "/dev/random", "/dev/zero"] {
            if Path::new(d).exists() {
                spec.binds.push((PathBuf::from(d), d.to_string()));
            }
        }
        // Our runtime dir becomes the guest's /tmp *and* /dev/shm, so the Wayland
        // socket, Electron's shm segments and sockets all live in one place.
        spec.binds.push((paths.runtime_dir(), "/tmp".into()));
        spec.binds.push((paths.tmp(), "/dev/shm".into()));
        spec.binds.push((paths.home(), "/root".into()));

        spec.env
            .insert("XDG_RUNTIME_DIR".into(), "/tmp".into());
        spec.env
            .insert("WAYLAND_DISPLAY".into(), "wayland-0".into());
        spec.env.insert("HOME".into(), "/root".into());
        spec.env
            .insert("XDG_SESSION_TYPE".into(), "wayland".into());
        spec.env.insert("XDG_CURRENT_DESKTOP".into(), "Samlator".into());
        spec.env.insert("LANG".into(), "C.UTF-8".into());
        spec.env.insert("TERM".into(), "xterm-256color".into());
        // Both implementations stage a helper binary in a temp dir; Android has
        // no /tmp, which is a plain ENOENT for the guest exec.
        spec.env
            .insert("TMPDIR".into(), paths.tmp().display().to_string());
        if kind == ProotKind::Classic {
            let base = paths.classic_proot();
            let dir = base
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| paths.base().join("sys/proot"));
            let libexec = dir.join("libexec").join("proot");
            spec.env.insert("PROOT_NO_SECCOMP".into(), "1".into());
            spec.env.insert(
                "PROOT_TMP_DIR".into(),
                dir.join("tmp").display().to_string(),
            );
            spec.env.insert(
                "PROOT_LOADER".into(),
                libexec.join("loader").display().to_string(),
            );
            spec.env.insert(
                "PROOT_LOADER_32".into(),
                libexec.join("loader32").display().to_string(),
            );
            spec.env.insert(
                "LD_LIBRARY_PATH".into(),
                dir.join("lib").display().to_string(),
            );
        }
        spec.env
            .insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/antigravity".into());
        spec.env
            .insert("MOZ_ENABLE_WAYLAND".into(), "1".into());
        // Electron prints its real startup errors only with this on, and the
        // sandbox cannot work under PRoot.
        spec.env
            .insert("ELECTRON_ENABLE_LOGGING".into(), "1".into());
        spec.env
            .insert("ELECTRON_DISABLE_SANDBOX".into(), "1".into());
        spec
    }

    pub fn bind(mut self, host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        self.binds.push((host.into(), guest.into()));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn command(mut self, cmd: Vec<String>) -> Self {
        self.command = cmd;
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Full argv for `proot-rs` (see `scripts/build-proot.sh`).
    pub fn argv(&self, proot: &Path) -> Vec<String> {
        let mut argv = vec![proot.display().to_string()];
        match self.kind {
            ProotKind::Classic => {
                argv.push("-r".into());
                argv.push(self.rootfs.display().to_string());
                argv.push("-w".into());
                argv.push(self.cwd.clone());
                for (host, guest) in &self.binds {
                    argv.push("-b".into());
                    argv.push(format!("{}:{}", host.display(), guest));
                }
                // No setuid in a plain app, so make sure nothing survives us.
                argv.push("--kill-on-exit".into());
            }
            ProotKind::Rs => {
                // proot-rs 0.1.0 knows only -b/--cwd/--rootfs; anything else
                // (e.g. `--kill-on-exit`) makes it refuse to start.
                argv.push("--rootfs".into());
                argv.push(self.rootfs.display().to_string());
                argv.push("--cwd".into());
                argv.push(self.cwd.clone());
                for (host, guest) in &self.binds {
                    argv.push("--bind".into());
                    argv.push(format!("{}:{}", host.display(), guest));
                }
            }
        }
        argv.push("--".into());
        argv.extend(self.command.iter().cloned());
        argv
    }
}

/// Minimum userland we need before the desktop can start.
///
/// Installed with apt inside PRoot on first launch, so the APK stays small and
/// the user only pays for what the app actually pulls.
pub const BOOTSTRAP_PACKAGES: &[&str] = &[
    // Wayland + toolkit stack required by Electron 39/Chromium 142.
    "libgtk-3-0",
    "libnss3",
    "libasound2",
    "libgbm1",
    "libdrm2",
    "libxkbcommon0",
    "libwayland-client0",
    "libwayland-cursor0",
    "libwayland-egl1",
    "libatk1.0-0",
    "libatk-bridge2.0-0",
    "libatspi2.0-0",
    "libcups2",
    "libpango-1.0-0",
    "libcairo2",
    "libexpat1",
    "libsecret-1-0",
    "libvulkan1",
    "libgl1-mesa-dri",
    "libglvnd0",
    // Shell, fonts and small utilities the desktop exposes.
    "foot",
    "mc",
    "nano",
    "curl",
    "ca-certificates",
    "fonts-liberation",
    "fonts-noto-color-emoji",
    "fonts-noto-cjk",
    "dbus",
    "xdg-utils",
    "procps",
    "unzip",
    "xz-utils",
    "git",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_proot_is_preferred_and_uses_its_own_options() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("app"));
        let base = paths.base().join("sys");
        std::fs::create_dir_all(base.join("proot/bin")).unwrap();
        std::fs::write(base.join("proot/bin/proot"), b"#!/bin/sh\n").unwrap();
        let s = ProotSpec::new(&paths, Path::new("/rootfs")).command(vec!["/bin/sh".into()]);
        assert_eq!(s.kind, ProotKind::Classic);
        let argv = s.argv(&paths.classic_proot());
        assert_eq!(argv[1], "-r");
        assert!(argv.contains(&"--kill-on-exit".to_string()));
        assert_eq!(s.env.get("PROOT_NO_SECCOMP").map(String::as_str), Some("1"));
        // Without the payload we stay on the Rust rewrite.
        let bare = Paths::new(tmp.path().join("nothing"));
        let s2 = ProotSpec::new(&bare, Path::new("/rootfs"));
        assert_eq!(s2.kind, ProotKind::Rs);
        assert_eq!(s2.argv(Path::new("/usr/bin/proot-rs"))[1], "--rootfs");
    }

    fn spec() -> ProotSpec {
        let dir = PathBuf::from("/data/user/0/dev.samlator/files");
        let paths = Paths::new(dir);
        ProotSpec::new(&paths, Path::new("/data/user/0/dev.samlator/files/sys/rootfs"))
    }

    #[test]
    fn argv_has_rootfs_and_wayland_binds() {
        let argv = spec()
            .command(vec!["/opt/antigravity/antigravity".into(), "--foo".into()])
            .argv(Path::new("/data/user/0/dev.samlator/files/sys/bin/proot-rs"));

        assert_eq!(argv[0], "/data/user/0/dev.samlator/files/sys/bin/proot-rs");
        assert!(argv.windows(2).any(|w| w[0] == "--rootfs"));
        assert!(argv
            .iter()
            .any(|a| a.ends_with("sys/run:/tmp")));
        assert!(argv.iter().any(|a| a.ends_with("sys/tmp:/dev/shm")));
        assert_eq!(argv[argv.len() - 1], "--foo");
        // The `--` separator must be present exactly once, right before the command.
        let split = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(split, argv.len() - 3);
    }

    #[test]
    fn guest_env_is_wayland() {
        let s = spec();
        assert_eq!(s.env.get("XDG_RUNTIME_DIR").unwrap(), "/tmp");
        assert_eq!(s.env.get("WAYLAND_DISPLAY").unwrap(), "wayland-0");
        assert_eq!(s.cwd, "/root");
    }
}

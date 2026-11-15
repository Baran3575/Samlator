use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::Paths;

/// Describes one guest invocation.
///
/// Samlator never needs root: PRoot intercepts path-bearing syscalls with
/// `ptrace` and rewrites them, which is exactly enough to fake a `chroot` with
/// ordinary app permissions. No kernel modules, no image files, no VM.
#[derive(Debug, Clone)]
pub struct ProotSpec {
    pub rootfs: PathBuf,
    pub binds: Vec<(PathBuf, String)>,
    pub cwd: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl ProotSpec {
    pub fn new(paths: &Paths, rootfs: &Path) -> Self {
        let mut spec = Self {
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
        spec.env
            .insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/antigravity".into());
        spec.env
            .insert("MOZ_ENABLE_WAYLAND".into(), "1".into());
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
        let mut argv = vec![
            proot.display().to_string(),
            "--rootfs".into(),
            self.rootfs.display().to_string(),
            "--cwd".into(),
            self.cwd.clone(),
            "--kill-on-exit".into(),
        ];
        for (host, guest) in &self.binds {
            argv.push("--bind".into());
            argv.push(format!("{}:{}", host.display(), guest));
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

use std::path::{Path, PathBuf};

/// Every mutable path Samlator owns, rooted at the app private directory.
///
/// Android gives us something like `/data/user/0/dev.samlator/files`; on the host
/// we use whatever the caller passes so the whole crate stays testable.
///
/// Layout (inside the app data dir):
/// ```text
/// sys/
///   rootfs/        Debian arm64 userland (the "guest /")
///   run/           XDG_RUNTIME_DIR: wayland-0 socket, pulse, dbus
///   tmp/
///   home/          bind-mounted over the guest /root
///   bin/           proot-rs (aarch64-linux-android), bootstrap helpers
///   downloads/     cached tarballs (sha256-verified)
///   logs/
///   installed.json
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    base: PathBuf,
}

impl Paths {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    fn sys(&self) -> PathBuf {
        self.base.join("sys")
    }

    pub fn rootfs(&self) -> PathBuf {
        self.sys().join("rootfs")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.sys().join("run")
    }

    pub fn tmp(&self) -> PathBuf {
        self.sys().join("tmp")
    }

    /// Host-side home directory, bind mounted to `/root` inside the guest.
    pub fn home(&self) -> PathBuf {
        self.sys().join("home")
    }

    pub fn bin(&self) -> PathBuf {
        self.sys().join("bin")
    }

    pub fn downloads(&self) -> PathBuf {
        self.sys().join("downloads")
    }

    pub fn logs(&self) -> PathBuf {
        self.sys().join("logs")
    }

    pub fn installed_marker(&self) -> PathBuf {
        self.sys().join("installed.json")
    }

    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.json")
    }

    pub fn proot(&self) -> PathBuf {
        self.bin().join("proot-rs")
    }

    /// Classic PRoot (the Termux lineage), laid out as `sys/proot/{bin,lib,...}`.
    ///
    /// proot-rs 0.1.0 cannot exec a glibc binary on modern Android, while classic
    /// proot does it with its embedded loader. When present we prefer it.
    pub fn classic_proot(&self) -> PathBuf {
        self.base().join("sys").join("proot").join("bin").join("proot")
    }

    /// `$XDG_RUNTIME_DIR/wayland-0`, the socket our Rust compositor listens on.
    pub fn wayland_socket(&self) -> PathBuf {
        self.runtime_dir().join("wayland-0")
    }

    pub fn log_file(&self, name: &str) -> PathBuf {
        self.logs().join(format!("{name}.log"))
    }

    /// Try every writable base we may use, most preferred first.
    ///
    /// Termux/dev testers can override with `SAMLATOR_HOME`.
    pub fn resolve(app_data: Option<&Path>) -> Self {
        if let Some(dir) = std::env::var_os("SAMLATOR_HOME") {
            return Self::new(PathBuf::from(dir));
        }
        if let Some(dir) = app_data {
            return Self::new(dir);
        }
        Self::new(PathBuf::from(".samlator"))
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [
            self.sys(),
            self.rootfs(),
            self.runtime_dir(),
            self.tmp(),
            self.home(),
            self.bin(),
            self.downloads(),
            self.logs(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        // 0700 on the runtime dir, like a real session manager.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                self.runtime_dir(),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        let p = Paths::new("/data/user/0/dev.samlator/files");
        assert!(p.rootfs().ends_with("sys/rootfs"));
        assert_eq!(
            p.wayland_socket(),
            PathBuf::from("/data/user/0/dev.samlator/files/sys/run/wayland-0")
        );
        assert!(p.proot().ends_with("sys/bin/proot-rs"));
    }

    #[test]
    fn ensure_creates_every_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path());
        p.ensure().unwrap();
        for d in [
            p.rootfs(),
            p.runtime_dir(),
            p.tmp(),
            p.home(),
            p.bin(),
            p.downloads(),
            p.logs(),
        ] {
            assert!(d.is_dir(), "{} missing", d.display());
        }
    }
}

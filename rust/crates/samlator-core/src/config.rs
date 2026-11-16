use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::{Error, Result};

pub const SCHEMA: u32 = 1;

/// Graphics backend handed to Electron/Chromium.
///
/// The S24 FE (Exynos 2400 / Xclipse 940, RDNA3) exposes Vulkan. Chromium can
/// either take that path through ANGLE, or stay safely on the CPU rasteriser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuBackend {
    /// ANGLE over the vendor Vulkan driver (fast, driver dependent).
    AngleVulkan,
    /// ANGLE over GLES (safe middle ground).
    AngleGles,
    /// Chromium's bundled software rasteriser. Always works, slowest.
    SwiftShader,
}

impl GpuBackend {
    /// Chromium switches appended to every Electron launch.
    ///
    /// Tuned for the S24 FE's Xclipse 940 (RDNA3, Vulkan 1.3): ANGLE on top of
    /// the native Vulkan driver is the only path that gives hardware
    /// rasterisation inside PRoot, because the guest cannot use DRM directly.
    pub fn chromium_flags(self) -> Vec<String> {
        // `--ozone-platform=wayland` is the whole point: no X11 anywhere.
        // We do not implement xdg-decoration, so client-side decorations are
        // forced; requesting server-side ones just gives a undecorated window.
        let mut features = vec!["UseOzonePlatform"];
        let mut flags: Vec<String> = vec![
            "--ozone-platform=wayland".into(),
            "--disable-features=WaylandWindowDecorations".into(),
            "--disable-gpu-sandbox".into(),
            "--no-sandbox".into(),
            "--disable-setuid-sandbox".into(),
            "--disable-seccomp-filter-sandbox".into(),
            // PRoot makes Chromium's zygote fail to fork; without this the
            // renderer dies before a window ever appears.
            "--no-zygote".into(),
            // PRoot confuses Chromium's zygote; one process avoids the hangs.
            "--in-process-gpu".into(),
            // /dev/shm inside the guest is a directory, not a tmpfs.
            "--disable-dev-shm-usage".into(),
            "--enable-gpu-rasterization".into(),
        ];
        match self {
            GpuBackend::AngleVulkan => {
                features.push("Vulkan");
                flags.push("--use-gl=angle".into());
                flags.push("--use-angle=vulkan".into());
                flags.push("--enable-unsafe-webgpu".into());
            }
            GpuBackend::AngleGles => {
                flags.push("--use-gl=angle".into());
                flags.push("--use-angle=gles-egl".into());
            }
            GpuBackend::SwiftShader => {
                flags.push("--use-gl=angle".into());
                flags.push("--use-angle=swiftshader".into());
                flags.push("--disable-gpu-compositing".into());
            }
        }
        flags.push(format!("--enable-features={}", features.join(",")));
        flags
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayCfg {
    /// Logical desktop size. 0 means "match the Android surface".
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// UI scale of the desktop; 1.15 suits a 6.7" 1080p-class panel.
    pub scale: f32,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self { width: 0, height: 0, refresh_hz: 60, scale: 1.15 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadKind {
    /// A Debian-style root filesystem tarball (tar.zst).
    Rootfs,
    /// A self-contained app tree (Electron/VS Code style tarball).
    AppTree,
    /// A single static executable copied into `sys/bin`.
    Helper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    pub id: String,
    pub url: String,
    /// Lowercase hex sha256. Empty disables verification (dev only).
    #[serde(default)]
    pub sha256: String,
    pub kind: PayloadKind,
    /// Leading path components to drop while extracting.
    #[serde(default)]
    pub strip: u32,
    /// Destination, relative to `sys/`.
    pub target: String,
}

impl Payload {
    pub fn is_verified(&self) -> bool {
        !self.sha256.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    /// Path inside the guest rootfs.
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: String,
    /// Per-app override of the global GPU backend.
    #[serde(default)]
    pub gpu: Option<GpuBackend>,
    /// Shown on the desktop tile.
    #[serde(default)]
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub schema: u32,
    /// Release manifest describing rootfs + payload checksums.
    pub manifest_url: String,
    pub display: DisplayCfg,
    pub gpu: GpuBackend,
    /// Advisory RAM budget for the guest (MB). 0 = let the kernel decide.
    pub memory_mb: u32,
    pub rootfs: Payload,
    #[serde(default)]
    pub payloads: Vec<Payload>,
    #[serde(default)]
    pub apps: Vec<AppEntry>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Antigravity IDE, Linux **arm64** tarball.
///
/// Confirmed live: `linux-arm/Antigravity IDE.tar.gz`, ~228 MB, Electron 39.
/// Using the ARM64 build is the whole point of Samlator: no x86 translation.
pub const ANTIGRAVITY_URL: &str = "https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/2.1.1-6123990880747520/linux-arm/Antigravity%20IDE.tar.gz";

pub const RELEASE_MANIFEST: &str = "https://github.com/Baran3575/Samlator/releases/download/rootfs-latest/manifest.json";

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            manifest_url: RELEASE_MANIFEST.to_string(),
            display: DisplayCfg::default(),
            gpu: GpuBackend::AngleVulkan,
            memory_mb: 0,
            rootfs: Payload {
                id: "rootfs".into(),
                url: String::new(),
                sha256: String::new(),
                kind: PayloadKind::Rootfs,
                strip: 0,
                target: "rootfs".into(),
            },
            payloads: vec![Payload {
                id: "antigravity".into(),
                url: ANTIGRAVITY_URL.into(),
                sha256: String::new(),
                kind: PayloadKind::AppTree,
                strip: 1,
                target: "rootfs/opt/antigravity".into(),
            }],
            apps: vec![
                AppEntry {
                    id: "antigravity".into(),
                    name: "Antigravity".into(),
                    exec: "/opt/antigravity/antigravity".into(),
                    args: vec!["%GPU%".into(), "--disable-dev-shm-usage".into()],
                    working_dir: "/root".into(),
                    gpu: None,
                    category: "dev".into(),
                },
                AppEntry {
                    id: "terminal".into(),
                    name: "Terminal".into(),
                    exec: "/usr/bin/foot".into(),
                    args: vec![],
                    working_dir: "/root".into(),
                    gpu: None,
                    category: "system".into(),
                },
                AppEntry {
                    id: "files".into(),
                    name: "Files".into(),
                    exec: "/usr/bin/foot".into(),
                    args: vec!["-e".into(), "/usr/bin/mc".into()],
                    working_dir: "/root".into(),
                    gpu: None,
                    category: "system".into(),
                },
            ],
            env: BTreeMap::new(),
        }
    }
}

/// Release manifest published by CI (`rootfs-latest/manifest.json`).
///
/// Deliberately looser than [`Config`]: a manifest only has to say where the
/// rootfs and helpers live, so unknown or missing fields never break an
/// installed build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub rootfs: Option<Payload>,
    #[serde(default)]
    pub payloads: Vec<Payload>,
    #[serde(default)]
    pub apps: Vec<AppEntry>,
}

impl Config {
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(cfg) if cfg.schema == SCHEMA => cfg,
                Ok(_) => {
                    tracing::warn!("config schema changed, using defaults");
                    Self::default()
                }
                Err(e) => {
                    tracing::warn!("config parse error ({e}), using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn app(&self, id: &str) -> Option<&AppEntry> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn payload(&self, id: &str) -> Option<&Payload> {
        if self.rootfs.id == id {
            return Some(&self.rootfs);
        }
        self.payloads.iter().find(|p| p.id == id)
    }

    /// Resolve `%GPU%` style placeholders in app arguments.
    pub fn resolve_args(&self, app: &AppEntry) -> Vec<String> {
        let backend = app.gpu.unwrap_or(self.gpu);
        let mut out = Vec::new();
        for arg in &app.args {
            if arg == "%GPU%" {
                out.extend(backend.chromium_flags());
            } else {
                out.push(arg.clone());
            }
        }
        out
    }

    /// Merge a release manifest (same JSON shape) over the built-in defaults.
    pub fn apply_manifest(&mut self, manifest: &Manifest) {
        if let Some(rootfs) = manifest.rootfs.as_ref() {
            if !rootfs.url.is_empty() {
                self.rootfs = rootfs.clone();
            }
        }
        // Payloads are merged by id so a manifest can add the proot helper
        // without dropping the app entries baked into the binary.
        for payload in manifest.payloads.iter() {
            match self.payloads.iter_mut().find(|p| p.id == payload.id) {
                Some(slot) => *slot = payload.clone(),
                None => self.payloads.push(payload.clone()),
            }
        }
        if !manifest.apps.is_empty() {
            self.apps = manifest.apps.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_merges_rootfs_and_keeps_apps() {
        let mut cfg = Config::default();
        let json = r#"{
            "rootfs": {"id":"rootfs","url":"https://x/rootfs.tar.zst","sha256":"ab",
                       "kind":"rootfs","strip":0,"target":"rootfs"},
            "payloads": [
              {"id":"antigravity","url":"https://x/ag.tar.gz","sha256":"",
               "kind":"app-tree","strip":1,"target":"rootfs/opt/antigravity"},
              {"id":"proot","url":"https://x/proot-rs","sha256":"cd",
               "kind":"helper","strip":0,"target":"bin"}
            ]
        }"#;
        let manifest: Manifest = serde_json::from_str(json).unwrap();
        cfg.apply_manifest(&manifest);
        assert_eq!(cfg.rootfs.sha256, "ab");
        assert!(cfg.payloads.iter().any(|p| p.id == "proot"));
        assert_eq!(
            cfg.payloads.iter().filter(|p| p.id == "antigravity").count(),
            1,
            "existing payload must be replaced, not duplicated"
        );
        assert!(cfg.app("antigravity").is_some(), "app entries survive a manifest");
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        let mut cfg = Config::default();
        cfg.gpu = GpuBackend::SwiftShader;
        cfg.save(&file).unwrap();
        let back = Config::load_or_default(&file);
        assert_eq!(back.gpu, GpuBackend::SwiftShader);
        assert_eq!(back.apps.len(), 3);
    }

    #[test]
    fn gpu_placeholder_expands() {
        let cfg = Config::default();
        let app = cfg.app("antigravity").unwrap();
        let args = cfg.resolve_args(app);
        assert!(args.iter().any(|a| a == "--ozone-platform=wayland"));
        assert!(args.iter().any(|a| a.starts_with("--use-angle=")));
    }

    #[test]
    fn missing_file_falls_back() {
        let cfg = Config::load_or_default(Path::new("/nonexistent/x.json"));
        assert_eq!(cfg.schema, SCHEMA);
    }
}

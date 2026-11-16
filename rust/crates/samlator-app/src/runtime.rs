//! Orchestration: turn an empty app directory into a running Linux desktop.
//!
//! Everything in here is plain Rust with no Android dependency, so the whole
//! provisioning path is unit-testable on the CI host.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use samlator_compositor::{Command, Compositor, DisplayConfig};
use samlator_core::config::{Config, Payload, PayloadKind};
use samlator_core::paths::Paths;
use samlator_core::proot::{ProotSpec, BOOTSTRAP_PACKAGES};
use samlator_core::rootfs::{self, InstalledMarkers};
use samlator_core::supervisor::{Session, SessionState};
use samlator_core::{download, Progress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Manifest,
    Rootfs,
    Payloads,
    Proot,
    Bootstrap,
    Ready,
    Failed,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Idle => "hazır değil",
            Stage::Manifest => "manifest indiriliyor",
            Stage::Rootfs => "Linux kök dosya sistemi indiriliyor",
            Stage::Payloads => "Antigravity indiriliyor",
            Stage::Proot => "proot hazırlanıyor",
            Stage::Bootstrap => "masaüstü paketleri kuruluyor",
            Stage::Ready => "hazır",
            Stage::Failed => "hata",
        }
    }

    pub fn fraction(self) -> f32 {
        match self {
            Stage::Idle => 0.0,
            Stage::Manifest => 0.05,
            Stage::Rootfs => 0.55,
            Stage::Payloads => 0.85,
            Stage::Proot => 0.9,
            Stage::Bootstrap => 0.95,
            Stage::Ready => 1.0,
            Stage::Failed => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Msg {
    Stage(Stage),
    Detail(String),
    Progress { done: u64, total: Option<u64> },
    Error(String),
    SessionStarted(String),
    SessionExited(String),
}

/// Shared, UI-readable runtime state.
pub struct Runtime {
    pub paths: Paths,
    pub config: Config,
    pub stage: Stage,
    pub detail: String,
    pub progress: f32,
    pub error: Option<String>,
    pub session_label: Option<String>,
    pub log: Vec<String>,
    pub compositor: Option<Compositor>,
    session: Option<Session>,
    rx: Option<Receiver<Msg>>,
    busy: Arc<Mutex<bool>>,
}

impl Runtime {
    pub fn new(paths: Paths, config: Config) -> Self {
        let _ = paths.ensure();
        let ready = rootfs::is_rootfs_ready(&paths.rootfs())
            && paths.proot().is_file()
            && markers_complete(&paths);
        Self {
            paths,
            config,
            stage: if ready { Stage::Ready } else { Stage::Idle },
            detail: String::new(),
            progress: if ready { 1.0 } else { 0.0 },
            error: None,
            session_label: None,
            log: Vec::new(),
            compositor: None,
            session: None,
            rx: None,
            busy: Arc::new(Mutex::new(false)),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy.lock().map(|b| *b).unwrap_or(false)
    }

    pub fn is_ready(&self) -> bool {
        self.stage == Stage::Ready
    }

    /// True when the compositor has no mapped client window (show a hint).
    pub fn windows_empty_hint(&self) -> bool {
        self.compositor
            .as_ref()
            .map(|c| c.windows().iter().all(|w| !w.mapped))
            .unwrap_or(true)
    }

    /// Start (or restart) the desktop compositor at the given surface size.
    pub fn ensure_compositor(&mut self, width: u32, height: u32, scale: f64) {
        let need_new = self.compositor.is_none();
        if need_new {
            let cfg = DisplayConfig {
                width,
                height,
                scale,
                refresh_hz: self.config.display.refresh_hz.max(1),
            };
            match Compositor::start(cfg, &self.paths.runtime_dir()) {
                Ok(c) => {
                    self.push_log(format!("wayland oturumu hazır: {}", c.socket_name()));
                    self.compositor = Some(c);
                }
                Err(e) => {
                    self.error = Some(format!("compositor: {e}"));
                    self.stage = Stage::Failed;
                }
            }
        } else if let Some(c) = &self.compositor {
            c.set_display(width, height, scale);
        }
    }

    /// Kick off provisioning on a worker thread. Safe to call repeatedly.
    pub fn start_setup(&mut self) {
        if self.is_busy() {
            return;
        }
        if let Ok(mut busy) = self.busy.lock() {
            if *busy {
                return;
            }
            *busy = true;
        }
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.error = None;
        let paths = self.paths.clone();
        let mut config = self.config.clone();
        let busy = self.busy.clone();

        std::thread::Builder::new()
            .name("samlator-setup".into())
            .spawn(move || {
                let result = provision(&paths, &mut config, &tx);
                if let Err(e) = result {
                    let _ = tx.send(Msg::Error(e.to_string()));
                }
                if let Ok(mut b) = busy.lock() {
                    *b = false;
                }
            })
            .ok();
    }

    /// Drain worker messages; call once per frame.
    pub fn poll(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                messages.push(msg);
            }
        }
        for msg in messages {
            match msg {
                Msg::Stage(stage) => {
                    self.stage = stage;
                    self.progress = self.progress.max(stage.fraction());
                }
                Msg::Detail(d) => self.detail = d,
                Msg::Progress { done, total } => {
                    if let Some(total) = total.filter(|t| *t > 0) {
                        let base = self.stage.fraction();
                        let span = match self.stage {
                            Stage::Rootfs => 0.5,
                            Stage::Payloads => 0.3,
                            _ => 0.05,
                        };
                        self.progress = (base + span * (done as f32 / total as f32)).min(0.99);
                        self.detail = format!(
                            "{} / {} MB",
                            done / 1_048_576,
                            total / 1_048_576
                        );
                    } else if done > 0 {
                        // Extraction reports entries without a total; without
                        // this line the bar sits still and the install looks
                        // hung.
                        self.detail = format!("{done} öğe açıldı");
                    }
                }
                Msg::Error(e) => {
                    self.push_log(format!("hata: {e}"));
                    self.error = Some(e);
                    self.stage = Stage::Failed;
                }
                Msg::SessionStarted(label) => {
                    self.session_label = Some(label);
                    self.stage = Stage::Ready;
                    self.progress = 1.0;
                }
                Msg::SessionExited(label) => {
                    self.push_log(format!("{label} kapandı"));
                    self.session_label = None;
                }
            }
        }

        if let Some(session) = self.session.as_mut() {
            match session.poll() {
                SessionState::Running => {}
                SessionState::Exited(code) => {
                    self.push_log(format!("süreç çıktı, kod {code}"));
                    self.session_label = None;
                    self.session = None;
                }
                SessionState::Signaled(sig) => {
                    self.push_log(format!("süreç sinyalle bitti: {sig}"));
                    self.session_label = None;
                    self.session = None;
                }
            }
        }
    }

    /// Launch one of the configured guest apps under PRoot.
    pub fn launch(&mut self, app_id: &str) {
        if !self.is_ready() {
            self.push_log("önce kurulum gerekli".into());
            return;
        }
        if self.session.is_some() {
            self.push_log("zaten çalışan bir uygulama var".into());
            return;
        }
        let Some(app) = self.config.app(app_id).cloned() else {
            self.push_log(format!("bilinmeyen uygulama: {app_id}"));
            return;
        };
        if self.compositor.is_none() {
            self.push_log("compositor yok, uygulama başlatılamadı".into());
            return;
        }

        let mut args = self.config.resolve_args(&app);
        let mut cmd = vec![app.exec.clone()];
        // Electron needs a way to find the guest's own libraries.
        cmd.append(&mut args);

        let spec = ProotSpec::new(&self.paths, &self.paths.rootfs())
            .cwd(if app.working_dir.is_empty() { "/root".into() } else { app.working_dir.clone() })
            .command(cmd);

        let log_path = self.paths.log_file(&app.id);
        match Session::spawn(&spec.argv(&self.paths.proot()), &spec.env, &log_path, app.id.clone()) {
            Ok(session) => {
                self.push_log(format!(
                    "{} başlatıldı (pid {:?})",
                    app.name,
                    session.pid()
                ));
                self.session_label = Some(app.name);
                self.session = Some(session);
            }
            Err(e) => {
                self.push_log(format!("başlatma hatası: {e}"));
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn stop_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.terminate();
            self.push_log("oturum kapatıldı".into());
        }
        self.session_label = None;
    }

    pub fn push_log(&mut self, line: String) {
        tracing::info!("{line}");
        self.log.push(line);
        if self.log.len() > 400 {
            self.log.drain(0..100);
        }
    }

    pub fn tail_session_log(&self) -> String {
        let Some(label) = &self.session_label else {
            return String::from("(oturum yok)");
        };
        std::fs::read_to_string(self.paths.log_file(label))
            .unwrap_or_else(|_| String::from("(log yok)"))
    }

    pub fn pointer_motion(&self, x: f64, y: f64) {
        if let Some(c) = &self.compositor {
            c.pointer_motion(x, y);
        }
    }

    pub fn pointer_button(&self, button: u32, pressed: bool) {
        if let Some(c) = &self.compositor {
            c.pointer_button(button, pressed);
        }
    }

    pub fn scroll(&self, v: f64) {
        if let Some(c) = &self.compositor {
            c.scroll(v);
        }
    }

    pub fn key(&self, code: u32, pressed: bool) {
        if let Some(c) = &self.compositor {
            c.key(code, pressed);
        }
    }

    pub fn focus_window(&self, index: usize) {
        if let Some(c) = &self.compositor {
            c.send(Command::Focus { index });
        }
    }
}

fn markers_complete(paths: &Paths) -> bool {
    InstalledMarkers::load(&paths.installed_marker())
        .bootstrapped_at
        .is_some()
}

/// Blocking provisioning pipeline, run on a worker thread.
fn provision(paths: &Paths, config: &mut Config, tx: &Sender<Msg>) -> samlator_core::Result<()> {
    let mut notify = |p: Progress| {
        let msg = match p {
            Progress::Stage(s) => Msg::Detail(s),
            Progress::Bytes { done, total } => Msg::Progress { done, total },
            Progress::Done => return,
        };
        let _ = tx.send(msg);
    };

    // 1. Release manifest (checksums for rootfs/payloads).
    if !config.manifest_url.is_empty() {
        let _ = tx.send(Msg::Stage(Stage::Manifest));
        if let Some(manifest) = fetch_manifest(&config.manifest_url, &paths.downloads()) {
            match serde_json::from_str::<samlator_core::config::Manifest>(&manifest) {
                Ok(m) => config.apply_manifest(&m),
                Err(e) => {
                    let _ = tx.send(Msg::Detail(format!("manifest okunamadı: {e}")));
                }
            }
            let _ = config.save(&paths.config_file());
        }
    }

    // 2. Root filesystem.
    let rootfs_payload = config.rootfs.clone();
    if !rootfs::is_rootfs_ready(&paths.rootfs()) {
        if rootfs_payload.url.is_empty() {
            let _ = tx.send(Msg::Error(
                "rootfs indirme adresi yok: release manifest yayınlanmamış".into(),
            ));
            return Ok(());
        }
        let _ = tx.send(Msg::Stage(Stage::Rootfs));
        let archive = paths.downloads().join("rootfs.tar.zst");
        download::fetch(
            &rootfs_payload.url,
            &archive,
            &rootfs_payload.sha256,
            "rootfs",
            &mut notify,
        )?;
        // A truncated download is the usual cause of "Unknown frame
        // descriptor", so verify before touching the tree.
        if !rootfs_payload.sha256.is_empty() {
            let got = download::sha256_file(&archive)?;
            if !got.eq_ignore_ascii_case(rootfs_payload.sha256.trim()) {
                let _ = std::fs::remove_file(&archive);
                return Err(samlator_core::Error::Checksum {
                    what: "rootfs".into(),
                    expected: rootfs_payload.sha256.clone(),
                    actual: got,
                });
            }
        }
        if let Err(e) = rootfs::extract_tar_zst(&archive, &paths.rootfs(), rootfs_payload.strip, &mut notify) {
            // Never leave a half-extracted tree behind: it looks installed.
            let _ = std::fs::remove_dir_all(paths.rootfs());
            let _ = std::fs::remove_file(&archive);
            return Err(e);
        }
        let _ = std::fs::remove_file(&archive);
    }
    // proot mounts nothing, so /dev, /proc and DNS have to be real directories
    // and files inside the tree before apt can run.
    rootfs::prepare_guest(&paths.rootfs())?;

    // 3. Helper binaries (proot-rs). Nothing can run in the guest without
    // them, so they come before the heavy apt step.
    let _ = tx.send(Msg::Stage(Stage::Proot));
    for payload in config
        .payloads
        .clone()
        .into_iter()
        .filter(|p| p.kind == PayloadKind::Helper)
    {
        let _ = tx.send(Msg::Detail(format!("{} indiriliyor", payload.id)));
        if let Err(e) = install_helper(paths, &payload, &mut notify) {
            let _ = tx.send(Msg::Error(format!("{} kurulamadı: {e}", payload.id)));
            return Err(e);
        }
    }
    if !paths.proot().is_file() {
        let msg = "proot-rs bulunamadı: manifest yardımcı ikili içermiyor".to_string();
        let _ = tx.send(Msg::Error(msg.clone()));
        return Err(samlator_core::Error::Config(msg));
    }
    // Android refuses execve() on files in the app data dir when the app
    // targets API 29+ (W^X). That turns into a bare "Permission denied" much
    // later, so prove the binary actually runs right here.
    match std::process::Command::new(paths.proot()).arg("--version").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let _ = tx.send(Msg::Detail(format!("proot-rs hazır: {text}")));
        }
        Err(e) => {
            let msg = format!("proot-rs çalıştırılamadı: {e} (targetSdk 28 gerekli)");
            let _ = tx.send(Msg::Error(msg.clone()));
            return Err(samlator_core::Error::Archive(msg));
        }
    }

    // 5. First-boot provisioning inside the guest (apt, fonts, dbus).
    let mut markers = InstalledMarkers::load(&paths.installed_marker());
    markers.rootfs_sha256 = rootfs_payload.sha256.clone();
    if markers.bootstrapped_at.is_none() && paths.proot().is_file() {
        let _ = tx.send(Msg::Stage(Stage::Bootstrap));
        let script_path = paths.tmp().join("bootstrap.sh");
        std::fs::write(&script_path, rootfs::bootstrap_script(BOOTSTRAP_PACKAGES))?;
        make_executable(&script_path)?;

        let spec = ProotSpec::new(paths, &paths.rootfs())
            .command(vec!["/bin/bash".into(), "/tmp/bootstrap.sh".into()]);
        let mut session = Session::spawn(
            &spec.argv(&paths.proot()),
            &spec.env,
            &paths.log_file("bootstrap"),
            "bootstrap",
        )?;
        // Bootstrap installs ~600 MB of packages; give it room but never hang.
        let deadline = std::time::Instant::now() + Duration::from_secs(60 * 30);
        loop {
            match session.poll() {
                SessionState::Running => {
                    if std::time::Instant::now() > deadline {
                        session.terminate();
                        let _ = tx.send(Msg::Detail("bootstrap zaman aşımı".into()));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                state => {
                    let _ = tx.send(Msg::Detail(format!("bootstrap bitti: {state:?}")));
                    break;
                }
            }
        }
        markers.bootstrapped_at = Some(format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        markers.save(&paths.installed_marker())?;
    }

    let _ = tx.send(Msg::Stage(Stage::Ready));
    // 6. App trees (Antigravity, ~228 MB) last: the desktop is already usable
    // without them and a failure here must not undo a finished setup.
    for payload in config
        .payloads
        .clone()
        .into_iter()
        .filter(|p| p.kind == PayloadKind::AppTree)
    {
        let target = paths.base().join("sys").join(&payload.target);
        if target.exists() {
            continue;
        }
        let _ = tx.send(Msg::Stage(Stage::Payloads));
        let _ = tx.send(Msg::Detail(format!("{} indiriliyor", payload.id)));
        let name = download_name(&payload);
        let file = paths.downloads().join(&name);
        let result = download::fetch(&payload.url, &file, &payload.sha256, &payload.id, &mut notify)
            .and_then(|_| rootfs::extract_archive(&file, &target, payload.strip, &mut notify));
        let _ = std::fs::remove_file(&file);
        match result {
            Ok(()) => {
                let _ = tx.send(Msg::Detail(format!("{} kuruldu", payload.id)));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&target);
                let _ = tx.send(Msg::Detail(format!("{} indirilemedi: {e}", payload.id)));
            }
        }
    }

    let _ = tx.send(Msg::Detail("kurulum tamam".into()));
    Ok(())
}

/// File name a payload is downloaded to, taken from its URL.
fn download_name(payload: &Payload) -> String {
    payload
        .url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .map(|n| n.replace("%20", " "))
        .unwrap_or_else(|| payload.id.clone())
}

/// Helper payloads are bare executables (proot-rs), never archives.
fn install_helper(
    paths: &Paths,
    payload: &Payload,
    notify: &mut impl FnMut(Progress),
) -> samlator_core::Result<()> {
    let target_dir = paths.base().join("sys").join(&payload.target);
    std::fs::create_dir_all(&target_dir)?;
    let file = paths.downloads().join(download_name(payload));
    download::fetch(&payload.url, &file, &payload.sha256, &payload.id, notify)?;
    let dest = if payload.id == "proot" || payload.target == "bin" {
        paths.proot()
    } else {
        target_dir.join(download_name(payload))
    };
    std::fs::copy(&file, &dest)?;
    make_executable(&dest)?;
    let _ = std::fs::remove_file(&file);
    Ok(())
}

fn fetch_manifest(url: &str, downloads: &std::path::Path) -> Option<String> {
    let dest = downloads.join("manifest.json");
    let mut noop = |_p: Progress| {};
    download::fetch(url, &dest, "", "manifest", &mut noop).ok()?;
    std::fs::read_to_string(dest).ok()
}

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_monotonic() {
        let order = [
            Stage::Idle,
            Stage::Manifest,
            Stage::Rootfs,
            Stage::Payloads,
            Stage::Proot,
            Stage::Bootstrap,
            Stage::Ready,
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0].fraction() < pair[1].fraction(),
                "{:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn fresh_runtime_is_idle_and_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let rt = Runtime::new(paths, Config::default());
        assert_eq!(rt.stage, Stage::Idle);
        assert!(!rt.is_ready());
        assert!(!rt.is_busy());
    }

    #[test]
    fn launch_without_setup_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new(Paths::new(dir.path()), Config::default());
        rt.launch("antigravity");
        assert!(rt.session.is_none());
        assert!(rt.log.iter().any(|l| l.contains("kurulum")));
    }

    #[test]
    fn compositor_starts_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new(Paths::new(dir.path()), Config::default());
        rt.ensure_compositor(640, 480, 1.0);
        assert!(rt.compositor.is_some());
        // Second call resizes instead of rebinding.
        rt.ensure_compositor(800, 600, 1.0);
        assert!(rt.compositor.is_some());
        assert!(rt.log.iter().any(|l| l.contains("wayland")));
    }
}

//! Samlator's desktop engine.
//!
//! One process, one thread for the Wayland protocol and one shared RGBA frame.
//! There is no X11, no Xwayland, no VNC and no extra display server app: guests
//! connect to `$XDG_RUNTIME_DIR/wayland-0` and we hand the pixels to the Android
//! surface as a texture.

pub mod desktop;
pub mod state;

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use smithay::reexports::calloop::channel::{
    channel as make_channel, Channel, Event as ChannelEvent, Sender as ChannelSender,
};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};

pub use desktop::{Frame, Rect};
pub use state::{
    Command, Shared, SharedHandle, WindowInfo, POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE,
    POINTER_BUTTON_RIGHT,
};
use state::{ClientState, State};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("event loop: {0}")]
    Loop(#[from] smithay::reexports::calloop::Error),
    #[error("source registration: {0}")]
    Insert(String),
    #[error("wayland display: {0}")]
    Display(String),
    #[error("socket: {0}")]
    Socket(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("compositor thread died during startup")]
    Startup,
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub refresh_hz: u32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self { width: 1080, height: 2340, scale: 2.625, refresh_hz: 60 }
    }
}

struct Running {
    cmd: ChannelSender<Command>,
    shared: SharedHandle,
    socket: String,
}

/// Running compositor. Dropping it tears the Wayland session down.
pub struct Compositor {
    cmd: ChannelSender<Command>,
    shared: SharedHandle,
    join: Option<JoinHandle<()>>,
    socket: String,
}

impl Compositor {
    /// Bind `$XDG_RUNTIME_DIR/wayland-0` and start the protocol thread.
    pub fn start(cfg: DisplayConfig, runtime_dir: &Path) -> Result<Self, Error> {
        // libwayland binds relative to XDG_RUNTIME_DIR; set it before the socket.
        std::fs::create_dir_all(runtime_dir)?;
        std::env::set_var("XDG_RUNTIME_DIR", runtime_dir);

        let runtime_dir = runtime_dir.to_path_buf();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Running, String>>();

        let join = std::thread::Builder::new()
            .name("samlator-wayland".into())
            .spawn(move || match build_session(cfg, &runtime_dir) {
                Ok((running, mut event_loop, mut display, mut state)) => {
                    let _ = ready_tx.send(Ok(Running {
                        cmd: running.0,
                        shared: running.1,
                        socket: running.2,
                    }));
                    loop {
                        if state.shutdown {
                            break;
                        }
                        if let Err(e) = event_loop.dispatch(Duration::from_millis(8), &mut state) {
                            tracing::error!("event loop dispatch: {e}");
                            break;
                        }
                        if let Err(e) = display.dispatch_clients(&mut state) {
                            tracing::error!("client dispatch: {e}");
                            break;
                        }
                        if let Err(e) = display.flush_clients() {
                            tracing::warn!("flush: {e}");
                        }
                    }
                    tracing::info!("wayland loop finished");
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(Error::Io)?;

        let running = ready_rx.recv().map_err(|_| Error::Startup)?;
        match running {
            Ok(r) => {
                tracing::info!("wayland socket ready: {}", r.socket);
                Ok(Self {
                    cmd: r.cmd,
                    shared: r.shared,
                    join: Some(join),
                    socket: r.socket,
                })
            }
            Err(e) => {
                let _ = join.join();
                Err(Error::Socket(e))
            }
        }
    }

    pub fn socket_name(&self) -> &str {
        &self.socket
    }

    pub fn shared(&self) -> SharedHandle {
        self.shared.clone()
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }

    pub fn set_display(&self, width: u32, height: u32, scale: f64) {
        self.send(Command::Resize { width, height, scale });
    }

    pub fn pointer_motion(&self, x: f64, y: f64) {
        self.send(Command::Motion { x, y });
    }

    pub fn pointer_button(&self, button: u32, pressed: bool) {
        self.send(Command::Button { button, pressed });
    }

    pub fn scroll(&self, vertical: f64) {
        self.send(Command::Axis { vertical, horizontal: 0.0 });
    }

    pub fn key(&self, evdev_code: u32, pressed: bool) {
        self.send(Command::Key { code: evdev_code, pressed });
    }

    /// Latest composited frame, if the compositor produced one.
    pub fn frame(&self) -> Option<Frame> {
        self.shared.lock().ok().map(|s| s.frame.clone())
    }

    pub fn frame_seq(&self) -> u64 {
        self.shared.lock().map(|s| s.frame.seq).unwrap_or(0)
    }

    pub fn windows(&self) -> Vec<WindowInfo> {
        self.shared
            .lock()
            .map(|s| s.windows.clone())
            .unwrap_or_default()
    }

    pub fn stop(&mut self) {
        let _ = self.cmd.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.stop();
    }
}

type Session = (
    (ChannelSender<Command>, SharedHandle, String),
    EventLoop<'static, State>,
    Display<State>,
    State,
);

/// Everything that must happen on the compositor thread (calloop is `!Send`).
fn build_session(cfg: DisplayConfig, runtime_dir: &Path) -> Result<Session, String> {
    let event_loop: EventLoop<'static, State> =
        EventLoop::try_new().map_err(|e| format!("{e}"))?;
    let display: Display<State> = Display::new().map_err(|e| e.to_string())?;
    let handle = display.handle();

    let width = cfg.width.max(320);
    let height = cfg.height.max(320);
    let state = State::new(&handle, width, height, cfg.scale, cfg.refresh_hz);
    let shared = state.shared();

    // Bind the socket ourselves instead of using `ListeningSocketSource`:
    // that helper resolves the path through the process-wide `XDG_RUNTIME_DIR`,
    // which makes tests (and any second instance) racy.
    std::fs::create_dir_all(runtime_dir).map_err(|e| format!("{e}"))?;
    let socket_path = runtime_dir.join("wayland-0");
    let _ = std::fs::remove_file(&socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("{e}"))?;
    let socket = "wayland-0".to_string();

    let loop_handle = event_loop.handle();
    loop_handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state: &mut State| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            match state
                                .display_handle
                                .insert_client(stream, Arc::new(ClientState::default()))
                            {
                                Ok(_) => tracing::info!("wayland client connected"),
                                Err(e) => tracing::warn!("client insert failed: {e}"),
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!("wayland accept failed: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("{e:?}"))?;

    let (cmd, rx): (ChannelSender<Command>, Channel<Command>) = make_channel();
    loop_handle
        .insert_source(rx, |event, _, state: &mut State| {
            if let ChannelEvent::Msg(cmd) = event {
                state.handle_command(cmd);
            }
        })
        .map_err(|e| format!("{e:?}"))?;

    // 60 Hz tick: composite only when something changed.
    let timer = Timer::from_duration(Duration::from_millis(16));
    loop_handle
        .insert_source(timer, |_instant, _, state: &mut State| {
            state.tick();
            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .map_err(|e| format!("{e:?}"))?;

    Ok(((cmd, shared, socket), event_loop, display, state))
}

/// Convenience for tests: a tiny in-process compositor on a temp socket dir.
pub fn probe(runtime_dir: &Path) -> Result<Compositor, Error> {
    Compositor::start(
        DisplayConfig { width: 640, height: 480, scale: 1.0, refresh_hz: 60 },
        runtime_dir,
    )
}

/// Shared frame type, re-exported for the UI layer.
pub type SharedFrame = Arc<Mutex<Frame>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_binds_socket_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = probe(dir.path()).expect("compositor should start");
        assert_eq!(c.socket_name(), "wayland-0");
        assert!(dir.path().join("wayland-0").exists());
        assert_eq!(c.windows().len(), 0);
        // A frame must exist even with zero clients.
        std::thread::sleep(Duration::from_millis(80));
        let frame = c.frame().expect("frame");
        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 480);
        c.stop();
    }

    #[test]
    fn pointer_commands_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = probe(dir.path()).unwrap();
        c.pointer_motion(10.0, 120.0);
        c.pointer_button(POINTER_BUTTON_LEFT, true);
        c.pointer_button(POINTER_BUTTON_LEFT, false);
        c.scroll(-15.0);
        c.key(30, true);
        c.key(30, false);
        std::thread::sleep(Duration::from_millis(80));
        let shared = c.shared.lock().unwrap();
        assert_eq!(shared.pointer, (10.0, 120.0));
        drop(shared);
        c.stop();
    }
}

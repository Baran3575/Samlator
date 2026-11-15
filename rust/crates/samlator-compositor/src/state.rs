//! The Wayland state: globals, handlers, input injection and the compositing
//! tick. Protocol plumbing only — pixel work lives in [`crate::desktop`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_output, wl_seat, wl_shm, wl_surface::WlSurface,
};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{with_buffer_contents, ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell};

use crate::desktop::{blit_argb, draw_cursor, draw_shadow, Frame, Rect};

/// Commands the UI thread sends into the compositor loop.
#[derive(Debug, Clone)]
pub enum Command {
    Motion { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    Axis { vertical: f64, horizontal: f64 },
    Key { code: u32, pressed: bool },
    Resize { width: u32, height: u32, scale: f64 },
    Focus { index: usize },
    CloseWindow { index: usize },
    Screenshot(std::sync::mpsc::Sender<Frame>),
    Shutdown,
}

/// Read-only view of a client window, for the desktop's taskbar.
#[derive(Debug, Clone, Default)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub mapped: bool,
    pub focused: bool,
}

pub const POINTER_BUTTON_LEFT: u32 = 0x110;
pub const POINTER_BUTTON_RIGHT: u32 = 0x111;
pub const POINTER_BUTTON_MIDDLE: u32 = 0x112;

/// State shared between the compositor thread and the Android UI thread.
pub struct Shared {
    pub frame: Frame,
    pub windows: Vec<WindowInfo>,
    pub pointer: (f64, f64),
    pub cursor_visible: bool,
    pub scale: f64,
}

impl Shared {
    pub fn new(width: u32, height: u32, scale: f64) -> Self {
        Self {
            frame: Frame::new(width, height),
            windows: Vec::new(),
            pointer: (width as f64 / 2.0, height as f64 / 2.0),
            cursor_visible: true,
            scale,
        }
    }
}

pub type SharedHandle = Arc<Mutex<Shared>>;

#[derive(Debug, Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub(crate) struct Window {
    pub(crate) id: u64,
    pub(crate) toplevel: ToplevelSurface,
    pub(crate) title: String,
    pub(crate) app_id: String,
    pub(crate) mapped: bool,
    /// Pixels copied out of the last committed buffer, so a moving cursor does
    /// not force a full copy of every window.
    pub(crate) pixels: Option<CopyResult>,
    /// Set on every surface commit; cleared once the new buffer was copied and
    /// released (releasing twice would confuse the client).
    pub(crate) dirty: bool,
}

pub struct State {
    pub(crate) display_handle: DisplayHandle,
    pub(crate) compositor_state: CompositorState,
    pub(crate) shm_state: ShmState,
    pub(crate) xdg_shell_state: XdgShellState,
    pub(crate) seat_state: SeatState<State>,
    pub(crate) seat: Seat<State>,
    pub(crate) output: Output,
    #[allow(dead_code)]
    pub(crate) output_manager_state: OutputManagerState,
    pub(crate) shared: SharedHandle,
    pub(crate) windows: Vec<Window>,
    pub(crate) next_id: u64,
    pub(crate) focused: Option<usize>,
    pub(crate) pointer: Point<f64, Logical>,
    pub(crate) start: Instant,
    pub(crate) size: (u32, u32),
    pub(crate) scale: f64,
    pub(crate) dirty: bool,
    pub(crate) keyboard_focus: Option<WlSurface>,
    pub(crate) popups: Vec<PopupSurface>,
    pub(crate) shutdown: bool,
}

impl State {
    pub fn new(
        display_handle: &DisplayHandle,
        width: u32,
        height: u32,
        scale: f64,
        refresh_hz: u32,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(display_handle);
        let shm_state = ShmState::new::<Self>(display_handle, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(display_handle);

        let output = Output::new(
            "samlator-0".to_string(),
            PhysicalProperties {
                // A 6.7" panel is roughly 70x156mm; close enough for DPI hints.
                size: (70, 156).into(),
                subpixel: Subpixel::Unknown,
                make: "Samsung".into(),
                model: "S24 FE".into(),
            },
        );
        let mode = Mode {
            size: (width as i32, height as i32).into(),
            refresh: (refresh_hz.max(1) as i32) * 1000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Fractional(scale)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        let _ = output.create_global::<Self>(display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(display_handle, "seat-0");
        seat.add_pointer();
        let _ = seat.add_keyboard(Default::default(), 250, 35);

        let shared = Arc::new(Mutex::new(Shared::new(width, height, scale)));

        Self {
            display_handle: display_handle.clone(),
            compositor_state,
            shm_state,
            xdg_shell_state,
            seat_state,
            seat,
            output,
            output_manager_state,
            shared,
            windows: Vec::new(),
            next_id: 1,
            focused: None,
            pointer: (width as f64 / 2.0, height as f64 / 2.0).into(),
            start: Instant::now(),
            size: (width, height),
            scale,
            dirty: true,
            keyboard_focus: None,
            popups: Vec::new(),
            shutdown: false,
        }
    }

    pub fn shared(&self) -> SharedHandle {
        self.shared.clone()
    }

    pub(crate) fn time_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub(crate) fn screen_rect(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.size.1)
    }

    /// Topmost window under a logical pointer position.
    fn window_under(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let rect = self.screen_rect();
        if !rect.contains(pos.x as i32, pos.y as i32) {
            return None;
        }
        // All windows are maximised in v0.1, so "under" is simply the focused one.
        if self.windows.is_empty() {
            None
        } else {
            self.focused.or(Some(self.windows.len() - 1))
        }
    }

    pub(crate) fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Shutdown => self.shutdown = true,
            Command::Motion { x, y } => {
                let clamped = Point::from((
                    x.clamp(0.0, self.size.0 as f64 - 1.0),
                    y.clamp(0.0, self.size.1 as f64 - 1.0),
                ));
                self.pointer = clamped;
                let focus = self
                    .window_under(clamped)
                    .and_then(|i| self.windows.get(i))
                    .map(|w| w.toplevel.wl_surface().clone());
                if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    let event = MotionEvent {
                        location: clamped,
                        serial,
                        time: self.time_ms(),
                    };
                    // Every window is maximised at the origin, so the surface
                    // local position is the screen position. Passing (0, 0) here
                    // would send every click to the client's top-left corner.
                    let local: Point<f64, Logical> = clamped;
                    pointer.motion(self, focus.map(|s| (s, local)), &event);
                    pointer.frame(self);
                }
                self.dirty = true;
            }
            Command::Button { button, pressed } => {
                self.focus_at_pointer();
                if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.button(
                        self,
                        &ButtonEvent {
                            serial,
                            time: self.time_ms(),
                            button,
                            state: if pressed {
                                smithay::backend::input::ButtonState::Pressed
                            } else {
                                smithay::backend::input::ButtonState::Released
                            },
                        },
                    );
                    pointer.frame(self);
                }
            }
            Command::Axis { vertical, horizontal } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    let time = self.time_ms();
                    let mut frame = AxisFrame::new(time);
                    if vertical != 0.0 {
                        frame = frame.value(smithay::backend::input::Axis::Vertical, vertical);
                    }
                    if horizontal != 0.0 {
                        frame = frame.value(smithay::backend::input::Axis::Horizontal, horizontal);
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            Command::Key { code, pressed } => {
                if self.keyboard_focus.is_none() {
                    self.focus_at_pointer();
                }
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let serial = SERIAL_COUNTER.next_serial();
                    let state = if pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    keyboard.input::<(), _>(
                        self,
                        Keycode::new(code),
                        state,
                        serial,
                        self.time_ms(),
                        |_, _, _| FilterResult::Forward,
                    );
                }
                self.dirty = true;
            }
            Command::Resize { width, height, scale } => {
                if width == 0 || height == 0 {
                    return;
                }
                self.size = (width, height);
                self.scale = scale;
                let mode = Mode {
                    size: (width as i32, height as i32).into(),
                    refresh: 60000,
                };
                self.output.change_current_state(
                    Some(mode),
                    None,
                    Some(Scale::Fractional(scale)),
                    None,
                );
                self.output.set_preferred(mode);
                for idx in 0..self.windows.len() {
                    self.configure_window(idx);
                }
                if let Ok(mut shared) = self.shared.lock() {
                    shared.frame.resize(width, height);
                    shared.scale = scale;
                    shared.frame.bump();
                }
                self.dirty = true;
            }
            Command::Focus { index } => {
                if index < self.windows.len() {
                    self.focused = Some(index);
                    self.give_keyboard_focus();
                    self.dirty = true;
                }
            }
            Command::CloseWindow { index } => {
                if let Some(win) = self.windows.get(index) {
                    win.toplevel.send_close();
                }
                self.dirty = true;
            }
            Command::Screenshot(tx) => {
                if let Ok(shared) = self.shared.lock() {
                    let _ = tx.send(shared.frame.clone());
                }
            }
        }
    }

    fn focus_at_pointer(&mut self) {
        if let Some(idx) = self.window_under(self.pointer) {
            self.focused = Some(idx);
            self.give_keyboard_focus();
        }
    }

    fn give_keyboard_focus(&mut self) {
        let target = self
            .focused
            .and_then(|i| self.windows.get(i))
            .map(|w| w.toplevel.wl_surface().clone());
        self.keyboard_focus = target.clone();
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, target, serial);
        }
    }

    pub(crate) fn configure_window(&mut self, index: usize) {
        let (w, h) = self.size;
        let Some(win) = self.windows.get(index) else { return };
        win.toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel_state_maximized());
            state.size = Some((w as i32, h as i32).into());
            state.bounds = Some((w as i32, h as i32).into());
        });
        win.toplevel.send_configure();
    }

    /// Rebuild the shared frame from the current window buffers.
    pub(crate) fn composite(&mut self) {
        let Ok(mut shared) = self.shared.lock() else { return };
        if shared.frame.width != self.size.0 || shared.frame.height != self.size.1 {
            shared.frame.resize(self.size.0, self.size.1);
        }
        shared.frame.clear_transparent();
        let area = Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1));
        draw_shadow(&mut shared.frame, area, 6);

        for win in self.windows.iter_mut() {
            if !win.mapped || !win.toplevel.alive() {
                continue;
            }
            let target = Rect::new(0, 0, shared.frame.width, shared.frame.height);
            if win.dirty {
                let surface = win.toplevel.wl_surface().clone();
                // Copy the shm buffer while the client state is borrowed, then
                // release it. Nothing is held across the borrow.
                let copied = with_states(&surface, |states| {
                    let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                    let current = attrs.current();
                    let Some(BufferAssignment::NewBuffer(buffer)) = current.buffer.as_ref() else {
                        return None;
                    };
                    let out = copy_buffer(buffer);
                    // Without this the client never learns the buffer is free
                    // again and stalls after the second frame.
                    buffer.release();
                    out
                });
                if copied.is_some() {
                    win.pixels = copied;
                    win.dirty = false;
                }
            }
            if let Some(src) = win.pixels.as_ref() {
                blit_argb(
                    &mut shared.frame,
                    target,
                    &src.bytes,
                    src.width,
                    src.height,
                    src.stride,
                    src.opaque,
                );
            }
        }

        if shared.cursor_visible {
            let scale = (self.scale.round() as u32).max(1);
            let (px, py) = (shared.pointer.0 as i32, shared.pointer.1 as i32);
            draw_cursor(&mut shared.frame, px, py, scale);
        }
        shared.frame.bump();

        shared.windows = self
            .windows
            .iter()
            .enumerate()
            .map(|(i, w)| WindowInfo {
                id: w.id,
                title: w.title.clone(),
                app_id: w.app_id.clone(),
                mapped: w.mapped,
                focused: self.focused == Some(i),
            })
            .collect();
        shared.pointer = (self.pointer.x, self.pointer.y);
    }

    pub(crate) fn tick(&mut self) {
        if self.dirty {
            self.composite();
            self.dirty = false;
        }
    }
}

pub(crate) struct CopyResult {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
    opaque: bool,
}

fn copy_buffer(buffer: &WlBuffer) -> Option<CopyResult> {
    with_buffer_contents(buffer, |ptr, len, data| {
        if ptr.is_null() || len == 0 {
            return None;
        }
        let start = data.offset.max(0) as usize;
        if start >= len {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(start), len - start) }.to_vec();
        Some(CopyResult {
            bytes,
            width: data.width.max(0) as u32,
            height: data.height.max(0) as u32,
            stride: data.stride.max(0) as u32,
            opaque: data.format == wl_shm::Format::Xrgb8888,
        })
    })
    .ok()
    .flatten()
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        // wayland-server stores client data behind Arc; this is the documented
        // pattern but cannot be constructed lazily, so fall back to a leaked one.
        Box::leak(Box::new(CompositorClientState::default()))
    }

    fn commit(&mut self, surface: &WlSurface) {
        for (idx, win) in self.windows.iter().enumerate() {
            if win.toplevel.wl_surface() == surface {
                if !win.mapped && win.toplevel.alive() {
                    if let Some(w) = self.windows.get_mut(idx) {
                        w.mapped = true;
                    }
                    if self.focused.is_none() {
                        self.focused = Some(idx);
                        self.give_keyboard_focus();
                    }
                }
                if let Some(w) = self.windows.get_mut(idx) {
                    w.dirty = true;
                }
                self.dirty = true;
                return;
            }
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for State {
    fn output_bound(&mut self, _output: Output, _wl_output: wl_output::WlOutput) {}
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        self.keyboard_focus = focused.cloned();
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        let (title, app_id) = xdg_surface_labels(&surface);
        let title = if title.is_empty() { format!("Window {id}") } else { title };
        tracing::info!("new toplevel #{id} {:?} app_id={:?}", title, app_id);
        self.windows.push(Window {
            id,
            toplevel: surface,
            title,
            app_id,
            mapped: false,
            pixels: None,
            dirty: true,
        });
        let index = self.windows.len() - 1;
        self.focused = Some(index);
        self.configure_window(index);
        self.dirty = true;
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // v0.1 composites popups out of band; keep them alive so menus do not
        // desync, and log so configures can be added next milestone.
        tracing::debug!("popup created (not yet composited)");
        self.popups.push(surface);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: smithay::utils::Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        let _ = surface.send_configure();
        surface.send_repositioned(token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.windows.retain(|w| w.toplevel != surface);
        if self.focused.map(|i| i >= self.windows.len()).unwrap_or(false) {
            self.focused = self.windows.len().checked_sub(1);
            self.give_keyboard_focus();
        }
        self.dirty = true;
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(idx) = self.windows.iter().position(|w| w.toplevel == surface) {
            self.configure_window(idx);
        }
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);

/// `xdg_toplevel::State::Maximized`, resolved through the protocol re-export so
/// smithay's private module stays an implementation detail.
fn xdg_toplevel_state_maximized(
) -> smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State {
    smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized
}

/// Title and app_id live in the surface's role data, not on the surface handle.
fn xdg_surface_labels(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return (String::new(), String::new());
        };
        let Ok(guard) = data.lock() else {
            return (String::new(), String::new());
        };
        (
            guard.title.clone().unwrap_or_default(),
            guard.app_id.clone().unwrap_or_default(),
        )
    })
}

/// Helper for tests and for callers that need the logical output rectangle.
pub fn output_rect(width: u32, height: u32) -> Rectangle<i32, Logical> {
    Rectangle::from_size((width as i32, height as i32).into())
}

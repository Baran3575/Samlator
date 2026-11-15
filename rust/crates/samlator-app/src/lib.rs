//! Samlator — a lean Linux/ARM64 desktop for Android.
//!
//! There is no Java, no X server, no VNC and no x86 emulation in the data path:
//! a Rust Wayland compositor hands frames to the Android surface, and guest
//! programs run **natively** inside a Debian arm64 rootfs via `proot-rs`.
//!
//! `samlator-core` holds provisioning/supervision, `samlator-compositor` the
//! desktop engine; this crate is the Android front end.

pub mod boot;
pub mod gpu;
pub mod keyboard;
mod pointer;
pub mod runtime;
#[cfg(feature = "safe-boot")]
pub mod safe;
pub mod ui;

/// Uygulama giriş noktası; NativeActivity tarafından çağrılır.
#[cfg(target_os = "android")]
pub use android_entry::android_main;

#[cfg(target_os = "android")]
mod android_entry {
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::pointer;

    use android_activity::input::{Axis, KeyAction, MotionAction};
    use android_activity::input::InputEvent;
    use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};

    use crate::boot;
    use crate::gpu::Gpu;
    use crate::keyboard::KeyRequest;
    use crate::runtime::Runtime;
    use crate::ui::{self, Action};
    use samlator_compositor::POINTER_BUTTON_LEFT;
    use samlator_core::config::Config;
    use samlator_core::paths::Paths;

    /// Android key code -> Linux evdev, for keyboards that report no scan code.
    fn android_to_evdev(key_code: i32, scan_code: i32) -> Option<u32> {
        if scan_code > 0 {
            return Some(scan_code as u32);
        }
        let code = match key_code {
            66 => 28,   // ENTER
            67 => 14,   // DEL
            111 => 1,   // ESCAPE
            61 => 15,   // TAB
            62 => 57,   // SPACE
            19 => 103,  // DPAD_UP
            20 => 108,  // DPAD_DOWN
            21 => 105,  // DPAD_LEFT
            22 => 106,  // DPAD_RIGHT
            113 => 29,  // CTRL_LEFT
            59 => 42,   // SHIFT_LEFT
            57 => 56,   // ALT_LEFT
            117 => 125, // META_LEFT
            122 => 102, // MOVE_HOME
            123 => 107, // MOVE_END
            112 => 111, // FORWARD_DEL
            _ => return None,
        };
        Some(code)
    }

    /// Entry point the Android glue calls on its dedicated thread.
    ///
    /// Everything runs inside `catch_unwind`: if the desktop cannot start we
    /// must not let the Activity vanish without a trace — a silent close is
    /// impossible to diagnose on a phone without `adb`.
    #[no_mangle]
    pub fn android_main(app: AndroidApp) {
        // Deliberately first and dependency-free: this is the proof that the
        // native entry point ran at all.
        let early_base = app
            .internal_data_path()
            .unwrap_or_else(|| PathBuf::from("/data/local/tmp/samlator"));
        boot::mark_raw(&early_base, "MARK android_main entered");

        samlator_core::init_logging("Samlator");
        boot::install_panic_hook();
        boot::mark_raw(&early_base, "MARK logging ready");

        let guardian = app.clone();
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(app))) {
            let why = boot::describe_panic(&payload);
            let base = guardian
                .internal_data_path()
                .unwrap_or_else(|| PathBuf::from("/data/local/tmp/samlator"));
            boot::note_path_buf(&base, &format!("FATAL android_main: {why}"));
            stay_alive(guardian);
        }
    }

    /// Keep the process (and so the screen) alive after a fatal error, so the
    /// launcher does not look like it "closed by itself".
    fn stay_alive(app: AndroidApp) {
        loop {
            let mut destroy = false;
            app.poll_events(Some(Duration::from_millis(250)), |event| {
                if let PollEvent::Main(MainEvent::Destroy) = event {
                    destroy = true;
                }
            });
            if destroy {
                return;
            }
        }
    }

    #[cfg(feature = "safe-boot")]
    fn run(app: AndroidApp) {
        let early_app = app.clone();
        crate::safe::run(app)
    }

    #[cfg(not(feature = "safe-boot"))]
    fn run(app: AndroidApp) {
        let early_app = app.clone();

        let base = app
            .internal_data_path()
            .unwrap_or_else(|| PathBuf::from("/data/local/tmp/samlator"));
        // The panic hook runs before the app has a `Paths`, so publish it first.
        std::env::set_var("SAMLATOR_HOME", &base);
        if let Some(ext) = app.external_data_path() {
            std::env::set_var("SAMLATOR_EXT", &ext);
        }
        boot::note_path_buf(&base, "android_main: start");

        let paths = Paths::resolve(Some(&base));
        boot::note(&paths, format!("data dir: {}", paths.base().display()));
        let config = Config::load_or_default(&paths.config_file());
        boot::note(&paths, "config loaded");
        let mut rt = Runtime::new(paths, config);
        boot::note(&rt.paths, "runtime ready");
        rt.push_log(format!("Samlator {} başladı", env!("CARGO_PKG_VERSION")));
        rt.push_log(format!("veri dizini: {}", rt.paths.base().display()));
        // The compositor's seat needs XKB rules on the filesystem.
        if let Some(xkb) = boot::ensure_xkb(&early_app, &rt.paths) {
            std::env::set_var("XKB_CONFIG_ROOT", &xkb);
        }

        let mut ui_state = ui::UiState::default();
        let mut pointer = pointer::Pointer::default();
        if let Some(previous) = boot::take_last_panic() {
            rt.push_log(format!("önceki çökme: {previous}"));
            ui_state.show_logs = true;
        }
        let mut gpu: Option<Gpu> = None;
        // Landscape by default; real values arrive with InitWindow.
        let mut size = (2340u32, 1080u32);
        let mut scale = 2.625f32;
        let mut quit = false;
        let mut setup_started = false;
        let mut gpu_error: Option<String> = None;
        let mut last_frame_seq = 0u64;

        while !quit {
            let mut need_redraw = false;
            let mut input_available = false;

            app.poll_events(Some(Duration::from_millis(16)), |event| match event {
                PollEvent::Main(MainEvent::InitWindow { .. })
                | PollEvent::Main(MainEvent::WindowResized { .. }) => need_redraw = true,
                PollEvent::Main(MainEvent::TerminateWindow { .. }) => gpu = None,
                PollEvent::Main(MainEvent::RedrawNeeded { .. }) => need_redraw = true,
                PollEvent::Main(MainEvent::InputAvailable) => input_available = true,
                PollEvent::Main(MainEvent::Destroy) => quit = true,
                _ => {}
            });

            if let Some(window) = app.native_window() {
                let w = window.width().max(1) as u32;
                let h = window.height().max(1) as u32;
                if (w, h) != size {
                    size = (w, h);
                    // Scale from the short edge so the UI looks the same in both
                    // orientations (411dp is the S24 FE's logical short side).
                    scale = (w.min(h) as f32 / 411.0).max(1.0);
                    boot::note(&rt.paths, format!("surface {w}x{h} @ scale {scale:.3}"));
                    if let Some(g) = gpu.as_mut() {
                        g.resize(w, h, scale);
                    }
                    if let Some(c) = rt.compositor.as_ref() {
                        c.set_display(w, h, scale as f64);
                    }
                }
                if gpu.is_none() && gpu_error.is_none() {
                    let target = wgpu::SurfaceTarget::from_window_without_display(window.clone());
                    let (w0, h0) = size;
                    let ppp = scale;
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Gpu::new(target, w0, h0, ppp)
                    })) {
                        Ok(Ok(g)) => {
                            boot::note(&rt.paths, "gpu ready");
                            gpu = Some(g);
                        }
                        Ok(Err(why)) => {
                            boot::note(&rt.paths, format!("gpu failed: {why}"));
                            rt.push_log(format!("GPU yok: {why}"));
                            gpu_error = Some(why);
                        }
                        Err(panic) => {
                            let why = boot::describe_panic(&panic);
                            boot::note(&rt.paths, format!("gpu panic: {why}"));
                            gpu_error = Some(format!("GPU hatası: {why}"));
                            rt.push_log(format!("GPU hatası: {why}"));
                        }
                    }
                }
            }

            let mut events: Vec<egui::Event> = Vec::new();
            if input_available {
                if let Ok(mut iter) = app.input_events_iter() {
                    loop {
                        let handled = iter.next(|event| {
                            match event {
                                InputEvent::MotionEvent(m) => {
                                    let fingers = m.pointer_count().max(1);
                                    let first = m.pointer_at_index(0);
                                    let (px, py) = (first.raw_x() as f64, first.raw_y() as f64);
                                    let mut mid_y = 0.0;
                                    for i in 0..fingers {
                                        mid_y += m.pointer_at_index(i).raw_y() as f64;
                                    }
                                    mid_y /= fingers as f64;

                                    let (tx, ty) = pointer.points(scale);
                                    ui_state.pointer = (tx as f64, ty as f64);
                                    // The on-screen keyboard covers the bottom half;
                                    // touches there belong to egui, not the guest.
                                    let keyboard_band = ui_state.keyboard_visible
                                        && ty > (size.1 as f32 / scale) * 0.5;
                                    let mut acted = Vec::new();
                                    match m.action() {
                                        MotionAction::Down => {
                                            events.push(egui::Event::PointerButton {
                                                pos: egui::pos2(tx, ty),
                                                button: egui::PointerButton::Primary,
                                                pressed: true,
                                                modifiers: egui::Modifiers::default(),
                                            });
                                            if keyboard_band {
                                                pointer.cancel();
                                            } else if fingers >= 2 {
                                                acted.extend(pointer.two_finger(mid_y));
                                            } else {
                                                acted.extend(pointer.down(px, py));
                                            }
                                        }
                                        MotionAction::Up | MotionAction::Cancel => {
                                            events.push(egui::Event::PointerButton {
                                                pos: egui::pos2(tx, ty),
                                                button: egui::PointerButton::Primary,
                                                pressed: false,
                                                modifiers: egui::Modifiers::default(),
                                            });
                                            acted.extend(pointer.up(px, py));
                                        }
                                        MotionAction::Move | MotionAction::HoverMove => {
                                            events.push(egui::Event::PointerMoved(egui::pos2(tx, ty)));
                                            if fingers >= 2 {
                                                acted.extend(pointer.two_finger(mid_y));
                                            } else if !keyboard_band {
                                                acted.extend(pointer.motion(px, py));
                                            }
                                        }
                                        MotionAction::Scroll => {
                                            let delta = first.axis_value(Axis::Vscroll) as f64;
                                            events.push(egui::Event::MouseWheel {
                                                unit: egui::MouseWheelUnit::Point,
                                                delta: egui::vec2(0.0, delta as f32 * scale),
                                                phase: egui::TouchPhase::Move,
                                                source: egui::MouseWheelSource::Unknown,
                                                modifiers: egui::Modifiers::default(),
                                            });
                                            acted.push(pointer::Action::Scroll { dy: -delta * 20.0 });
                                        }
                                        _ => {}
                                    }
                                    for action in acted {
                                        match action {
                                            pointer::Action::Motion { x, y } => rt.pointer_motion(x, y),
                                            pointer::Action::Button { pressed } => {
                                                if keyboard_band {
                                                    continue;
                                                }
                                                rt.pointer_button(POINTER_BUTTON_LEFT, pressed);
                                            }
                                            pointer::Action::Scroll { dy } => {
                                                rt.scroll(dy / scale as f64 / 4.0)
                                            }
                                        }
                                    }
                                }
                                InputEvent::KeyEvent(k) => {
                                    let code: u32 = k.key_code().into();
                                    if let Some(evdev) = android_to_evdev(code as i32, k.scan_code()) {
                                        rt.key(evdev, k.action() == KeyAction::Down);
                                    }
                                }
                                _ => {}
                            }
                            InputStatus::Handled
                        });
                        if !handled {
                            break;
                        }
                    }
                }
            }

            rt.poll();
            rt.ensure_compositor(size.0, size.1, scale as f64);

            let mut new_frame = false;
            if let Some(frame) = rt.compositor.as_ref().and_then(|c| c.frame()) {
                if frame.seq != last_frame_seq {
                    last_frame_seq = frame.seq;
                    new_frame = true;
                }
                if let Some(g) = gpu.as_mut() {
                    g.upload(&frame);
                }
            }

            if let Some(g) = gpu.as_mut() {
                // Painting every frame would idle the GPU at 60 Hz for nothing.
                if need_redraw || input_available || rt.is_busy() || new_frame {
                    let raw = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(size.0 as f32 / scale, size.1 as f32 / scale),
                        )),
                        events,
                        ..Default::default()
                    };
                    let action = g.render(&mut rt, &mut ui_state, raw).unwrap_or(Action::None);

                    // Keys the on-screen keyboard produced this frame. Latched
                    // modifiers have to be pressed *around* the key, otherwise a
                    // Ctrl+click style chord never reaches the guest.
                    let modifiers = ui_state.mods.active_codes();
                    for (request, pressed) in ui_state.keys.drain(..) {
                        let Some(code) = (match request {
                            KeyRequest::Char(c) => crate::keyboard::code_for_char(c),
                            KeyRequest::Key(code) => Some(code),
                        }) else {
                            continue;
                        };
                        if pressed {
                            for m in &modifiers {
                                rt.key(*m, true);
                            }
                            rt.key(code, true);
                        } else {
                            rt.key(code, false);
                            for m in &modifiers {
                                rt.key(*m, false);
                            }
                        }
                    }

                    // Provision only once the desktop is visibly running, so a
                    // download problem can never keep the window from appearing.
                    if !setup_started && gpu.is_some() && !rt.is_ready() {
                        setup_started = true;
                        rt.start_setup();
                    }

                    match action {
                        Action::Setup => rt.start_setup(),
                        Action::Launch(id) => rt.launch(&id),
                        Action::StopSession => rt.stop_session(),
                        Action::ToggleSettings => {
                            ui_state.show_settings = !ui_state.show_settings
                        }
                        Action::ToggleLogs => ui_state.show_logs = !ui_state.show_logs,
                        Action::Reset => {
                            let _ = std::fs::remove_file(rt.paths.installed_marker());
                            rt.push_log("kurulum işareti silindi; yeniden kurulum gerekli".into());
                        }
                        Action::Quit => quit = true,
                        Action::None => {}
                    }
                }
            }
        }

        boot::note(&rt.paths, "android_main: exit");
        rt.stop_session();
    }

}

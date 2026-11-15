//! Minimal "safe boot" front end, enabled with the `safe-boot` feature.
//!
//! It touches no compositor, no guest session and no provisioning — only the
//! Android surface and egui. If the normal build dies while this one stays up,
//! the fault is in the desktop stack, not in the native entry point.

use std::time::Duration;

use android_activity::{AndroidApp, MainEvent, PollEvent};

use crate::boot;
use crate::gpu::Gpu;

pub fn run(app: AndroidApp) {
    let base = app
        .internal_data_path()
        .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp/samlator"));
    std::env::set_var("SAMLATOR_HOME", &base);
    if let Some(ext) = app.external_data_path() {
        std::env::set_var("SAMLATOR_EXT", &ext);
    }
    boot::note_path_buf(&base, "safe-boot: android_main start");

    let mut gpu: Option<Gpu> = None;
    let mut size = (2340u32, 1080u32);
    let mut note = String::from("safe-boot: ekran hazır");
    let mut quit = false;
    let mut tried_gpu = false;

    while !quit {
        let mut redraw = false;
        app.poll_events(Some(Duration::from_millis(16)), |event| match event {
            PollEvent::Main(MainEvent::InitWindow { .. })
            | PollEvent::Main(MainEvent::WindowResized { .. })
            | PollEvent::Main(MainEvent::RedrawNeeded { .. }) => redraw = true,
            PollEvent::Main(MainEvent::TerminateWindow { .. }) => gpu = None,
            PollEvent::Main(MainEvent::Destroy) => quit = true,
            _ => {}
        });

        if let Some(window) = app.native_window() {
            let (w, h) = (window.width().max(1) as u32, window.height().max(1) as u32);
            if (w, h) != size {
                size = (w, h);
                if let Some(g) = gpu.as_mut() {
                    g.resize(w, h, (w.min(h) as f32 / 411.0).max(1.0));
                }
                boot::note_path_buf(&base, &format!("safe-boot: surface {w}x{h}"));
            }
            if gpu.is_none() && !tried_gpu {
                tried_gpu = true;
                let target = wgpu::SurfaceTarget::from_window_without_display(window.clone());
                let ppp = (w.min(h) as f32 / 411.0).max(1.0);
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Gpu::new(target, size.0, size.1, ppp)
                })) {
                    Ok(Ok(g)) => {
                        gpu = Some(g);
                        note = format!("safe-boot: GPU hazır ({w}x{h})");
                        boot::note_path_buf(&base, "safe-boot: gpu ready");
                    }
                    Ok(Err(why)) => {
                        note = format!("GPU yok: {why}");
                        boot::note_path_buf(&base, &format!("safe-boot: gpu failed: {why}"));
                    }
                    Err(p) => {
                        let why = boot::describe_panic(&p);
                        note = format!("safe-boot: GPU panic — {why}");
                        boot::note_path_buf(&base, &format!("safe-boot: gpu panic {why}"));
                    }
                }
            }
        }

        if let Some(g) = gpu.as_mut() {
            if redraw || !note.is_empty() {
                let scale = (size.0.min(size.1) as f32 / 411.0).max(1.0);
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(size.0 as f32 / scale, size.1 as f32 / scale),
                    )),
                    ..Default::default()
                };
                let text = note.clone();
                let _ = g.render_raw(raw, move |ctx, _root| {
                    egui::Area::new(egui::Id::new("safe".to_owned()))
                        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                        .show(ctx, |ui| {
                            ui.label(egui::RichText::new(text.clone()).size(20.0));
                        });
                });
                note.clear();
            }
        }
    }
    boot::note_path_buf(&base, "safe-boot: exit");
}

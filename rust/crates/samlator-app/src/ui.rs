//! The desktop UI: top bar, dock, setup wizard, settings and logs.
//!
//! Rendered with egui straight into the Android surface. Deliberately plain: no
//! icon fonts, no theme engine, no extra asset pipeline.

use egui::{Color32, RichText};

use crate::runtime::{Runtime, Stage};

/// What the UI wants the host app to do after this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Setup,
    Launch(String),
    StopSession,
    Reset,
    ToggleLogs,
    ToggleSettings,
    Quit,
}

use crate::keyboard::{self, KeyRequest, Modifiers};

#[derive(Default)]
pub struct UiState {
    pub show_settings: bool,
    pub show_logs: bool,
    pub show_setup: bool,
    /// Last measured pointer position, shared with the compositor.
    pub pointer: (f64, f64),
    /// On-screen keyboard state.
    pub keyboard_visible: bool,
    pub mods: Modifiers,
    /// Key events produced by the on-screen keyboard this frame.
    pub keys: Vec<(KeyRequest, bool)>,
}

const BG_BAR: Color32 = Color32::from_rgb(18, 18, 24);
const BG_DOCK: Color32 = Color32::from_rgb(22, 22, 30);
const ACCENT: Color32 = Color32::from_rgb(120, 190, 255);
const OK: Color32 = Color32::from_rgb(110, 210, 150);
const WARN: Color32 = Color32::from_rgb(240, 170, 90);
const ERR: Color32 = Color32::from_rgb(240, 110, 110);
const TEXT: Color32 = Color32::from_rgb(230, 232, 240);
const DIM: Color32 = Color32::from_rgb(150, 154, 168);

pub fn draw(root: &mut egui::Ui, rt: &mut Runtime, ui_state: &mut UiState) -> Action {
    let mut action = Action::None;
    let ctx = root.ctx().clone();

    let screen = root.max_rect();

    // ---- top bar -----------------------------------------------------------
    egui::Panel::top("samlator-bar")
        .frame(egui::Frame::NONE.fill(BG_BAR))
        .show(root, |ui| {
            ui.set_width(screen.width());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("Samlator").strong().color(TEXT).size(18.0));
                ui.add_space(8.0);
                let (chip, color) = status_chip(rt);
                ui.label(RichText::new(chip).color(color).size(12.0));
                ui.add_space(6.0);
                if !rt.is_ready() && !rt.is_busy() {
                    if ui.button(RichText::new("Kur").color(ACCENT)).clicked() {
                        action = Action::Setup;
                    }
                }
                if rt.is_busy() {
                    ui.add(egui::ProgressBar::new(rt.progress).desired_width(160.0));
                    ui.label(RichText::new(&rt.detail).color(DIM).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    if ui.button("Çıkış").clicked() {
                        action = Action::Quit;
                    }
                    if ui.button("Log").clicked() {
                        action = Action::ToggleLogs;
                    }
                    if ui.button("Ayarlar").clicked() {
                        action = Action::ToggleSettings;
                    }
                    ui.label(RichText::new(backend_name(rt)).color(DIM).size(11.0));
                });
            });
            ui.add_space(6.0);
        });

    // ---- dock --------------------------------------------------------------
    egui::Panel::bottom("samlator-dock")
        .frame(egui::Frame::NONE.fill(BG_DOCK))
        .show(root, |ui| {
            ui.set_width(screen.width());
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(8.0);
                let apps: Vec<(String, String)> = rt
                    .config
                    .apps
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect();
                for (id, name) in apps {
                    let enabled = rt.is_ready() && rt.session_label.is_none();
                    let btn = egui::Button::new(RichText::new(&name).color(if enabled { TEXT } else { DIM }))
                        .fill(Color32::from_rgb(38, 38, 52));
                    if ui.add_enabled(enabled, btn).clicked() {
                        action = Action::Launch(id);
                    }
                }
                if rt.session_label.is_some() && ui.button("Durdur").clicked() {
                    action = Action::StopSession;
                }
                let kb_label = if ui_state.keyboard_visible { "⌨ kapat" } else { "⌨ klavye" };
                if ui.button(kb_label).clicked() {
                    ui_state.keyboard_visible = !ui_state.keyboard_visible;
                }
                ui.separator();
                for w in rt
                    .compositor
                    .as_ref()
                    .map(|c| c.windows())
                    .unwrap_or_default()
                {
                    if !w.mapped {
                        continue;
                    }
                    let label = if w.title.is_empty() { "pencere" } else { &w.title };
                    let text = RichText::new(format!("> {label}")).size(12.0).color(if w.focused { ACCENT } else { DIM });
                    ui.label(text);
                }
            });
            ui.add_space(6.0);
        });

    // ---- desktop: keep the pointer in sync with the compositor --------------
    if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
        ui_state.pointer = (pos.x as f64, pos.y as f64);
    }
    if rt.windows_empty_hint() {
        egui::Area::new(egui::Id::unique("samlator-hint"))
            .order(egui::Order::Background)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, clamp_hint_y(screen)))
            .show(&ctx, |ui| {
                ui.label(
                    RichText::new(if rt.is_ready() {
                        "Masaüstü hazır — dock'tan Antigravity'yi başlat"
                    } else {
                        "Kurulum gerekli — üst çubuktan «Kur»"
                    })
                    .color(DIM)
                    .size(15.0),
                );
            });
    }

    // ---- on-screen keyboard ------------------------------------------------
    if ui_state.keyboard_visible {
        let keys = keyboard::draw(&ctx, &mut ui_state.mods, &mut ui_state.keyboard_visible);
        ui_state.keys.extend(keys);
    }

    // ---- floating panels ---------------------------------------------------
    if ui_state.show_setup || (!rt.is_ready() && !rt.is_busy() && rt.stage != Stage::Ready) {
        if let Some(a) = setup_window(&ctx, rt, ui_state) {
            action = a;
        }
    }
    if ui_state.show_settings {
        if let Some(a) = settings_window(&ctx, rt, ui_state) {
            action = a;
        }
    }
    if ui_state.show_logs {
        logs_window(&ctx, rt, ui_state);
    }

    // Where egui believes the finger is. The guest gets its own cursor; this
    // dot is ours, and if it does not follow a touch the events never made it.
    if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
        egui::Area::new(egui::Id::unique("samlator-dot"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(false)
            .show(&ctx, |ui| {
                ui.label(RichText::new("+").color(ACCENT).size(18.0).strong());
            });
    }

    action
}

/// Keep the hint clear of the top bar on very tall screens.
fn clamp_hint_y(screen: egui::Rect) -> f32 {
    -(screen.height() * 0.5) + 90.0 + screen.height() * 0.5 - 120.0
}

fn status_chip(rt: &Runtime) -> (String, Color32) {
    if let Some(label) = &rt.session_label {
        return (format!("● {label} çalışıyor"), OK);
    }
    match rt.stage {
        Stage::Ready => ("● hazır".into(), OK),
        Stage::Failed => ("● hata".into(), ERR),
        Stage::Idle => ("● kurulmadı".into(), WARN),
        _ => (format!("● {}", rt.stage.label()), ACCENT),
    }
}

fn backend_name(rt: &Runtime) -> String {
    match rt.config.gpu {
        samlator_core::config::GpuBackend::AngleVulkan => "GPU: ANGLE/Vulkan".into(),
        samlator_core::config::GpuBackend::AngleGles => "GPU: ANGLE/GLES".into(),
        samlator_core::config::GpuBackend::SwiftShader => "GPU: SwiftShader".into(),
    }
}

fn setup_window(ctx: &egui::Context, rt: &mut Runtime, ui_state: &mut UiState) -> Option<Action> {
    let mut action = None;
    let mut open = true;
    egui::Window::new("Kurulum")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Samlator, telefonunuzda gerçek bir Linux ARM64 masaüstü açar.").color(TEXT));
            ui.add_space(4.0);
            ui.label(RichText::new("Windows emülasyonu yok: Antigravity'nin ARM64 sürümü doğrudan çalışır.").color(DIM).size(11.0));
            ui.add_space(8.0);
            ui.add(egui::ProgressBar::new(rt.progress).show_percentage());
            ui.label(RichText::new(rt.stage.label()).color(ACCENT).size(12.0));
            if !rt.detail.is_empty() {
                ui.label(RichText::new(&rt.detail).color(DIM).size(11.0));
            }
            if let Some(err) = &rt.error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).color(ERR).size(11.0));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let busy = rt.is_busy();
                if ui
                    .add_enabled(!busy, egui::Button::new("Kurulumu başlat").fill(Color32::from_rgb(40, 80, 130)))
                    .clicked()
                {
                    action = Some(Action::Setup);
                }
                if ui.add_enabled(!busy, egui::Button::new("Sıfırla")).clicked() {
                    action = Some(Action::Reset);
                }
            });
        });
    if !open {
        ui_state.show_setup = false;
    }
    action
}

fn settings_window(
    ctx: &egui::Context,
    rt: &mut Runtime,
    ui_state: &mut UiState,
) -> Option<Action> {
    let mut open = true;
    let mut action = None;
    egui::Window::new("Ayarlar")
        .open(&mut open)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_width(320.0);
            ui.label(RichText::new("Grafik arka ucu").color(TEXT).strong());
            let current = rt.config.gpu;
            for (backend, label, hint) in [
                (samlator_core::config::GpuBackend::AngleVulkan, "ANGLE / Vulkan", "Xclipse 940 üzerinde en hızlı, sürücüye duyarlı"),
                (samlator_core::config::GpuBackend::AngleGles, "ANGLE / GLES", "dengeli seçenek"),
                (samlator_core::config::GpuBackend::SwiftShader, "SwiftShader", "her zaman çalışır, en yavaş"),
            ] {
                if ui
                    .radio(current == backend, RichText::new(label).color(TEXT))
                    .on_hover_text(hint)
                    .clicked()
                {
                    rt.config.gpu = backend;
                    let _ = rt.config.save(&rt.paths.config_file());
                }
            }
            ui.separator();
            ui.label(RichText::new("Arayüz ölçeği").color(TEXT).strong());
            ui.add(egui::Slider::new(&mut rt.config.display.scale, 0.7..=2.0).fixed_decimals(2));
            ui.separator();
            ui.label(RichText::new("Çalışma dizini").color(TEXT).strong());
            ui.label(RichText::new(rt.paths.base().display().to_string()).color(DIM).size(10.0));
            ui.separator();
            if ui.button("Antigravity topluluğunu aç (tarayıcı)").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab("https://antigravity.google/download"));
            }
        });
    if !open {
        ui_state.show_settings = false;
    }
    action
}

fn logs_window(ctx: &egui::Context, rt: &Runtime, ui_state: &mut UiState) {
    let mut open = true;
    egui::Window::new("Log")
        .open(&mut open)
        .default_size([420.0, 300.0])
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in &rt.log {
                    ui.label(RichText::new(line).color(DIM).size(11.0).monospace());
                }
                ui.separator();
                for line in rt.tail_session_log().lines().rev().take(60).collect::<Vec<_>>().iter().rev() {
                    ui.label(RichText::new(*line).color(TEXT).size(10.0).monospace());
                }
            });
        });
    if !open {
        ui_state.show_logs = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use samlator_core::config::Config;
    use samlator_core::paths::Paths;

    #[test]
    fn ui_draws_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new(Paths::new(dir.path()), Config::default());
        let ctx = egui::Context::default();
        let mut state = UiState::default();

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1080.0, 2340.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |root| {
            let action = draw(root, &mut rt, &mut state);
            assert_eq!(action, Action::None);
        });
        assert!(!out.shapes.is_empty(), "expected the UI to paint something");
        // Unsent texture deltas are a bug in production, so the test retires them.
        out.textures_delta.clear();
    }

    #[test]
    fn ready_runtime_shows_ready_chip() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new(Paths::new(dir.path()), Config::default());
        rt.stage = Stage::Ready;
        let (chip, _) = status_chip(&rt);
        assert!(chip.contains("hazır"));
    }
}

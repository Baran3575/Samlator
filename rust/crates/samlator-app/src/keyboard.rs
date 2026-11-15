//! On-screen keyboard.
//!
//! Android's IME needs Java, and Samlator has no Java at all — so the desktop
//! ships its own keyboard that speaks Linux evdev codes. That is exactly what a
//! Wayland client expects, which means the same code path works for a physical
//! Bluetooth keyboard too.

use egui::{Color32, RichText};

/// Linux input-event codes (`linux/input-event-codes.h`).
pub mod evdev {
    pub const ESC: u32 = 1;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const ENTER: u32 = 28;
    pub const LEFT_CTRL: u32 = 29;
    pub const LEFT_SHIFT: u32 = 42;
    pub const LEFT_ALT: u32 = 56;
    pub const SPACE: u32 = 57;
    pub const CAPS_LOCK: u32 = 58;
    pub const LEFT_META: u32 = 125;
    pub const UP: u32 = 103;
    pub const DOWN: u32 = 108;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const HOME: u32 = 102;
    pub const END: u32 = 107;
    pub const DELETE: u32 = 111;
}

/// evdev code for a printable ASCII character, if any.
pub fn code_for_char(c: char) -> Option<u32> {
    let c = c.to_ascii_lowercase();
    // evdev codes follow the physical QWERTY rows, not the alphabet.
    const ROW1: [char; 10] = ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'];
    const ROW2: [char; 9] = ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'];
    const ROW3: [char; 7] = ['z', 'x', 'c', 'v', 'b', 'n', 'm'];
    if let Some(i) = ROW1.iter().position(|&k| k == c) {
        return Some(16 + i as u32);
    }
    if let Some(i) = ROW2.iter().position(|&k| k == c) {
        return Some(30 + i as u32);
    }
    if let Some(i) = ROW3.iter().position(|&k| k == c) {
        return Some(44 + i as u32);
    }
    let code = match c {
        '1'..='9' => 2 + (c as u32 - '1' as u32),
        '0' => 11,
        '-' => 12,
        '=' => 13,
        '[' => 26,
        ']' => 27,
        ';' => 39,
        '\'' => 40,
        '`' => 41,
        '\\' => 43,
        ',' => 51,
        '.' => 52,
        '/' => 53,
        ' ' => evdev::SPACE,
        _ => return None,
    };
    Some(code)
}

/// Modifier bits tracked between frames so a tap can send a full chord.
#[derive(Default, Clone, Copy)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Modifiers {
    pub fn active_codes(&self) -> Vec<u32> {
        let mut v = Vec::new();
        if self.ctrl {
            v.push(evdev::LEFT_CTRL);
        }
        if self.alt {
            v.push(evdev::LEFT_ALT);
        }
        if self.shift {
            v.push(evdev::LEFT_SHIFT);
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyRequest {
    /// Type this character (with current modifiers held).
    Char(char),
    /// Send a non-printable key once.
    Key(u32),
}

pub const ROWS: [&str; 4] = [
    "1234567890",
    "qwertyuiop",
    "asdfghjkl;",
    "zxcvbnm,./",
];

const BG: Color32 = Color32::from_rgb(26, 26, 34);
const KEY: Color32 = Color32::from_rgb(48, 48, 62);
const KEY_ACTIVE: Color32 = Color32::from_rgb(70, 110, 160);
const TEXT: Color32 = Color32::from_rgb(228, 230, 238);

/// Draw the keyboard and return what should be typed this frame.
pub fn draw(ctx: &egui::Context, mods: &mut Modifiers, visible: &mut bool) -> Vec<(KeyRequest, bool)> {
    let mut out = Vec::new();
    if !*visible {
        return out;
    }
    egui::Area::new(egui::Id::unique("samlator-keys"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(0.0, -74.0))
        .show(ctx, |ui| {
            egui::Frame::NONE.fill(BG).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                for row in ROWS {
                    for ch in row.chars() {
                        if key_button(ui, &ch.to_uppercase().to_string()).clicked() {
                            out.push((KeyRequest::Char(ch), true));
                            out.push((KeyRequest::Char(ch), false));
                        }
                    }
                    ui.add_space(6.0);
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                if toggle(ui, "SHIFT", mods.shift).clicked() {
                    mods.shift = !mods.shift;
                }
                if toggle(ui, "CTRL", mods.ctrl).clicked() {
                    mods.ctrl = !mods.ctrl;
                }
                if toggle(ui, "ALT", mods.alt).clicked() {
                    mods.alt = !mods.alt;
                }
                if key_button(ui, "TAB").clicked() {
                    out.push((KeyRequest::Key(evdev::TAB), true));
                    out.push((KeyRequest::Key(evdev::TAB), false));
                }
                if key_button(ui, "ESC").clicked() {
                    out.push((KeyRequest::Key(evdev::ESC), true));
                    out.push((KeyRequest::Key(evdev::ESC), false));
                }
                if ui.add(egui::Button::new(RichText::new("SPC").color(TEXT)).fill(KEY)).clicked() {
                    out.push((KeyRequest::Key(evdev::SPACE), true));
                    out.push((KeyRequest::Key(evdev::SPACE), false));
                }
                if key_button(ui, "BKSP").clicked() {
                    out.push((KeyRequest::Key(evdev::BACKSPACE), true));
                    out.push((KeyRequest::Key(evdev::BACKSPACE), false));
                }
                if key_button(ui, "⏎").clicked() {
                    out.push((KeyRequest::Key(evdev::ENTER), true));
                    out.push((KeyRequest::Key(evdev::ENTER), false));
                }
                if key_button(ui, "LEFT").clicked() {
                    send_arrow(&mut out, evdev::LEFT);
                }
                if key_button(ui, "↑").clicked() {
                    send_arrow(&mut out, evdev::UP);
                }
                if key_button(ui, "↓").clicked() {
                    send_arrow(&mut out, evdev::DOWN);
                }
                if key_button(ui, "RIGHT").clicked() {
                    send_arrow(&mut out, evdev::RIGHT);
                }
                if ui.add(egui::Button::new(RichText::new("gizle").color(TEXT)).fill(KEY)).clicked() {
                    *visible = false;
                }
            });
            ui.add_space(4.0);
            });
        });
    out
}

fn send_arrow(out: &mut Vec<(KeyRequest, bool)>, code: u32) {
    out.push((KeyRequest::Key(code), true));
    out.push((KeyRequest::Key(code), false));
}

fn key_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(TEXT).size(13.0))
            .fill(KEY)
            .min_size(egui::vec2(24.0, 30.0)),
    )
}

fn toggle(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(if on { Color32::WHITE } else { TEXT }).size(12.0))
            .fill(if on { KEY_ACTIVE } else { KEY })
            .min_size(egui::vec2(46.0, 30.0)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_evdev() {
        assert_eq!(code_for_char('a'), Some(30));
        assert_eq!(code_for_char('q'), Some(16));
        assert_eq!(code_for_char('z'), Some(44));
        assert_eq!(code_for_char('1'), Some(2));
        assert_eq!(code_for_char('0'), Some(11));
        assert_eq!(code_for_char(' '), Some(evdev::SPACE));
        assert_eq!(code_for_char('€'), None);
    }

    #[test]
    fn modifiers_emit_in_ctrl_alt_shift_order() {
        let m = Modifiers { shift: true, ctrl: true, alt: true };
        assert_eq!(
            m.active_codes(),
            vec![evdev::LEFT_CTRL, evdev::LEFT_ALT, evdev::LEFT_SHIFT]
        );
    }

    #[test]
    fn every_letter_on_the_rows_is_typeable() {
        for row in ROWS {
            for ch in row.chars() {
                assert!(
                    code_for_char(ch).is_some(),
                    "{ch} missing from the evdev table"
                );
            }
        }
    }
}

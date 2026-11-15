//! Touch → guest pointer translation.
//!
//! Two coordinate spaces are in play and mixing them up is what made the cursor
//! crawl: the guest compositor works in surface **pixels**, egui works in
//! **points** (dp, pixels / scale). Everything that crosses a boundary goes
//! through this module, in pixels, so there is exactly one place to get it
//! wrong.
//!
//! The state machine also owns the gesture decision: one finger drives the
//! pointer, two fingers scroll and must never leave a button held down.

/// Something the guest has to be told about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Motion { x: f64, y: f64 },
    Button { pressed: bool },
    Scroll { dy: f64 },
}

/// Pointer state in surface pixels.
#[derive(Debug, Default)]
pub struct Pointer {
    /// Last known position, in pixels.
    pub pos: (f64, f64),
    down: bool,
    /// Midpoint of the fingers while a two-finger scroll is in progress.
    scroll_from: Option<f64>,
}

impl Pointer {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_down(&self) -> bool {
        self.down
    }

    /// Convert surface pixels to egui points.
    pub fn points(&self, scale: f32) -> (f32, f32) {
        let s = scale.max(0.5);
        ((self.pos.0 as f32) / s, (self.pos.1 as f32) / s)
    }

    /// One finger pressed down: move there, then press.
    pub fn down(&mut self, x: f64, y: f64) -> Vec<Action> {
        self.pos = (x, y);
        self.scroll_from = None;
        let mut out = vec![Action::Motion { x, y }];
        if !self.down {
            self.down = true;
            out.push(Action::Button { pressed: true });
        }
        out
    }

    /// One finger sliding: pure motion, buttons keep their state.
    pub fn motion(&mut self, x: f64, y: f64) -> Vec<Action> {
        self.pos = (x, y);
        vec![Action::Motion { x, y }]
    }

    /// Finger lifted: move once more so the click lands where the finger was.
    pub fn up(&mut self, x: f64, y: f64) -> Vec<Action> {
        self.pos = (x, y);
        self.scroll_from = None;
        if !self.down {
            return Vec::new();
        }
        self.down = false;
        vec![Action::Motion { x, y }, Action::Button { pressed: false }]
    }

    /// Two fingers moving. Releases a held button first, then scrolls by the
    /// change of the finger midpoint.
    pub fn two_finger(&mut self, mid_y: f64) -> Vec<Action> {
        let mut out = Vec::new();
        if self.down {
            self.down = false;
            out.push(Action::Button { pressed: false });
        }
        if let Some(prev) = self.scroll_from {
            // Fingers moving up scrolls the content down, like every desktop.
            let dy = prev - mid_y;
            if dy.abs() >= 1.0 {
                out.push(Action::Scroll { dy });
            }
        }
        self.scroll_from = Some(mid_y);
        out
    }

    /// Forget any gesture in progress (pointer cancelled by the system).
    pub fn cancel(&mut self) -> Vec<Action> {
        self.scroll_from = None;
        if std::mem::replace(&mut self.down, false) {
            vec![Action::Button { pressed: false }]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_moves_then_clicks() {
        let mut p = Pointer::default();
        assert_eq!(
            p.down(100.0, 200.0),
            vec![
                Action::Motion { x: 100.0, y: 200.0 },
                Action::Button { pressed: true }
            ]
        );
        assert!(p.is_down());
        assert_eq!(
            p.up(100.0, 200.0),
            vec![
                Action::Motion { x: 100.0, y: 200.0 },
                Action::Button { pressed: false }
            ]
        );
        assert!(!p.is_down());
    }

    #[test]
    fn drag_keeps_the_button_and_follows_the_finger() {
        let mut p = Pointer::default();
        p.down(10.0, 10.0);
        assert_eq!(p.motion(30.0, 40.0), vec![Action::Motion { x: 30.0, y: 40.0 }]);
        assert!(p.is_down(), "dragging must not release the button");
    }

    #[test]
    fn two_fingers_scroll_and_release() {
        let mut p = Pointer::default();
        p.down(5.0, 5.0);
        assert_eq!(p.two_finger(300.0), vec![Action::Button { pressed: false }]);
        assert_eq!(p.two_finger(280.0), vec![Action::Scroll { dy: 20.0 }]);
        assert!(!p.is_down());
    }

    #[test]
    fn a_lift_after_a_scroll_does_not_click() {
        let mut p = Pointer::default();
        p.down(1.0, 1.0);
        p.two_finger(100.0);
        assert!(p.up(1.0, 1.0).is_empty(), "no ghost clicks after a scroll");
    }

    #[test]
    fn points_divide_by_the_display_scale() {
        let p = Pointer { pos: (1170.0, 540.0), ..Default::default() };
        let (x, y) = p.points(2.6);
        assert!((x - 450.0).abs() < 0.01, "x was {x}");
        assert!((y - 207.6923).abs() < 0.01, "y was {y}");
    }
}

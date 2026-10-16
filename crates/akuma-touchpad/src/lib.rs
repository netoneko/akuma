//! The clickpad policy: turn HID touch contacts plus one physical button
//! into the pointer vocabulary — motion, left/right clicks, and scroll
//! wheel — that the panel consumers speak.
//!
//! The machine's touchpad (Lenovo IdeaPad 5 16AHP9) is a **clickpad**: one
//! physical button under the whole surface; the button bit is button 0
//! wherever the finger was. Everything else is synthesized here, following
//! libinput's semantics (doc "Clickpad software button behavior",
//! `evdev-mt-touchpad-buttons.c`; OpenBSD `wstpad(4)` is the same ideas in
//! BSD):
//!
//! - **Soft button areas.** The bottom band of the pad (min(10 mm, 15%) —
//!   we use the fraction form) splits at 50% width: a click with the finger
//!   *started* in the right half is a right-click, anywhere else is left.
//!   A finger that starts in the main area never becomes a button finger by
//!   wandering into the band; a band finger that leaves the band becomes a
//!   pointer finger (permanently — it cannot re-enter) and moves the
//!   pointer only while it is the only finger down.
//! - **Clicks with no positional data** (finger past the sensor edge) are
//!   ignored if no finger ever arrives, decided from the zone if one does
//!   — libinput's `click_pending` state, including its "X1 Carbon rule".
//! - **Two-finger scroll.** Two fingers down stop pointer motion; once the
//!   pair moves `scroll_start_px`, scrolling engages and the pair's motion
//!   becomes wheel ticks at `wheel_px` per tick with the remainder carried.
//!   A physical click ends scrolling.
//! - **Click-and-drag** works: motion while the button is held is emitted,
//!   so terminal selection and drag land for free.
//! - **Tap-to-click**, macOS-style and on by default: a quick, still
//!   one-finger touch clicks without pressing the pad down, and two quick
//!   taps are the two click pairs a consumer calls a double click. Off with
//!   `touchpad.tap=0` on the command line or `echo tap=0 >
//!   /dev/input/mouse0` live.
//!
//! Wheel events use the REL_WHEEL convention: `dy = +1` is scroll *up*
//! (fingers moving away from the user, toward the top of the pad).
//!
//! The input is a [`Frame`] per device report; the output is a bounded
//! event ring drained with [`Clickpad::pop`]. No allocation anywhere —
//! fixed arrays, all state in the struct — so the kernel can hold one in a
//! `static` beside the other polled input devices.

#![no_std]
#![forbid(unsafe_code)]

pub mod ptp;
pub mod tty;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_ptp;

/// Report slots the machine tracks (precision touchpads report 5).
pub const MAX_CONTACTS: usize = 5;

/// Event ring capacity. The console pump drains every lap, so 16 is far
/// above a lap's production (one Move per frame plus button pairs); the
/// drop policy below covers a stalled consumer anyway.
pub const EVENT_CAP: usize = 16;

/// Longest touch (µs from tip-down to tip-up) that still counts as a tap.
///
/// Above it the touch was a rest or the start of a drag, and lifting must
/// not click. libinput's tap time is 180 ms; 200 ms is the same idea with
/// the kernel's 8 ms poll period rounding against the limit.
pub const TAP_MAX_US: u64 = 200_000;

/// Travel from the touch's origin, as % of each sensor axis span, within
/// which the touch can still be a tap.
///
/// Beyond it the finger is dragging, and the press/release must not fire on
/// lift. The slop is a fraction of the span, not a constant, because sensor
/// units differ per panel.
pub const TAP_SLOP_PCT: i32 = 3;

/// Sensor range and panel size, for the absolute 1:1 mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    EmptyX,
    EmptyY,
    NoPixels,
}

impl Geometry {
    pub fn new(
        x_min: i32,
        x_max: i32,
        y_min: i32,
        y_max: i32,
        width_px: u32,
        height_px: u32,
    ) -> Result<Self, GeometryError> {
        if x_min >= x_max {
            return Err(GeometryError::EmptyX);
        }
        if y_min >= y_max {
            return Err(GeometryError::EmptyY);
        }
        if width_px == 0 || height_px == 0 {
            return Err(GeometryError::NoPixels);
        }
        Ok(Self {
            x_min,
            x_max,
            y_min,
            y_max,
            width_px,
            height_px,
        })
    }

    fn map_x(&self, x: i32) -> u32 {
        let clamped = x.clamp(self.x_min, self.x_max - 1);
        let scaled = i64::from(clamped - self.x_min) * i64::from(self.width_px)
            / i64::from(self.x_max - self.x_min);
        (scaled.min(i64::from(self.width_px - 1))) as u32
    }

    fn map_y(&self, y: i32) -> u32 {
        let clamped = y.clamp(self.y_min, self.y_max - 1);
        let scaled = i64::from(clamped - self.y_min) * i64::from(self.height_px)
            / i64::from(self.y_max - self.y_min);
        (scaled.min(i64::from(self.height_px - 1))) as u32
    }

    /// First sensor row of the bottom band: `min(10mm, 15%)` — the mm form
    /// needs the pad's physical size, which HID descriptors routinely lie
    /// about, so the fraction form is the default (libinput's own fallback
    /// when it cannot measure).
    fn band_top(&self, band_pct: u32) -> i32 {
        self.y_min + (i64::from(self.y_max - self.y_min) * i64::from(100 - band_pct) / 100) as i32
    }

    /// Sensor column where the right button area begins.
    fn right_split(&self, split_pct: u32) -> i32 {
        self.x_min + (i64::from(self.x_max - self.x_min) * i64::from(split_pct) / 100) as i32
    }
}

/// Policy knobs. The defaults are libinput's for a pad with no visible
/// markings; every value exists so a boot knob can change it without a
/// second code path.
// The bools are one knob each, named like the libinput toggle they map to;
// packing them into bitflags would buy nothing and cost readability.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Bottom band height, % of sensor height (libinput: min(10 mm, 15%)).
    pub band_pct: u32,
    /// Where the right button area starts, % of sensor width.
    pub right_split_pct: u32,
    /// Combined finger travel (px) before scrolling engages.
    pub scroll_start_px: i32,
    /// Finger travel (px) per wheel tick once engaged.
    pub wheel_px: i32,
    /// Clickfinger mode: 1 finger = left, 2 = right, 3 = middle. Replaces
    /// the button areas (the same state machine, the area check swapped for
    /// a finger count).
    pub clickfinger: bool,
    /// Allow the middle button (soft areas: none; clickfinger: 3 fingers).
    pub middle: bool,
    /// "Natural" scrolling, as on a MacBook: the content follows the fingers,
    /// so two fingers moving up scroll the view *down* (and right moves it
    /// left). Off is the traditional wheel direction.
    pub natural_scroll: bool,
    /// Tap-to-click, as on a MacBook: a quick, still touch of one finger is
    /// a left click (press + release), without pressing the pad down. A
    /// second tap inside the window is simply another click pair, which is
    /// what "double click" is to a consumer. Off: only the physical button
    /// clicks.
    pub tap_to_click: bool,
}

impl Config {
    pub const DEFAULT: Self = Self {
        band_pct: 15,
        right_split_pct: 50,
        scroll_start_px: 8,
        wheel_px: 50,
        clickfinger: false,
        middle: false,
        natural_scroll: true,
        tap_to_click: true,
    };

    /// [`Config::DEFAULT`] with the traditional (wheel) scroll direction and
    /// no tap-to-click: the pad answers only its physical button, which is
    /// what the pre-tap tests and a clickpad purist expect.
    pub const TRADITIONAL: Self = Self {
        natural_scroll: false,
        tap_to_click: false,
        ..Self::DEFAULT
    };
}

/// One report slot as the driver decoded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contact {
    /// Tip switch: the finger is down on the surface.
    pub down: bool,
    pub x: i32,
    pub y: i32,
}

impl Contact {
    pub const UP: Self = Self {
        down: false,
        x: 0,
        y: 0,
    };
}

/// One device report: the slot array plus the physical clickpad button.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub contacts: &'a [Contact],
    pub button: bool,
}

/// Which button a synthesized press carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Pointer events, in the order the policy decided them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    /// Absolute pointer position, panel pixels.
    Move {
        x: u32,
        y: u32,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// REL_WHEEL convention: `dy = +1` is scroll up (fingers moving toward
    /// the top of the pad); `dx = +1` is scroll right.
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// The pointer stroke ended: every finger lifted, or a second finger came
    /// down for a scroll. The next `Move` begins a new stroke at wherever the
    /// finger lands, so a consumer that moves the pointer *relatively* (a
    /// touchpad, not a tablet) must not treat the jump as motion.
    Lift,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Main,
    Left,
    Right,
}

#[derive(Clone, Copy)]
struct Finger {
    active: bool,
    cur: (i32, i32),
    zone: Zone,
    // (tap-to-click keeps its timestamps in `Clickpad::tap`, not here: only
    // single-finger taps are recognized, so the state lives one level up.
    // The zone decision needs only where the finger started, which `zone`
    // freezes.)
}

impl Finger {
    const UP: Self = Self {
        active: false,
        cur: (0, 0),
        zone: Zone::Main,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scroll {
    Idle,
    /// Two fingers down, waiting for `scroll_start_px` of travel. Motion is
    /// already suppressed (libinput stops the pointer with two fingers).
    Armed {
        origin: (i64, i64),
    },
    /// Scrolling; `acc` carries sub-tick remainder, px.
    Engaged {
        acc: (i64, i64),
    },
}

/// A touch in flight that may end in a tap. `slop` is the per-axis travel
/// budget in sensor units, computed once from the geometry at touch start.
#[derive(Clone, Copy)]
struct Tap {
    start_us: u64,
    origin: (i32, i32),
    slop: (i32, i32),
}

/// The state machine. One per touchpad, held by the kernel beside the other
/// polled input devices.
pub struct Clickpad {
    geom: Geometry,
    cfg: Config,
    band_top: i32,
    right_split: i32,

    fingers: [Finger; MAX_CONTACTS],
    button_down: bool,
    /// The button state to release when the physical button goes up.
    active_button: Option<Button>,
    /// Button went down with no finger on the pad — wait for one before
    /// deciding the area (libinput's `click_pending`).
    click_pending: bool,
    /// A click was decided and the event emitted; drag motion may follow.
    dragging: bool,

    scroll: Scroll,
    /// Mean of the tracked pair last frame, for the per-frame delta.
    last_pair_mean: Option<(i64, i64)>,
    /// A single-finger touch in flight that may end in a tap-to-click, when
    /// [`Config::tap_to_click`] is on. `None` when idle, cancelled, or
    /// already decided.
    tap: Option<Tap>,

    events: [Option<PointerEvent>; EVENT_CAP],
    head: usize,
    len: usize,
    /// Moves dropped by the ring's overflow policy — a debug tap, read by
    /// the dmesg gate, never a correctness input.
    dropped_moves: u32,
    /// The last position posted in the current pointer stroke; `Some` means a
    /// stroke is open (a `Move` since the last `Lift`).
    last_move: Option<(u32, u32)>,
}

impl Clickpad {
    #[must_use]
    pub fn new(geom: Geometry, cfg: Config) -> Self {
        let band_top = geom.band_top(cfg.band_pct);
        let right_split = geom.right_split(cfg.right_split_pct);
        Self {
            geom,
            cfg,
            band_top,
            right_split,
            fingers: [Finger::UP; MAX_CONTACTS],
            button_down: false,
            active_button: None,
            click_pending: false,
            dragging: false,
            scroll: Scroll::Idle,
            last_pair_mean: None,
            tap: None,
            events: [None; EVENT_CAP],
            head: 0,
            len: 0,
            dropped_moves: 0,
            last_move: None,
        }
    }

    /// Consume one device report. `now_us` is the monotonic clock the tap
    /// windows are measured against — the kernel's uptime, in µs.
    pub fn update(&mut self, frame: &Frame, now_us: u64) {
        self.track_fingers(frame);
        self.handle_button(frame.button);
        self.post_motion_or_scroll();
        self.tap_step(frame, now_us);
    }

    /// Switch natural scrolling on or off (takes effect from the next tick).
    pub fn set_natural_scroll(&mut self, on: bool) {
        self.cfg.natural_scroll = on;
    }

    /// Is natural scrolling on?
    #[must_use]
    pub fn natural_scroll(&self) -> bool {
        self.cfg.natural_scroll
    }

    /// Switch tap-to-click on or off (takes effect from the next tick; a
    /// tap already in flight is dropped, never half-fired).
    pub fn set_tap_to_click(&mut self, on: bool) {
        self.cfg.tap_to_click = on;
        self.tap = None;
    }

    /// Is tap-to-click on?
    #[must_use]
    pub fn tap_to_click(&self) -> bool {
        self.cfg.tap_to_click
    }

    /// Pop the next event, in decision order.
    pub fn pop(&mut self) -> Option<PointerEvent> {
        if self.len == 0 {
            return None;
        }
        let ev = self.events[self.head].take();
        self.head = (self.head + 1) % EVENT_CAP;
        self.len -= 1;
        ev
    }

    /// Any events waiting? (`poll(2)`'s readiness answer.)
    #[must_use]
    pub fn pending(&self) -> bool {
        self.len != 0
    }

    /// Moves the ring dropped — a debug tap for the dmesg gate.
    #[must_use]
    pub fn dropped_moves(&self) -> u32 {
        self.dropped_moves
    }

    fn push(&mut self, ev: PointerEvent) {
        if self.len == EVENT_CAP {
            // Moves are droppable (the next frame re-reports position);
            // buttons and wheel ticks are not — drop the oldest event,
            // which is overwhelmingly a stale Move, and take the slot.
            if matches!(ev, PointerEvent::Move { .. }) {
                self.dropped_moves += 1;
                return;
            }
            self.head = (self.head + 1) % EVENT_CAP;
            self.len -= 1;
        }
        let tail = (self.head + self.len) % EVENT_CAP;
        self.events[tail] = Some(ev);
        self.len += 1;
    }

    fn track_fingers(&mut self, frame: &Frame) {
        for slot in 0..MAX_CONTACTS {
            let contact = frame.contacts.get(slot).copied().unwrap_or(Contact::UP);
            if contact.down {
                // Zone is decided from the *start* position of a tip-down.
                let zone = self.zone_of(contact.x, contact.y);
                let finger = &mut self.fingers[slot];
                if !finger.active {
                    *finger = Finger {
                        active: true,
                        cur: (contact.x, contact.y),
                        zone,
                    };
                } else {
                    let band_top = self.band_top;
                    finger.cur = (contact.x, contact.y);
                    // A band finger that leaves the band becomes a pointer
                    // finger, permanently (it cannot re-enter the areas).
                    if finger.zone != Zone::Main && finger.cur.1 < band_top {
                        finger.zone = Zone::Main;
                    }
                }
            } else {
                self.fingers[slot] = Finger::UP;
            }
        }
    }

    fn zone_of(&self, x: i32, y: i32) -> Zone {
        if y >= self.band_top && x >= self.right_split {
            Zone::Right
        } else if y >= self.band_top {
            Zone::Left
        } else {
            Zone::Main
        }
    }

    fn active_fingers(&self) -> u32 {
        self.fingers.iter().filter(|f| f.active).count() as u32
    }

    /// The one finger allowed to move the pointer, if motion is possible.
    fn pointer_finger(&self) -> Option<(u32, u32)> {
        if self.scroll != Scroll::Idle {
            return None; // two-finger operation: motion suppressed
        }
        let mut it = self.fingers.iter().filter(|f| f.active);
        let first = it.next()?;
        if it.next().is_some() {
            return None; // more than one finger: not a pointer gesture
        }
        if first.zone != Zone::Main {
            return None; // parked in a button area: fingers there don't move
        }
        Some((self.geom.map_x(first.cur.0), self.geom.map_y(first.cur.1)))
    }

    fn handle_button(&mut self, button: bool) {
        if button && !self.button_down {
            // Rising edge: end scroll (a click ends scrolling), then decide.
            self.scroll = Scroll::Idle;
            self.last_pair_mean = None;
            self.button_down = true;
            let decision = self.decide_click();
            match decision {
                Some(b) => {
                    self.active_button = Some(b);
                    self.dragging = true;
                    self.push(PointerEvent::Button {
                        button: b,
                        down: true,
                    });
                }
                None => {
                    // No finger: libinput's click_pending — wait for one.
                    self.click_pending = true;
                }
            }
        } else if !button && self.button_down {
            self.button_down = false;
            self.click_pending = false;
            self.dragging = false;
            if let Some(b) = self.active_button.take() {
                self.push(PointerEvent::Button {
                    button: b,
                    down: false,
                });
            }
            // Pending click released without any finger ever arriving:
            // ignored, the X1 Carbon rule (no positional data → no decision).
        }
        // A finger arriving while the click is pending decides the area now.
        if self.click_pending
            && self.button_down
            && self.active_button.is_none()
            && self.active_fingers() > 0
            && let Some(b) = self.decide_click()
        {
            self.click_pending = false;
            self.active_button = Some(b);
            self.dragging = true;
            self.push(PointerEvent::Button {
                button: b,
                down: true,
            });
        }
    }

    /// Which button this click carries: soft areas by the click fingers'
    /// zones, or the finger count in clickfinger mode. `None` means "wait" —
    /// no finger to decide from (the pending-click path).
    fn decide_click(&self) -> Option<Button> {
        if self.cfg.clickfinger {
            let n = self.active_fingers();
            return match n {
                1 => Some(Button::Left),
                2 => Some(Button::Right),
                3 if self.cfg.middle => Some(Button::Middle),
                _ => None, // no fingers yet (pending), or middle disabled
            };
        }
        if self.active_fingers() == 0 {
            return None; // nobody on the pad: wait for one (click_pending)
        }
        let any_right = self
            .fingers
            .iter()
            .any(|f| f.active && f.zone == Zone::Right);
        Some(if any_right {
            Button::Right
        } else {
            Button::Left
        })
    }

    /// Close the pointer stroke (once) so the next one is not measured from a
    /// stale position.
    fn end_stroke(&mut self) {
        if self.last_move.take().is_some() {
            self.push(PointerEvent::Lift);
        }
    }

    fn post_motion_or_scroll(&mut self) {
        let n = self.active_fingers();
        if n >= 2 && !self.button_down {
            self.end_stroke();
            self.update_scroll();
            return; // scroll (armed or engaged) suppresses motion
        }
        if n < 2 {
            self.scroll = Scroll::Idle;
            self.last_pair_mean = None;
        }
        if let Some((x, y)) = self.pointer_finger() {
            if self.last_move != Some((x, y)) {
                self.last_move = Some((x, y));
                self.push(PointerEvent::Move { x, y });
            }
        } else {
            // No finger is allowed to move the pointer now.
            self.end_stroke();
        }
        let _ = self.dragging; // drag = motion with button down, already emitted
    }

    /// Tap-to-click: one finger down and up again quickly, having stayed
    /// within slop of where it landed, is a left click (press + release).
    /// Runs last in [`Self::update`], so the pair lands after the `Lift`
    /// the same frame pushed — the stroke is over before the click, which
    /// is the order a consumer expects.
    ///
    /// There is deliberately no double-tap state: each tap fires its own
    /// pair as it lifts, so two taps in quick succession are two pairs —
    /// which is exactly what a double click is to a consumer. No single
    /// tap ever waits to find out whether it was alone, which is the same
    /// immediacy macOS's tap-to-click has. (A two-finger tap stays a
    /// no-op: it is scroll-arming territory, and inventing a right-click
    /// there can wait for evidence anyone wants it.)
    fn tap_step(&mut self, frame: &Frame, now: u64) {
        if !self.cfg.tap_to_click {
            self.tap = None;
            return;
        }
        let n = self.active_fingers();
        // The finger lifted (without the physical button coming down in the
        // same report — then this became a click-and-drag, not a tap):
        // a click pair if the touch was quick enough. Travel was checked
        // frame by frame below, so the last in-budget position is what
        // counts.
        if let Some(t) = self.tap
            && n == 0
            && !frame.button
        {
            self.tap = None;
            if now.saturating_sub(t.start_us) <= TAP_MAX_US {
                self.push(PointerEvent::Button { button: Button::Left, down: true });
                self.push(PointerEvent::Button { button: Button::Left, down: false });
            }
            return;
        }
        // Anything else in flight — the physical button, a second finger
        // (scroll arming), a scroll still engaged — cancels the tap: it
        // became another gesture, and must not click on lift.
        if frame.button || n != 1 || !matches!(self.scroll, Scroll::Idle) {
            self.tap = None;
            return;
        }
        let Some(f) = self.fingers.iter().find(|f| f.active) else { return };
        match self.tap {
            None => {
                let slop = |span: i32| {
                    (i64::from(span) * i64::from(TAP_SLOP_PCT) / 100).max(1) as i32
                };
                self.tap = Some(Tap {
                    start_us: now,
                    origin: f.cur,
                    slop: (
                        slop(self.geom.x_max - self.geom.x_min),
                        slop(self.geom.y_max - self.geom.y_min),
                    ),
                });
            }
            Some(t) => {
                if (f.cur.0 - t.origin.0).abs() > t.slop.0
                    || (f.cur.1 - t.origin.1).abs() > t.slop.1
                {
                    self.tap = None; // it travelled: a drag, not a tap
                }
            }
        }
    }

    /// Two-finger scroll over the first two active slots (stable pair).
    fn update_scroll(&mut self) {
        let mut pair: [(i32, i32); 2] = [(0, 0); 2];
        let mut found = 0;
        for f in &self.fingers {
            if f.active {
                pair[found] = f.cur;
                found += 1;
                if found == 2 {
                    break;
                }
            }
        }
        if found < 2 {
            return;
        }
        let mean = (
            i64::midpoint(i64::from(pair[0].0), i64::from(pair[1].0)),
            i64::midpoint(i64::from(pair[0].1), i64::from(pair[1].1)),
        );
        match self.scroll {
            Scroll::Idle => {
                self.scroll = Scroll::Armed { origin: mean };
                self.last_pair_mean = Some(mean);
            }
            Scroll::Armed { .. } => {
                let Some(last) = self.last_pair_mean else {
                    self.last_pair_mean = Some(mean);
                    return;
                };
                let (dx, dy) = (mean.0 - last.0, mean.1 - last.1);
                self.last_pair_mean = Some(mean);
                let Scroll::Armed { origin } = self.scroll else {
                    return;
                };
                let acc = (mean.0 - origin.0, mean.1 - origin.1);
                let moved = acc.0.abs() >= i64::from(self.cfg.scroll_start_px)
                    || acc.1.abs() >= i64::from(self.cfg.scroll_start_px);
                if moved {
                    // Engage: start the accumulator from the armed travel so
                    // the engagement gesture itself scrolls.
                    self.scroll = Scroll::Engaged { acc };
                } else {
                    self.scroll = Scroll::Armed { origin };
                    let _ = (dx, dy);
                }
            }
            Scroll::Engaged { acc } => {
                let Some(last) = self.last_pair_mean else {
                    self.last_pair_mean = Some(mean);
                    return;
                };
                let (dx, dy) = (mean.0 - last.0, mean.1 - last.1);
                self.last_pair_mean = Some(mean);
                let acc = (acc.0 + dx, acc.1 + dy);
                let px = i64::from(self.cfg.wheel_px);
                let ticks_x = acc.0 / px;
                let ticks_y = acc.1 / px;
                if ticks_x != 0 || ticks_y != 0 {
                    let acc = (acc.0 - ticks_x * px, acc.1 - ticks_y * px);
                    self.scroll = Scroll::Engaged { acc };
                    // Traditional: fingers moving up (sensor y decreasing)
                    // scroll up, REL_WHEEL +1; fingers right scroll right.
                    // Natural is the mirror image: the content follows the
                    // fingers, so the view goes the other way.
                    let (dx, dy) = if self.cfg.natural_scroll {
                        (-ticks_x as i32, ticks_y as i32)
                    } else {
                        (ticks_x as i32, -ticks_y as i32)
                    };
                    self.push(PointerEvent::Wheel { dx, dy });
                } else {
                    self.scroll = Scroll::Engaged { acc };
                }
            }
        }
    }
}

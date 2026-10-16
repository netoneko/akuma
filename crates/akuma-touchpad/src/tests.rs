//! Scenario tests: sequences of frames, expected event streams. The
//! geometry is a typical precision touchpad (4096×2048 sensor, libinput's
//! default 15%/50% areas) mapped onto this machine's 1920×1200 panel.

#[cfg(test)]
extern crate std;

use crate::*;
use std::vec;
use std::vec::Vec;

const SENSOR_X: i32 = 4096;
const SENSOR_Y: i32 = 2048;
const PANEL_W: u32 = 1920;
const PANEL_H: u32 = 1200;

fn geom() -> Geometry {
    Geometry::new(0, SENSOR_X, 0, SENSOR_Y, PANEL_W, PANEL_H).unwrap()
}

fn down(x: i32, y: i32) -> Contact {
    Contact { down: true, x, y }
}

const UP: Contact = Contact::UP;

fn frame(contacts: &[Contact; MAX_CONTACTS], button: bool) -> Frame<'_> {
    Frame { contacts, button }
}

fn drain(cp: &mut Clickpad) -> Vec<PointerEvent> {
    let mut out = Vec::new();
    while let Some(ev) = cp.pop() {
        out.push(ev);
    }
    out
}

/// Sensor (1000,1000) maps to panel (468,585) at 1920×1200 — pin the math.
#[test]
fn absolute_mapping() {
    let cp = Clickpad::new(geom(), Config::TRADITIONAL);
    assert_eq!(cp.geom.map_x(1000), 468);
    assert_eq!(cp.geom.map_y(1000), 585);
    assert_eq!(cp.geom.map_x(SENSOR_X), PANEL_W - 1, "clamped");
    assert_eq!(cp.geom.map_y(SENSOR_Y), PANEL_H - 1);
    assert_eq!(cp.geom.map_x(-100), 0);
}

#[test]
fn geometry_validation() {
    assert_eq!(
        Geometry::new(1, 1, 0, 2, 10, 10),
        Err(GeometryError::EmptyX)
    );
    assert_eq!(
        Geometry::new(0, 1, 5, 5, 10, 10),
        Err(GeometryError::EmptyY)
    );
    assert_eq!(
        Geometry::new(0, 1, 0, 1, 0, 10),
        Err(GeometryError::NoPixels)
    );
}

#[test]
fn single_finger_moves_pointer() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);

    // Touch down in the main area: the pointer jumps to the finger.
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Move { x: 468, y: 585 }]);

    // Motion.
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Move { x: 937, y: 585 }]);

    // Lift: the stroke ends (a relative consumer re-anchors on the next one).
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Lift]);
}

#[test]
fn move_dedup() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![], "same position, same button state");
}

#[test]
fn plain_click_is_left() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(500, 500), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);

    cp.update(&frame(&[down(500, 500), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
    cp.update(&frame(&[down(500, 500), UP, UP, UP, UP], false), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: false
        }]
    );
}

#[test]
fn right_click_needs_a_finger_started_in_the_band() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    // Band starts at 2048*85/100 = 1740; right half at x >= 2048.
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], false), 0);
    // A finger parked in the button area does not move the pointer.
    assert_eq!(drain(&mut cp), vec![]);

    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Right,
            down: true
        }]
    );
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], false), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Right,
            down: false
        }]
    );
}

#[test]
fn left_band_area_is_still_left() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(1000, 1900), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(1000, 1900), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
}

#[test]
fn wandering_into_the_band_keeps_left() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    // Starts in the main area...
    cp.update(&frame(&[down(1000, 500), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    // ...wanders into the right area: zones never re-apply to it.
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
}

#[test]
fn band_finger_leaving_the_band_becomes_pointer() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![], "in the area: no pointer");
    // Leaves the band: becomes the pointer finger, permanently.
    cp.update(&frame(&[down(3000, 500), UP, UP, UP, UP], false), 0);
    let evs = drain(&mut cp);
    assert_eq!(evs, vec![PointerEvent::Move { x: 1406, y: 292 }]);
    // And its click is a left click now.
    cp.update(&frame(&[down(3000, 500), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
}

#[test]
fn click_and_drag_moves_while_button_held() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], true), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(1500, 1000), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Move { x: 703, y: 585 }],
        "motion with the button held: terminal selection"
    );
}

#[test]
fn click_without_a_finger_waits_for_one() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    // Button down with nobody on the pad (finger past the sensor edge).
    cp.update(&frame(&[UP, UP, UP, UP, UP], true), 0);
    assert_eq!(drain(&mut cp), vec![], "pending: no decision yet");

    // A finger lands in the right area: the click becomes a right click.
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Right,
            down: true
        }]
    );
    cp.update(&frame(&[down(3000, 1900), UP, UP, UP, UP], false), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Right,
            down: false
        }]
    );
}

#[test]
fn pending_click_released_with_no_finger_is_ignored() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[UP, UP, UP, UP, UP], true), 0);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 0);
    assert_eq!(
        drain(&mut cp),
        vec![],
        "the X1 Carbon rule: no data, no click"
    );
}

#[test]
fn two_finger_scroll() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);

    // Two fingers down: armed, no events yet (motion suppressed too).
    cp.update(&frame(
        &[down(2000, 1000), down(2000, 1200), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![]);

    // Move the pair up 100 px: engages; the engagement travel counts, but
    // the first tick waits for the next engaged frame.
    cp.update(&frame(
        &[down(2000, 900), down(2000, 1100), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![]);

    // 30 px more: acc -130 -> 2 ticks up (REL_WHEEL +2).
    cp.update(&frame(
        &[down(2000, 870), down(2000, 1070), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Wheel { dx: 0, dy: 2 }]);

    // 30 px more: acc -60 -> 1 tick.
    cp.update(&frame(
        &[down(2000, 840), down(2000, 1040), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Wheel { dx: 0, dy: 1 }]);

    // Lift one finger: scroll ends, the survivor moves the pointer.
    cp.update(&frame(&[down(2000, 840), UP, UP, UP, UP], false), 0);
    let evs = drain(&mut cp);
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], PointerEvent::Move { .. }));
}

#[test]
fn scroll_down_is_negative_wheel() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(
        &[down(2000, 1000), down(2000, 1200), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
    // Engage downward and travel: the engagement travel banks into acc
    // (200 px here), so the first engaged frame emits the whole run.
    cp.update(&frame(
        &[down(2000, 1200), down(2000, 1400), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(
        &[down(2000, 1250), down(2000, 1450), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Wheel { dx: 0, dy: -5 }]);
}

#[test]
fn scroll_start_threshold_holds_off_jitter() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(
        &[down(2000, 1000), down(2100, 1200), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
    // 5 px: below the 8 px start threshold.
    cp.update(&frame(
        &[down(2000, 996), down(2100, 1196), UP, UP, UP],
        false,
    ), 0);
    cp.update(&frame(
        &[down(2000, 995), down(2100, 1195), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![]);
}

#[test]
fn click_ends_scroll() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(
        &[down(2000, 1000), down(2000, 1200), UP, UP, UP],
        false,
    ), 0);
    cp.update(&frame(
        &[down(2000, 900), down(2000, 1100), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp); // engaged, nothing emitted yet

    // Physical click: scroll ends, click proceeds, no wheel from the pair.
    cp.update(&frame(
        &[down(2000, 900), down(2000, 1100), UP, UP, UP],
        true,
    ), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
    // Two fingers + button: still no motion, no scroll.
    cp.update(&frame(
        &[down(2000, 800), down(2000, 1000), UP, UP, UP],
        true,
    ), 0);
    assert_eq!(drain(&mut cp), vec![]);
    cp.update(&frame(
        &[down(2000, 800), down(2000, 1000), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: false
        }]
    );
}

#[test]
fn clickfinger_mode() {
    let cfg = Config {
        clickfinger: true,
        middle: true,
        ..Config::TRADITIONAL
    };
    let mut cp = Clickpad::new(geom(), cfg);

    // 1 finger + click = left.
    cp.update(&frame(&[down(1000, 500), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(&[down(1000, 500), UP, UP, UP, UP], true), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Left,
            down: true
        }]
    );
    cp.update(&frame(&[down(1000, 500), UP, UP, UP, UP], false), 0);
    let _ = drain(&mut cp);

    // 2 fingers + click = right, wherever they are.
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), UP, UP, UP],
        true,
    ), 0);
    // The second finger also ends the pointer stroke (it is a two-finger
    // gesture now), after the button.
    assert_eq!(
        drain(&mut cp),
        vec![
            PointerEvent::Button {
                button: Button::Right,
                down: true
            },
            PointerEvent::Lift
        ]
    );
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);

    // 3 fingers + click = middle (allowed here).
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), down(1400, 700), UP, UP],
        true,
    ), 0);
    assert_eq!(
        drain(&mut cp),
        vec![PointerEvent::Button {
            button: Button::Middle,
            down: true
        }]
    );
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), down(1400, 700), UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
}

#[test]
fn clickfinger_middle_disabled_ignores_three_fingers() {
    let cfg = Config {
        clickfinger: true,
        ..Config::TRADITIONAL
    };
    let mut cp = Clickpad::new(geom(), cfg);
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), down(1400, 700), UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), down(1400, 700), UP, UP],
        true,
    ), 0);
    cp.update(&frame(
        &[down(1000, 500), down(1200, 600), down(1400, 700), UP, UP],
        false,
    ), 0);
    assert_eq!(
        drain(&mut cp),
        vec![],
        "no middle: three-finger click is nothing"
    );
}

#[test]
fn event_ring_drops_moves_keeps_buttons() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    // 24 moving frames without draining: the ring holds 16, drops the rest.
    for i in 0..24 {
        cp.update(&frame(&[down(200 + i * 60, 500), UP, UP, UP, UP], false), 0);
    }
    assert!(cp.pending());
    assert!(cp.dropped_moves() >= 8, "overflowing moves counted");
    // A button squeezes in by evicting the oldest entry.
    cp.update(&frame(&[down(200 + 24 * 60, 500), UP, UP, UP, UP], true), 0);
    // Drain everything: exactly 16 events, ending with the button press.
    let evs = drain(&mut cp);
    assert_eq!(evs.len(), EVENT_CAP);
    assert_eq!(
        evs[EVENT_CAP - 1],
        PointerEvent::Button {
            button: Button::Left,
            down: true
        }
    );
    assert!(!cp.pending());
}

#[test]
fn wheel_convention_and_remainder() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(
        &[down(2000, 1000), down(2000, 1200), UP, UP, UP],
        false,
    ), 0);
    let _ = drain(&mut cp);
    // Engage upward: 100 px of travel banked.
    cp.update(&frame(
        &[down(2000, 900), down(2000, 1100), UP, UP, UP],
        false,
    ), 0);
    // +25: acc -125 -> 2 ticks (100 px), 25 carried.
    cp.update(&frame(
        &[down(2000, 875), down(2000, 1075), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Wheel { dx: 0, dy: 2 }]);
    // +24: acc 49 -> no tick yet.
    cp.update(&frame(
        &[down(2000, 851), down(2000, 1051), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![]);
    // +2: acc 51 -> 1 tick, 1 carried.
    cp.update(&frame(
        &[down(2000, 849), down(2000, 1049), UP, UP, UP],
        false,
    ), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Wheel { dx: 0, dy: 1 }]);
}

/// The drift bug (2026-10-16): after a two-finger scroll the finger left on
/// the pad is a *new* pointer stroke. The policy must close the old stroke
/// (`Lift`) when the second finger arrives, once, and not before.
#[test]
fn a_second_finger_ends_the_pointer_stroke_once() {
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(1000, 1000), UP, UP, UP, UP], false), 0);
    assert!(matches!(drain(&mut cp).as_slice(), [PointerEvent::Move { .. }]));

    // Second finger lands: stroke over, exactly one Lift.
    cp.update(&frame(&[down(1000, 1000), down(1500, 1000), UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Lift]);
    cp.update(&frame(&[down(1000, 1100), down(1500, 1100), UP, UP, UP], false), 0);
    assert!(!drain(&mut cp).contains(&PointerEvent::Lift), "no repeated Lift while scrolling");

    // One finger stays, far to the right: a new stroke begins (a relative
    // consumer anchors on this Move instead of jumping to it).
    cp.update(&frame(&[UP, down(3000, 1100), UP, UP, UP], false), 0);
    assert!(matches!(drain(&mut cp).as_slice(), [PointerEvent::Move { .. }]));
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 0);
    assert_eq!(drain(&mut cp), vec![PointerEvent::Lift]);
}

/// MacBook-style scrolling: the content follows the fingers. Two fingers moving
/// up scroll the view down (negative REL_WHEEL), and the toggle flips it live.
#[test]
fn natural_scroll_is_the_mirror_image_and_toggles() {
    fn scroll_up_with_fingers(cp: &mut Clickpad) -> Vec<PointerEvent> {
        cp.update(&frame(&[down(1000, 1500), down(1400, 1500), UP, UP, UP], false), 0);
        let _ = drain(cp);
        // Fingers move up the pad by more than a wheel tick.
        cp.update(&frame(&[down(1000, 1200), down(1400, 1200), UP, UP, UP], false), 0);
        cp.update(&frame(&[down(1000, 900), down(1400, 900), UP, UP, UP], false), 0);
        let ev = drain(cp);
        cp.update(&frame(&[UP, UP, UP, UP, UP], false), 0);
        let _ = drain(cp);
        ev
    }
    let wheel_dy = |ev: &[PointerEvent]| -> i32 {
        ev.iter().map(|e| if let PointerEvent::Wheel { dy, .. } = e { *dy } else { 0 }).sum()
    };

    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    assert!(cp.natural_scroll(), "natural scrolling is on by default");
    let natural = wheel_dy(&scroll_up_with_fingers(&mut cp));
    assert!(natural < 0, "fingers up must scroll the view down when natural, got {natural}");

    cp.set_natural_scroll(false);
    let traditional = wheel_dy(&scroll_up_with_fingers(&mut cp));
    assert!(traditional > 0, "fingers up scroll up in the traditional direction, got {traditional}");
    assert_eq!(natural, -traditional, "exact mirror");
}

// ---------------------------------------------------------------------------
// Tap-to-click (macOS-style; `Config::DEFAULT` has it on)
// ---------------------------------------------------------------------------

#[test]
fn tap_clicks_without_the_button() {
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 100_000);
    let ev = drain(&mut cp);
    // The stroke the touch itself produced (Move, Lift), then the pair.
    assert!(matches!(ev[0], PointerEvent::Move { .. }));
    assert_eq!(
        &ev[ev.len() - 3..],
        &[
            PointerEvent::Lift,
            PointerEvent::Button { button: Button::Left, down: true },
            PointerEvent::Button { button: Button::Left, down: false },
        ],
    );
}

#[test]
fn double_tap_is_two_click_pairs() {
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    // Two taps in quick succession. Each fires as it lifts — nothing waits
    // to find out whether it was alone — so the consumer sees a plain
    // double click.
    for (down_t, up_t) in [(0, 80_000), (150_000, 230_000)] {
        cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), down_t);
        cp.update(&frame(&[UP, UP, UP, UP, UP], false), up_t);
    }
    let clicks: Vec<bool> = drain(&mut cp)
        .iter()
        .filter_map(|e| if let PointerEvent::Button { down, .. } = e { Some(*down) } else { None })
        .collect();
    assert_eq!(clicks, vec![true, false, true, false]);
}

#[test]
fn slow_touch_is_not_a_tap() {
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 400_000);
    let ev = drain(&mut cp);
    assert!(!ev.iter().any(|e| matches!(e, PointerEvent::Button { .. })));
}

#[test]
fn travelling_touch_is_not_a_tap() {
    // Slop is 3% of the span: 122 x, 61 y sensor px on this geometry.
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    cp.update(&frame(&[down(2400, 1000), UP, UP, UP, UP], false), 50_000);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 100_000);
    let ev = drain(&mut cp);
    assert!(!ev.iter().any(|e| matches!(e, PointerEvent::Button { .. })));
    // It still dragged the pointer: the second Move at the new spot.
    assert!(matches!(ev[1], PointerEvent::Move { .. }));
}

#[test]
fn tap_respects_the_config_and_live_toggle() {
    // TRADITIONAL is the tap-free preset: only the physical button clicks.
    let mut cp = Clickpad::new(geom(), Config::TRADITIONAL);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 50_000);
    assert!(!drain(&mut cp).iter().any(|e| matches!(e, PointerEvent::Button { .. })));

    cp.set_tap_to_click(true);
    assert!(cp.tap_to_click());
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 1_000_000);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 1_050_000);
    assert!(drain(&mut cp).iter().any(|e| matches!(e, PointerEvent::Button { button: Button::Left, down: true })));

    // Off again with a tap in flight: dropped, never half-fired.
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 2_000_000);
    cp.set_tap_to_click(false);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 2_050_000);
    assert!(!drain(&mut cp).iter().any(|e| matches!(e, PointerEvent::Button { .. })));
}

#[test]
fn second_finger_cancels_the_tap() {
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    // A second finger before the lift: scroll-arming territory, not a tap.
    cp.update(&frame(&[down(2000, 1000), down(2000, 1200), UP, UP, UP], false), 30_000);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 60_000);
    let ev = drain(&mut cp);
    assert!(!ev.iter().any(|e| matches!(e, PointerEvent::Button { .. })));
}

#[test]
fn physical_click_cancels_the_tap() {
    let mut cp = Clickpad::new(geom(), Config::DEFAULT);
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], false), 0);
    // The pad is pressed while the finger rests: a click-and-drag, whose
    // pair is the physical button's. The lift must not add a tap pair.
    cp.update(&frame(&[down(2000, 1000), UP, UP, UP, UP], true), 30_000);
    cp.update(&frame(&[UP, UP, UP, UP, UP], false), 60_000);
    let ev = drain(&mut cp);
    let buttons = ev.iter().filter(|e| matches!(e, PointerEvent::Button { .. })).count();
    assert_eq!(buttons, 2, "exactly the physical press+release, got {ev:?}");
}

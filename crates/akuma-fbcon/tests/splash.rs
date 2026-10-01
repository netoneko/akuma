//! The boot splash: layout, the colour wave, and drawing it.

use akuma_fbcon::splash::{art_color, layout, paint, sin_permille};
use akuma_fbcon::{Console, Rgb, Surface};

const ART: &str = "  .:-=+*#%@\n .:-=+*#%@@@\n  :-=+*#%@\n";

struct Mem(usize, usize, Vec<Rgb>, usize);
impl Mem {
    fn new(w: usize, h: usize) -> Self {
        Self(w, h, vec![Rgb::BLACK; w * h], 0)
    }
}
impl Surface for Mem {
    fn width(&self) -> usize {
        self.0
    }
    fn height(&self) -> usize {
        self.1
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x >= self.0 || y >= self.1 {
            self.3 += 1;
        } else {
            self.2[y * self.0 + x] = c;
        }
    }
}

#[test]
fn hsv_hits_the_primaries_and_wraps() {
    assert_eq!(Rgb::from_hsv(0, 255, 255), Rgb::new(255, 0, 0));
    assert_eq!(Rgb::from_hsv(120, 255, 255), Rgb::new(0, 255, 0));
    assert_eq!(Rgb::from_hsv(240, 255, 255), Rgb::new(0, 0, 255));
    assert_eq!(Rgb::from_hsv(360, 255, 255), Rgb::from_hsv(0, 255, 255), "wraps");
    assert_eq!(Rgb::from_hsv(37, 0, 200), Rgb::new(200, 200, 200), "no saturation is grey");
    assert_eq!(Rgb::from_hsv(180, 255, 0), Rgb::BLACK, "no value is black");
}

#[test]
fn hsv_never_exceeds_its_value() {
    for h in (0..720).step_by(7) {
        for s in [0u8, 60, 128, 255] {
            let c = Rgb::from_hsv(h, s, 200);
            assert!(c.r <= 200 && c.g <= 200 && c.b <= 200, "h={h} s={s} -> {c:?}");
            assert_eq!(c.r.max(c.g).max(c.b), 200, "the brightest channel is the value");
        }
    }
}

#[test]
fn the_sine_swell_is_close_and_bounded() {
    assert_eq!(sin_permille(0), 0);
    assert!((sin_permille(90) - 1000).abs() <= 2);
    assert_eq!(sin_permille(180), 0);
    assert!((sin_permille(270) + 1000).abs() <= 2);
    for d in -720..720 {
        assert!(sin_permille(d).abs() <= 1000);
    }
}

#[test]
fn the_art_sits_on_the_left_and_the_splash_is_centred_vertically() {
    let (top, left) = layout(ART, 4, 45, 80);
    assert_eq!(left, 2);
    let total = 3 + 2 + 4;
    assert_eq!(top, (45 - total) / 2);
    // A screen too narrow for the art: flush left, never negative.
    assert_eq!(layout(ART, 4, 45, 6).1, 0);
    assert_eq!(layout(ART, 4, 3, 80).0, 0, "a short screen starts at the top");
}

#[test]
fn colour_moves_with_time_and_position() {
    let c = |x, y, t| art_color(b'*', x, y, t);
    assert_ne!(c(3, 3, 0), c(3, 3, 3000), "the same cell changes colour over time");
    assert_ne!(c(3, 3, 0), c(9, 3, 0), "neighbours differ across the art");
    assert_ne!(c(3, 3, 0), c(3, 8, 0));
}

#[test]
fn dense_cells_are_brighter_and_whiter_than_sparse_ones() {
    let lum = |c: Rgb| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
    let (sparse, dense) = (art_color(b'.', 5, 5, 1000), art_color(b'@', 5, 5, 1000));
    assert!(lum(dense) > lum(sparse), "{dense:?} vs {sparse:?}");
    // Whiter: the smallest channel is a larger share of the largest.
    let whiteness = |c: Rgb| u32::from(c.r.min(c.g).min(c.b)) * 255 / u32::from(c.r.max(c.g).max(c.b)).max(1);
    assert!(whiteness(dense) > whiteness(sparse));
}

#[test]
fn the_glow_breathes_but_never_goes_dark() {
    let lum = |c: Rgb| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
    let (mut lo, mut hi) = (u32::MAX, 0);
    for t in (0..6000).step_by(50) {
        // The same hue (`t` only through the swell would change hue too, so compare
        // the brightest channel, which is the value, not the hue).
        let c = art_color(b'#', 4, 4, t);
        let v = u32::from(c.r.max(c.g).max(c.b));
        lo = lo.min(v);
        hi = hi.max(v);
        assert!(lum(c) > 60, "t={t}: too dark ({c:?})");
    }
    assert!(hi > lo + 20, "no swell: {lo}..{hi}");
}

#[test]
fn painting_draws_ink_where_the_art_is_and_nowhere_off_the_surface() {
    let mut con = Console::new(Mem::new(1280, 720)).unwrap();
    con.clear();
    paint(&mut con, ART, &["Akuma/amd64  0.0.8", "uname line", "up 3s"], 1234);
    let s = con.into_surface();
    assert_eq!(s.3, 0, "nothing drawn outside the surface");
    assert!(s.2.iter().filter(|&&c| c != Rgb::BLACK).count() > 500, "nothing was drawn");
}

#[test]
fn successive_frames_change_colours_but_not_shapes() {
    let draw = |t| {
        let mut con = Console::new(Mem::new(1280, 720)).unwrap();
        con.clear();
        paint(&mut con, ART, &["title"], t);
        con.into_surface().2
    };
    let (a, b) = (draw(0), draw(4000));
    let ink = |f: &[Rgb]| f.iter().map(|&c| c != Rgb::BLACK).collect::<Vec<_>>();
    assert_eq!(ink(&a), ink(&b), "the same pixels are lit");
    assert_ne!(a, b, "but in different colours");
}

#[test]
fn a_shrinking_info_line_leaves_no_debris() {
    let fresh = |line: &str| {
        let mut con = Console::new(Mem::new(1280, 720)).unwrap();
        con.clear();
        paint(&mut con, ART, &["title", line], 0);
        con.into_surface().2
    };
    // Draw a long line, then a short one over it, on the same console.
    let mut con = Console::new(Mem::new(1280, 720)).unwrap();
    con.clear();
    paint(&mut con, ART, &["title", "up 123456789 seconds and counting"], 0);
    paint(&mut con, ART, &["title", "up 9s"], 0);
    assert_eq!(con.into_surface().2, fresh("up 9s"), "the long line left pixels behind");
}

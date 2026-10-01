//! Writes frames to disk so a human can look at them. Not an assertion: ignored by
//! default, run with
//!
//!     AKUMA_FBCON_DUMP=/some/dir cargo test -p akuma-fbcon --test dump -- --ignored
//!
//! and convert the `.ppm` files with any image tool.

use akuma_fbcon::splash;
use akuma_fbcon::{Console, Rgb, Surface};

struct Mem(usize, usize, Vec<Rgb>);
impl Surface for Mem {
    fn width(&self) -> usize {
        self.0
    }
    fn height(&self) -> usize {
        self.1
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.0 && y < self.1 {
            self.2[y * self.0 + x] = c;
        }
    }
}

fn ppm(dir: &str, name: &str, s: &Mem) {
    let mut out = format!("P6\n{} {}\n255\n", s.0, s.1).into_bytes();
    for c in &s.2 {
        out.extend_from_slice(&[c.r, c.g, c.b]);
    }
    std::fs::write(format!("{dir}/{name}.ppm"), out).unwrap();
}

fn console(w: usize, h: usize) -> Console<Mem> {
    let mut con = Console::new(Mem(w, h, vec![Rgb::BLACK; w * h])).unwrap();
    con.set_bg(Rgb::new(0x08, 0x0C, 0x14));
    con.clear();
    con.set_margin(0, 0);
    con
}

const ART: &str = include_str!("../../../amd64/src/akuma_40.txt");

#[test]
#[ignore = "writes images; run on demand"]
fn dump_frames() {
    let Ok(dir) = std::env::var("AKUMA_FBCON_DUMP") else { return };
    std::fs::create_dir_all(&dir).unwrap();

    // The splash, on the trashcan's screen, half-width as `console.conf` sets it.
    for t in [0u64, 2500, 6000] {
        let mut con = console(3840, 2160);
        con.apply_config(&akuma_fbcon::config::ConsoleConfig::parse("margin = 0\ncols = 50%"));
        let info = [
            "Akuma/amd64  0.0.8-amd64",
            "Akuma akuma 0.0.8 2a81b72d-release-smp-shared x86_64",
            "kernel  0.0.8   commit 2a81b72d   release-smp-shared",
            &format!("up {}s - starting services", t / 1000),
        ];
        splash::paint(&mut con, ART, &info, t);
        ppm(&dir, &format!("splash-{t}"), &con.into_surface());
    }

    // Text: scripts, symbols, emoji, boxes.
    let mut con = console(1920, 1080);
    con.write_str_bytes(
        "\x1b[1;36mUnicode on the TV\x1b[0m\r\n\r\n\
         Latin     : café naïve Łódź Žluťoučký Việt\r\n\
         Cyrillic  : Привет, мир! Съешь же ещё этих мягких французских булок\r\n\
         Greek     : Γειά σου κόσμε αβγδεζηθ ΑΒΓΔΕΖΗΘ\r\n\
         Japanese  : こんにちは世界 カタカナ 日本語のテキスト\r\n\
         Chinese   : 你好，世界 简体中文 繁體中文\r\n\
         Korean    : 안녕하세요 세계 한국어\r\n\
         Symbols   : ● ○ ■ □ ▲ ▶ ▼ ◀ ◆ ✓ ✗ → ← ↑ ↓ ↔ · • … — –\r\n\
         Emoji     : 😂 🚀 ✅ 🔥 🏆 👍 🎉 💯 ❤️ 🟢🔴🔵🟡 🐍🦀🐧\r\n\
         Box       : ┌──┬──┐ ╔══╦══╗ ░▒▓█ ⣿⣶⣤⣀\r\n\
                     │  │  │ ║  ║  ║\r\n\
                     └──┴──┘ ╚══╩══╝\r\n\
         \x1b[7m reverse \x1b[0m \x1b[1mbold\x1b[0m \x1b[4munder\x1b[0m \x1b[31mred\x1b[32mgreen\x1b[34mblue\x1b[0m\r\n",
    );
    ppm(&dir, "text", &con.into_surface());
}

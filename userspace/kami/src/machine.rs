//! kami's session as a state machine with no I/O.
//!
//! [`Machine::handle`] takes an [`Event`] (a clock tick, a message from
//! Chromium, a decoded key, "the daemon connected", "that frame was shown")
//! and returns [`Effect`]s (send this message, show this frame, set this
//! status, log this, finish). It never waits for a reply: it numbers its own
//! requests, remembers which are outstanding and for how long, and carries on.
//! A Chromium that stops answering therefore costs nothing but the status line
//! saying so: keys, the clock and quitting keep working. (The code this
//! replaced blocked inside each CDP call; while blocked it read no keys and
//! drew nothing, which is what "kami hangs" was.)
//!
//! The shell in `main.rs` is the only part that touches sockets, the tty, PNG
//! decoding and the framebuffer; it feeds this machine and performs what it
//! says. Nothing here names a file descriptor, so the whole startup, hint and
//! stall behaviour is host-tested below.

use crate::cdp::{escape, event_session, find, first_page, method, num_field, str_field};
use crate::nav::{self, Action, Mode, Nav, Scroll};
use crate::Input;

/// Milliseconds on any monotonic clock the shell likes. Only differences are
/// used, and always saturating: Akuma's per-core clocks were seen to step back.
pub type Ms = u64;

/// The in-page helper (hint overlay); see `hints.js`.
const HINTS_JS: &str = include_str!("hints.js");

/// How long a cold Chromium may take to bring the daemon socket up.
const CONNECT_BUDGET: Ms = 30_000;
/// How long to wait for `Page.frameNavigated` before starting the screencast anyway.
const NAVIGATE_BUDGET: Ms = 15_000;
/// A request unanswered this long is shown on the status line.
const STALL_AFTER: Ms = 4_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Screencast,
    Shot,
}

#[derive(Debug)]
pub enum Event {
    /// The clock. Delivered first in every shell loop iteration.
    Tick(Ms),
    /// One whole message from Chromium (reply or event).
    Cdp(Vec<u8>),
    Key(Input),
    InputClosed,
    Connected,
    ConnectFailed,
    DaemonGone,
    /// The shell decoded and (unless `empty`) blitted a [`Effect::Present`].
    Presented { source: Source, ok: bool, empty: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// Connect to the daemon socket, starting the daemon first if `spawn`.
    /// Answer with [`Event::Connected`] or [`Event::ConnectFailed`].
    TryConnect { spawn: bool },
    /// A complete CDP message to write (without its NUL).
    Send(String),
    /// Decode this base64 PNG and show it; answer with [`Event::Presented`].
    Present { b64: String, source: Source },
    Status(String),
    Log(String),
    /// The session is over; `Some` is an error to report.
    Done(Option<String>),
}

pub struct Config {
    pub url: Option<String>,
    pub home: String,
    /// The page viewport in pixels (status bar excluded).
    pub view: (usize, usize),
    /// Poll `Page.captureScreenshot` from the start, every N ms.
    pub poll_ms: Option<u64>,
    pub max_frames: Option<u64>,
    pub seconds: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Connecting,
    Targets,
    Attaching,
    Setup,
    Navigating,
    Screencast,
    Running,
    Done,
}

/// What a reply is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Req {
    GetTargets,
    CreateTarget,
    Attach,
    PageEnable,
    StartScreencast,
    Shot,
    Collect,
    /// The page helper's account of why a collection found nothing.
    Why,
    Click,
    Ignore,
}

struct Pending {
    id: u64,
    req: Req,
    method: &'static str,
    sent: Ms,
}

pub struct Machine {
    cfg: Config,
    phase: Phase,
    now: Ms,
    started: Option<Ms>,
    lap_at: Ms,
    next_id: u64,
    pending: Vec<Pending>,
    nav: Nav,
    session: Option<String>,
    target: String,
    fresh: bool,
    // connecting
    connect_inflight: bool,
    connect_wait_until: Ms,
    tried: bool,
    spawned: bool,
    // navigating / screencast
    navigate_deadline: Ms,
    navigated: bool,
    screencast_tries: u32,
    retry_at: Option<Ms>,
    // running
    loading: bool,
    url: String,
    poll_ms: Option<u64>,
    next_shot: Ms,
    shot_inflight: bool,
    last_shot: u64,
    frames: u64,
    first_pixels: bool,
    last_status: String,
}

/// FNV-1a over the bytes: enough to tell "same screenshot" from "different".
fn hash(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &c| (h ^ c as u64).wrapping_mul(0x100_0000_01b3))
}

impl Machine {
    pub fn new(cfg: Config) -> Machine {
        let poll_ms = cfg.poll_ms;
        Machine {
            cfg,
            phase: Phase::Connecting,
            now: 0,
            started: None,
            lap_at: 0,
            next_id: 0,
            pending: Vec::new(),
            nav: Nav::new(),
            session: None,
            target: String::new(),
            fresh: false,
            connect_inflight: false,
            connect_wait_until: 0,
            tried: false,
            spawned: false,
            navigate_deadline: 0,
            navigated: false,
            screencast_tries: 0,
            retry_at: None,
            loading: false,
            url: String::new(),
            poll_ms,
            next_shot: 0,
            shot_inflight: false,
            last_shot: 0,
            frames: 0,
            first_pixels: true,
            last_status: String::new(),
        }
    }

    pub fn handle(&mut self, ev: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        match ev {
            Event::Tick(now) => self.tick(now, &mut out),
            Event::Connected => {
                self.connect_inflight = false;
                self.lap("connected to the daemon", &mut out);
                self.phase = Phase::Targets;
                self.send(Req::GetTargets, "Target.getTargets", "{}", false, &mut out);
            }
            Event::ConnectFailed => {
                self.connect_inflight = false;
                self.tried = true;
                self.connect_wait_until = self.now + 100;
            }
            Event::DaemonGone => self.finish(Some("daemon went away".into()), &mut out),
            Event::InputClosed => out.push(Effect::Log("tty input closed".into())),
            Event::Cdp(m) => self.on_cdp(&m, &mut out),
            Event::Key(k) => self.on_key(k, &mut out),
            Event::Presented { source, ok, empty } => self.on_presented(source, ok, empty, &mut out),
        }
        let text = self.status_text();
        if text != self.last_status {
            self.last_status = text.clone();
            out.push(Effect::Status(text));
        }
        out
    }

    // ---- plumbing ------------------------------------------------------

    fn secs(&self, since: Ms) -> u64 {
        self.now.saturating_sub(since) / 1000
    }

    /// Log how long the phase that just ended took.
    fn lap(&mut self, what: &str, out: &mut Vec<Effect>) {
        out.push(Effect::Log(format!("startup: {what}: {} ms", self.now.saturating_sub(self.lap_at))));
        self.lap_at = self.now;
    }

    fn send(&mut self, req: Req, method: &'static str, params: &str, sess: bool, out: &mut Vec<Effect>) {
        self.next_id += 1;
        let mut msg = format!("{{\"id\":{},\"method\":\"{method}\",\"params\":{params}", self.next_id);
        if let (true, Some(s)) = (sess, &self.session) {
            msg.push_str(&format!(",\"sessionId\":\"{s}\""));
        }
        msg.push('}');
        if self.pending.len() >= 256 {
            self.pending.remove(0);
        }
        self.pending.push(Pending { id: self.next_id, req, method, sent: self.now });
        out.push(Effect::Send(msg));
    }

    /// Run a call into the in-page helper (installing it first if the
    /// document is new).
    fn eval(&mut self, req: Req, call: &str, out: &mut Vec<Effect>) {
        let expr = format!("{HINTS_JS}\n;{call}");
        let params = format!("{{\"expression\":\"{}\",\"returnByValue\":true}}", escape(&expr));
        self.send(req, "Runtime.evaluate", &params, true, out);
    }

    fn finish(&mut self, err: Option<String>, out: &mut Vec<Effect>) {
        if self.phase == Phase::Done {
            return;
        }
        // Leave Chromium and the page for the next session, with nothing of
        // this one attached.
        if let Some(s) = self.session.clone() {
            self.send(Req::Ignore, "Page.stopScreencast", "{}", true, out);
            self.send(Req::Ignore, "Target.detachFromTarget", &format!("{{\"sessionId\":\"{s}\"}}"), false, out);
        }
        self.phase = Phase::Done;
        out.push(Effect::Done(err));
    }

    fn start_screencast(&mut self, out: &mut Vec<Effect>) {
        self.phase = Phase::Screencast;
        self.retry_at = None;
        let (w, h) = self.cfg.view;
        let p = format!("{{\"format\":\"png\",\"everyNthFrame\":1,\"maxWidth\":{w},\"maxHeight\":{h}}}");
        self.send(Req::StartScreencast, "Page.startScreencast", &p, true, out);
    }

    // ---- clock ---------------------------------------------------------

    fn tick(&mut self, now: Ms, out: &mut Vec<Effect>) {
        // The clock may step back between cores; never let it.
        self.now = self.now.max(now);
        let now = self.now;
        if self.started.is_none() {
            self.started = Some(now);
            self.lap_at = now;
        }
        let started = self.started.unwrap_or(now);
        if self.cfg.seconds.is_some_and(|s| now.saturating_sub(started) as f64 >= s * 1000.0)
            && self.phase != Phase::Done
        {
            self.finish(None, out);
            return;
        }
        match self.phase {
            Phase::Connecting => {
                if now.saturating_sub(started) > CONNECT_BUDGET {
                    self.finish(Some("the kami daemon did not come up".into()), out);
                } else if !self.connect_inflight && now >= self.connect_wait_until {
                    self.connect_inflight = true;
                    // First try attaches to a daemon that is already there;
                    // only if that fails is one started.
                    let spawn = self.tried && !self.spawned;
                    self.spawned |= spawn;
                    if spawn {
                        out.push(Effect::Log("startup: no daemon; spawning one".into()));
                    }
                    out.push(Effect::TryConnect { spawn });
                }
            }
            Phase::Navigating => {
                if self.navigated || now >= self.navigate_deadline {
                    let what = format!("Page.navigate + frameNavigated (arrived={})", self.navigated);
                    self.lap(&what, out);
                    self.start_screencast(out);
                }
            }
            Phase::Screencast => {
                if self.retry_at.is_some_and(|t| now >= t) {
                    self.start_screencast(out);
                }
            }
            Phase::Running => {
                if let Some(ms) = self.poll_ms {
                    if !self.shot_inflight && now >= self.next_shot {
                        self.shot_inflight = true;
                        self.next_shot = now + ms;
                        self.send(Req::Shot, "Page.captureScreenshot", "{\"format\":\"png\"}", true, out);
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Chromium ------------------------------------------------------

    fn on_cdp(&mut self, m: &[u8], out: &mut Vec<Effect>) {
        if m.starts_with(b"{\"id\":") {
            self.on_reply(m, out);
        } else if let Some(name) = method(m) {
            self.on_event(name, m, out);
        }
    }

    fn on_event(&mut self, name: &str, m: &[u8], out: &mut Vec<Effect>) {
        if self.session.is_none() || event_session(m) != self.session.as_deref() {
            return;
        }
        if name.starts_with("Page.frame") && !name.contains("screencast")
            || name.starts_with("Page.load")
            || name.starts_with("Page.domContent")
            || name.starts_with("Page.navigat")
        {
            out.push(Effect::Log(format!("page event {name}")));
        }
        match name {
            "Page.frameStartedLoading" => self.loading = true,
            "Page.frameStoppedLoading" | "Page.loadEventFired" => self.loading = false,
            // Only the top frame: a subframe's object carries a parentId.
            "Page.frameNavigated" if find(m, b"\"parentId\"").is_none() => {
                self.navigated = true;
                if let Some(u) = str_field(m, "url") {
                    self.url = u.to_string();
                    out.push(Effect::Log(format!("top frame now {u}")));
                }
            }
            "Page.screencastFrame" => {
                let ack = num_field(m, "sessionId");
                // Once polling, the screencast's frames are acknowledged and
                // dropped: shown, each empty one painted the screen white over
                // the last screenshot (the blink, 2026-10-08).
                if self.poll_ms.is_none() && self.phase == Phase::Running {
                    let b64 = str_field(m, "data").unwrap_or("").to_string();
                    out.push(Effect::Present { b64, source: Source::Screencast });
                }
                if let Some(id) = ack {
                    self.send(Req::Ignore, "Page.screencastFrameAck", &format!("{{\"sessionId\":{id}}}"), true, out);
                }
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, m: &[u8], out: &mut Vec<Effect>) {
        let Some(id) = num_field(m, "id") else { return };
        let Some(i) = self.pending.iter().position(|p| p.id == id) else { return };
        let p = self.pending.remove(i);
        let err = m.starts_with(format!("{{\"id\":{id},\"error\"").as_bytes());
        let detail = || String::from_utf8_lossy(m).chars().take(200).collect::<String>();
        match p.req {
            Req::Ignore => {}
            Req::GetTargets if err => self.finish(Some(format!("Target.getTargets: {}", detail())), out),
            Req::GetTargets => {
                self.lap("Target.getTargets", out);
                match first_page(m) {
                    Some(t) => {
                        self.target = t;
                        self.fresh = false;
                        self.attach(out);
                    }
                    None => {
                        let url = escape(self.cfg.url.as_deref().unwrap_or(&self.cfg.home));
                        self.send(Req::CreateTarget, "Target.createTarget", &format!("{{\"url\":\"{url}\"}}"), false, out);
                    }
                }
            }
            Req::CreateTarget => match str_field(m, "targetId") {
                Some(t) if !err => {
                    self.target = t.to_string();
                    self.fresh = true;
                    self.attach(out);
                }
                _ => self.finish(Some(format!("Target.createTarget: {}", detail())), out),
            },
            Req::Attach => match str_field(m, "sessionId") {
                Some(s) if !err => {
                    self.session = Some(s.to_string());
                    let what = format!("tab {} (fresh={}) attached", self.target, self.fresh);
                    self.lap(&what, out);
                    self.phase = Phase::Setup;
                    let (w, h) = self.cfg.view;
                    // `--window-size` includes the (invisible) window frame in
                    // new headless mode, so pin the viewport to what the screen
                    // shows; and an opaque white base, so an unpainted page is
                    // white, not the transparent black Chromium sends.
                    let metrics = format!("{{\"width\":{w},\"height\":{h},\"deviceScaleFactor\":1,\"mobile\":false}}");
                    self.send(Req::Ignore, "Emulation.setDeviceMetricsOverride", &metrics, true, out);
                    self.send(
                        Req::Ignore,
                        "Emulation.setDefaultBackgroundColorOverride",
                        "{\"color\":{\"r\":255,\"g\":255,\"b\":255,\"a\":1}}",
                        true,
                        out,
                    );
                    self.send(Req::PageEnable, "Page.enable", "{}", true, out);
                }
                _ => self.finish(Some(format!("Target.attachToTarget: {}", detail())), out),
            },
            Req::PageEnable => {
                self.lap("viewport + Page.enable", out);
                match (self.fresh, self.cfg.url.clone()) {
                    (false, Some(url)) => {
                        self.phase = Phase::Navigating;
                        self.navigated = false;
                        self.navigate_deadline = self.now + NAVIGATE_BUDGET;
                        self.send(Req::Ignore, "Page.navigate", &format!("{{\"url\":\"{}\"}}", escape(&url)), true, out);
                    }
                    _ => self.start_screencast(out),
                }
            }
            Req::StartScreencast if err => {
                // A navigation in flight: the new page is not attached yet.
                if detail().contains("Not attached to an active page") && self.screencast_tries < 50 {
                    self.screencast_tries += 1;
                    self.retry_at = Some(self.now + 100);
                } else {
                    self.finish(Some(format!("Page.startScreencast: {}", detail())), out);
                }
            }
            Req::StartScreencast => {
                let what = format!("Page.startScreencast ({} retries)", self.screencast_tries);
                self.lap(&what, out);
                let since = self.started.unwrap_or(0);
                out.push(Effect::Log(format!(
                    "startup: ready for frames, {} ms since launch",
                    self.now.saturating_sub(since)
                )));
                self.phase = Phase::Running;
                self.next_shot = self.now;
            }
            Req::Shot => {
                self.shot_inflight = false;
                out.push(Effect::Log(format!(
                    "captureScreenshot: {} ms, reply {} B{}",
                    self.now.saturating_sub(p.sent),
                    m.len(),
                    if err { " (error)" } else { "" }
                )));
                if !err {
                    let b64 = str_field(m, "data").unwrap_or("");
                    let h = hash(b64.as_bytes());
                    if h != self.last_shot {
                        self.last_shot = h;
                        out.push(Effect::Present { b64: b64.to_string(), source: Source::Shot });
                    }
                }
            }
            Req::Collect => {
                out.push(Effect::Log(format!("hints: collect reply {}", detail())));
                let n = if err { 0 } else { num_field(m, "value").unwrap_or(0) as usize };
                self.nav.begin_hints(n);
                if n == 0 {
                    self.eval(Req::Why, "__kami.why()", out);
                }
                if n > 0 {
                    let len = nav::label_len(n);
                    let list = (0..n).map(|i| format!("\"{}\"", nav::label(i, len))).collect::<Vec<_>>().join(",");
                    self.eval(Req::Ignore, &format!("__kami.draw([{list}])"), out);
                }
            }
            Req::Why => out.push(Effect::Log(format!("hints: nothing found; {}", detail()))),
            Req::Click => {
                out.push(Effect::Log(format!("hints: click reply {}", detail())));
                let v = if err { "" } else { str_field(m, "value").unwrap_or("") };
                let mut f = v.split(',');
                if let (Some(x), Some(y), Some(e)) = (f.next(), f.next(), f.next()) {
                    if let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) {
                        self.mouse("mouseMoved", x, y, "", out);
                        self.mouse("mousePressed", x, y, ",\"button\":\"left\",\"buttons\":1,\"clickCount\":1", out);
                        self.mouse("mouseReleased", x, y, ",\"button\":\"left\",\"buttons\":0,\"clickCount\":1", out);
                        if e == "1" {
                            self.nav.mode = Mode::Insert;
                        }
                    }
                }
            }
        }
    }

    fn attach(&mut self, out: &mut Vec<Effect>) {
        self.phase = Phase::Attaching;
        let p = format!("{{\"targetId\":\"{}\",\"flatten\":true}}", self.target);
        self.send(Req::Attach, "Target.attachToTarget", &p, false, out);
    }

    // ---- frames --------------------------------------------------------

    fn on_presented(&mut self, source: Source, ok: bool, empty: bool, out: &mut Vec<Effect>) {
        if ok && !empty {
            self.frames += 1;
            if self.first_pixels {
                self.first_pixels = false;
                out.push(Effect::Log(format!("FIRST PIXELS ({source:?})")));
            }
            if self.cfg.max_frames == Some(self.frames) {
                self.finish(None, out);
            }
        }
        // On Akuma the screencast's frames can arrive fully transparent (the
        // compositor's video capture reads an empty buffer) while
        // captureScreenshot is correct: the first empty frame switches to
        // polling.
        if source == Source::Screencast && ok && empty && self.poll_ms.is_none() {
            out.push(Effect::Log("screencast frames are empty; polling Page.captureScreenshot instead".into()));
            self.poll_ms = Some(500);
            self.next_shot = self.now;
        }
    }

    // ---- keys ----------------------------------------------------------

    fn on_key(&mut self, k: Input, out: &mut Vec<Effect>) {
        if self.phase != Phase::Running {
            if matches!(k, Input::Quit) {
                self.finish(None, out);
            }
            return;
        }
        // Look at the page soon after input rather than at the next poll.
        if self.poll_ms.is_some() {
            self.next_shot = self.next_shot.min(self.now + 150);
        }
        let acts = self.nav.feed(&k);
        out.push(Effect::Log(format!("input {k:?} (mode {:?}) -> {acts:?}", self.nav.mode)));
        for act in acts {
            self.act(act, out);
        }
    }

    fn mouse(&mut self, kind: &str, x: f64, y: f64, extra: &str, out: &mut Vec<Effect>) {
        let p = format!("{{\"type\":\"{kind}\",\"x\":{x},\"y\":{y}{extra}}}");
        self.send(Req::Ignore, "Input.dispatchMouseEvent", &p, true, out);
    }

    fn key(&mut self, key: &str, code: u32, out: &mut Vec<Effect>) {
        let (kind, text) = if key == "Enter" { ("keyDown", ",\"text\":\"\\r\"") } else { ("rawKeyDown", "") };
        let down = format!("{{\"type\":\"{kind}\",\"key\":\"{key}\",\"code\":\"{key}\",\"windowsVirtualKeyCode\":{code}{text}}}");
        let up = format!("{{\"type\":\"keyUp\",\"key\":\"{key}\",\"code\":\"{key}\",\"windowsVirtualKeyCode\":{code}}}");
        self.send(Req::Ignore, "Input.dispatchKeyEvent", &down, true, out);
        self.send(Req::Ignore, "Input.dispatchKeyEvent", &up, true, out);
    }

    fn act(&mut self, act: Action, out: &mut Vec<Effect>) {
        let (w, h) = (self.cfg.view.0 as f64, self.cfg.view.1 as f64);
        match act {
            Action::Quit => self.finish(None, out),
            Action::Reload => self.send(Req::Ignore, "Page.reload", "{}", true, out),
            Action::Key(name, code) => self.key(name, code, out),
            Action::Text(t) => {
                self.send(Req::Ignore, "Input.insertText", &format!("{{\"text\":\"{}\"}}", escape(&t)), true, out)
            }
            Action::Scroll(s) => {
                let dy = match s {
                    Scroll::Line(n) => n as f64 * 120.0,
                    Scroll::Half(n) => n as f64 * h / 2.0,
                    Scroll::Edge(n) => n as f64 * 1.0e6,
                };
                self.mouse("mouseWheel", w / 2.0, h / 2.0, &format!(",\"deltaX\":0,\"deltaY\":{dy}"), out);
            }
            Action::Back => self.eval(Req::Ignore, "history.back()", out),
            Action::Forward => self.eval(Req::Ignore, "history.forward()", out),
            Action::Blur => self.eval(Req::Ignore, "__kami.blur()", out),
            Action::HintStart => self.eval(Req::Collect, "__kami.collect()", out),
            Action::HintFilter(p) => self.eval(Req::Ignore, &format!("__kami.filter(\"{}\")", escape(&p)), out),
            Action::HintCancel => self.eval(Req::Ignore, "__kami.clear()", out),
            Action::HintClick(i) => self.eval(Req::Click, &format!("__kami.click({i})"), out),
        }
    }

    // ---- status --------------------------------------------------------

    /// The status line for the current state; the shell shows it as is.
    fn status_text(&self) -> String {
        let mut s = match self.phase {
            Phase::Connecting if self.spawned => {
                format!("kami: starting chromium (a cold start takes ~10 s)  {}s", self.secs(self.started.unwrap_or(0)))
            }
            Phase::Connecting => "kami: connecting".into(),
            Phase::Targets => format!("kami: waiting for chromium to answer  {}s", self.secs(self.lap_at)),
            Phase::Attaching => "kami: opening the tab".into(),
            Phase::Setup => "kami: preparing the tab".into(),
            Phase::Navigating => format!("kami: loading the page  {}s", self.secs(self.lap_at)),
            Phase::Screencast => format!("kami: waiting for the first frame  {}s", self.secs(self.lap_at)),
            Phase::Running => format!(
                "{}{}  {}",
                self.nav.status(),
                if self.loading { "  [loading]" } else { "" },
                self.url
            ),
            Phase::Done => "kami: closing".into(),
        };
        if let Some(p) = self.pending.iter().filter(|p| self.now.saturating_sub(p.sent) > STALL_AFTER).min_by_key(|p| p.sent) {
            s.push_str(&format!("  !! no answer to {} for {}s", p.method, self.secs(p.sent)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: Option<&str>) -> Config {
        Config {
            url: url.map(str::to_string),
            home: "https://home/".into(),
            view: (960, 584),
            poll_ms: None,
            max_frames: None,
            seconds: None,
        }
    }

    /// The `Send` messages in `effs`.
    fn sent(effs: &[Effect]) -> Vec<&str> {
        effs.iter().filter_map(|e| if let Effect::Send(s) = e { Some(s.as_str()) } else { None }).collect()
    }

    fn has(effs: &[Effect], pred: impl Fn(&Effect) -> bool) -> bool {
        effs.iter().any(pred)
    }

    /// The id of the last request in `effs`.
    fn last_id(effs: &[Effect]) -> u64 {
        let s = sent(effs).last().copied().expect("a request was sent").to_string();
        num_field(s.as_bytes(), "id").unwrap()
    }

    fn reply(id: u64, result: &str) -> Event {
        Event::Cdp(format!("{{\"id\":{id},\"result\":{result}}}").into_bytes())
    }

    fn event(name: &str, params: &str, session: &str) -> Event {
        Event::Cdp(format!("{{\"method\":\"{name}\",\"params\":{params},\"sessionId\":\"{session}\"}}").into_bytes())
    }

    /// Drive a machine through connect, attach, setup and screencast start.
    fn running(url: Option<&str>) -> Machine {
        let mut m = Machine::new(cfg(url));
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T1","type":"page","url":"about:blank"}]}"#));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S1"}"#));
        assert!(sent(&e).iter().any(|s| s.contains("Page.enable")));
        let e = m.handle(reply(last_id(&e), "{}"));
        if url.is_some() {
            assert!(sent(&e)[0].contains("Page.navigate"));
            m.handle(event("Page.frameNavigated", r#"{"frame":{"id":"F","url":"http://x/"}}"#, "S1"));
            let e = m.handle(Event::Tick(10));
            m.handle(reply(last_id(&e), "{}"));
        } else {
            m.handle(reply(last_id(&e), "{}"));
        }
        assert_eq!(m.phase, Phase::Running);
        m
    }

    #[test]
    fn connects_to_a_running_daemon_without_spawning() {
        let mut m = Machine::new(cfg(None));
        let e = m.handle(Event::Tick(0));
        assert!(has(&e, |x| *x == Effect::TryConnect { spawn: false }));
        // Not asked twice while the first attempt is out.
        assert!(!has(&m.handle(Event::Tick(50)), |x| matches!(x, Effect::TryConnect { .. })));
        let e = m.handle(Event::Connected);
        assert!(sent(&e)[0].contains("Target.getTargets"));
    }

    #[test]
    fn spawns_the_daemon_once_after_the_first_failure_and_then_retries_quietly() {
        let mut m = Machine::new(cfg(None));
        m.handle(Event::Tick(0));
        m.handle(Event::ConnectFailed);
        assert!(!has(&m.handle(Event::Tick(50)), |x| matches!(x, Effect::TryConnect { .. })), "retry waits 100 ms");
        let e = m.handle(Event::Tick(120));
        assert!(has(&e, |x| *x == Effect::TryConnect { spawn: true }));
        m.handle(Event::ConnectFailed);
        let e = m.handle(Event::Tick(300));
        assert!(has(&e, |x| *x == Effect::TryConnect { spawn: false }), "never spawns twice");
    }

    #[test]
    fn gives_up_when_the_daemon_never_comes_up() {
        let mut m = Machine::new(cfg(None));
        m.handle(Event::Tick(0));
        let e = m.handle(Event::Tick(CONNECT_BUDGET + 1));
        assert!(has(&e, |x| matches!(x, Effect::Done(Some(_)))));
    }

    #[test]
    fn startup_walks_through_the_phases_and_reports_each() {
        let mut m = Machine::new(cfg(None));
        let e = m.handle(Event::Tick(0));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.contains("connecting"))));
        let e = m.handle(Event::Connected);
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.contains("waiting for chromium"))));
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T1","type":"page"}]}"#));
        assert!(sent(&e)[0].contains("Target.attachToTarget") && sent(&e)[0].contains("T1"));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S1"}"#));
        let s = sent(&e);
        assert!(s[0].contains("setDeviceMetricsOverride") && s[0].contains("\"sessionId\":\"S1\""));
        assert!(s[0].contains("\"width\":960") && s[0].contains("\"height\":584"));
        assert!(s[2].contains("Page.enable"));
        let e = m.handle(reply(last_id(&e), "{}"));
        assert!(sent(&e)[0].contains("Page.startScreencast"));
        let e = m.handle(reply(last_id(&e), "{}"));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.starts_with("NORMAL") || s.starts_with("Normal") || s.contains("scroll"))));
    }

    #[test]
    fn creates_a_tab_when_there_is_none() {
        let mut m = Machine::new(cfg(Some("http://example.com")));
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[]}"#));
        assert!(sent(&e)[0].contains("Target.createTarget") && sent(&e)[0].contains("example.com"));
        let e = m.handle(reply(last_id(&e), r#"{"targetId":"NEW"}"#));
        assert!(sent(&e)[0].contains("Target.attachToTarget") && sent(&e)[0].contains("NEW"));
        assert!(m.fresh);
    }

    #[test]
    fn navigates_an_existing_tab_and_waits_for_the_frame_but_not_forever() {
        let mut m = Machine::new(cfg(Some("http://example.com")));
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T","type":"page"}]}"#));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S"}"#));
        let e = m.handle(reply(last_id(&e), "{}")); // Page.enable
        assert!(sent(&e)[0].contains("Page.navigate"));
        // Nothing happens until the event or the deadline...
        assert!(sent(&m.handle(Event::Tick(5_000))).is_empty());
        // ...and after the deadline the screencast starts regardless (the
        // 2026-10-09 ryzen run: frameNavigated never came).
        let e = m.handle(Event::Tick(NAVIGATE_BUDGET + 1));
        assert!(sent(&e)[0].contains("Page.startScreencast"));
    }

    #[test]
    fn retries_the_screencast_while_the_page_is_not_attached_yet() {
        let mut m = Machine::new(cfg(None));
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T","type":"page"}]}"#));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S"}"#));
        let e = m.handle(reply(last_id(&e), "{}"));
        let id = last_id(&e);
        let e = m.handle(Event::Cdp(
            format!("{{\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"Not attached to an active page\"}}}}").into_bytes(),
        ));
        assert!(sent(&e).is_empty() && !has(&e, |x| matches!(x, Effect::Done(_))));
        assert!(sent(&m.handle(Event::Tick(50))).is_empty());
        assert!(sent(&m.handle(Event::Tick(150)))[0].contains("Page.startScreencast"));
    }

    #[test]
    fn a_silent_chromium_shows_on_the_status_line_and_keys_still_work() {
        let mut m = Machine::new(cfg(None));
        m.handle(Event::Tick(0));
        m.handle(Event::Connected); // getTargets goes unanswered
        let e = m.handle(Event::Tick(6_000));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.contains("no answer to Target.getTargets"))));
        // Quitting works even now: this is the point of the machine.
        let e = m.handle(Event::Key(Input::Quit));
        assert!(has(&e, |x| *x == Effect::Done(None)));
    }

    #[test]
    fn quit_detaches_the_session() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Quit));
        let s = sent(&e);
        assert!(s.iter().any(|x| x.contains("Page.stopScreencast")));
        assert!(s.iter().any(|x| x.contains("Target.detachFromTarget") && x.contains("S1")));
        assert!(has(&e, |x| *x == Effect::Done(None)));
    }

    #[test]
    fn screencast_frames_are_presented_and_acked() {
        let mut m = running(None);
        let e = m.handle(event("Page.screencastFrame", r#"{"data":"QUJD","sessionId":7}"#, "S1"));
        assert!(has(&e, |x| matches!(x, Effect::Present { source: Source::Screencast, .. })));
        assert!(sent(&e).iter().any(|s| s.contains("screencastFrameAck") && s.contains("\"sessionId\":7")));
        // Another session's frames are not ours.
        let e = m.handle(event("Page.screencastFrame", r#"{"data":"QUJD","sessionId":8}"#, "OTHER"));
        assert!(e.iter().all(|x| !matches!(x, Effect::Present { .. })));
    }

    #[test]
    fn an_empty_first_frame_switches_to_polling_and_later_frames_are_only_acked() {
        let mut m = running(None);
        m.handle(event("Page.screencastFrame", r#"{"data":"QUJD","sessionId":1}"#, "S1"));
        let e = m.handle(Event::Presented { source: Source::Screencast, ok: true, empty: true });
        assert!(has(&e, |x| matches!(x, Effect::Log(s) if s.contains("polling"))));
        let e = m.handle(event("Page.screencastFrame", r#"{"data":"QUJD","sessionId":2}"#, "S1"));
        assert!(e.iter().all(|x| !matches!(x, Effect::Present { .. })));
        assert!(sent(&e).iter().any(|s| s.contains("screencastFrameAck")));
        // And the next tick asks for a screenshot.
        let e = m.handle(Event::Tick(1_000));
        assert!(sent(&e)[0].contains("Page.captureScreenshot"));
    }

    #[test]
    fn polling_keeps_one_screenshot_in_flight_and_skips_identical_ones() {
        let mut c = cfg(None);
        c.poll_ms = Some(500);
        let mut m = Machine::new(c);
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T","type":"page"}]}"#));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S"}"#));
        let e = m.handle(reply(last_id(&e), "{}"));
        m.handle(reply(last_id(&e), "{}"));
        let e = m.handle(Event::Tick(1_000));
        let shot = last_id(&e);
        assert!(sent(&e)[0].contains("captureScreenshot"));
        // Still in flight: no second request however long it takes.
        assert!(sent(&m.handle(Event::Tick(3_000))).is_empty());
        let e = m.handle(reply(shot, r#"{"data":"AAAA"}"#));
        assert!(has(&e, |x| matches!(x, Effect::Present { source: Source::Shot, .. })));
        let e = m.handle(Event::Tick(4_000));
        let e = m.handle(reply(last_id(&e), r#"{"data":"AAAA"}"#));
        assert!(e.iter().all(|x| !matches!(x, Effect::Present { .. })), "same pixels, nothing to show");
        let e = m.handle(Event::Tick(5_000));
        let e = m.handle(reply(last_id(&e), r#"{"data":"BBBB"}"#));
        assert!(has(&e, |x| matches!(x, Effect::Present { .. })));
    }

    #[test]
    fn j_scrolls_with_a_wheel_event_at_the_viewport_centre() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Text("j".into())));
        let s = sent(&e)[0];
        assert!(s.contains("mouseWheel") && s.contains("\"x\":480") && s.contains("\"y\":292") && s.contains("\"deltaY\":120"));
    }

    #[test]
    fn hints_collect_draw_and_click() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Text("f".into())));
        assert!(sent(&e)[0].contains("__kami.collect()"));
        let e = m.handle(reply(last_id(&e), r#"{"result":{"type":"number","value":3}}"#));
        // The expression sits inside a JSON string, so its quotes are escaped.
        assert!(sent(&e)[0].contains(r#"__kami.draw([\"a\",\"s\",\"d\"])"#));
        assert_eq!(m.nav.mode, Mode::Hint);
        let e = m.handle(Event::Key(Input::Text("s".into())));
        assert!(sent(&e)[0].contains("__kami.click(1)"));
        let e = m.handle(reply(last_id(&e), r#"{"result":{"type":"string","value":"120.5,300,0"}}"#));
        let s = sent(&e);
        assert_eq!(s.len(), 3);
        assert!(s[0].contains("mouseMoved") && s[1].contains("mousePressed") && s[2].contains("mouseReleased"));
        assert!(s[1].contains("\"x\":120.5") && s[1].contains("\"y\":300"));
        assert_eq!(m.nav.mode, Mode::Normal);
    }

    #[test]
    fn clicking_a_text_field_enters_insert_mode() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Text("f".into())));
        let e = m.handle(reply(last_id(&e), r#"{"result":{"value":1}}"#));
        assert!(!sent(&e).is_empty());
        let e = m.handle(Event::Key(Input::Text("a".into())));
        m.handle(reply(last_id(&e), r#"{"result":{"value":"10,20,1"}}"#));
        assert_eq!(m.nav.mode, Mode::Insert);
    }

    #[test]
    fn no_hints_found_stays_in_normal_mode() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Text("f".into())));
        let e = m.handle(reply(last_id(&e), r#"{"result":{"value":0}}"#));
        // It asks the page why, so an empty result is explained in the log.
        assert!(sent(&e).len() == 1 && sent(&e)[0].contains("__kami.why()"));
        assert_eq!(m.nav.mode, Mode::Normal);
        let e = m.handle(reply(last_id(&e), r#"{"result":{"value":"{\"matched\":2}"}}"#));
        assert!(has(&e, |x| matches!(x, Effect::Log(s) if s.contains("nothing found") && s.contains("matched"))));
    }

    #[test]
    fn enter_and_text_reach_the_page() {
        let mut m = running(None);
        let e = m.handle(Event::Key(Input::Key("Enter", 13)));
        let s = sent(&e);
        assert!(s[0].contains("keyDown") && s[0].contains("\\r") && s[1].contains("keyUp"));
        m.handle(Event::Key(Input::Text("i".into())));
        let e = m.handle(Event::Key(Input::Text("hi".into())));
        assert!(sent(&e)[0].contains("Input.insertText"));
    }

    #[test]
    fn tracks_loading_and_the_top_frame_url() {
        let mut m = running(None);
        let e = m.handle(event("Page.frameStartedLoading", r#"{"frameId":"F"}"#, "S1"));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.contains("[loading]"))));
        m.handle(event("Page.frameNavigated", r#"{"frame":{"id":"SUB","parentId":"F","url":"http://ads/"}}"#, "S1"));
        let e = m.handle(event("Page.frameNavigated", r#"{"frame":{"id":"F","url":"http://top/"}}"#, "S1"));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if s.contains("http://top/") && !s.contains("ads"))));
        let e = m.handle(event("Page.frameStoppedLoading", r#"{"frameId":"F"}"#, "S1"));
        assert!(has(&e, |x| matches!(x, Effect::Status(s) if !s.contains("[loading]"))));
    }

    #[test]
    fn time_limits_end_the_session() {
        let mut c = cfg(None);
        c.seconds = Some(2.0);
        let mut m = Machine::new(c);
        m.handle(Event::Tick(0));
        assert!(has(&m.handle(Event::Tick(2_100)), |x| *x == Effect::Done(None)));

        let mut c = cfg(None);
        c.max_frames = Some(2);
        let mut m = Machine::new(c);
        m.handle(Event::Tick(0));
        m.handle(Event::Connected);
        let e = m.handle(reply(1, r#"{"targetInfos":[{"targetId":"T","type":"page"}]}"#));
        let e = m.handle(reply(last_id(&e), r#"{"sessionId":"S"}"#));
        let e = m.handle(reply(last_id(&e), "{}"));
        m.handle(reply(last_id(&e), "{}"));
        m.handle(Event::Presented { source: Source::Shot, ok: true, empty: false });
        let e = m.handle(Event::Presented { source: Source::Shot, ok: true, empty: false });
        assert!(has(&e, |x| *x == Effect::Done(None)));
    }

    #[test]
    fn the_clock_stepping_back_is_harmless() {
        let mut m = running(None);
        m.handle(Event::Tick(10_000));
        let e = m.handle(Event::Tick(9_000)); // another core's clock
        assert!(!has(&e, |x| matches!(x, Effect::Done(_))));
        assert_eq!(m.now, 10_000);
    }

    #[test]
    fn a_vanished_daemon_ends_the_session_with_an_error() {
        let mut m = running(None);
        let e = m.handle(Event::DaemonGone);
        assert!(has(&e, |x| matches!(x, Effect::Done(Some(_)))));
    }
}

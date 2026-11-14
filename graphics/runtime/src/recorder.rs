//! Per-frame recorder: turns observed render-target bindings and draws into a `Vec<Pass>`.
//!
//! `Recorder` is plain data (no D3D), so it is tested on the host. The `d3d` hooks translate
//! D3D objects into `Target` values and call the free functions at the bottom.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::classify::{ao_stage, classify, copy_pass_after, is_copy_shape, stages_through_combine, AoStage, ClassifyError, SceneMap};
use crate::d3d::{self, Hooks};
use crate::diag::{self, Limiter};
use crate::gate::{self, Gate};
use crate::log;
use crate::passes::{Pass, Target};
use crate::{ao, halo, output, resolve, shaders, ssr, upgrade};

const MAX_PASSES: usize = 4096;

/// A binding that started (or re-bound, before any draw) a pass: its index within the frame, the
/// first render target of the pass before it, and whether this pass is the UI pass (decided from
/// the passes before it, see `is_ui_start`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassStart {
    pub index: usize,
    pub prev_rt0: Option<Target>,
    pub ui_start: bool,
    /// Where the pass sits for the AO (only evaluated while the AO is on, otherwise `None`).
    pub ao: AoStage,
}

/// Does a pass that starts now, after `frame_so_far` (the passes completed in the current frame),
/// begin the UI? Structural, independent of absolute pass indices: the combine pass must already be
/// there, and the last completed pass must be the first full-size, no-depth, scene-colour,
/// one-draw pass that follows it (the copy, #17 of the reference frame).
pub fn is_ui_start(frame_so_far: &[Pass], screen: (u32, u32)) -> bool {
    // Cheap rejection first: this runs at every pass start.
    let Some(last) = frame_so_far.last() else { return false };
    if !is_copy_shape(last, screen) {
        return false;
    }
    let Ok(stages) = stages_through_combine(frame_so_far, screen) else { return false };
    copy_pass_after(frame_so_far, stages.combine, screen) == Some(frame_so_far.len() - 1)
}

/// The pass a draw belongs to (see `Recorder::site`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub index: usize,
    pub rt0: Option<Target>,
    pub depth: Option<Target>,
}

#[derive(Default)]
pub struct Recorder {
    passes: Vec<Pass>,
    open: bool,
    screen: (u32, u32),
}

impl Recorder {
    pub const fn new() -> Self {
        Recorder { passes: Vec::new(), open: false, screen: (0, 0) }
    }

    /// Back-buffer size used to recognise the UI start.
    pub fn set_screen(&mut self, screen: (u32, u32)) {
        self.screen = screen;
    }

    /// A binding that differs from the current pass's starts a new pass. A pass that has not
    /// drawn anything yet is just re-bound instead of kept (an unused binding is not a pass).
    /// Returns where a pass started or was re-bound; `None` when the binding changed nothing.
    pub fn om_set_render_targets(&mut self, rt: &[Target], depth: Option<Target>) -> Option<PassStart> {
        if self.open {
            if let Some(last) = self.passes.last_mut() {
                if last.rt == rt && last.depth == depth {
                    return None;
                }
                if last.draws == 0 && last.dispatches == 0 {
                    last.rt = rt.to_vec();
                    last.depth = depth;
                    return Some(self.start_of(self.passes.len() - 1));
                }
            }
        }
        if self.passes.len() < MAX_PASSES {
            self.passes.push(Pass { rt: rt.to_vec(), depth, draws: 0, dispatches: 0 });
            self.open = true;
            return Some(self.start_of(self.passes.len() - 1));
        }
        None
    }

    fn start_of(&self, index: usize) -> PassStart {
        PassStart {
            index,
            prev_rt0: index.checked_sub(1).and_then(|i| self.passes[i].rt.first().copied()),
            ui_start: is_ui_start(&self.passes[..index], self.screen),
            ao: if ao::needs_stages() { ao_stage(&self.passes[..index], &self.passes[index], self.screen) } else { AoStage::None },
        }
    }

    /// Where the current pass is (draw diagnostic): its index and first colour / depth target.
    pub fn site(&self) -> Option<Site> {
        let i = self.passes.len().checked_sub(1).filter(|_| self.open)?;
        let p = &self.passes[i];
        Some(Site { index: i, rt0: p.rt.first().copied(), depth: p.depth })
    }

    pub fn draw(&mut self) {
        if let (true, Some(p)) = (self.open, self.passes.last_mut()) {
            p.draws = p.draws.saturating_add(1);
        }
    }

    pub fn dispatch(&mut self) {
        if let (true, Some(p)) = (self.open, self.passes.last_mut()) {
            p.dispatches = p.dispatches.saturating_add(1);
        }
    }

    /// Returns the frame's passes and starts the next frame.
    pub fn frame_end(&mut self) -> Vec<Pass> {
        self.open = false;
        std::mem::take(&mut self.passes)
    }
}

/// Immediate contexts recorded at the same time.
pub const MAX_CONTEXTS: usize = 8;

/// One `Recorder` per immediate context: the game renders through one of them, hudhook / ReShade
/// usually through the same, but a probe or a second device has its own pass sequence.
#[derive(Default)]
pub struct ContextRecorders {
    list: Vec<(usize, Recorder)>,
    /// Contexts that did not fit (all slots busy this frame).
    pub dropped: u64,
}

impl ContextRecorders {
    pub const fn new() -> Self {
        ContextRecorders { list: Vec::new(), dropped: 0 }
    }

    /// The recorder of `ctx`, created on its first call. When all slots are in use, one that has
    /// recorded nothing this frame (a stale context) is taken over; otherwise the call is dropped.
    pub fn recorder(&mut self, ctx: usize) -> Option<&mut Recorder> {
        let i = match self.list.iter().position(|(c, _)| *c == ctx) {
            Some(i) => i,
            None if self.list.len() < MAX_CONTEXTS => {
                self.list.push((ctx, Recorder::new()));
                self.list.len() - 1
            }
            None => match self.list.iter().position(|(_, r)| r.passes.is_empty()) {
                Some(i) => {
                    self.list[i] = (ctx, Recorder::new());
                    i
                }
                None => {
                    self.dropped += 1;
                    return None;
                }
            },
        };
        Some(&mut self.list[i].1)
    }

    /// Ends the frame of every context and returns what each recorded.
    pub fn frame_end(&mut self) -> Vec<(usize, Vec<Pass>)> {
        self.list.iter_mut().map(|(c, r)| (*c, r.frame_end())).collect()
    }
}

/// Draws of a frame.
pub fn frame_draws(frame: &[Pass]) -> u64 {
    frame.iter().map(|p| p.draws as u64).sum()
}

/// The frame the classifier gets.
#[derive(Debug, PartialEq, Eq)]
pub struct Picked {
    /// The context with the most draws (0 when no context recorded anything).
    pub ctx: usize,
    pub frame: Vec<Pass>,
    /// (context, draws) of every context that recorded a pass, for the diagnostics.
    pub draws: Vec<(usize, u64)>,
}

/// The game's frame is the one of the immediate context with the MOST draws this frame; the others
/// (probes, other tools' private contexts) are ignored. A tie goes to the first (then to more passes).
pub fn pick_game_frame(frames: Vec<(usize, Vec<Pass>)>) -> Picked {
    let draws: Vec<(usize, u64)> = frames.iter().filter(|(_, f)| !f.is_empty()).map(|(c, f)| (*c, frame_draws(f))).collect();
    let mut best: Option<(usize, Vec<Pass>)> = None;
    for (ctx, frame) in frames {
        if frame.is_empty() {
            continue;
        }
        let key = |f: &[Pass]| (frame_draws(f), f.len());
        if best.as_ref().is_none_or(|(_, b)| key(&frame) > key(b)) {
            best = Some((ctx, frame));
        }
    }
    let (ctx, frame) = best.unwrap_or((0, Vec::new()));
    Picked { ctx, frame, draws }
}

fn format_target(prefix: &str, t: &Target) -> String {
    format!("{prefix} {}x{} {}", t.w, t.h, crate::passes::format_name(t.fmt))
}

/// One frame in the diagnostic's text format (the inverse of `passes::parse_frame_log`), numbered `n`.
/// Shader lines are not written.
pub fn format_numbered(n: u64, frame: &[Pass]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "===== FRAME {n} : {} segments =====", frame.len());
    for (i, p) in frame.iter().enumerate() {
        let mut parts: Vec<String> = p.rt.iter().enumerate().map(|(k, t)| format_target(&format!("RT{k}"), t)).collect();
        if let Some(d) = &p.depth {
            parts.push(format_target("DS", d));
        }
        let targets = if parts.is_empty() { "(no targets)".to_string() } else { parts.join(" | ") };
        let _ = writeln!(s, "#{i}  {targets}");
        let _ = writeln!(s, "     draws={} dispatches={}", p.draws, p.dispatches);
    }
    s
}

#[allow(dead_code)] // the dump numbers its frames (`format_numbered`); this is the unnumbered form
pub fn format_frame_log(frame: &[Pass]) -> String {
    format_numbered(0, frame)
}

static DUMP: AtomicBool = AtomicBool::new(false);
static DUMP_DIR: OnceLock<PathBuf> = OnceLock::new();
static FRAME_NO: AtomicU64 = AtomicU64::new(0);
static DUMP_STATUS: Mutex<String> = Mutex::new(String::new());

/// Folder the frame dumps go to (`GW2Graphics`).
pub fn set_dump_dir(dir: PathBuf) {
    let _ = DUMP_DIR.set(dir);
}

/// Asks for the next complete frame to be written to `frame-passes-<unix seconds>.log`.
pub fn request_dump() {
    DUMP.store(true, Ordering::Relaxed);
}

pub fn take_dump_request() -> bool {
    DUMP.swap(false, Ordering::Relaxed)
}

/// Result of the last dump (empty before the first).
pub fn dump_status() -> String {
    DUMP_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Header of the dump written when no frame has been recorded (the overlay shows the same text).
pub const NO_FRAME: &str = "no frame recorded yet (the hooks have not seen any render-target binding)";

/// Content of the dump file when there is no frame: the reason and the hook counters.
pub fn format_no_frame(counters: &str) -> String {
    format!("{NO_FRAME}\n{counters}\n")
}

/// Overlay status after a dump without a frame.
pub fn no_frame_status(file: &str, counters: &str) -> String {
    format!("wrote {file}: {NO_FRAME}; {counters}")
}

/// Writes `text` to `frame-passes-<unix seconds>.log`; Ok(file name) or the error text.
fn write_dump_file(text: &str) -> Result<String, String> {
    write_dump_named("frame-passes", text)
}

/// Writes `text` to `<prefix>-<unix seconds>.log` in the dump folder; Ok(file name) or the error text.
pub fn write_dump_named(prefix: &str, text: &str) -> Result<String, String> {
    let Some(dir) = DUMP_DIR.get() else { return Err("no data folder".to_string()) };
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let name = format!("{prefix}-{secs}.log");
    std::fs::write(dir.join(&name), text).map(|()| name.clone()).map_err(|e| format!("could not write {name}: {e}"))
}

fn finish_dump(msg: String) {
    log!("frame dump: {msg}");
    *DUMP_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = msg;
}

fn write_dump(n: u64, frame: &[Pass]) {
    finish_dump(match write_dump_file(&format_numbered(n, frame)) {
        Ok(name) => format!("wrote {name} ({} passes)", frame.len()),
        Err(e) => e,
    });
}

/// A dump was asked for but no frame has been recorded: a file with the reason and the counters is
/// written anyway, so the click always has an answer.
fn write_no_frame(counters: &str) {
    finish_dump(match write_dump_file(&format_no_frame(counters)) {
        Ok(name) => no_frame_status(&name, counters),
        Err(e) => format!("{NO_FRAME}; {e}; {counters}"),
    });
}

/// A verdict is logged when its state changes (match <-> no match, another reason, another screen
/// size) and as a heartbeat every `HEARTBEAT_MS` while it stays the same.
pub const HEARTBEAT_MS: u64 = 30_000;

#[derive(Default)]
pub struct ChangeGate {
    last: Option<(String, (u32, u32))>,
    last_logged_ms: u64,
}

impl ChangeGate {
    pub const fn new() -> Self {
        ChangeGate { last: None, last_logged_ms: 0 }
    }

    pub fn should_log(&mut self, key: &str, screen: (u32, u32), now_ms: u64) -> bool {
        let same = self.last.as_ref().is_some_and(|l| l.0 == key && l.1 == screen);
        if same && now_ms.saturating_sub(self.last_logged_ms) < HEARTBEAT_MS {
            return false;
        }
        if !same {
            self.last = Some((key.to_string(), screen));
        }
        self.last_logged_ms = now_ms;
        true
    }
}

fn describe(r: &Result<SceneMap, ClassifyError>) -> (String, String) {
    match r {
        Ok(m) => ("match".to_string(), format!("frame matches: opaque={} ui={}", m.opaque, m.ui)),
        Err(e) => (e.to_string(), format!("frame does not match: {e}")),
    }
}

#[cfg(test)]
/// Verdict of a frame: (key that identifies the outcome, log line).
pub fn verdict(frame: &[Pass], screen: (u32, u32)) -> (String, String) {
    describe(&classify(frame, screen))
}

/// What a finished frame decides.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    /// New state of the enable gate (HDR substitutes and resolve).
    pub enabled: bool,
    /// Index of the UI pass when the frame classified.
    pub ui: Option<usize>,
    pub key: String,
    pub line: String,
}

/// Classifies the frame and feeds the gate. A frame only counts as a match while the float upgrade
/// is ready (at least one target upgraded, no failure): without it the gate stays disabled.
pub fn step(frame: &[Pass], screen: (u32, u32), gate: &mut Gate, upgrade_ready: bool) -> Step {
    let r = classify(frame, screen);
    let (key, line) = describe(&r);
    let enabled = gate.update(r.is_ok() && upgrade_ready);
    Step { enabled, ui: r.ok().map(|m| m.ui), key, line }
}

static RECORDERS: Mutex<ContextRecorders> = Mutex::new(ContextRecorders::new());
/// The context whose frame was classified last (0 = none yet).
static GAME_CTX: AtomicUsize = AtomicUsize::new(0);
static CTX_DRAWS: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());
static GATE: Mutex<ChangeGate> = Mutex::new(ChangeGate::new());
static ENABLE_GATE: Mutex<Gate> = Mutex::new(Gate::new());
static GATE_ON: AtomicBool = AtomicBool::new(false);
static LAST_VERDICT: Mutex<String> = Mutex::new(String::new());

/// The enable gate is open (HDR substitutes and resolve may run).
pub fn gate_on() -> bool {
    GATE_ON.load(Ordering::Relaxed)
}

/// Last classifier line (`frame matches: ...` or `frame does not match: <reason>`).
pub fn last_verdict() -> String {
    LAST_VERDICT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn rec() -> std::sync::MutexGuard<'static, ContextRecorders> {
    RECORDERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The immediate context whose frame the classifier got last (0 until the first frame).
pub fn game_context() -> usize {
    GAME_CTX.load(Ordering::Relaxed)
}

// Called from the D3D hooks (GW2 renders from one thread, so the lock is uncontended).
pub fn om_set_render_targets(ctx: usize, rt: &[Target], depth: Option<Target>, screen: (u32, u32)) -> Option<PassStart> {
    let mut all = rec();
    let r = all.recorder(ctx)?;
    r.set_screen(screen);
    r.om_set_render_targets(rt, depth)
}

pub fn draw(ctx: usize) {
    if let Some(r) = rec().recorder(ctx) {
        r.draw();
    }
}

/// The pass the next draw of `ctx` belongs to (only used while the draw capture is armed).
pub fn current_site(ctx: usize) -> Option<Site> {
    rec().recorder(ctx)?.site()
}

pub fn dispatch(ctx: usize) {
    if let Some(r) = rec().recorder(ctx) {
        r.dispatch();
    }
}

/// Frame tick (from the overlay's render loop): classifies the finished frame, feeds the enable
/// gate and, at this frame boundary only, switches the HDR substitutes and the resolve. Logs the
/// verdict and the gate state when they change.
pub fn frame_end(hooks: &Hooks) {
    // One recorder per immediate context: the frame of the one with the most draws is the game's.
    let picked = pick_game_frame(rec().frame_end());
    GAME_CTX.store(picked.ctx, Ordering::Relaxed);
    if let Ok(mut d) = CTX_DRAWS.try_lock() {
        *d = picked.draws;
    }
    let frame = picked.frame;
    let n = FRAME_NO.fetch_add(1, Ordering::Relaxed);
    LAST_PASSES.store(frame.len(), Ordering::Relaxed);
    // Draw diagnostic: when a capture was armed, the frame that just ended was the captured one.
    crate::drawdiag::finish_frame(n, picked.ctx, &frame, hooks.screen());
    if take_dump_request() {
        // The same click also records the per-draw state of the NEXT frame (draws-<unix>.log).
        crate::drawdiag::arm();
        if frame.is_empty() {
            write_no_frame(&diag::counters_text(&snapshot_now(hooks, 0)));
        } else {
            write_dump(n, &frame);
        }
    }
    let screen = hooks.screen();
    // The user's Enabled switch takes effect here, at the frame boundary.
    let user = gate::user_enabled();
    upgrade::set_enabled(user);
    crate::hdr_shaders::set_vanilla(!user);
    let (st, was) = {
        let mut g = ENABLE_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let was = shaders::substitution_enabled();
        // The gate does not depend on the user's Enabled any more: with the mod off the frame is still recognised
        // so that the substitutes can reproduce the vanilla look on the (still float) scene.
        (step(&frame, screen, &mut g, upgrade::float_scene_exists()), was)
    };
    resolve::frame_begin();
    ao::frame_begin();
    halo::frame_begin();
    ssr::frame_begin();
    crate::shafts::frame_begin();
    crate::aa::frame_begin();
    crate::look::frame_begin();
    crate::fog::frame_begin();
    crate::contact::frame_begin();
    crate::exposure::frame_begin();
    crate::furmask::frame_begin();
    crate::shellfur::frame_begin();
    crate::furpick::frame_begin();
    let sw = gate::switches(user, st.enabled, output::mode());
    shaders::set_enabled(sw.substitutes);
    resolve::set_enabled(sw.resolve);
    ao::set_enabled(sw.resolve);
    halo::set_enabled(sw.resolve);
    crate::exposure::set_enabled(sw.resolve);
    output::apply(st.enabled);
    GATE_ON.store(st.enabled, Ordering::Relaxed);
    if let Ok(mut v) = LAST_VERDICT.try_lock() {
        if *v != st.line {
            *v = st.line.clone();
        }
    }
    let now = crate::log::uptime_ms();
    if GATE.lock().unwrap_or_else(|e| e.into_inner()).should_log(&st.key, screen, now) {
        log!("{} (passes={}, screen={}x{})", st.line, frame.len(), screen.0, screen.1);
    }
    // While the classifier does not match: the hook counters, every 5 s, 30 lines at most.
    let not_matching = st.ui.is_none();
    if DIAG_LIMIT.lock().unwrap_or_else(|e| e.into_inner()).due(now, not_matching) {
        log!("{}", diag::hooks_line(&snapshot_now(hooks, frame.len()), &st.line));
    }
    if st.enabled != was {
        log!("gate: HDR substitutes and resolve {}", if st.enabled { "enabled" } else { "disabled" });
    }
}

static DIAG_LIMIT: Mutex<Limiter> = Mutex::new(Limiter::new(5_000, 30));
/// Passes of the last finished frame.
static LAST_PASSES: AtomicUsize = AtomicUsize::new(0);

fn snapshot_now(_hooks: &Hooks, passes: usize) -> diag::Snapshot {
    let ctx_draws = CTX_DRAWS.try_lock().map(|d| d.clone()).unwrap_or_default();
    let (tex, ps) = d3d::device_top3();
    diag::snapshot(ctx_draws, game_context(), passes, d3d::method_snaps(), tex, ps)
}

/// The hook counters as one text (overlay).
pub fn counters_text(hooks: &Hooks) -> String {
    diag::counters_text(&snapshot_now(hooks, LAST_PASSES.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::FMT_B8G8R8A8_UNORM;

    const A: Target = Target { w: 100, h: 50, fmt: FMT_B8G8R8A8_UNORM };
    const B: Target = Target { w: 25, h: 12, fmt: 10 };
    const D: Target = Target { w: 100, h: 50, fmt: 45 };

    #[test]
    fn new_pass_when_targets_change_and_draws_accumulate() {
        let mut r = Recorder::new();
        r.om_set_render_targets(&[A], Some(D));
        r.draw();
        r.draw();
        r.dispatch();
        r.om_set_render_targets(&[A], Some(D)); // same binding: same pass
        r.draw();
        r.om_set_render_targets(&[B], None);
        r.draw();
        let f = r.frame_end();
        assert_eq!(
            f,
            vec![
                Pass { rt: vec![A], depth: Some(D), draws: 3, dispatches: 1 },
                Pass { rt: vec![B], depth: None, draws: 1, dispatches: 0 },
            ]
        );
    }

    #[test]
    fn depth_change_alone_starts_a_pass() {
        let mut r = Recorder::new();
        r.om_set_render_targets(&[A], Some(D));
        r.draw();
        r.om_set_render_targets(&[A], None);
        r.draw();
        assert_eq!(r.frame_end().len(), 2);
    }

    #[test]
    fn binding_without_draws_is_replaced_not_kept() {
        let mut r = Recorder::new();
        r.om_set_render_targets(&[A], None);
        r.om_set_render_targets(&[B], None);
        r.draw();
        assert_eq!(r.frame_end(), vec![Pass { rt: vec![B], depth: None, draws: 1, dispatches: 0 }]);
    }

    #[test]
    fn draws_before_any_binding_are_ignored() {
        let mut r = Recorder::new();
        r.draw();
        r.dispatch();
        assert!(r.frame_end().is_empty());
    }

    #[test]
    fn frame_end_resets_for_the_next_frame() {
        let mut r = Recorder::new();
        r.om_set_render_targets(&[A], None);
        r.draw();
        assert_eq!(r.frame_end().len(), 1);
        assert!(r.frame_end().is_empty());
        r.om_set_render_targets(&[A], None); // same binding as last frame still opens a pass
        r.draw();
        assert_eq!(r.frame_end().len(), 1);
    }

    #[test]
    fn pass_count_is_capped() {
        let mut r = Recorder::new();
        for i in 0..(MAX_PASSES as u32 + 10) {
            r.om_set_render_targets(&[Target { w: i + 1, h: 1, fmt: 0 }], None);
            r.draw();
        }
        assert_eq!(r.frame_end().len(), MAX_PASSES);
    }

    #[test]
    fn gate_logs_state_changes() {
        let mut g = ChangeGate::new();
        let s = (3440, 1440);
        assert!(g.should_log("match", s, 0));
        assert!(!g.should_log("match", s, 1_000));
        assert!(g.should_log("no opaque pass", s, 2_000)); // match -> no match
        assert!(!g.should_log("no opaque pass", s, 3_000));
        assert!(g.should_log("no linear depth pass", s, 4_000)); // another reason
        assert!(g.should_log("match", s, 5_000)); // no match -> match
    }

    #[test]
    fn gate_logs_a_screen_change_while_the_verdict_stays() {
        // The real log: "no opaque pass" at 3424x1369, then the window became 3440x1440 and nothing was logged.
        let mut g = ChangeGate::new();
        assert!(g.should_log("no opaque pass", (3424, 1369), 10_000));
        assert!(!g.should_log("no opaque pass", (3424, 1369), 11_000));
        assert!(g.should_log("no opaque pass", (3440, 1440), 14_000));
        assert!(!g.should_log("no opaque pass", (3440, 1440), 15_000));
    }

    #[test]
    fn gate_repeats_the_same_verdict_every_30_seconds() {
        let mut g = ChangeGate::new();
        let s = (100, 50);
        assert!(g.should_log("no opaque pass", s, 5_000));
        assert!(!g.should_log("no opaque pass", s, 5_000 + HEARTBEAT_MS - 1));
        assert!(g.should_log("no opaque pass", s, 5_000 + HEARTBEAT_MS));
        assert!(!g.should_log("no opaque pass", s, 5_000 + HEARTBEAT_MS + 1));
        assert!(g.should_log("no opaque pass", s, 5_000 + 2 * HEARTBEAT_MS));
        // A heartbeat also applies to a match.
        assert!(g.should_log("match", s, 5_000 + 2 * HEARTBEAT_MS + 1));
        assert!(g.should_log("match", s, 5_000 + 3 * HEARTBEAT_MS + 1));
    }

    // ---- one recorder per immediate context ----

    fn drawn(rt: Target, draws: u32) -> Pass {
        Pass { rt: vec![rt], depth: None, draws, dispatches: 0 }
    }

    #[test]
    fn each_context_records_its_own_passes() {
        let mut c = ContextRecorders::new();
        c.recorder(0x10).unwrap().om_set_render_targets(&[A], None);
        c.recorder(0x10).unwrap().draw();
        c.recorder(0x20).unwrap().om_set_render_targets(&[B], None);
        c.recorder(0x20).unwrap().draw();
        c.recorder(0x20).unwrap().draw();
        let frames = c.frame_end();
        assert_eq!(frames, vec![(0x10, vec![drawn(A, 1)]), (0x20, vec![drawn(B, 2)])]);
        // The next frame starts empty for both.
        assert!(c.frame_end().iter().all(|(_, f)| f.is_empty()));
    }

    #[test]
    fn the_context_with_the_most_draws_is_the_games() {
        let game = vec![drawn(A, 500), drawn(B, 300)];
        let hudhook = vec![drawn(D, 40)];
        let picked = pick_game_frame(vec![(0x10, hudhook.clone()), (0x20, game.clone())]);
        assert_eq!(picked.ctx, 0x20);
        assert_eq!(picked.frame, game);
        assert_eq!(picked.draws, vec![(0x10, 40), (0x20, 800)]);
        // Order does not matter.
        assert_eq!(pick_game_frame(vec![(0x20, game.clone()), (0x10, hudhook)]).ctx, 0x20);
    }

    #[test]
    fn a_tie_goes_to_the_first_context() {
        let p = pick_game_frame(vec![(0x10, vec![drawn(A, 5)]), (0x20, vec![drawn(B, 5)])]);
        assert_eq!(p.ctx, 0x10);
        // Same draws: the one with more passes wins.
        let p = pick_game_frame(vec![(0x10, vec![drawn(A, 6)]), (0x20, vec![drawn(B, 3), drawn(A, 3)])]);
        assert_eq!(p.ctx, 0x20);
    }

    #[test]
    fn no_context_with_passes_gives_an_empty_frame() {
        let p = pick_game_frame(vec![(0x10, vec![]), (0x20, vec![])]);
        assert_eq!((p.ctx, p.frame.len(), p.draws.len()), (0, 0, 0));
        assert_eq!(pick_game_frame(vec![]).ctx, 0);
    }

    #[test]
    fn a_context_without_draws_still_loses_to_one_with_draws() {
        let p = pick_game_frame(vec![(0x10, vec![drawn(A, 0), drawn(B, 0), drawn(D, 0)]), (0x20, vec![drawn(A, 1)])]);
        assert_eq!(p.ctx, 0x20);
    }

    #[test]
    fn context_slots_are_bounded_and_stale_ones_are_reused() {
        let mut c = ContextRecorders::new();
        for i in 0..MAX_CONTEXTS {
            let r = c.recorder(0x100 + i * 0x10).unwrap();
            r.om_set_render_targets(&[A], None);
            r.draw();
        }
        // All busy this frame: a ninth is dropped.
        assert!(c.recorder(0x900).is_none());
        assert_eq!(c.dropped, 1);
        // After the frame ended every slot is idle: the ninth takes one over.
        c.frame_end();
        assert!(c.recorder(0x900).is_some());
        assert_eq!(c.dropped, 1);
    }

    #[test]
    fn om_set_reports_pass_starts() {
        let mut r = Recorder::new();
        assert_eq!(r.om_set_render_targets(&[A], None), Some(PassStart { index: 0, prev_rt0: None, ui_start: false, ao: AoStage::None }));
        assert_eq!(r.om_set_render_targets(&[A], None), None); // same binding
        r.draw();
        assert_eq!(r.om_set_render_targets(&[B], None), Some(PassStart { index: 1, prev_rt0: Some(A), ui_start: false, ao: AoStage::None }));
        // Re-binding an undrawn pass keeps its index.
        assert_eq!(r.om_set_render_targets(&[D], None), Some(PassStart { index: 1, prev_rt0: Some(A), ui_start: false, ao: AoStage::None }));
        r.draw();
        assert_eq!(r.om_set_render_targets(&[A], None), Some(PassStart { index: 2, prev_rt0: Some(D), ui_start: false, ao: AoStage::None }));
    }

    #[test]
    fn step_enables_after_three_matching_frames_and_disables_on_mismatch() {
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let screen = (3440, 1440);
        let mut g = Gate::new();
        assert!(!step(&f, screen, &mut g, true).enabled);
        assert!(!step(&f, screen, &mut g, true).enabled);
        let s = step(&f, screen, &mut g, true);
        assert!(s.enabled);
        assert_eq!(s.ui, Some(18));
        // One mismatching frame does not turn the gate off, two in a row do.
        let s = step(&[], screen, &mut g, true);
        assert!(s.enabled);
        assert_eq!(s.ui, None);
        assert!(!step(&[], screen, &mut g, true).enabled);
    }

    #[test]
    fn step_stays_disabled_without_a_ready_upgrade() {
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let mut g = Gate::new();
        for _ in 0..5 {
            let s = step(&f, (3440, 1440), &mut g, false);
            assert!(!s.enabled);
            assert_eq!(s.key, "match"); // the classification itself is still reported
        }
    }

    const FIXTURE: &str = crate::frame_fixture::FRAME_PASSES;

    #[test]
    fn format_then_parse_roundtrips_fixture() {
        let f = crate::passes::parse_frame_log(FIXTURE);
        assert_eq!(f.len(), 21);
        let text = format_frame_log(&f);
        assert!(text.starts_with("===== FRAME 0 : 21 segments ====="), "{text}");
        assert_eq!(crate::passes::parse_frame_log(&text), f);
    }

    #[test]
    fn format_frame_log_shapes() {
        let f = vec![
            Pass { rt: vec![A, B], depth: Some(D), draws: 3, dispatches: 1 },
            Pass { rt: vec![], depth: Some(D), draws: 1, dispatches: 0 },
            Pass { rt: vec![], depth: None, draws: 0, dispatches: 0 },
            Pass { rt: vec![Target { w: 1, h: 1, fmt: 9999 }], depth: None, draws: 2, dispatches: 0 },
        ];
        let text = format_numbered(7, &f);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "===== FRAME 7 : 4 segments =====");
        assert_eq!(lines[1], "#0  RT0 100x50 B8G8R8A8_UNORM | RT1 25x12 R16G16B16A16_FLOAT | DS 100x50 D24_UNORM_S8_UINT");
        assert_eq!(lines[2], "     draws=3 dispatches=1");
        assert_eq!(lines[3], "#1  DS 100x50 D24_UNORM_S8_UINT");
        assert_eq!(lines[5], "#2  (no targets)");
        assert!(lines[7].contains("FMT_9999"));
        assert_eq!(crate::passes::parse_frame_log(&text), f);
    }

    #[test]
    fn no_frame_dump_says_why_and_carries_the_counters() {
        let text = format_no_frame("hooks: om_set calls=0 accepted=0");
        assert!(text.starts_with("no frame recorded yet (the hooks have not seen any render-target binding)
"), "{text}");
        assert!(text.contains("hooks: om_set calls=0 accepted=0"));
        assert!(text.ends_with('\n'));
        let status = no_frame_status("frame-passes-1.log", "hooks: x");
        assert_eq!(status, "wrote frame-passes-1.log: no frame recorded yet (the hooks have not seen any render-target binding); hooks: x");
    }

    #[test]
    fn dump_request_is_taken_once() {
        request_dump();
        assert!(take_dump_request());
        assert!(!take_dump_request());
    }

    #[test]
    fn verdict_keys_and_lines() {
        let (k, l) = verdict(&[], (100, 50));
        assert_eq!((k.as_str(), l.as_str()), ("no opaque pass", "frame does not match: no opaque pass"));
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let (k, l) = verdict(&f, (3440, 1440));
        assert_eq!((k.as_str(), l.as_str()), ("match", "frame matches: opaque=3 ui=18"));
    }

    // ---- is_ui_start on the reference frame ----

    const SCREEN: (u32, u32) = (3440, 1440);

    fn overlay() -> Pass {
        Pass { rt: vec![Target { w: 3440, h: 1440, fmt: FMT_B8G8R8A8_UNORM }], depth: None, draws: 5, dispatches: 0 }
    }

    #[test]
    fn ui_starts_right_after_the_copy_pass() {
        let f = crate::passes::parse_frame_log(FIXTURE);
        assert!(is_ui_start(&f[..18], SCREEN)); // at the start of #18
        assert!(!is_ui_start(&f[..17], SCREEN)); // at the start of #17 (the copy)
        assert!(!is_ui_start(&f[..16], SCREEN)); // at the start of #16
        assert!(!is_ui_start(&f[..15], SCREEN));
        assert!(!is_ui_start(&f[..19], SCREEN)); // after the UI pass started
        assert!(!is_ui_start(&f[..21], SCREEN));
        assert!(!is_ui_start(&[], SCREEN));
    }

    #[test]
    fn ui_start_does_not_depend_on_a_leading_overlay_pass() {
        let mut f = vec![overlay()];
        f.extend(crate::passes::parse_frame_log(FIXTURE));
        assert!(is_ui_start(&f[..19], SCREEN));
        assert!(!is_ui_start(&f[..18], SCREEN));
        assert!(!is_ui_start(&f[..17], SCREEN));
        assert!(!is_ui_start(&f[..20], SCREEN));
    }

    #[test]
    fn frame_without_combine_has_no_ui_start() {
        let mut f = crate::passes::parse_frame_log(FIXTURE);
        f.remove(15); // combine
        f.remove(12); // luminance
        assert!(!is_ui_start(&f[..16], SCREEN));
        let loading = vec![Pass { rt: vec![A], depth: None, draws: 1, dispatches: 0 }; 3];
        assert!(!is_ui_start(&loading, (100, 50)));
    }

    #[test]
    fn recorder_flags_the_ui_pass_start_only() {
        // Replay the reference frame through the recorder: only the binding of #18 is a UI start.
        let f = crate::passes::parse_frame_log(FIXTURE);
        let mut r = Recorder::new();
        r.set_screen(SCREEN);
        let mut starts = Vec::new();
        for (i, p) in f.iter().enumerate() {
            let st = r.om_set_render_targets(&p.rt, p.depth).expect("new pass");
            assert_eq!(st.index, i);
            if st.ui_start {
                starts.push(i);
            }
            for _ in 0..p.draws {
                r.draw();
            }
        }
        assert_eq!(starts, vec![18]);
        // The same with an overlay pass first: the UI start moves with the pass, nothing else flags.
        let mut r = Recorder::new();
        r.set_screen(SCREEN);
        let mut starts = Vec::new();
        for (i, p) in std::iter::once(overlay()).chain(f.iter().cloned()).enumerate() {
            if r.om_set_render_targets(&p.rt, p.depth).is_some_and(|s| s.ui_start) {
                starts.push(i);
            }
            for _ in 0..p.draws {
                r.draw();
            }
        }
        assert_eq!(starts, vec![19]);
    }
}

//! Hook diagnostics: counters kept by the D3D hooks (atomics, counted BEFORE any filtering) and the
//! one-line summary that explains what the hooks see. Rate limits and formatting are pure so they are tested.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub static OM_CALLS: AtomicU64 = AtomicU64::new(0);
pub static OM_ACCEPTED: AtomicU64 = AtomicU64::new(0);
pub static OM_IGNORED: AtomicU64 = AtomicU64::new(0);
pub static DRAWS: AtomicU64 = AtomicU64::new(0);
pub static PS_CALLS: AtomicU64 = AtomicU64::new(0);
pub static PS_RECORDED: AtomicU64 = AtomicU64::new(0);
pub static PS_ENGINE: AtomicU64 = AtomicU64::new(0);

/// Distinct `this` values remembered per method.
pub const SEEN: usize = 8;
/// Marker for "GetType not asked / not a context".
pub const TYPE_NONE: u32 = u32::MAX;

/// What one call of a hooked method reported.
#[derive(Debug, PartialEq, Eq)]
pub struct Noted {
    /// The first call of this method.
    pub first_call: bool,
    /// Index in the distinct list when this `this` was not seen before (and there was room).
    pub new_this: Option<usize>,
}

/// Per-method call statistics (atomics, no allocation). `last_vtbl` / `last_slot` are read from the live
/// object at call time (the object is valid then), so the summary never dereferences a stored pointer.
pub struct MethodStats {
    calls: AtomicU64,
    seen: [AtomicUsize; SEEN],
    types: [AtomicU32; SEEN],
    last_vtbl: AtomicUsize,
    last_slot: AtomicUsize,
}

impl MethodStats {
    pub const fn new() -> Self {
        MethodStats {
            calls: AtomicU64::new(0),
            seen: [const { AtomicUsize::new(0) }; SEEN],
            types: [const { AtomicU32::new(TYPE_NONE) }; SEEN],
            last_vtbl: AtomicUsize::new(0),
            last_slot: AtomicUsize::new(0),
        }
    }

    pub fn note(&self, this: usize, vtbl: usize, slot: usize) -> Noted {
        let first_call = self.calls.fetch_add(1, Relaxed) == 0;
        self.last_vtbl.store(vtbl, Relaxed);
        self.last_slot.store(slot, Relaxed);
        let mut new_this = None;
        if this != 0 {
            for (i, s) in self.seen.iter().enumerate() {
                match s.compare_exchange(0, this, Relaxed, Relaxed) {
                    Ok(_) => {
                        new_this = Some(i);
                        break;
                    }
                    Err(cur) if cur == this => break,
                    Err(_) => {}
                }
            }
        }
        Noted { first_call, new_this }
    }

    /// Records the `GetType` result of the distinct value at `idx`.
    pub fn set_type(&self, idx: usize, ty: u32) {
        if let Some(t) = self.types.get(idx) {
            t.store(ty, Relaxed);
        }
    }

    /// (calls, distinct `this` with their types, last vtable pointer, last slot value).
    pub fn read(&self) -> (u64, Vec<(usize, u32)>, usize, usize) {
        let seen = self.seen.iter().zip(&self.types).map(|(s, t)| (s.load(Relaxed), t.load(Relaxed))).filter(|&(s, _)| s != 0).collect();
        (self.calls.load(Relaxed), seen, self.last_vtbl.load(Relaxed), self.last_slot.load(Relaxed))
    }
}

/// Bounded lock-free counter per key (devices): the first `N` distinct keys are counted.
pub struct Counted<const N: usize> {
    keys: [AtomicUsize; N],
    counts: [AtomicU64; N],
}

impl<const N: usize> Counted<N> {
    pub const fn new() -> Self {
        Counted { keys: [const { AtomicUsize::new(0) }; N], counts: [const { AtomicU64::new(0) }; N] }
    }

    pub fn note(&self, key: usize) {
        if key == 0 {
            return;
        }
        for (k, c) in self.keys.iter().zip(&self.counts) {
            match k.compare_exchange(0, key, Relaxed, Relaxed) {
                Ok(_) => {
                    c.fetch_add(1, Relaxed);
                    return;
                }
                Err(cur) if cur == key => {
                    c.fetch_add(1, Relaxed);
                    return;
                }
                Err(_) => {}
            }
        }
    }

    /// The `n` keys with the most calls, most first.
    pub fn top(&self, n: usize) -> Vec<(usize, u64)> {
        let mut v: Vec<(usize, u64)> = self.keys.iter().zip(&self.counts).map(|(k, c)| (k.load(Relaxed), c.load(Relaxed))).filter(|&(k, _)| k != 0).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
}

/// One hooked method in the summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodSnap {
    pub name: &'static str,
    /// "inline", "vtable", "inline+vtable" or "none".
    pub how: &'static str,
    pub calls: u64,
    /// Distinct `this` values seen (up to `SEEN`) with their `GetType` (`TYPE_NONE` = n/a).
    pub seen: Vec<(usize, u32)>,
    pub last_vtbl: usize,
    pub vtbl_module: String,
    pub last_slot: usize,
    pub slot_module: String,
}

/// Everything the summary line shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub om_calls: u64,
    pub om_accepted: u64,
    pub om_ignored: u64,
    pub draws: u64,
    pub ps_calls: u64,
    pub ps_recorded: u64,
    pub ps_engine: u64,
    /// (immediate context, draws) of the last finished frame.
    pub contexts: Vec<(usize, u64)>,
    /// The context whose frame was classified (0 = none yet).
    pub game_ctx: usize,
    pub passes: usize,
    pub methods: Vec<MethodSnap>,
    /// Top devices by CreateTexture2D / CreatePixelShader calls.
    pub dev_tex: Vec<(usize, u64)>,
    pub dev_ps: Vec<(usize, u64)>,
}

pub fn snapshot(contexts: Vec<(usize, u64)>, game_ctx: usize, passes: usize, methods: Vec<MethodSnap>, dev_tex: Vec<(usize, u64)>, dev_ps: Vec<(usize, u64)>) -> Snapshot {
    Snapshot {
        om_calls: OM_CALLS.load(Relaxed),
        om_accepted: OM_ACCEPTED.load(Relaxed),
        om_ignored: OM_IGNORED.load(Relaxed),
        draws: DRAWS.load(Relaxed),
        ps_calls: PS_CALLS.load(Relaxed),
        ps_recorded: PS_RECORDED.load(Relaxed),
        ps_engine: PS_ENGINE.load(Relaxed),
        contexts,
        game_ctx,
        passes,
        methods,
        dev_tex,
        dev_ps,
    }
}

fn pairs(v: &[(usize, u64)]) -> String {
    v.iter().map(|(a, n)| format!("{a:#x}:{n}")).collect::<Vec<_>>().join(",")
}

fn ty_text(t: u32) -> String {
    if t == TYPE_NONE { "?".to_string() } else { t.to_string() }
}

fn at_module(addr: usize, module: &str) -> String {
    format!("{addr:#x}@{}", if module.is_empty() { "?" } else { module })
}

/// One method of the summary: how it is hooked, calls, the first three distinct `this` with their
/// `GetType`, and where the vtable pointer / slot of the last `this` point.
pub fn method_text(m: &MethodSnap) -> String {
    if m.calls == 0 {
        return format!("{}[{} calls=0]", m.name, m.how);
    }
    let first: Vec<String> = m.seen.iter().take(3).map(|&(t, ty)| format!("{t:#x}:t{}", ty_text(ty))).collect();
    format!(
        "{}[{} calls={} distinct this={} ({}) vtbl={} slot={}]",
        m.name,
        m.how,
        m.calls,
        m.seen.len(),
        first.join(","),
        at_module(m.last_vtbl, &m.vtbl_module),
        at_module(m.last_slot, &m.slot_module)
    )
}

/// The counters without the per-method detail and the classifier verdict (the dump header and the overlay).
pub fn counters_text(s: &Snapshot) -> String {
    format!(
        "hooks: om_set calls={} recorded={} rejected={} (ignored={}), draws={}; immediate contexts (draws last frame): [{}], classified: {:#x}; recorder: passes this frame={}; create_ps calls={} recorded={} engine={}",
        s.om_calls,
        s.om_accepted,
        s.om_calls.saturating_sub(s.om_accepted).saturating_sub(s.om_ignored),
        s.om_ignored,
        s.draws,
        pairs(&s.contexts),
        s.game_ctx,
        s.passes,
        s.ps_calls,
        s.ps_recorded,
        s.ps_engine
    )
}

/// The 5 s log line (one line, 30 at most).
pub fn hooks_line(s: &Snapshot, verdict: &str) -> String {
    let methods: Vec<String> = s.methods.iter().map(method_text).collect();
    format!(
        "{}; devices top3 CreateTexture2D: [{}] CreatePixelShader: [{}]; last verdict: {}; methods: {}",
        counters_text(s),
        pairs(&s.dev_tex),
        pairs(&s.dev_ps),
        if verdict.is_empty() { "(none yet)" } else { verdict },
        methods.join(" ")
    )
}

/// The file name of a module path (`C:\Windows\System32\d3d11.dll` -> `d3d11.dll`).
pub fn file_name_of(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

/// The `first <method> call` log line (once per method). `ty` is the `GetType` result for a context.
pub fn first_call_line(name: &str, this: usize, ty: Option<u32>) -> String {
    match ty {
        Some(t) => format!("d3d: first {name} call this={this:#x} type={t}"),
        None => format!("d3d: first {name} call this={this:#x} type=n/a (not a context)"),
    }
}

/// Lets a line through every `interval_ms` while `active`, at most `max` times in all.
pub struct Limiter {
    interval_ms: u64,
    max: u32,
    count: u32,
    next_ms: u64,
}

impl Limiter {
    pub const fn new(interval_ms: u64, max: u32) -> Self {
        Limiter { interval_ms, max, count: 0, next_ms: 0 }
    }

    pub fn due(&mut self, now_ms: u64, active: bool) -> bool {
        if !active || self.count >= self.max || now_ms < self.next_ms {
            return false;
        }
        self.count += 1;
        self.next_ms = now_ms + self.interval_ms;
        true
    }
}

/// Lets a bounded number of lines through (creation results, per-call logs).
pub struct Budget(AtomicUsize);

impl Budget {
    pub const fn new() -> Self {
        Budget(AtomicUsize::new(0))
    }

    /// True while fewer than `max` lines were let through.
    pub fn take(&self, max: usize) -> bool {
        self.0.fetch_add(1, Relaxed) < max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &'static str, how: &'static str, calls: u64) -> MethodSnap {
        MethodSnap { name, how, calls, seen: vec![(0x100, 0), (0x200, 1), (0x300, 0), (0x400, 0)], last_vtbl: 0x7ffe0000, vtbl_module: "d3d11.dll".into(), last_slot: 0x7ffe1234, slot_module: "dxgi.dll".into() }
    }

    fn snap() -> Snapshot {
        Snapshot {
            om_calls: 120,
            om_accepted: 100,
            om_ignored: 3,
            draws: 5000,
            ps_calls: 9,
            ps_recorded: 9,
            ps_engine: 2,
            contexts: vec![(0xabc0, 4000), (0xdef0, 12)],
            game_ctx: 0xabc0,
            passes: 4,
            methods: vec![m("OMSetRenderTargets", "inline", 120), m("Dispatch", "none", 0)],
            dev_tex: vec![(0x10, 7), (0x20, 2)],
            dev_ps: vec![(0x10, 300)],
        }
    }

    #[test]
    fn counters_line_has_the_context_fields() {
        let t = counters_text(&snap());
        assert_eq!(
            t,
            "hooks: om_set calls=120 recorded=100 rejected=17 (ignored=3), draws=5000; immediate contexts (draws last frame): [0xabc0:4000,0xdef0:12], classified: 0xabc0; recorder: passes this frame=4; create_ps calls=9 recorded=9 engine=2"
        );
    }

    #[test]
    fn full_line_has_methods_devices_and_verdict() {
        let l = hooks_line(&snap(), "frame does not match: no opaque pass");
        assert!(l.starts_with("hooks: om_set calls=120"), "{l}");
        assert!(l.contains("devices top3 CreateTexture2D: [0x10:7,0x20:2] CreatePixelShader: [0x10:300]"), "{l}");
        assert!(l.contains("last verdict: frame does not match: no opaque pass"), "{l}");
        assert!(l.contains("OMSetRenderTargets[inline calls=120 distinct this=4 (0x100:t0,0x200:t1,0x300:t0) vtbl=0x7ffe0000@d3d11.dll slot=0x7ffe1234@dxgi.dll]"), "{l}");
        assert!(l.ends_with("Dispatch[none calls=0]"), "{l}");
        assert!(!l.contains('\n'));
    }

    #[test]
    fn line_with_nothing_seen() {
        let l = hooks_line(&Snapshot::default(), "");
        assert!(l.starts_with("hooks: om_set calls=0 recorded=0 rejected=0 (ignored=0), draws=0; immediate contexts (draws last frame): [], classified: 0x0;"), "{l}");
        assert!(l.contains("last verdict: (none yet)"), "{l}");
    }

    #[test]
    fn counters_text_has_no_verdict_and_no_method_detail() {
        let t = counters_text(&snap());
        assert!(!t.contains("last verdict") && !t.contains("methods:"));
    }

    #[test]
    fn rejected_never_underflows() {
        let s = Snapshot { om_calls: 1, om_accepted: 2, om_ignored: 5, ..Snapshot::default() };
        assert!(counters_text(&s).contains("rejected=0"));
    }

    #[test]
    fn unknown_module_and_type_are_shown_as_question_marks() {
        let mut x = m("Draw", "vtable", 3);
        x.seen = vec![(0x100, TYPE_NONE)];
        x.vtbl_module = String::new();
        let t = method_text(&x);
        assert!(t.contains("(0x100:t?)") && t.contains("vtbl=0x7ffe0000@?"), "{t}");
    }

    #[test]
    fn method_stats_count_distinct_this_and_cap_at_eight() {
        let s = MethodStats::new();
        let first = s.note(0x1000, 0xAA, 0xBB);
        assert_eq!(first, Noted { first_call: true, new_this: Some(0) });
        assert_eq!(s.note(0x1000, 0xAA, 0xBB), Noted { first_call: false, new_this: None });
        for i in 2..=10usize {
            s.note(0x1000 * i, 1, 2);
        }
        let (calls, seen, vtbl, slot) = s.read();
        assert_eq!(calls, 11);
        assert_eq!(seen.len(), SEEN);
        assert_eq!((vtbl, slot), (1, 2)); // the last call's
        assert_eq!(seen[0].0, 0x1000);
        assert_eq!(s.note(0, 0, 0).new_this, None); // null this is not a value
    }

    #[test]
    fn method_stats_keep_the_type_per_value() {
        let s = MethodStats::new();
        let n = s.note(0x1000, 0, 0).new_this.unwrap();
        s.set_type(n, 1);
        s.set_type(99, 1); // out of range: ignored
        assert_eq!(s.read().1, vec![(0x1000, 1)]);
        let n2 = s.note(0x2000, 0, 0).new_this.unwrap();
        assert_eq!(s.read().1[n2], (0x2000, TYPE_NONE));
    }

    #[test]
    fn counted_gives_the_top_three_devices() {
        let c: Counted<8> = Counted::new();
        for _ in 0..5 {
            c.note(0x10);
        }
        for _ in 0..9 {
            c.note(0x20);
        }
        c.note(0x30);
        for _ in 0..3 {
            c.note(0x40);
        }
        c.note(0);
        assert_eq!(c.top(3), vec![(0x20, 9), (0x10, 5), (0x40, 3)]);
    }

    #[test]
    fn counted_is_bounded() {
        let c: Counted<2> = Counted::new();
        c.note(0x10);
        c.note(0x20);
        c.note(0x30); // no room: not counted, no panic
        assert_eq!(c.top(8).len(), 2);
    }

    #[test]
    fn module_file_names() {
        assert_eq!(file_name_of(r"C:\Windows\System32\d3d11.dll"), "d3d11.dll");
        assert_eq!(file_name_of("G:/Games/GW2/dxgi.dll"), "dxgi.dll");
        assert_eq!(file_name_of("x.dll"), "x.dll");
        assert_eq!(file_name_of(""), "");
    }

    #[test]
    fn first_call_lines() {
        assert_eq!(first_call_line("Draw", 0x1230, Some(0)), "d3d: first Draw call this=0x1230 type=0");
        assert!(first_call_line("CreateTexture2D", 0x10, None).contains("n/a"));
    }

    #[test]
    fn budget_lets_a_bounded_number_through() {
        let b = Budget::new();
        let n = (0..100).filter(|_| b.take(40)).count();
        assert_eq!(n, 40);
    }

    #[test]
    fn limiter_spaces_lines_and_caps_the_total() {
        let mut l = Limiter::new(5_000, 3);
        assert!(l.due(0, true)); // first line right away
        assert!(!l.due(4_999, true));
        assert!(l.due(5_000, true));
        assert!(!l.due(5_001, true));
        assert!(l.due(12_000, true));
        assert!(!l.due(100_000, true)); // 3 lines per session at most
    }

    #[test]
    fn limiter_is_silent_while_inactive_and_does_not_burn_its_budget() {
        let mut l = Limiter::new(5_000, 2);
        assert!(!l.due(0, false));
        assert!(!l.due(60_000, false));
        assert!(l.due(61_000, true)); // matching ended, not matching again: speaks at once
        assert!(!l.due(62_000, true));
        assert!(l.due(66_000, true));
        assert!(!l.due(80_000, true));
    }
}

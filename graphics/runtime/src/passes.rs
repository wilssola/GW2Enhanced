//! Render-pass model and parser for the frame-pass diagnostic log.

pub const FMT_UNKNOWN: u32 = 0;
pub const FMT_R16G16B16A16_FLOAT: u32 = 10;
pub const FMT_R32_FLOAT: u32 = 41;
pub const FMT_B8G8R8A8_UNORM: u32 = 87;

/// DXGI format name -> DXGI_FORMAT number.
const FORMATS: &[(&str, u32)] = &[
    ("UNKNOWN", 0),
    ("R32G32B32A32_FLOAT", 2),
    ("R16G16B16A16_TYPELESS", 9),
    ("R16G16B16A16_FLOAT", 10),
    ("R16G16B16A16_UNORM", 11),
    ("R32G32_FLOAT", 16),
    ("R32G8X24_TYPELESS", 19),
    ("R10G10B10A2_TYPELESS", 23),
    ("R10G10B10A2_UNORM", 24),
    ("R11G11B10_FLOAT", 26),
    ("R8G8B8A8_TYPELESS", 27),
    ("R8G8B8A8_UNORM", 28),
    ("R8G8B8A8_UNORM_SRGB", 29),
    ("R16G16_FLOAT", 34),
    ("R16G16_UNORM", 35),
    ("R32_TYPELESS", 39),
    ("D32_FLOAT", 40),
    ("R32_FLOAT", 41),
    ("R24G8_TYPELESS", 44),
    ("D24_UNORM_S8_UINT", 45),
    ("R8G8_UNORM", 49),
    ("R16_TYPELESS", 15),
    ("R16_FLOAT", 54),
    ("R16_UNORM", 56),
    ("R8_UNORM", 61),
    ("BC1_UNORM", 71),
    ("BC3_UNORM", 77),
    ("BC4_UNORM", 80),
    ("BC5_UNORM", 83),
    ("B8G8R8A8_UNORM", 87),
    ("B8G8R8A8_UNORM_SRGB", 91),
    ("BC7_UNORM", 98),
];

/// Format name (or `FMT_<n>`) to DXGI_FORMAT number; unknown names give 0.
pub fn format_from_name(name: &str) -> u32 {
    if let Some(&(_, n)) = FORMATS.iter().find(|(s, _)| *s == name) {
        return n;
    }
    name.strip_prefix("FMT_").and_then(|n| n.parse().ok()).unwrap_or(FMT_UNKNOWN)
}

/// DXGI_FORMAT number to name (the inverse of `format_from_name`); numbers not in the table give `FMT_<n>`.
pub fn format_name(n: u32) -> String {
    match FORMATS.iter().find(|(_, v)| *v == n) {
        Some((s, _)) => s.to_string(),
        None => format!("FMT_{n}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub w: u32,
    pub h: u32,
    pub fmt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Pass {
    pub rt: Vec<Target>,
    pub depth: Option<Target>,
    pub draws: u32,
    pub dispatches: u32,
}

fn parse_target(s: &str) -> Option<Target> {
    let mut it = s.split_whitespace();
    let (w, h) = it.next()?.split_once('x')?;
    let fmt = format_from_name(it.next()?);
    Some(Target { w: w.parse().ok()?, h: h.parse().ok()?, fmt })
}

/// Parses the diagnostic's text format (one frame) into passes.
pub fn parse_frame_log(text: &str) -> Vec<Pass> {
    let mut passes: Vec<Pass> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('#') {
            let mut pass = Pass::default();
            let targets = rest.split_once(char::is_whitespace).map_or("", |(_, r)| r);
            for part in targets.split('|') {
                let part = part.trim();
                if let Some(r) = part.strip_prefix("DS ") {
                    pass.depth = parse_target(r);
                } else if let Some((tag, r)) = part.split_once(' ') {
                    if tag.starts_with("RT") {
                        if let Some(tg) = parse_target(r) {
                            pass.rt.push(tg);
                        }
                    }
                }
            }
            passes.push(pass);
        } else if t.starts_with("draws=") {
            if let Some(p) = passes.last_mut() {
                for kv in t.split_whitespace() {
                    match kv.split_once('=') {
                        Some(("draws", v)) => p.draws = v.parse().unwrap_or(0),
                        Some(("dispatches", v)) => p.dispatches = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
        }
    }
    passes
}

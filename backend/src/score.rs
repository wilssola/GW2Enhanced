//! Vote scoring and the choice of a text's current translation.

/// z for a 95% confidence interval.
const Z: f64 = 1.96;
/// Down votes that make a translation "flagged by votes" (never current)...
pub const DOWNVOTE_FLAG_MIN: i64 = 5;
/// ...when they are also more than this many times the up votes.
pub const DOWNVOTE_FLAG_RATIO: i64 = 2;
/// Votes needed before a translation can count as "perfect".
pub const PERFECT_MIN_VOTES: i64 = 5;
/// Wilson lower bound needed to count as "perfect".
pub const PERFECT_MIN_SCORE: f64 = 0.8;

/// Lower bound of the Wilson score interval for `up` positive votes out of
/// `up + down`; `0` with no votes.
pub fn wilson_lower_bound(up: i64, down: i64) -> f64 {
    let (up, down) = (up.max(0) as f64, down.max(0) as f64);
    let n = up + down;
    if n == 0.0 {
        return 0.0;
    }
    let p = up / n;
    let z2 = Z * Z;
    let centre = p + z2 / (2.0 * n);
    let margin = Z * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).max(0.0)
}

/// Many more down votes than up votes: treated like a flag.
pub fn flagged_by_votes(up: i64, down: i64) -> bool {
    down >= DOWNVOTE_FLAG_MIN && down > up * DOWNVOTE_FLAG_RATIO
}

/// A translation that could become current.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    /// `passed` is the only review state that can be current.
    pub review: String,
    pub up: i64,
    pub down: i64,
    /// When it passed review (RFC 3339; sorts as text).
    pub passed_at: String,
}

/// The current translation: best Wilson lower bound among the ones that passed
/// review and are not flagged by votes; ties keep the one that passed first, so
/// the first passing translation stays current until votes beat it.
pub fn pick_current(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.review == "passed" && !flagged_by_votes(c.up, c.down))
        .min_by(|a, b| {
            let (sa, sb) = (wilson_lower_bound(a.up, a.down), wilson_lower_bound(b.up, b.down));
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.passed_at.cmp(&b.passed_at))
                .then_with(|| a.id.cmp(&b.id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, review: &str, up: i64, down: i64, at: &str) -> Candidate {
        Candidate {
            id: id.into(),
            review: review.into(),
            up,
            down,
            passed_at: at.into(),
        }
    }

    #[test]
    fn wilson_known_values() {
        assert_eq!(wilson_lower_bound(0, 0), 0.0);
        assert!((wilson_lower_bound(1, 0) - 0.2065).abs() < 1e-3);
        assert!((wilson_lower_bound(10, 0) - 0.7225).abs() < 1e-3);
        assert!((wilson_lower_bound(50, 50) - 0.4038).abs() < 1e-3);
        assert_eq!(wilson_lower_bound(0, 10), 0.0);
        // More evidence at the same ratio scores higher.
        assert!(wilson_lower_bound(90, 10) > wilson_lower_bound(9, 1));
    }

    #[test]
    fn first_passing_wins_until_votes_beat_it() {
        let mut v = vec![
            c("b", "passed", 0, 0, "2026-01-02"),
            c("a", "passed", 0, 0, "2026-01-01"),
            c("p", "pending", 9, 0, ""),
        ];
        assert_eq!(pick_current(&v).unwrap().id, "a");
        v[0].up = 1;
        assert_eq!(pick_current(&v).unwrap().id, "b");
    }

    #[test]
    fn flagged_and_downvoted_never_current() {
        let v = vec![
            c("f", "flagged", 99, 0, "2026-01-01"),
            c("h", "hidden", 99, 0, "2026-01-01"),
            c("d", "passed", 1, 5, "2026-01-01"),
        ];
        assert!(flagged_by_votes(1, 5));
        assert!(!flagged_by_votes(3, 5));
        assert_eq!(pick_current(&v), None);
    }
}

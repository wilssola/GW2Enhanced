//! When an English text or an ID↔English pair becomes trusted.
//!
//! English texts and IDs come from players and can be forged, so:
//! - a text is trusted when the AI review finds it plausible game text, or when
//!   [`TEXT_TRUST_REPORTERS`] distinct IP hashes sent it;
//! - an ID↔text pair is trusted when [`PAIR_AGREE_REPORTERS`] distinct IP
//!   hashes agree on it and it clearly leads any competing text for the same
//!   ID, or when it is the only candidate for that ID and its text is trusted;
//! - a pair that conflicts with an already trusted pair for the same ID, or
//!   whose text was rejected, is rejected.

/// Distinct reporters that make a text trusted without the AI.
pub const TEXT_TRUST_REPORTERS: i64 = 3;
/// Distinct reporters that must agree on an ID↔text pair.
pub const PAIR_AGREE_REPORTERS: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Trusted,
    Rejected,
}

impl State {
    pub fn parse(s: &str) -> State {
        match s {
            "trusted" => State::Trusted,
            "rejected" => State::Rejected,
            _ => State::Pending,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Trusted => "trusted",
            State::Rejected => "rejected",
        }
    }
}

/// Whether a pending text can be trusted from reporters alone.
pub fn text_trusted_by_reporters(reporters: i64) -> bool {
    reporters >= TEXT_TRUST_REPORTERS
}

/// One candidate text for a game ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCandidate {
    pub text_key: String,
    pub reporters: i64,
    pub state: State,
    pub text_state: State,
}

/// New state of every candidate pair for one game ID.
pub fn decide_pairs(cands: &[PairCandidate]) -> Vec<(String, State)> {
    let mut out: Vec<(String, State)> = cands.iter().map(|c| (c.text_key.clone(), c.state)).collect();
    // An already trusted pair wins over everything else.
    if let Some(t) = cands.iter().position(|c| c.state == State::Trusted) {
        for (i, o) in out.iter_mut().enumerate() {
            if i != t {
                o.1 = State::Rejected;
            }
        }
        return out;
    }
    for (i, c) in cands.iter().enumerate() {
        if c.text_state == State::Rejected {
            out[i].1 = State::Rejected;
        }
    }
    let live: Vec<usize> = (0..cands.len()).filter(|&i| out[i].1 == State::Pending).collect();
    let Some(&leader) = live.iter().max_by_key(|&&i| (cands[i].reporters, std::cmp::Reverse(i))) else {
        return out;
    };
    let second = live
        .iter()
        .filter(|&&i| i != leader)
        .map(|&i| cands[i].reporters)
        .max()
        .unwrap_or(0);
    let lead = &cands[leader];
    let agreed =
        lead.reporters >= PAIR_AGREE_REPORTERS && lead.reporters >= second * 2 && lead.text_state != State::Rejected;
    let alone = live.len() == 1 && lead.text_state == State::Trusted;
    if agreed || alone {
        for &i in &live {
            out[i].1 = if i == leader { State::Trusted } else { State::Rejected };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use State::*;

    fn c(k: &str, reporters: i64, state: State, text_state: State) -> PairCandidate {
        PairCandidate {
            text_key: k.into(),
            reporters,
            state,
            text_state,
        }
    }

    #[test]
    fn single_reporter_needs_a_trusted_text() {
        assert_eq!(
            decide_pairs(&[c("a", 1, Pending, Pending)]),
            vec![("a".into(), Pending)]
        );
        assert_eq!(
            decide_pairs(&[c("a", 1, Pending, Trusted)]),
            vec![("a".into(), Trusted)]
        );
    }

    #[test]
    fn agreement_across_reporters_wins_a_conflict() {
        let r = decide_pairs(&[c("a", 3, Pending, Pending), c("b", 1, Pending, Trusted)]);
        assert_eq!(r, vec![("a".into(), Trusted), ("b".into(), Rejected)]);
        // Too close to call: wait for more reports.
        let r = decide_pairs(&[c("a", 2, Pending, Trusted), c("b", 2, Pending, Trusted)]);
        assert_eq!(r, vec![("a".into(), Pending), ("b".into(), Pending)]);
    }

    #[test]
    fn conflicts_with_trusted_pair_and_rejected_texts_are_rejected() {
        let r = decide_pairs(&[c("a", 1, Trusted, Trusted), c("b", 50, Pending, Trusted)]);
        assert_eq!(r[1].1, Rejected);
        let r = decide_pairs(&[c("a", 5, Pending, Rejected)]);
        assert_eq!(r[0].1, Rejected);
    }

    #[test]
    fn text_reporters_threshold() {
        assert!(!text_trusted_by_reporters(2));
        assert!(text_trusted_by_reporters(3));
    }
}

//! The Screenshots tab of the enhanced overlay: one place for every feature's captures. The graphics photo mode (shot, with/without
//! the graphics, UI-free option) and on/off comparisons of the translation and of the HD textures, saved by the same photo mode
//! (`GW2Graphics\photos`, named `gw2-<timestamp>-<feature>-on/-off`).
//!
//! A comparison shoots the current state, switches the feature, waits until the picture follows, shoots again and puts the
//! feature back. The steps are a small pure machine (`advance`, tested); `Screens::tick` drives it once per frame.

use std::time::{Duration, Instant};

/// What a comparison switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    Translation,
    Textures,
}

impl Subject {
    pub fn key(self) -> &'static str {
        match self {
            Subject::Translation => "translation",
            Subject::Textures => "textures",
        }
    }

    /// Time for the picture to follow the switch.
    pub fn settle(self) -> Duration {
        match self {
            Subject::Translation => Duration::from_secs(2),
            #[cfg(feature = "textures")]
            Subject::Textures => gw2remastered::overlay::SWITCH_SETTLE,
            #[cfg(not(feature = "textures"))]
            Subject::Textures => Duration::from_secs(9),
        }
    }

    /// The translation's text is part of the game's UI: its comparison always keeps the UI.
    pub fn hide_ui(self, user_hide: bool) -> bool {
        match self {
            Subject::Translation => false,
            Subject::Textures => user_hide,
        }
    }
}

/// A shot that does not end in this long is given up.
pub const SHOT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The shot of the state the player had.
    First,
    /// The feature was switched; waiting for the picture to follow.
    Settle,
    /// The shot of the other state.
    Second,
}

/// What the driver does now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Wait,
    /// First shot done: switch the feature to the other state.
    Switch,
    /// Settled: take the second shot.
    Shoot,
    /// Second shot done: put the feature back; the comparison is complete.
    Finish,
    /// A shot did not end in time: put the feature back if it was switched, and stop.
    Abort,
}

/// One step of the machine: `since` = time in this step, `shot_done` = the shot asked for in this step ended.
pub fn advance(step: Step, since: Duration, settle: Duration, shot_done: bool) -> Act {
    match step {
        Step::First | Step::Second if shot_done => {
            if step == Step::First {
                Act::Switch
            } else {
                Act::Finish
            }
        }
        Step::First | Step::Second if since >= SHOT_TIMEOUT => Act::Abort,
        Step::Settle if since >= settle => Act::Shoot,
        _ => Act::Wait,
    }
}

/// The file suffix of a shot of the feature in state `on`.
pub fn suffix(on: bool) -> &'static str {
    if on { "-on" } else { "-off" }
}

struct Running {
    subject: Subject,
    stem: String,
    was_on: bool,
    step: Step,
    at: Instant,
    /// `photo::finished_count` when the current shot was asked for.
    base: u64,
}

#[derive(Default)]
pub struct Screens {
    run: Option<Running>,
    status: String,
}

#[cfg(feature = "graphics")]
use gw2graphics::photo;

#[cfg(feature = "graphics")]
impl Screens {
    pub fn busy(&self) -> bool {
        self.run.is_some()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    /// Starts a comparison (ignored while one runs).
    pub fn start(&mut self, subject: Subject) {
        if self.run.is_some() {
            return;
        }
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let was_on = state(subject);
        let stem = photo::compare_stem(unix, subject.key());
        let base = shoot(subject, &stem, was_on);
        self.status = format!("{stem}: first shot ({})", if was_on { "on" } else { "off" });
        self.run = Some(Running { subject, stem, was_on, step: Step::First, at: Instant::now(), base });
    }

    /// Once per frame.
    pub fn tick(&mut self) {
        let Some(r) = self.run.as_mut() else { return };
        let done = photo::finished_count() > r.base;
        match advance(r.step, r.at.elapsed(), r.subject.settle(), done) {
            Act::Wait => {}
            Act::Switch => {
                set_state(r.subject, !r.was_on);
                r.step = Step::Settle;
                r.at = Instant::now();
                self.status = format!("{}: switched, waiting for the picture", r.stem);
            }
            Act::Shoot => {
                r.base = shoot(r.subject, &r.stem, !r.was_on);
                r.step = Step::Second;
                r.at = Instant::now();
                self.status = format!("{}: second shot ({})", r.stem, if r.was_on { "off" } else { "on" });
            }
            Act::Finish => {
                set_state(r.subject, r.was_on);
                self.status = format!("saved {0}-on and {0}-off ({1})", r.stem, photo::status_text());
                self.run = None;
            }
            Act::Abort => {
                if r.step == Step::Second {
                    set_state(r.subject, r.was_on);
                }
                self.status = format!("{}: a shot did not finish, stopped ({})", r.stem, photo::status_text());
                self.run = None;
            }
        }
    }
}

#[cfg(feature = "graphics")]
fn shoot(subject: Subject, stem: &str, on: bool) -> u64 {
    let base = photo::finished_count();
    photo::request_named(photo::Label {
        stem: stem.to_string(),
        suffix: suffix(on).to_string(),
        hide_ui: subject.hide_ui(photo::hide_ui()),
        note: format!("{} {}", subject.key(), if on { "on" } else { "off" }),
    });
    base
}

#[cfg(feature = "graphics")]
fn state(subject: Subject) -> bool {
    match subject {
        #[cfg(feature = "translation")]
        Subject::Translation => gw2translated::runtime::get(gw2translated::runtime::Flag::Translate),
        #[cfg(feature = "textures")]
        Subject::Textures => gw2remastered::overlay::enabled(),
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

#[cfg(feature = "graphics")]
fn set_state(subject: Subject, on: bool) {
    match subject {
        #[cfg(feature = "translation")]
        Subject::Translation => {
            gw2translated::runtime::set(gw2translated::runtime::Flag::Translate, on);
            gw2translated::refresh::request_now();
        }
        #[cfg(feature = "textures")]
        Subject::Textures => gw2remastered::overlay::set_enabled(on),
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(9);

    #[test]
    fn the_steps_follow_the_shots_and_the_settle_time() {
        assert_eq!(advance(Step::First, Duration::ZERO, S, false), Act::Wait);
        assert_eq!(advance(Step::First, Duration::from_secs(1), S, true), Act::Switch);
        assert_eq!(advance(Step::Settle, Duration::from_secs(8), S, false), Act::Wait);
        assert_eq!(advance(Step::Settle, S, S, false), Act::Shoot);
        // The first shot's counter move must not count for the settle step.
        assert_eq!(advance(Step::Settle, Duration::from_secs(1), S, true), Act::Wait);
        assert_eq!(advance(Step::Second, Duration::from_secs(1), S, true), Act::Finish);
    }

    #[test]
    fn a_shot_that_never_ends_stops_the_comparison() {
        assert_eq!(advance(Step::First, SHOT_TIMEOUT, S, false), Act::Abort);
        assert_eq!(advance(Step::Second, SHOT_TIMEOUT, S, false), Act::Abort);
        assert_eq!(advance(Step::Second, SHOT_TIMEOUT, S, true), Act::Finish);
    }

    #[test]
    fn the_translation_keeps_the_ui_and_the_textures_follow_the_switch() {
        assert!(!Subject::Translation.hide_ui(true));
        assert!(Subject::Textures.hide_ui(true));
        assert!(!Subject::Textures.hide_ui(false));
        assert_eq!((suffix(true), suffix(false)), ("-on", "-off"));
    }
}

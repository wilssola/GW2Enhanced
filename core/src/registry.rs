//! What happened to each feature and hook, for the overlay's Health section and the log.
//!
//! Features report here; nothing here changes game behaviour.

use std::sync::Mutex;

use crate::resolve::{Located, ResolveError};
use crate::site::Site;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Working as designed.
    Ok,
    /// Working, but found by a weaker strategy (or running with reduced function); the text says what.
    Degraded(String),
    /// Not running; the text says why (a site was not found, a check failed, turned off by the player or by safe mode).
    Off(String),
}

impl State {
    pub fn is_ok(&self) -> bool {
        matches!(self, State::Ok)
    }

    pub fn is_off(&self) -> bool {
        matches!(self, State::Off(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub feature: &'static str,
    /// A hook or part of the feature ("grtex2d_header"), or "" for the feature as a whole.
    pub item: String,
    pub state: State,
}

static REGISTRY: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

fn lock() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records (or updates) the state of `item` of `feature`.
pub fn report(feature: &'static str, item: &str, state: State) {
    let mut r = lock();
    match r.iter_mut().find(|e| e.feature == feature && e.item == item) {
        Some(e) => e.state = state,
        None => r.push(Entry {
            feature,
            item: item.to_string(),
            state,
        }),
    }
}

/// Records the outcome of locating `site` and returns the address when there is one.
pub fn report_resolved(feature: &'static str, site: &Site, result: &Result<Located, ResolveError>) -> Option<u32> {
    match result {
        Ok(l) if !l.degraded() => {
            report(feature, site.name, State::Ok);
            Some(l.rva)
        }
        Ok(l) => {
            report(
                feature,
                site.name,
                State::Degraded(format!("found by {} (strategy {})", l.method, l.strategy + 1)),
            );
            Some(l.rva)
        }
        Err(e) => {
            report(feature, site.name, State::Off(format!("not found: {e}")));
            None
        }
    }
}

pub fn snapshot() -> Vec<Entry> {
    lock().clone()
}

/// Worst state of a feature: off if any item is off, degraded if any is, else ok. `None` if nothing reported.
pub fn feature_state(feature: &str) -> Option<State> {
    let r = lock();
    let items: Vec<&Entry> = r.iter().filter(|e| e.feature == feature).collect();
    if items.is_empty() {
        return None;
    }
    if let Some(e) = items.iter().find(|e| e.state.is_off()) {
        return Some(State::Off(format!(
            "{}: {}",
            if e.item.is_empty() { "feature" } else { &e.item },
            off_text(&e.state)
        )));
    }
    if let Some(e) = items.iter().find(|e| matches!(e.state, State::Degraded(_)))
        && let State::Degraded(t) = &e.state
    {
        return Some(State::Degraded(format!("{}: {t}", e.item)));
    }
    Some(State::Ok)
}

fn off_text(s: &State) -> &str {
    match s {
        State::Off(t) => t,
        _ => "",
    }
}

#[cfg(test)]
pub fn clear_for_tests() {
    lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::Attempt;
    use crate::site::{Entry as SiteEntry, Risk, Strategy};
    use std::sync::Mutex;

    // the registry is global: tests that use it run one at a time
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    const SITE: Site = Site::new("hook_a", &[Strategy::Sig("48 8B")], SiteEntry::Anywhere, Risk::Low);

    #[test]
    fn reports_and_summarizes() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        clear_for_tests();
        assert_eq!(feature_state("textures"), None);
        let ok = Ok(Located {
            rva: 0x1000,
            strategy: 0,
            method: "signature",
            looks_like_start: true,
        });
        assert_eq!(report_resolved("textures", &SITE, &ok), Some(0x1000));
        assert_eq!(feature_state("textures"), Some(State::Ok));

        let weak = Ok(Located {
            rva: 0x2000,
            strategy: 1,
            method: "string reference",
            looks_like_start: true,
        });
        report_resolved("textures", &Site { name: "hook_b", ..SITE }, &weak);
        assert!(matches!(feature_state("textures"), Some(State::Degraded(t)) if t.contains("hook_b")));

        let bad = Err(ResolveError::Failed(vec![Attempt {
            method: "signature",
            reason: "not found".into(),
        }]));
        assert_eq!(
            report_resolved("textures", &Site { name: "hook_c", ..SITE }, &bad),
            None
        );
        let s = feature_state("textures").unwrap();
        assert!(s.is_off(), "{s:?}");
        assert_eq!(snapshot().len(), 3);
    }

    #[test]
    fn updating_replaces_the_entry() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        clear_for_tests();
        report("graphics", "ao", State::Off("x".into()));
        report("graphics", "ao", State::Ok);
        assert_eq!(snapshot().len(), 1);
        assert_eq!(feature_state("graphics"), Some(State::Ok));
    }
}

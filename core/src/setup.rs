//! The first-time setup, shared by every feature. Each feature gives its own pages (translation: languages, what to
//! translate, the local translator...; HD textures: the pack folder; graphics: HDR output and brightness) and knows whether
//! it still needs them; the host overlay draws one setup window with the pages of the features built in.
//!
//! A build with one feature shows that feature's setup, and its tab has its own "run the setup again" button. A build with
//! several features shows one setup for all of them, and one button (in the Version tab) instead of one per tab
//! ([`set_hosted`]).

use std::sync::atomic::{AtomicBool, Ordering};

static HOSTED: AtomicBool = AtomicBool::new(false);
static AGAIN: AtomicBool = AtomicBool::new(false);

/// The host draws one shared setup for several features: the features then leave out their own "run again" button.
pub fn set_hosted(on: bool) {
    HOSTED.store(on, Ordering::Relaxed);
}

pub fn hosted() -> bool {
    HOSTED.load(Ordering::Relaxed)
}

/// Asks the host to show the setup again from its first page (a "run the setup again" button).
pub fn request_again() {
    AGAIN.store(true, Ordering::Relaxed);
}

/// The host takes the request (true once per click).
pub fn take_again() -> bool {
    AGAIN.swap(false, Ordering::Relaxed)
}

/// Which page of `counts` (pages per feature, in order) the global page `i` is: (feature index, its own page).
/// None past the last page.
pub fn locate(counts: &[usize], i: usize) -> Option<(usize, usize)> {
    let mut left = i;
    for (f, n) in counts.iter().enumerate() {
        if left < *n {
            return Some((f, left));
        }
        left -= n;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_numbered_across_the_features() {
        let c = [1, 5, 0, 2];
        assert_eq!(locate(&c, 0), Some((0, 0)));
        assert_eq!(locate(&c, 1), Some((1, 0)));
        assert_eq!(locate(&c, 5), Some((1, 4)));
        // A feature without pages is skipped.
        assert_eq!(locate(&c, 6), Some((3, 0)));
        assert_eq!(locate(&c, 7), Some((3, 1)));
        assert_eq!(locate(&c, 8), None);
        assert_eq!(locate(&[], 0), None);
    }

    #[test]
    fn a_request_is_taken_once() {
        request_again();
        assert!(take_again());
        assert!(!take_again());
    }
}

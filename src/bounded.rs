//! "Wait at most a frame, then carry on": the hand-off every feature uses when an answer from a
//! worker would be nicest in the same frame as the key that asked for it, but must never hold
//! the window up. Showing a tab, typed paths, go-to completion, Get Info's details, Open With's
//! list and Batch Rename's reads and plans all go through [`read_waiting`]. The first window
//! (`app::Booting`) and Jump's history load do the same over their own channels.
//!
//! The answer comes back exactly once: from the call if it was in time, otherwise through the
//! `Latest` in `poll` like any other background answer. A newer request on the same `Latest`
//! makes an older one's answer drop, as usual.

use rune::Latest;

/// Runs `work` on `loader`'s worker and waits here up to `wait` for it. If it's done in time,
/// the answer comes back from this call and never lands through the loader; otherwise `None`,
/// and it lands through `loader.take()` in `poll` like any request. The hand-off is decided
/// under one lock, so an answer is never lost or delivered twice. Work that stops early
/// (`None`) hands nothing back.
pub(crate) fn read_waiting<T: Send + 'static>(
    loader: &Latest<T>,
    wait: std::time::Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    use std::sync::{Arc, Condvar, Mutex};
    struct Hand<T> {
        waiting: bool,
        got: Option<T>,
    }
    let hand = Arc::new((Mutex::new(Hand { waiting: !wait.is_zero(), got: None }), Condvar::new()));
    let worker = hand.clone();
    loader.request_opt((), move |()| {
        let listing = work()?;
        let (lock, cv) = &*worker;
        let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
        if h.waiting {
            h.got = Some(listing);
            cv.notify_one();
            None
        } else {
            Some(listing)
        }
    });
    if wait.is_zero() {
        return None;
    }
    let deadline = std::time::Instant::now() + wait;
    let (lock, cv) = &*hand;
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    while h.got.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        h = cv.wait_timeout(h, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    h.waiting = false;
    h.got.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Showing a tab hands its listing over within the wait, or later through `poll`; never both,
    /// never neither.
    #[test]
    fn read_waiting_hands_over_exactly_once() {
        for wait in [Duration::ZERO, Duration::from_millis(1), Duration::from_secs(5)] {
            let loader: Latest<u32> = Latest::default();
            let now = crate::bounded::read_waiting(&loader, wait, || {
                std::thread::sleep(Duration::from_millis(3));
                Some(7)
            });
            let until = Instant::now() + Duration::from_secs(5);
            let mut later = None;
            while now.is_none() && later.is_none() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
                later = loader.take();
            }
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(now.or(later), Some(7), "{wait:?}");
            assert_eq!(loader.take(), None, "{wait:?}: delivered once");
        }
    }
}

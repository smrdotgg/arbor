//! The preview's one thread, and how a request reaches it and its answer gets back.

use super::*;

/// The preview's one worker thread: started by the first preview, kept for the session (it
/// sleeps on a condition variable while there's nothing to do), and ended when the state is
/// dropped. One request runs at a time; a newer one replaces the one waiting.
pub struct Worker {
    pub(super) shared: Arc<Shared>,
}

pub(super) struct Shared {
    pub(super) slots: Mutex<Slots>,
    pub(super) wake: Condvar,
    /// Rung with every stop (a new request, a cancel, the state going), so a read waiting for
    /// the OS's thumbnailer stops waiting at once.
    pub(super) bell: Arc<Bell>,
}

#[derive(Default)]
pub(super) struct Slots {
    /// The next request: replaced by a newer one before it starts, so it never runs.
    pub(super) next: Option<(u64, Job, Arc<AtomicBool>)>,
    /// The last answer, until the UI takes it.
    pub(super) done: Option<Answer>,
    /// The last request number given out.
    pub(super) req: u64,
    pub(super) waker: Option<rune::Waker>,
    /// The state is gone: the thread ends after what it's doing.
    pub(super) closed: bool,
}

impl Worker {
    pub(super) fn spawn(waker: Option<rune::Waker>) -> Worker {
        let shared =
            Arc::new(Shared { slots: Mutex::new(Slots { waker, ..Slots::default() }), wake: Condvar::new(), bell: Arc::default() });
        let s = shared.clone();
        std::thread::Builder::new().name("satchel-preview".into()).spawn(move || s.run()).expect("can't start the preview thread");
        Worker { shared }
    }

    pub(super) fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.shared.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `job` as the next request, replacing one that hasn't started. Its number.
    pub(super) fn request(&self, job: Job, stop: Arc<AtomicBool>) -> u64 {
        let mut s = self.slots();
        s.req += 1;
        let req = s.req;
        s.next = Some((req, job, stop));
        s.done = None;
        self.shared.wake.notify_one();
        drop(s);
        // The caller set the old read's stop flag first.
        self.shared.bell.ring();
        req
    }

    /// Drops the waiting request and any answer not taken yet (the one running stops by its
    /// flag).
    pub(super) fn cancel(&self) {
        let mut s = self.slots();
        s.next = None;
        s.done = None;
        drop(s);
        self.shared.bell.ring();
    }

    pub(super) fn take(&self) -> Option<Answer> {
        self.slots().done.take()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut s = self.slots();
        s.closed = true;
        s.next = None;
        self.shared.wake.notify_one();
        drop(s);
        self.shared.bell.ring();
    }
}

impl Shared {
    pub(super) fn run(&self) {
        loop {
            let (req, job, stop) = {
                let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if s.closed {
                        return;
                    }
                    if let Some(next) = s.next.take() {
                        break next;
                    }
                    s = self.wake.wait(s).unwrap_or_else(|e| e.into_inner());
                }
            };
            // A decoder that panics on a corrupt file still ends in an answer, never in
            // "Loading…" for good, and the thread lives on for the next preview.
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_with(&job, &stop, &self.bell)))
                .unwrap_or_else(|_| Some(Loaded::body(Body::Error("Couldn't preview it: reading it failed unexpectedly".into()))));
            let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            // Only the newest request's answer is kept (a stopped read gives none).
            if let Some(loaded) = loaded
                && req == s.req
            {
                s.done = Some(Answer { req, loaded });
                if let Some(w) = s.waker.clone() {
                    drop(s);
                    w();
                }
            }
        }
    }
}

//! What every window of one Arbor shares. All windows run on the one UI thread, each with its
//! own `State` (panes, tabs, selection, history of folders, overlays); these are the things
//! there is only one of:
//!
//! - **the file-operation slot** ([`Job`]): one operation at a time in the whole app, held
//!   here rather than by a window, and shown (progress, Esc to cancel) by its *owner*, the
//!   window that started it; the others say "Another file operation is running in another
//!   window". A closing owner makes another window the owner on the spot, so the operation
//!   can't be lost between windows;
//! - **the undo and redo stacks**: one for the app, as in Finder, so ⌘Z in any window undoes
//!   the last operation, wherever it was done;
//! - **what ⌘C or ⌘X put aside** (the system clipboard is shared anyway);
//! - **the Jump history**.
//!
//! Settings, key bindings, the sidebar's favorites and the volumes are each window's copy,
//! kept the same by messages: a window that changes one tells the others.
//!
//! A window reaches another through its **inbox** here: a message (a closure over the other
//! `State`) and a wake, so it runs in that window's next `poll`. Nothing is polled; a window
//! nobody wrote to does nothing.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use rune::Tx;

use super::{Clip, State};
use crate::jump::History;
use crate::ops;

/// A change for another window, run in its `poll`.
pub(crate) type Msg = Box<dyn FnOnce(&mut State, &mut Tx)>;

/// One open window.
struct Member {
    id: u64,
    waker: Option<rune::Waker>,
    inbox: Vec<Msg>,
}

/// The running file operation, the window that shows it, and the progress it last showed.
pub(crate) struct Job {
    pub run: ops::Running,
    pub owner: u64,
    pub seen: (u64, usize),
    /// An undo or redo: the outcome whose record it is (`Shared::undo`'s numbers).
    pub undoing: Option<u64>,
    /// The window that started it, which alone renames what New File or New Folder made.
    pub asked: u64,
}

pub struct Shared {
    /// The file operation running, if one is.
    pub(crate) job: RefCell<Option<Job>>,
    /// Wakes the operation's owner: the worker holds this, so a window that takes over the
    /// operation (its owner closed) gets the worker's wakes from then on.
    pub(crate) op_waker: Arc<Mutex<Option<rune::Waker>>>,
    /// What can be undone, newest last, each with the number of the outcome it undoes: when it
    /// is undone, every window still saying that outcome's note stops (`State::outcome_undone`).
    pub(crate) undo: RefCell<Vec<(u64, ops::Undo)>>,
    /// Undone operations that can be redone, newest last, numbered the same way. A new
    /// operation clears it.
    pub(crate) redo: RefCell<Vec<(u64, ops::Undo)>>,
    /// The last outcome's number.
    pub(crate) outcomes: std::cell::Cell<u64>,
    pub(crate) clip: RefCell<Option<Clip>>,
    /// The folders visited, for Jump: nothing is read until it first opens.
    pub history: RefCell<History>,
    /// The one thread writing `sidebar.toml` for every window, from the first edit on, and the
    /// favorites' revision: each edit takes the next number, and an answer or a message about
    /// an older list than the newest is dropped.
    pub(crate) side_writer: RefCell<Option<crate::sidebar::Writer>>,
    pub(crate) side_rev: std::cell::Cell<u64>,
    /// The list's columns, for every window (`listcols.rs`), whether the user changed them
    /// since Arbor started (columns read late at startup then don't replace them), where
    /// they're kept, and the one thread writing them, from the first change on.
    pub(crate) cols: std::cell::Cell<crate::listcols::ListCols>,
    pub(crate) cols_set: std::cell::Cell<bool>,
    pub(crate) cols_file: RefCell<Option<std::path::PathBuf>>,
    pub(crate) cols_writer: RefCell<Option<crate::listcols::Writer>>,
    members: RefCell<Vec<Member>>,
    /// The open windows' wakers, the one focused last at the end: what a shared writer wakes
    /// (`live_waker`), so it never holds a window that has closed.
    live: Arc<Mutex<Vec<(u64, rune::Waker)>>>,
}

impl Shared {
    pub(crate) fn new(history: History) -> Shared {
        Shared {
            job: RefCell::new(None),
            op_waker: Arc::default(),
            undo: RefCell::default(),
            redo: RefCell::default(),
            outcomes: std::cell::Cell::new(0),
            clip: RefCell::default(),
            history: RefCell::new(history),
            side_writer: RefCell::default(),
            side_rev: std::cell::Cell::new(0),
            cols: std::cell::Cell::default(),
            cols_set: std::cell::Cell::new(false),
            cols_file: RefCell::default(),
            cols_writer: RefCell::default(),
            members: RefCell::default(),
            live: Arc::default(),
        }
    }

    /// Window `id` opened.
    pub(crate) fn join(&self, id: u64) {
        self.members.borrow_mut().push(Member { id, waker: None, inbox: Vec::new() });
    }

    /// Window `id`'s waker, from its `setup`. Wakes it at once when messages came before it.
    pub(crate) fn set_waker(&self, id: u64, waker: rune::Waker) {
        let mut members = self.members.borrow_mut();
        let Some(m) = members.iter_mut().find(|m| m.id == id) else { return };
        if !m.inbox.is_empty() {
            waker();
        }
        m.waker = Some(waker.clone());
        if let Ok(mut live) = self.live.lock() {
            live.retain(|(w, _)| *w != id);
            live.push((id, waker));
        }
    }

    /// Window `id` took focus: shared writers wake it first from now on.
    pub(crate) fn focus(&self, id: u64) {
        if let Ok(mut live) = self.live.lock()
            && let Some(i) = live.iter().position(|(w, _)| *w == id)
        {
            let w = live.remove(i);
            live.push(w);
        }
    }

    /// A waker for threads every window shares (the Jump history's and the sidebar's writers):
    /// it wakes the open window focused last (else the one set up last), whichever that is
    /// when it fires; with no window open, nothing.
    pub(crate) fn live_waker(&self) -> rune::Waker {
        let live = self.live.clone();
        Arc::new(move || {
            let w = live.lock().ok().and_then(|l| l.last().map(|(_, w)| w.clone()));
            if let Some(w) = w {
                w();
            }
        })
    }

    /// Window `id` closed: what was waiting for it is dropped, and nothing shared wakes it.
    pub(crate) fn leave(&self, id: u64) {
        self.members.borrow_mut().retain(|m| m.id != id);
        if let Ok(mut live) = self.live.lock() {
            live.retain(|(w, _)| *w != id);
        }
    }

    /// How many windows are open.
    pub(crate) fn windows(&self) -> usize {
        self.members.borrow().len()
    }

    /// Every open window but `me`, oldest first.
    pub(crate) fn others(&self, me: u64) -> Vec<u64> {
        self.members.borrow().iter().map(|m| m.id).filter(|&id| id != me).collect()
    }

    /// Puts `msg` in window `to`'s inbox and wakes it. Nothing happens for a window that has
    /// closed.
    pub(crate) fn send(&self, to: u64, msg: Msg) {
        let waker = {
            let mut members = self.members.borrow_mut();
            let Some(m) = members.iter_mut().find(|m| m.id == to) else { return };
            m.inbox.push(msg);
            m.waker.clone()
        };
        if let Some(w) = waker {
            w();
        }
    }

    /// Sends a message made by `make` to every window but `from`.
    pub(crate) fn tell_others(&self, from: u64, make: impl Fn() -> Msg) {
        for id in self.others(from) {
            self.send(id, make());
        }
    }

    /// What's waiting for window `id`, oldest first.
    pub(crate) fn take_inbox(&self, id: u64) -> Vec<Msg> {
        let mut members = self.members.borrow_mut();
        members.iter_mut().find(|m| m.id == id).map(|m| std::mem::take(&mut m.inbox)).unwrap_or_default()
    }

    /// The waker a file operation's worker gets: whichever window owns it when it wakes.
    pub(crate) fn op_wake(&self) -> rune::Waker {
        let cell = self.op_waker.clone();
        Arc::new(move || {
            let w = cell.lock().ok().and_then(|w| w.clone());
            if let Some(w) = w {
                w();
            }
        })
    }

    /// Whether a file operation is running in some window.
    pub(crate) fn busy(&self) -> bool {
        self.job.borrow().is_some()
    }

    /// Whether window `id` is open.
    pub(crate) fn has(&self, id: u64) -> bool {
        self.members.borrow().iter().any(|m| m.id == id)
    }

    /// Makes window `id` the operation's owner: its worker wakes that window from now on, and
    /// is woken once now (the operation may have finished while nobody owned it).
    pub(crate) fn own_job(&self, id: u64) {
        let waker = self.members.borrow().iter().find(|m| m.id == id).and_then(|m| m.waker.clone());
        if let Some(j) = self.job.borrow_mut().as_mut() {
            j.owner = id;
        }
        if let Ok(mut w) = self.op_waker.lock() {
            w.clone_from(&waker);
        }
        if let Some(w) = waker {
            w();
        }
    }
}

impl State {
    /// Runs what other windows sent this one. True when anything ran.
    pub(crate) fn poll_inbox(&mut self, tx: &mut Tx) -> bool {
        let msgs = self.shared.take_inbox(self.win);
        let ran = !msgs.is_empty();
        for m in msgs {
            m(self, tx);
        }
        ran
    }

    /// Whether a file operation is running, here or in another window.
    pub fn busy(&self) -> bool {
        self.shared.busy()
    }

    /// Whether the running file operation is this window's to show and cancel.
    pub fn owns_job(&self) -> bool {
        self.shared.job.borrow().as_ref().is_some_and(|j| j.owner == self.win)
    }

    /// How many windows are open (this one included).
    pub fn windows(&self) -> usize {
        self.shared.windows()
    }

    /// This window is closing: it leaves the others, making one of them (the one focused last)
    /// the owner of its running file operation, if any, which shows and lands it from then on.
    /// True when it was the last.
    pub fn leave(&mut self) -> bool {
        let others = self.shared.others(self.win);
        if self.owns_job() {
            match crate::window::most_recent(&others).or_else(|| others.last().copied()) {
                Some(to) => self.shared.own_job(to),
                // The last window: the app is quitting. The operation stops; its cleanup (a
                // cancelled copy removing what it made) goes on off this thread, and what it
                // can't finish is recorded for the next launch (`leftovers.rs`). The window
                // closes now.
                None => {
                    if let Some(job) = self.shared.job.borrow_mut().take() {
                        job.run.cancel();
                        match self.quit_record.clone() {
                            Some(file) => crate::leftovers::quitting(job.run, file, self.quit_wait),
                            None => drop(job),
                        }
                    }
                }
            }
        }
        self.shared.leave(self.win);
        crate::window::unregister(self.win);
        others.is_empty()
    }
}

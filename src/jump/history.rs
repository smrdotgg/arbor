//! The history on disk and in the session: the one writer thread that touches `history.log`
//! (appends, the load and its compaction, clear, all under the lock file), and [`History`].

use super::*;

pub(super) enum Msg {
    Append(String),
    /// Forget everything; the answer says whether it's gone from the disk.
    Clear(Sender<Result<(), String>>),
    Load(Sender<Loaded>, Option<rune::Waker>),
    /// Answered once everything sent before it is done (at exit).
    Flush(Sender<()>),
}

/// What a load sends back.
pub struct Loaded {
    pub agg: Frecency,
    pub error: Option<String>,
}

/// The one thread that touches `history.log`.
pub(super) struct Store {
    pub(super) tx: Sender<Msg>,
    /// What went wrong writing, once per run of failures, for the status bar.
    pub(super) errors: Receiver<String>,
    /// Disk operations it has done (appends, reads, rewrites, removals), for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) io: Arc<AtomicU64>,
}

impl Store {
    pub(super) fn spawn(log: Option<PathBuf>, waker: Option<rune::Waker>) -> Store {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (mut failures, errors) = persist::Failures::new(waker.clone());
        let io = Arc::new(AtomicU64::new(0));
        let count = io.clone();
        persist::at_exit(&tx, Msg::Flush);
        std::thread::Builder::new()
            .name("satchel-history".into())
            .spawn(move || {
                // Without a file (tests), the log is kept here.
                let mut memory = String::new();
                for msg in rx {
                    match msg {
                        Msg::Append(line) => match &log {
                            Some(p) => {
                                count.fetch_add(1, Relaxed);
                                failures.note(append(p, &line), |e| format!("Couldn't save the Jump history ({}): {e}", fs::pretty(p)));
                            }
                            None => memory.push_str(&line),
                        },
                        Msg::Clear(reply) => {
                            let done = match &log {
                                Some(p) => {
                                    count.fetch_add(1, Relaxed);
                                    clear(p).map_err(|e| e.to_string())
                                }
                                None => {
                                    memory.clear();
                                    Ok(())
                                }
                            };
                            let _ = reply.send(done);
                            if let Some(w) = &waker {
                                w();
                            }
                        }
                        Msg::Load(reply, waker) => {
                            let loaded = match &log {
                                Some(p) => load(p, &count),
                                None => Loaded { agg: Frecency::from_log(&memory).0, error: None },
                            };
                            if reply.send(loaded).is_ok()
                                && let Some(w) = waker
                            {
                                w();
                            }
                        }
                        Msg::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .expect("can't start the history thread");
        Store { tx, errors, io }
    }
}

/// Runs `f` holding the history's lock file (`history.lock` beside it), so two Satchel processes
/// never interleave a compaction (read, then rewrite) with an append or a clear.
pub(super) fn locked<T>(p: &Path, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    persist::locked(p, f)
}

/// Appends `line`. A last line cut short (a crash mid-write) is first ended with a NUL, so it
/// stays invalid: no path holds a NUL, and a line with one never parses.
pub(super) fn append(p: &Path, line: &str) -> std::io::Result<()> {
    locked(p, || {
        let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(p)?;
        let mut out = String::with_capacity(line.len() + 2);
        if f.metadata()?.len() > 0 {
            let mut last = [0u8];
            f.seek(SeekFrom::End(-1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                out.push_str("\0\n");
            }
        }
        out.push_str(line);
        f.write_all(out.as_bytes())
    })
}

/// Removes the log and any compaction's temporary files beside it (they hold the same paths).
pub(super) fn clear(p: &Path) -> std::io::Result<()> {
    locked(p, || {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let (Some(dir), Some(name)) = (p.parent(), p.file_name().map(|n| n.to_string_lossy().into_owned())) else { return Ok(()) };
        for de in std::fs::read_dir(dir)?.flatten() {
            let n = de.file_name().to_string_lossy().into_owned();
            if n.starts_with(&format!("{name}.")) && n.ends_with(".tmp") {
                std::fs::remove_file(de.path())?;
            }
        }
        Ok(())
    })
}

/// Reads the whole log, a line at a time (however big it is: nothing is cut off), and compacts
/// it when it's long, all under the lock so no other process's append falls between the read
/// and the rewrite.
pub(super) fn load(p: &Path, io: &AtomicU64) -> Loaded {
    io.fetch_add(1, Relaxed);
    let res = locked(p, || {
        let f = match std::fs::File::open(p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Frecency::default(), None)),
            Err(e) => return Err(e),
        };
        let (agg, lines) = Frecency::from_reader(std::io::BufReader::new(f))?;
        let mut error = None;
        if lines > COMPACT_AT {
            io.fetch_add(1, Relaxed);
            if let Err(e) = rewrite(p, &agg.compacted(now())) {
                error = Some(format!("Couldn't compact the Jump history: {e}"));
            }
        }
        Ok((agg, error))
    });
    match res {
        Ok((agg, error)) => Loaded { agg, error },
        Err(e) => Loaded { agg: Frecency::default(), error: Some(format!("Couldn't read the Jump history: {e}")) },
    }
}

/// Replaces the log with `text`, through a temporary file of its own (unique to this process
/// and call, `history.log.<pid>-<n>.tmp`, which [`clear`] knows) renamed over it.
pub(super) fn rewrite(p: &Path, text: &str) -> std::io::Result<()> {
    persist::replace(p, text.as_bytes(), persist::Durability::Plain)
}

/// Where the history lives: beside the settings file.
pub fn log_path() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name("history.log"))
}

// ---- the UI side ---------------------------------------------------------------------------

/// The history as the UI holds it.
pub struct History {
    /// `jump.enabled`: off, nothing is recorded or read.
    pub enabled: bool,
    /// `jump.exclude`: folders that aren't recorded.
    pub exclude: Vec<Glob>,
    /// The log's path (`None`: kept in memory only, as in tests).
    pub(crate) log: Option<PathBuf>,
    pub(super) store: Option<Store>,
    /// The aggregate, from the first time Jump opened.
    pub agg: Option<Frecency>,
    /// A load in flight.
    pub(super) loading: Option<Receiver<Loaded>>,
    /// Events sent after the load in flight was asked for (it won't have them).
    pub(super) since: Vec<Event>,
    /// Aggregates landed, for tests and timing.
    pub loads: u64,
    /// What went wrong with the last load (shown once in the status bar).
    pub error: Option<String>,
    /// A clear the writer hasn't answered yet.
    pub(super) clearing: Option<Receiver<Result<(), String>>>,
    /// Wakes the window when the writer has something to say (a failed write, a clear done).
    pub(crate) waker: Option<rune::Waker>,
}

impl History {
    pub fn new(log: Option<PathBuf>) -> History {
        History {
            enabled: true,
            exclude: Vec::new(),
            log,
            store: None,
            agg: None,
            loading: None,
            since: Vec::new(),
            loads: 0,
            error: None,
            clearing: None,
            waker: None,
        }
    }

    /// Disk operations so far (none until the writer starts).
    #[cfg(test)]
    pub fn io(&self) -> u64 {
        self.store.as_ref().map_or(0, |s| s.io.load(Relaxed))
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub(super) fn store(&mut self) -> &Store {
        let log = self.log.clone();
        let waker = self.waker.clone();
        self.store.get_or_insert_with(|| Store::spawn(log, waker))
    }

    /// Sends `e` to the log, and applies it to the aggregate if one is loaded.
    pub(super) fn send(&mut self, e: Event) {
        if let Some(line) = e.line(now()) {
            let _ = self.store().tx.send(Msg::Append(line));
        }
        if let Some(a) = &mut self.agg {
            a.apply(&e);
        } else if self.loading.is_some() {
            self.since.push(e);
        }
    }

    /// A navigation landed in `path`.
    pub fn record(&mut self, path: &Path) {
        if !self.enabled || self.excluded(path) {
            return;
        }
        self.send(Event::Visit { path: path.to_path_buf(), at: now(), rank: 1.0 });
    }

    pub fn excluded(&self, path: &Path) -> bool {
        if self.exclude.is_empty() {
            return false;
        }
        let s = path.to_string_lossy();
        self.exclude.iter().any(|g| g.matches(&s))
    }

    /// `path` is gone: out of the aggregate and the log.
    pub fn forget(&mut self, path: &Path) {
        self.send(Event::Drop { path: path.to_path_buf() });
    }

    /// Forgets everything: at once in this session, and on the disk when the writer gets to it
    /// ([`History::cleared`] says how that went).
    pub fn clear(&mut self) {
        let (reply, rx) = std::sync::mpsc::channel();
        let _ = self.store().tx.send(Msg::Clear(reply));
        self.clearing = Some(rx);
        self.agg = Some(Frecency::default());
        self.loading = None;
        self.since.clear();
    }

    /// The writer's answer to a clear, once it has one: `Err` when the log is still on the
    /// disk (then the session reads it again next time, rather than pretend it's gone).
    pub fn cleared(&mut self) -> Option<Result<(), String>> {
        let rx = self.clearing.as_ref()?;
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the history's writer stopped".into()),
        };
        self.clearing = None;
        if r.is_err() {
            self.agg = None;
        }
        Some(r)
    }

    /// A write that failed since the last look (said once per run of failures).
    pub fn write_error(&mut self) -> Option<String> {
        self.store.as_ref()?.errors.try_recv().ok()
    }

    /// Waits up to `wait` for the writer to finish what's queued. True when it did.
    #[cfg(test)]
    pub fn drain(&self, wait: Duration) -> bool {
        self.store.as_ref().is_none_or(|s| persist::flush(&s.tx, wait, Msg::Flush))
    }

    /// Asks for the aggregate if it isn't here, waiting up to `wait` for it. True once it's in.
    pub fn load(&mut self, wait: Duration, waker: Option<rune::Waker>) -> bool {
        if self.agg.is_some() {
            return true;
        }
        if self.loading.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = self.store().tx.send(Msg::Load(tx, waker));
            self.loading = Some(rx);
        }
        if let Some(rx) = &self.loading
            && let Ok(l) = rx.recv_timeout(wait)
        {
            self.landed(l);
        }
        self.agg.is_some()
    }

    /// Lands a load that finished since. True when one did.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.loading else { return false };
        match rx.try_recv() {
            Ok(l) => {
                self.landed(l);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.loading = None;
                false
            }
        }
    }

    pub(super) fn landed(&mut self, l: Loaded) {
        let mut agg = l.agg;
        for e in self.since.drain(..) {
            agg.apply(&e);
        }
        self.agg = Some(agg);
        self.loading = None;
        self.loads += 1;
        self.error = l.error;
    }
}

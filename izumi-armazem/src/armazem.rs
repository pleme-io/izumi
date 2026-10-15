use std::collections::BTreeMap;
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{ArmazemConfig, ConfigError, Mode};
use crate::shelf::{Change, Phase, Record, Seq, Shelf, Since, Snapshot};
use crate::zelador::{policy_for, Action, DemandRow, Policy};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub trait Source: Send + Sync + 'static {
    type Key: Clone + Ord + Display + Send + Sync + 'static;
    type Record: Record;

    fn open(&self, key: Self::Key, resume: Option<String>, feed: Feed<Self::Record>) -> BoxFuture<()>;

    fn fetch(&self, key: Self::Key) -> BoxFuture<Result<Vec<Self::Record>, String>>;
}

struct Stop {
    flag: AtomicBool,
    tx: watch::Sender<bool>,
}

impl Stop {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            tx: watch::Sender::new(false),
        }
    }

    fn stopped(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        self.tx.send_replace(true);
    }
}

pub struct Feed<R: Record> {
    shelf: Arc<Mutex<Shelf<R>>>,
    tx: watch::Sender<Seq>,
    stop: Arc<Stop>,
}

impl<R: Record> Feed<R> {
    fn apply(&self, change: Change<R>) {
        let seq = {
            let mut shelf = self.shelf.lock();
            if self.stop.stopped() || !shelf.push(change) {
                return;
            }
            shelf.seq()
        };
        self.tx.send_replace(seq);
    }

    pub fn reset(&self, records: Vec<R>) {
        self.apply(Change::Reset(records.into()));
    }

    pub fn upsert(&self, record: R) {
        self.apply(Change::Upsert(record));
    }

    pub fn remove(&self, id: impl Into<String>) {
        self.apply(Change::Remove(id.into()));
    }

    pub fn append(&self, record: R) {
        self.apply(Change::Append(record));
    }

    pub fn live(&self) {
        self.apply(Change::Phase(Phase::Live));
    }

    pub fn degraded(&self, cause: impl Into<String>) {
        self.apply(Change::Phase(Phase::Degraded {
            cause: cause.into(),
        }));
    }

    pub fn resume_at(&self, token: impl Into<String>) {
        let mut shelf = self.shelf.lock();
        if !self.stop.stopped() {
            shelf.set_resume(token.into());
        }
    }

    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stop.stopped()
    }

    pub async fn until_stopped(&self) {
        let mut rx = self.stop.tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

struct Run {
    stop: Arc<Stop>,
    handle: JoinHandle<()>,
}

struct Entry<R: Record> {
    shelf: Arc<Mutex<Shelf<R>>>,
    tx: watch::Sender<Seq>,
    run: Option<Run>,
    interest: usize,
    idle_since: Option<Instant>,
    dormant_since: Option<Instant>,
}

impl<R: Record> Entry<R> {
    fn new(cap: usize) -> Self {
        Self {
            shelf: Arc::new(Mutex::new(Shelf::new(cap))),
            tx: watch::Sender::new(Seq::default()),
            run: None,
            interest: 0,
            idle_since: None,
            dormant_since: None,
        }
    }

    fn mark(&self, change: Change<R>) {
        let seq = {
            let mut shelf = self.shelf.lock();
            if !shelf.push(change) {
                return;
            }
            shelf.seq()
        };
        self.tx.send_replace(seq);
    }
}

struct Inner<S: Source> {
    source: Arc<S>,
    cfg: ArmazemConfig,
    policy: Box<dyn Policy>,
    entries: Mutex<BTreeMap<S::Key, Entry<S::Record>>>,
    zelador_error: Mutex<Option<String>>,
}

pub struct Armazem<S: Source> {
    inner: Arc<Inner<S>>,
}

impl<S: Source> Clone for Armazem<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct KeyState {
    pub key: String,
    pub phase: Phase,
    pub seq: Seq,
    pub records: usize,
    pub interest: usize,
    pub live: bool,
    pub idle_ms: Option<u64>,
    pub dormant_ms: Option<u64>,
}

fn elapsed_ms(since: Option<Instant>, now: Instant) -> Option<u64> {
    since.map(|t| u64::try_from(now.saturating_duration_since(t).as_millis()).unwrap_or(u64::MAX))
}

impl<S: Source> Armazem<S> {
    pub fn new(source: S, cfg: ArmazemConfig) -> Result<Self, ConfigError> {
        let policy = policy_for(&cfg)?;
        Ok(Self::with_policy(source, cfg, policy))
    }

    #[must_use]
    pub fn with_policy(source: S, cfg: ArmazemConfig, policy: Box<dyn Policy>) -> Self {
        Self {
            inner: Arc::new(Inner {
                source: Arc::new(source),
                cfg,
                policy,
                entries: Mutex::new(BTreeMap::new()),
                zelador_error: Mutex::new(None),
            }),
        }
    }

    #[must_use]
    pub fn config(&self) -> &ArmazemConfig {
        &self.inner.cfg
    }

    #[must_use]
    pub fn interest(&self, key: S::Key) -> Interesse<S> {
        let mut entries = self.inner.entries.lock();
        let entry = entries
            .entry(key.clone())
            .or_insert_with(|| Entry::new(self.inner.cfg.journal_cap));
        entry.interest += 1;
        entry.idle_since = None;
        if self.inner.cfg.mode != Mode::Direct && entry.run.is_none() {
            self.start(&key, entry);
        }
        Interesse {
            armazem: self.clone(),
            key,
            shelf: Arc::clone(&entry.shelf),
            rx: entry.tx.subscribe(),
        }
    }

    fn start(&self, key: &S::Key, entry: &mut Entry<S::Record>) {
        let stop = Arc::new(Stop::new());
        let resume = entry.shelf.lock().resume().map(str::to_owned);
        entry.mark(Change::Phase(Phase::Absorbing));
        entry.dormant_since = None;
        let feed = Feed {
            shelf: Arc::clone(&entry.shelf),
            tx: entry.tx.clone(),
            stop: Arc::clone(&stop),
        };
        let fut = self.inner.source.open(key.clone(), resume, feed);
        let weak: Weak<Inner<S>> = Arc::downgrade(&self.inner);
        let owned = key.clone();
        let mine = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            fut.await;
            let Some(inner) = weak.upgrade() else { return };
            let mut entries = inner.entries.lock();
            let Some(entry) = entries.get_mut(&owned) else { return };
            let current = entry.run.as_ref().is_some_and(|r| Arc::ptr_eq(&r.stop, &mine));
            if current && !mine.stopped() {
                entry.run = None;
                entry.mark(Change::Phase(Phase::Degraded {
                    cause: "the stream ended".into(),
                }));
            }
        });
        entry.run = Some(Run { stop, handle });
    }

    fn release(&self, key: &S::Key) {
        let mut entries = self.inner.entries.lock();
        if let Some(entry) = entries.get_mut(key) {
            entry.interest = entry.interest.saturating_sub(1);
            if entry.interest == 0 {
                entry.idle_since = Some(Instant::now());
            }
        }
    }

    #[must_use]
    pub fn demand(&self) -> Vec<DemandRow> {
        let now = Instant::now();
        self.inner
            .entries
            .lock()
            .iter()
            .map(|(k, e)| DemandRow {
                key: k.to_string(),
                interest: e.interest,
                live: e.run.is_some(),
                idle_ms: if e.run.is_some() && e.interest == 0 {
                    elapsed_ms(e.idle_since, now)
                } else {
                    None
                },
                dormant_ms: if e.run.is_none() {
                    elapsed_ms(e.dormant_since, now)
                } else {
                    None
                },
            })
            .collect()
    }

    #[must_use]
    pub fn inventory(&self) -> Vec<KeyState> {
        let now = Instant::now();
        self.inner
            .entries
            .lock()
            .iter()
            .map(|(k, e)| {
                let snap = e.shelf.lock().snapshot();
                KeyState {
                    key: k.to_string(),
                    phase: snap.phase,
                    seq: snap.seq,
                    records: snap.records.len(),
                    interest: e.interest,
                    live: e.run.is_some(),
                    idle_ms: if e.run.is_some() && e.interest == 0 {
                        elapsed_ms(e.idle_since, now)
                    } else {
                        None
                    },
                    dormant_ms: if e.run.is_none() {
                        elapsed_ms(e.dormant_since, now)
                    } else {
                        None
                    },
                }
            })
            .collect()
    }

    pub fn apply(&self, action: &Action) -> bool {
        let mut entries = self.inner.entries.lock();
        let name = match action {
            Action::Start(k) | Action::Park(k) | Action::Evict(k) => k,
        };
        let Some(key) = entries.keys().find(|k| k.to_string() == *name).cloned() else {
            return false;
        };
        match action {
            Action::Start(_) => {
                let Some(entry) = entries.get_mut(&key) else { return false };
                if entry.run.is_some() || self.inner.cfg.mode == Mode::Direct {
                    return false;
                }
                self.start(&key, entry);
                true
            }
            Action::Park(_) => {
                let Some(entry) = entries.get_mut(&key) else { return false };
                if entry.interest > 0 {
                    return false;
                }
                let Some(run) = entry.run.take() else { return false };
                let resume = {
                    let shelf = entry.shelf.lock();
                    run.stop.stop();
                    shelf.resume().map(str::to_owned)
                };
                run.handle.abort();
                entry.mark(Change::Phase(Phase::Dormant { resume }));
                entry.dormant_since = Some(Instant::now());
                entry.idle_since = None;
                true
            }
            Action::Evict(_) => {
                let evictable = entries
                    .get(&key)
                    .is_some_and(|e| e.interest == 0 && e.run.is_none());
                if evictable {
                    entries.remove(&key);
                }
                evictable
            }
        }
    }

    pub fn tick(&self) -> Result<Vec<Action>, String> {
        let rows = self.demand();
        let decided = self.inner.policy.decide(&rows, &self.inner.cfg);
        *self.inner.zelador_error.lock() = decided.as_ref().err().cloned();
        let actions = decided?;
        for a in &actions {
            self.apply(a);
        }
        Ok(actions)
    }

    #[must_use]
    pub fn zelador_error(&self) -> Option<String> {
        self.inner.zelador_error.lock().clone()
    }

    #[must_use]
    pub fn spawn_zelador(&self) -> JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        let period = self.inner.cfg.tick();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                let armazem = Armazem { inner };
                if let Err(e) = armazem.tick() {
                    tracing::warn!(error = %e, "armazem zelador policy failed; nothing was started or parked this tick");
                }
            }
        })
    }
}

impl<S: Source> Drop for Inner<S> {
    fn drop(&mut self) {
        for entry in self.entries.get_mut().values_mut() {
            if let Some(run) = entry.run.take() {
                run.stop.stop();
                run.handle.abort();
            }
        }
    }
}

pub struct Interesse<S: Source> {
    armazem: Armazem<S>,
    key: S::Key,
    shelf: Arc<Mutex<Shelf<S::Record>>>,
    rx: watch::Receiver<Seq>,
}

impl<S: Source> Interesse<S> {
    #[must_use]
    pub fn key(&self) -> &S::Key {
        &self.key
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot<S::Record> {
        self.shelf.lock().snapshot()
    }

    #[must_use]
    pub fn since(&self, after: Seq) -> Since<S::Record> {
        self.shelf.lock().since(after)
    }

    pub async fn changed(&mut self) -> Option<Seq> {
        self.rx.changed().await.ok()?;
        Some(*self.rx.borrow_and_update())
    }

    pub async fn settled(&mut self, within: Duration) -> Snapshot<S::Record> {
        let deadline = Instant::now() + within;
        loop {
            let snap = self.snapshot();
            if snap.phase.settled() {
                return snap;
            }
            match tokio::time::timeout_at(deadline, self.rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return self.snapshot(),
            }
        }
    }

    pub async fn read(&mut self, within: Duration) -> Result<Snapshot<S::Record>, String> {
        if self.armazem.inner.cfg.mode != Mode::Direct {
            return Ok(self.settled(within).await);
        }
        let fetched = tokio::time::timeout(within, self.armazem.inner.source.fetch(self.key.clone()))
            .await
            .map_err(|_| format!("direct read of {} did not answer within {within:?}", self.key))??;
        let mut shelf = self.shelf.lock();
        shelf.push(Change::Reset(fetched.into()));
        shelf.push(Change::Phase(Phase::Direct));
        Ok(shelf.snapshot())
    }
}

impl<S: Source> Drop for Interesse<S> {
    fn drop(&mut self) {
        self.armazem.release(&self.key);
    }
}

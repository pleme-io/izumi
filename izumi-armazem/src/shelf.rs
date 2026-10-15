use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde::Serialize;

pub trait Record: Clone + Send + Sync + 'static {
    fn id(&self) -> Option<&str>;
    fn fingerprint(&self) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
pub struct Seq(pub u64);

impl Seq {
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Phase {
    Absorbing,
    Live,
    Degraded { cause: String },
    Dormant { resume: Option<String> },
    Direct,
}

impl Phase {
    #[must_use]
    pub fn settled(&self) -> bool {
        !matches!(self, Self::Absorbing)
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Absorbing => "absorbing",
            Self::Live => "live",
            Self::Degraded { .. } => "degraded",
            Self::Dormant { .. } => "dormant",
            Self::Direct => "direct",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Change<R> {
    Reset(Arc<[R]>),
    Upsert(R),
    Remove(String),
    Append(R),
    Phase(Phase),
}

#[derive(Debug, Clone)]
pub struct Snapshot<R> {
    pub records: Arc<[R]>,
    pub phase: Phase,
    pub seq: Seq,
    pub resume: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Since<R> {
    Changes { changes: Vec<(Seq, Change<R>)>, head: Seq },
    Gap(Snapshot<R>),
}

#[derive(Debug)]
pub struct Shelf<R: Record> {
    keyed: BTreeMap<String, R>,
    ring: VecDeque<R>,
    journal: VecDeque<(Seq, Change<R>)>,
    seq: Seq,
    phase: Phase,
    resume: Option<String>,
    cap: usize,
    cached: Option<Arc<[R]>>,
}

impl<R: Record> Shelf<R> {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            keyed: BTreeMap::new(),
            ring: VecDeque::new(),
            journal: VecDeque::new(),
            seq: Seq::default(),
            phase: Phase::Absorbing,
            resume: None,
            cap: cap.max(1),
            cached: None,
        }
    }

    #[must_use]
    pub fn seq(&self) -> Seq {
        self.seq
    }

    #[must_use]
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    #[must_use]
    pub fn resume(&self) -> Option<&str> {
        self.resume.as_deref()
    }

    pub fn set_resume(&mut self, token: String) {
        self.resume = Some(token);
    }

    pub fn push(&mut self, change: Change<R>) -> bool {
        let moved = match &change {
            Change::Reset(records) => {
                let same = self.ring.is_empty()
                    && records.len() == self.keyed.len()
                    && records.iter().all(|r| {
                        r.id().is_some_and(|id| {
                            self.keyed
                                .get(id)
                                .is_some_and(|old| old.fingerprint() == r.fingerprint())
                        })
                    });
                if !same {
                    self.keyed.clear();
                    self.ring.clear();
                    for r in records.iter() {
                        self.place(r.clone());
                    }
                }
                !same
            }
            Change::Upsert(r) => match r.id() {
                Some(id) => {
                    let unchanged = self
                        .keyed
                        .get(id)
                        .is_some_and(|old| old.fingerprint() == r.fingerprint());
                    if !unchanged {
                        self.keyed.insert(id.to_owned(), r.clone());
                    }
                    !unchanged
                }
                None => {
                    self.place(r.clone());
                    true
                }
            },
            Change::Remove(id) => self.keyed.remove(id).is_some(),
            Change::Append(r) => {
                self.place(r.clone());
                true
            }
            Change::Phase(p) => {
                let flipped = *p != self.phase;
                if flipped {
                    self.phase = p.clone();
                }
                flipped
            }
        };
        if moved {
            self.seq = self.seq.next();
            if !matches!(change, Change::Phase(_)) {
                self.cached = None;
            }
            self.journal.push_back((self.seq, change));
            while self.journal.len() > self.cap {
                self.journal.pop_front();
            }
        }
        moved
    }

    fn place(&mut self, r: R) {
        match r.id() {
            Some(id) => {
                self.keyed.insert(id.to_owned(), r);
            }
            None => {
                self.ring.push_back(r);
                while self.ring.len() > self.cap {
                    self.ring.pop_front();
                }
            }
        }
    }

    pub fn snapshot(&mut self) -> Snapshot<R> {
        let records = match &self.cached {
            Some(c) => Arc::clone(c),
            None => {
                let built: Arc<[R]> = self
                    .keyed
                    .values()
                    .cloned()
                    .chain(self.ring.iter().cloned())
                    .collect();
                self.cached = Some(Arc::clone(&built));
                built
            }
        };
        Snapshot {
            records,
            phase: self.phase.clone(),
            seq: self.seq,
            resume: self.resume.clone(),
        }
    }

    pub fn since(&mut self, after: Seq) -> Since<R> {
        if after >= self.seq {
            return Since::Changes {
                changes: Vec::new(),
                head: self.seq,
            };
        }
        let floor = self.journal.front().map_or(self.seq.next(), |(s, _)| *s);
        if after.next() < floor {
            return Since::Gap(self.snapshot());
        }
        Since::Changes {
            changes: self
                .journal
                .iter()
                .filter(|(s, _)| *s > after)
                .cloned()
                .collect(),
            head: self.seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Row {
        id: Option<String>,
        v: u64,
    }

    impl Record for Row {
        fn id(&self) -> Option<&str> {
            self.id.as_deref()
        }
        fn fingerprint(&self) -> u64 {
            self.v
        }
    }

    fn keyed(id: &str, v: u64) -> Row {
        Row {
            id: Some(id.into()),
            v,
        }
    }

    fn line(v: u64) -> Row {
        Row { id: None, v }
    }

    #[test]
    fn an_unchanged_upsert_does_not_advance_the_sequence() {
        let mut s = Shelf::new(16);
        assert!(s.push(Change::Upsert(keyed("a", 1))));
        assert!(!s.push(Change::Upsert(keyed("a", 1))));
        assert!(s.push(Change::Upsert(keyed("a", 2))));
        assert_eq!(s.seq(), Seq(2));
    }

    #[test]
    fn a_reset_to_the_same_content_is_not_a_change() {
        let mut s = Shelf::new(16);
        let set: Arc<[Row]> = vec![keyed("a", 1), keyed("b", 2)].into();
        assert!(s.push(Change::Reset(Arc::clone(&set))));
        assert!(!s.push(Change::Reset(set)));
        assert_eq!(s.seq(), Seq(1));
    }

    #[test]
    fn phase_flips_are_journaled_in_order_and_a_repeat_is_not() {
        let mut s: Shelf<Row> = Shelf::new(16);
        s.push(Change::Phase(Phase::Live));
        s.push(Change::Phase(Phase::Live));
        s.push(Change::Phase(Phase::Dormant { resume: Some("7".into()) }));
        s.push(Change::Phase(Phase::Live));
        let Since::Changes { changes, head } = s.since(Seq(0)) else {
            panic!("no gap expected");
        };
        let phases: Vec<&str> = changes
            .iter()
            .filter_map(|(_, c)| match c {
                Change::Phase(p) => Some(p.label()),
                _ => None,
            })
            .collect();
        assert_eq!(phases, ["live", "dormant", "live"]);
        assert_eq!(head, Seq(3));
    }

    #[test]
    fn a_cursor_older_than_the_journal_gets_a_gap_with_a_full_snapshot() {
        let mut s = Shelf::new(2);
        for v in 0..5 {
            s.push(Change::Upsert(keyed("a", v)));
        }
        assert!(matches!(s.since(Seq(1)), Since::Gap(snap) if snap.seq == Seq(5)));
        let Since::Changes { changes, .. } = s.since(Seq(3)) else {
            panic!("seq 3 is inside the journal");
        };
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn appended_records_are_a_bounded_ring() {
        let mut s = Shelf::new(3);
        for v in 0..10 {
            s.push(Change::Append(line(v)));
        }
        let snap = s.snapshot();
        assert_eq!(snap.records.iter().map(|r| r.v).collect::<Vec<_>>(), [7, 8, 9]);
    }

    #[test]
    fn a_remove_of_an_absent_id_is_not_a_change() {
        let mut s: Shelf<Row> = Shelf::new(4);
        assert!(!s.push(Change::Remove("ghost".into())));
        assert_eq!(s.seq(), Seq(0));
    }
}

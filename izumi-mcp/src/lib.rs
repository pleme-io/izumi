#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use izumi_armazem::{Armazem, Change, Interesse, Phase, Record, Seq, Since, Source};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct StreamConfig {
    pub budget_bytes: usize,
    pub settle_ms: u64,
    pub cursor_ttl_ms: u64,
    pub max_cursors: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            budget_bytes: 32 * 1024,
            settle_ms: 5000,
            cursor_ttl_ms: 300_000,
            max_cursors: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    stream: u64,
    seq: Seq,
    offset: Option<usize>,
}

impl Cursor {
    #[must_use]
    pub fn encode(&self) -> String {
        match self.offset {
            Some(o) => format!("p{}.{}.{o}", self.stream, self.seq.0),
            None => format!("f{}.{}", self.stream, self.seq.0),
        }
    }

    pub fn decode(s: &str) -> Result<Self, Refusal> {
        let bad = || Refusal::malformed(s);
        let (kind, rest) = s.split_at_checked(1).ok_or_else(bad)?;
        let parts: Vec<&str> = rest.split('.').collect();
        let num = |i: usize| -> Result<u64, Refusal> {
            parts.get(i).and_then(|p| p.parse().ok()).ok_or_else(bad)
        };
        match (kind, parts.len()) {
            ("p", 3) => Ok(Self {
                stream: num(0)?,
                seq: Seq(num(1)?),
                offset: Some(usize::try_from(num(2)?).map_err(|_| bad())?),
            }),
            ("f", 2) => Ok(Self {
                stream: num(0)?,
                seq: Seq(num(1)?),
                offset: None,
            }),
            _ => Err(bad()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub outcome: &'static str,
    pub error: String,
    pub legal: String,
}

impl Refusal {
    fn malformed(s: &str) -> Self {
        Self {
            outcome: "refused",
            error: format!("`{s}` is not a cursor this server issued"),
            legal: "pass a `next` or `follow` value exactly as a previous answer returned it, or omit the cursor to start over".into(),
        }
    }

    fn expired(s: &str) -> Self {
        Self {
            outcome: "refused",
            error: format!("cursor `{s}` has expired: its stream was idle past the cursor ttl or was displaced"),
            legal: "omit the cursor to start a fresh read".into(),
        }
    }

    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

struct Held<S: Source> {
    lease: Interesse<S>,
    pinned: Option<(Seq, Arc<[S::Record]>)>,
    touched: Instant,
}

pub struct Streams<S: Source> {
    armazem: Armazem<S>,
    cfg: StreamConfig,
    next_id: AtomicU64,
    held: Mutex<BTreeMap<u64, Held<S>>>,
}

fn size_of(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

fn phase_value(p: &Phase) -> Value {
    serde_json::to_value(p).unwrap_or(Value::Null)
}

fn change_value<R>(seq: Seq, c: &Change<R>, render: &dyn Fn(&R) -> Value) -> Value {
    match c {
        Change::Reset(rs) => json!({"seq": seq.0, "op": "reset", "records": rs.iter().map(render).collect::<Vec<_>>()}),
        Change::Upsert(r) => json!({"seq": seq.0, "op": "upsert", "record": render(r)}),
        Change::Remove(id) => json!({"seq": seq.0, "op": "remove", "id": id}),
        Change::Append(r) => json!({"seq": seq.0, "op": "append", "record": render(r)}),
        Change::Phase(p) => json!({"seq": seq.0, "op": "phase", "phase": phase_value(p)}),
    }
}

impl<S: Source> Streams<S> {
    #[must_use]
    pub fn new(armazem: Armazem<S>, cfg: StreamConfig) -> Self {
        Self {
            armazem,
            cfg,
            next_id: AtomicU64::new(1),
            held: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn armazem(&self) -> &Armazem<S> {
        &self.armazem
    }

    #[must_use]
    pub fn config(&self) -> &StreamConfig {
        &self.cfg
    }

    fn sweep(&self, now: Instant) {
        let ttl = Duration::from_millis(self.cfg.cursor_ttl_ms);
        let mut held = self.held.lock();
        held.retain(|_, h| now.saturating_duration_since(h.touched) < ttl);
        while held.len() > self.cfg.max_cursors.max(1) {
            let oldest = held
                .iter()
                .min_by_key(|(_, h)| h.touched)
                .map(|(id, _)| *id);
            match oldest {
                Some(id) => {
                    held.remove(&id);
                }
                None => break,
            }
        }
    }

    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.held.lock().len()
    }

    pub async fn read(
        &self,
        key: S::Key,
        cursor: Option<&str>,
        render: &(dyn Fn(&S::Record) -> Value + Sync),
    ) -> Result<Value, Refusal> {
        let now = Instant::now();
        self.sweep(now);
        match cursor {
            None => Ok(self.first(key, render, now).await),
            Some(raw) => {
                let c = Cursor::decode(raw)?;
                let mut held = self.held.lock();
                let Some(h) = held.get_mut(&c.stream) else {
                    return Err(Refusal::expired(raw));
                };
                h.touched = now;
                Ok(match c.offset {
                    Some(offset) => self.page(c.stream, h, offset, render),
                    None => self.follow(c.stream, h, c.seq, render),
                })
            }
        }
    }

    async fn first(
        &self,
        key: S::Key,
        render: &(dyn Fn(&S::Record) -> Value + Sync),
        now: Instant,
    ) -> Value {
        let mut lease = self.armazem.interest(key);
        let snap = match lease.read(Duration::from_millis(self.cfg.settle_ms)).await {
            Ok(s) => s,
            Err(e) => {
                return json!({"outcome": "blind", "error": e, "key": lease.key().to_string()});
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut h = Held {
            lease,
            pinned: Some((snap.seq, snap.records)),
            touched: now,
        };
        let answer = self.page(id, &mut h, 0, render);
        self.held.lock().insert(id, h);
        answer
    }

    fn page(
        &self,
        id: u64,
        h: &mut Held<S>,
        offset: usize,
        render: &(dyn Fn(&S::Record) -> Value + Sync),
    ) -> Value {
        let snap = h.lease.snapshot();
        let (seq, records) = match &h.pinned {
            Some((seq, records)) => (*seq, Arc::clone(records)),
            None => (snap.seq, Arc::clone(&snap.records)),
        };
        let mut items = Vec::new();
        let mut used = 0usize;
        let mut end = offset.min(records.len());
        for r in records.iter().skip(offset) {
            let v = render(r);
            let n = size_of(&v).saturating_add(1);
            if !items.is_empty() && used.saturating_add(n) > self.cfg.budget_bytes {
                break;
            }
            used = used.saturating_add(n);
            items.push(v);
            end += 1;
        }
        let more = end < records.len();
        if !more {
            h.pinned = None;
        }
        let next = more.then(|| {
            Cursor {
                stream: id,
                seq,
                offset: Some(end),
            }
            .encode()
        });
        json!({
            "outcome": "found",
            "key": h.lease.key().to_string(),
            "phase": phase_value(&snap.phase),
            "seq": seq.0,
            "total": records.len(),
            "offset": offset,
            "count": items.len(),
            "items": items,
            "next": next,
            "follow": Cursor { stream: id, seq, offset: None }.encode(),
            "truncated": more.then(|| json!({"by": "budget-bytes", "budget": self.cfg.budget_bytes})),
        })
    }

    fn follow(
        &self,
        id: u64,
        h: &mut Held<S>,
        after: Seq,
        render: &(dyn Fn(&S::Record) -> Value + Sync),
    ) -> Value {
        match h.lease.since(after) {
            Since::Gap(snap) => {
                h.pinned = Some((snap.seq, snap.records));
                let mut answer = self.page(id, h, 0, render);
                if let Some(o) = answer.as_object_mut() {
                    o.insert("gap".into(), json!(true));
                }
                answer
            }
            Since::Changes { changes, head } => {
                let mut out = Vec::new();
                let mut used = 0usize;
                let mut reached = after;
                for (seq, c) in &changes {
                    let v = change_value(*seq, c, render);
                    let n = size_of(&v).saturating_add(1);
                    if !out.is_empty() && used.saturating_add(n) > self.cfg.budget_bytes {
                        break;
                    }
                    used = used.saturating_add(n);
                    out.push(v);
                    reached = *seq;
                }
                let behind = reached < head;
                let snap = h.lease.snapshot();
                json!({
                    "outcome": "found",
                    "key": h.lease.key().to_string(),
                    "phase": phase_value(&snap.phase),
                    "seq": reached.0,
                    "head": head.0,
                    "changes": out,
                    "follow": Cursor { stream: id, seq: reached, offset: None }.encode(),
                    "truncated": behind.then(|| json!({"by": "budget-bytes", "budget": self.cfg.budget_bytes})),
                })
            }
        }
    }
}

pub fn record_json<R: Record + Serialize>(r: &R) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_garbage_is_refused_with_the_legal_shape() {
        for c in [
            Cursor { stream: 3, seq: Seq(9), offset: Some(40) },
            Cursor { stream: 1, seq: Seq(0), offset: None },
        ] {
            assert_eq!(Cursor::decode(&c.encode()).unwrap(), c);
        }
        for bad in ["", "x1.2", "p1.2", "f1", "p1.a.3", "f.."] {
            let r = Cursor::decode(bad).unwrap_err();
            assert_eq!(r.outcome, "refused");
            assert!(r.legal.contains("omit the cursor"));
        }
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use izumi_armazem::{
    Action, Armazem, ArmazemConfig, BoxFuture, Change, Feed, Mode, Phase, PolicyEngine, Record,
    Seq, Since, Source,
};
use parking_lot::Mutex;

#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    v: u64,
}

impl Record for Row {
    fn id(&self) -> Option<&str> {
        Some(&self.id)
    }
    fn fingerprint(&self) -> u64 {
        self.v
    }
}

#[derive(Default)]
struct Fake {
    opens: Arc<Mutex<Vec<(String, Option<String>)>>>,
    fetches: Arc<AtomicUsize>,
    ends_on_its_own: bool,
    feeds: Arc<Mutex<Vec<Feed<Row>>>>,
}

impl Source for Fake {
    type Key = String;
    type Record = Row;

    fn open(&self, key: String, resume: Option<String>, feed: Feed<Row>) -> BoxFuture<()> {
        let n = {
            let mut opens = self.opens.lock();
            opens.push((key.clone(), resume));
            opens.len()
        };
        let ends = self.ends_on_its_own;
        let feeds = Arc::clone(&self.feeds);
        Box::pin(async move {
            feed.reset(vec![Row { id: format!("{key}-row"), v: n as u64 }]);
            feed.resume_at(format!("rv-{n}"));
            feed.live();
            if ends {
                return;
            }
            feeds.lock().push(feed);
            std::future::pending::<()>().await;
        })
    }

    fn fetch(&self, key: String) -> BoxFuture<Result<Vec<Row>, String>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(vec![Row { id: key, v: 0 }]) })
    }
}

fn cfg(mode: Mode) -> ArmazemConfig {
    ArmazemConfig {
        mode,
        policy: PolicyEngine::Vigy,
        linger_ms: 1000,
        evict_dormant_after_ms: 5000,
        ..ArmazemConfig::default()
    }
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn interest_opens_once_idle_parks_to_dormant_and_interest_resumes_from_the_cursor() {
    let fake = Fake::default();
    let opens = Arc::clone(&fake.opens);
    let a = Armazem::new(fake, cfg(Mode::Lazy)).unwrap();

    let mut first = a.interest("pods".to_owned());
    let second = a.interest("pods".to_owned());
    let snap = first.settled(Duration::from_secs(1)).await;
    assert_eq!(snap.phase, Phase::Live);
    assert_eq!(snap.records.len(), 1);
    assert_eq!(opens.lock().len(), 1);
    let cursor = snap.seq;
    drop(first);
    drop(second);

    tokio::time::advance(Duration::from_millis(999)).await;
    assert!(a.tick().unwrap().is_empty());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(a.tick().unwrap(), [Action::Park("pods".into())]);

    let state = &a.inventory()[0];
    assert_eq!(state.phase, Phase::Dormant { resume: Some("rv-1".into()) });
    assert_eq!(state.records, 1);
    assert!(!state.live);

    let mut back = a.interest("pods".to_owned());
    back.settled(Duration::from_secs(1)).await;
    assert_eq!(opens.lock()[1], ("pods".to_owned(), Some("rv-1".to_owned())));

    let Since::Changes { changes, .. } = back.since(cursor) else {
        panic!("a cursor taken before the park must still be inside the journal");
    };
    let flips: Vec<&str> = changes
        .iter()
        .filter_map(|(_, c)| match c {
            Change::Phase(p) => Some(p.label()),
            _ => None,
        })
        .collect();
    assert_eq!(flips, ["dormant", "absorbing", "live"]);
}

#[tokio::test(start_paused = true)]
async fn a_dormant_key_is_evicted_after_the_window_and_reopens_without_a_cursor() {
    let fake = Fake::default();
    let opens = Arc::clone(&fake.opens);
    let a = Armazem::new(fake, cfg(Mode::Lazy)).unwrap();
    drop(a.interest("svc".to_owned()));
    settle().await;
    tokio::time::advance(Duration::from_millis(1000)).await;
    a.tick().unwrap();
    tokio::time::advance(Duration::from_millis(5000)).await;
    assert_eq!(a.tick().unwrap(), [Action::Evict("svc".into())]);
    assert!(a.inventory().is_empty());
    let mut again = a.interest("svc".to_owned());
    again.settled(Duration::from_secs(1)).await;
    assert_eq!(opens.lock()[1], ("svc".to_owned(), None));
}

#[tokio::test(start_paused = true)]
async fn eager_keeps_an_idle_stream_live() {
    let a = Armazem::new(Fake::default(), cfg(Mode::Eager)).unwrap();
    drop(a.interest("nodes".to_owned()));
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(a.tick().unwrap().is_empty());
    assert!(a.inventory()[0].live);
}

#[tokio::test(start_paused = true)]
async fn direct_never_opens_a_stream_and_reads_through_fetch() {
    let fake = Fake::default();
    let opens = Arc::clone(&fake.opens);
    let fetches = Arc::clone(&fake.fetches);
    let a = Armazem::new(fake, cfg(Mode::Direct)).unwrap();
    let mut i = a.interest("ns".to_owned());
    let snap = i.read(Duration::from_secs(1)).await.unwrap();
    assert_eq!(snap.phase, Phase::Direct);
    assert_eq!(snap.records.len(), 1);
    assert!(opens.lock().is_empty());
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_is_degraded_and_the_zelador_reopens_it_while_wanted() {
    let fake = Fake {
        ends_on_its_own: true,
        ..Fake::default()
    };
    let opens = Arc::clone(&fake.opens);
    let a = Armazem::new(fake, cfg(Mode::Lazy)).unwrap();
    let _held = a.interest("events".to_owned());
    settle().await;
    let state = &a.inventory()[0];
    assert_eq!(state.phase, Phase::Degraded { cause: "the stream ended".into() });
    assert!(!state.live);
    assert_eq!(a.tick().unwrap(), [Action::Start("events".into())]);
    settle().await;
    assert_eq!(opens.lock().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_parked_stream_cannot_write_and_a_wanted_key_cannot_be_parked() {
    let fake = Fake::default();
    let feeds = Arc::clone(&fake.feeds);
    let a = Armazem::new(fake, cfg(Mode::Lazy)).unwrap();
    let held = a.interest("cm".to_owned());
    settle().await;
    assert!(!a.apply(&Action::Park("cm".into())));
    drop(held);
    assert!(a.apply(&Action::Park("cm".into())));
    let before = a.inventory()[0].seq;
    let stale = feeds.lock().remove(0);
    stale.upsert(Row { id: "late".into(), v: 9 });
    stale.live();
    assert!(stale.stopped());
    let after = &a.inventory()[0];
    assert_eq!(after.seq, before);
    assert_eq!(after.records, 1);
    assert!(matches!(after.phase, Phase::Dormant { .. }));
}

#[tokio::test(start_paused = true)]
async fn the_spawned_zelador_parks_on_its_own_tick() {
    let a = Armazem::new(Fake::default(), cfg(Mode::Lazy)).unwrap();
    let _z = a.spawn_zelador();
    drop(a.interest("deploy".to_owned()));
    settle().await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!a.inventory()[0].live);
    assert_eq!(a.zelador_error(), None);
    let _ = Seq::default();
}

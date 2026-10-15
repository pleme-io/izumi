use std::sync::Arc;
use std::time::Duration;

use izumi_armazem::{Armazem, ArmazemConfig, BoxFuture, Feed, Mode, Record, Source};
use izumi_mcp::{StreamConfig, Streams};
use parking_lot::Mutex;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
struct Line {
    id: Option<String>,
    text: String,
}

impl Record for Line {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    fn fingerprint(&self) -> u64 {
        self.text.len() as u64
    }
}

#[derive(Default, Clone)]
struct Fake {
    rows: usize,
    feeds: Arc<Mutex<Vec<Feed<Line>>>>,
}

impl Source for Fake {
    type Key = String;
    type Record = Line;

    fn open(&self, _key: String, _resume: Option<String>, feed: Feed<Line>) -> BoxFuture<()> {
        let rows = self.rows;
        let feeds = Arc::clone(&self.feeds);
        Box::pin(async move {
            feed.reset(
                (0..rows)
                    .map(|i| Line {
                        id: Some(format!("r{i:04}")),
                        text: "x".repeat(100),
                    })
                    .collect(),
            );
            feed.live();
            feeds.lock().push(feed);
            std::future::pending::<()>().await;
        })
    }

    fn fetch(&self, _key: String) -> BoxFuture<Result<Vec<Line>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn render(l: &Line) -> Value {
    json!({"id": l.id, "text": l.text})
}

fn streams(rows: usize, budget: usize) -> (Streams<Fake>, Arc<Mutex<Vec<Feed<Line>>>>) {
    let fake = Fake {
        rows,
        ..Fake::default()
    };
    let feeds = Arc::clone(&fake.feeds);
    let cfg = ArmazemConfig {
        mode: Mode::Lazy,
        ..ArmazemConfig::default()
    };
    let s = Streams::new(
        Armazem::new(fake, cfg).unwrap(),
        StreamConfig {
            budget_bytes: budget,
            ..StreamConfig::default()
        },
    );
    (s, feeds)
}

fn bytes(v: &Value) -> usize {
    serde_json::to_vec(&v["items"]).unwrap().len()
}

#[tokio::test(start_paused = true)]
async fn a_large_read_pages_inside_the_budget_and_the_pages_reassemble_the_whole_set() {
    let (s, _) = streams(500, 4096);
    let mut seen = Vec::new();
    let mut page = s.read("pods".into(), None, &render).await.unwrap();
    let total = page["total"].as_u64().unwrap();
    assert_eq!(total, 500);
    loop {
        assert!(bytes(&page) <= 4096 + 2, "page over budget: {}", bytes(&page));
        for it in page["items"].as_array().unwrap() {
            seen.push(it["id"].as_str().unwrap().to_owned());
        }
        let Some(next) = page["next"].as_str().map(str::to_owned) else { break };
        assert_eq!(page["truncated"]["by"], "budget-bytes");
        page = s.read("pods".into(), Some(&next), &render).await.unwrap();
    }
    let want: Vec<String> = (0..500).map(|i| format!("r{i:04}")).collect();
    assert_eq!(seen, want);
}

#[tokio::test(start_paused = true)]
async fn follow_returns_only_what_changed_since_the_cursor() {
    let (s, feeds) = streams(3, 32 * 1024);
    let first = s.read("pods".into(), None, &render).await.unwrap();
    assert!(first["next"].is_null());
    let follow = first["follow"].as_str().unwrap().to_owned();

    let quiet = s.read("pods".into(), Some(&follow), &render).await.unwrap();
    assert_eq!(quiet["changes"], json!([]));

    feeds.lock()[0].upsert(Line {
        id: Some("r0001".into()),
        text: "changed".into(),
    });
    feeds.lock()[0].remove("r0002");
    let delta = s.read("pods".into(), Some(&follow), &render).await.unwrap();
    let ops: Vec<&str> = delta["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["op"].as_str().unwrap())
        .collect();
    assert_eq!(ops, ["upsert", "remove"]);
    assert_eq!(delta["changes"][0]["record"]["text"], "changed");

    let next = delta["follow"].as_str().unwrap().to_owned();
    let after = s.read("pods".into(), Some(&next), &render).await.unwrap();
    assert_eq!(after["changes"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn an_unknown_or_expired_cursor_is_refused_not_answered_empty() {
    let (s, _) = streams(3, 32 * 1024);
    let first = s.read("pods".into(), None, &render).await.unwrap();
    let follow = first["follow"].as_str().unwrap().to_owned();
    tokio::time::advance(Duration::from_millis(StreamConfig::default().cursor_ttl_ms)).await;
    let r = s.read("pods".into(), Some(&follow), &render).await.unwrap_err();
    assert_eq!(r.outcome, "refused");
    assert!(r.error.contains("expired"));
    assert_eq!(s.open_streams(), 0);
    let r = s.read("pods".into(), Some("p999.0.0"), &render).await.unwrap_err();
    assert!(r.error.contains("expired"));
}

#[tokio::test(start_paused = true)]
async fn one_record_larger_than_the_budget_still_makes_progress() {
    let (s, _) = streams(2, 10);
    let page = s.read("pods".into(), None, &render).await.unwrap();
    assert_eq!(page["count"], 1);
    assert!(page["next"].is_string());
}

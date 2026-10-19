#![cfg(feature = "vigy")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use izumi_armazem::vigy::VigyPolicy;
use izumi_armazem::{ArmazemConfig, DemandRow, Policy, ZELADOR_PROGRAM};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size().cast_signed(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size().cast_signed(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size.cast_signed() - layout.size().cast_signed(),
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn demand() -> Vec<DemandRow> {
    vec![
        DemandRow {
            key: "pod/*".into(),
            interest: 1,
            live: true,
            idle_ms: None,
            dormant_ms: None,
        },
        DemandRow {
            key: "log/ns/p".into(),
            interest: 0,
            live: true,
            idle_ms: Some(40_000),
            dormant_ms: None,
        },
    ]
}

#[test]
fn a_zelador_tick_retains_nothing() {
    let policy = VigyPolicy::new(ZELADOR_PROGRAM);
    let cfg = ArmazemConfig::default();
    let rows = demand();
    for _ in 0..20 {
        policy.decide(&rows, &cfg).expect("decides");
    }
    let before = LIVE.load(Ordering::Relaxed);
    let ticks = 500;
    for _ in 0..ticks {
        policy.decide(&rows, &cfg).expect("decides");
    }
    let retained = LIVE.load(Ordering::Relaxed) - before;
    assert!(
        retained < 64 * 1024,
        "{ticks} zelador ticks retained {retained} bytes ({} per tick); a session ticking once a second leaks without bound",
        retained / ticks
    );
}

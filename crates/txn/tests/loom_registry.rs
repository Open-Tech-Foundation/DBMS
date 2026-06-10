//! `loom` model check of the writer/reader snapshot handoff.
//!
//! Only built under `--cfg loom`; run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p txn --test loom_registry --release
//! ```
//!
//! Loom explores every interleaving of the threads in each model, so an
//! assertion here is a proof over the schedule space, not a sampled test.
#![cfg(loom)]
#![allow(clippy::unwrap_used)]

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc;
use loom::thread;

use pager::PageId;
use txn::Registry;

fn page(n: u64) -> PageId {
    PageId::new(n)
}

/// The core reclamation invariant: a reader pinned at version 1 never observes
/// the version-1 root page reclaimed, no matter how its acquire/release
/// interleaves with the writer's publish + watermark check.
#[test]
fn a_pinned_reader_never_sees_its_pages_reclaimed() {
    loom::model(|| {
        let registry = Arc::new(Registry::new(Some(page(2)), 1));
        // Simulates the fate of the version-1 root (page 2), which commit 2
        // supersedes: the writer reclaims it only when the watermark allows.
        let v1_reclaimed = Arc::new(AtomicBool::new(false));

        let reader = {
            let registry = Arc::clone(&registry);
            let v1_reclaimed = Arc::clone(&v1_reclaimed);
            thread::spawn(move || {
                let (root, txn) = registry.acquire();
                match txn {
                    // Pinned the old version: its pages must still be intact
                    // for as long as the pin is held.
                    1 => {
                        assert_eq!(root, Some(page(2)));
                        assert!(
                            !v1_reclaimed.load(Ordering::SeqCst),
                            "reader pinned v1 but its root was reclaimed"
                        );
                    }
                    // Pinned the new version: never touches the old page.
                    2 => assert_eq!(root, Some(page(3))),
                    other => panic!("impossible version {other}"),
                }
                registry.release(txn);
            })
        };

        // The writer: commit 2 moves the root to page 3 and supersedes page 2,
        // then reclaims it only if no live snapshot can still see version 1.
        registry.publish(Some(page(3)), 2);
        if registry.watermark() >= 2 {
            v1_reclaimed.store(true, Ordering::SeqCst);
        }

        reader.join().unwrap();
    });
}

/// Refcounting: two readers sharing a version hold the watermark down until
/// the *last* one releases, across every interleaving.
#[test]
fn the_watermark_holds_until_the_last_reader_releases() {
    loom::model(|| {
        let registry = Arc::new(Registry::new(Some(page(2)), 1));
        let v1_reclaimed = Arc::new(AtomicBool::new(false));

        let spawn_reader = |registry: &Arc<Registry>, v1_reclaimed: &Arc<AtomicBool>| {
            let registry = Arc::clone(registry);
            let v1_reclaimed = Arc::clone(v1_reclaimed);
            thread::spawn(move || {
                let (_root, txn) = registry.acquire();
                if txn == 1 {
                    assert!(
                        !v1_reclaimed.load(Ordering::SeqCst),
                        "v1 reclaimed while a reader held it"
                    );
                }
                registry.release(txn);
            })
        };
        let r1 = spawn_reader(&registry, &v1_reclaimed);
        let r2 = spawn_reader(&registry, &v1_reclaimed);

        registry.publish(Some(page(3)), 2);
        if registry.watermark() >= 2 {
            v1_reclaimed.store(true, Ordering::SeqCst);
        }

        r1.join().unwrap();
        r2.join().unwrap();
        // Once everyone is done, nothing constrains reclamation.
        assert_eq!(registry.live_count(), 0);
    });
}

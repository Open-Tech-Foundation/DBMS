//! A long-lived reader: a snapshot pinned across heavy write churn keeps its
//! exact frozen view, the pages it can see are never reclaimed while it lives,
//! and releasing it advances the watermark so the parked pages return to the
//! allocator.
#![allow(clippy::unwrap_used)]

use common::MemoryBackend;
use txn::Db;

const KEYS: u8 = 50;
const ROUNDS: u8 = 10;

fn key(i: u8) -> Vec<u8> {
    vec![b'k', i]
}

fn value(i: u8, round: u8) -> Vec<u8> {
    let mut v = vec![round; 100];
    v[0] = i;
    v
}

#[test]
fn a_pinned_snapshot_keeps_its_view_and_defers_reclamation() {
    let db = Db::create(MemoryBackend::new()).unwrap();
    for i in 0..KEYS {
        db.put(key(i), value(i, 0)).unwrap();
    }

    // Pin a snapshot and freeze what it must keep seeing.
    let pinned = db.snapshot();
    let frozen = pinned.scan().unwrap();
    assert_eq!(frozen.len(), usize::from(KEYS));
    let at_pin = db.validate().unwrap();

    // Heavy churn: overwrite every key, many rounds. Each round supersedes the
    // whole tree's pages; with the snapshot live they must all stay parked.
    for round in 1..=ROUNDS {
        for i in 0..KEYS {
            db.put(key(i), value(i, round)).unwrap();
        }
    }

    // The pinned snapshot still reads its exact frozen view — byte for byte.
    // If any page it can see had been reclaimed and reused, these reads would
    // return corrupt or wrong data.
    assert_eq!(pinned.scan().unwrap(), frozen);
    for i in 0..KEYS {
        assert_eq!(pinned.get(&key(i)).unwrap(), Some(value(i, 0)));
    }

    // A fresh snapshot sees the latest round, so the writer really moved on.
    let fresh = db.snapshot();
    assert_eq!(fresh.get(&key(0)).unwrap(), Some(value(0, ROUNDS)));
    assert!(fresh.txn_id() > pinned.txn_id());
    drop(fresh);

    // With the watermark pinned, churned pages could not be reused: the file
    // had to grow well past the handful of free pages available at the pin.
    let while_pinned = db.validate().unwrap();
    assert!(
        while_pinned.page_count > at_pin.page_count + u64::from(ROUNDS),
        "churn under a pinned snapshot should extend the file \
         (at pin: {}, after churn: {})",
        at_pin.page_count,
        while_pinned.page_count
    );

    // Release the reader; the next batch reclaims everything parked behind it.
    drop(pinned);
    db.put(b"trigger".to_vec(), b"x".to_vec()).unwrap();

    let after_release = db.validate().unwrap();
    assert!(
        after_release.free_ids > while_pinned.free_ids + u64::from(ROUNDS),
        "releasing the snapshot should return the parked pages \
         (free before: {}, free after: {})",
        while_pinned.free_ids,
        after_release.free_ids
    );

    // And the reclaimed pages really are reused: another full round of churn
    // barely grows the file compared to the pinned rounds.
    let grown_pinned = while_pinned.page_count - at_pin.page_count;
    for i in 0..KEYS {
        db.put(key(i), value(i, ROUNDS + 1)).unwrap();
    }
    let after_churn = db.validate().unwrap();
    let grown_free = after_churn.page_count - after_release.page_count;
    assert!(
        grown_free < grown_pinned / u64::from(ROUNDS),
        "churn with free pages available should mostly reuse them \
         (pinned rounds grew {grown_pinned} pages over {ROUNDS} rounds; \
          one free round grew {grown_free})"
    );

    // Everything is still readable and intact after all the recycling.
    let last = db.snapshot();
    for i in 0..KEYS {
        assert_eq!(last.get(&key(i)).unwrap(), Some(value(i, ROUNDS + 1)));
    }
}

#[test]
fn the_oldest_of_several_snapshots_governs_reclamation() {
    let db = Db::create(MemoryBackend::new()).unwrap();
    for i in 0..KEYS {
        db.put(key(i), value(i, 0)).unwrap();
    }

    // Two pinned readers at different versions.
    let old = db.snapshot();
    db.put(key(0), value(0, 1)).unwrap();
    let mid = db.snapshot();
    for i in 0..KEYS {
        db.put(key(i), value(i, 2)).unwrap();
    }

    // Dropping the *newer* snapshot must not unpin the older one's pages.
    drop(mid);
    db.put(b"trigger".to_vec(), b"x".to_vec()).unwrap();
    assert_eq!(old.get(&key(0)).unwrap(), Some(value(0, 0)));
    assert_eq!(old.scan().unwrap().len(), usize::from(KEYS));

    let while_old = db.validate().unwrap();
    drop(old);
    db.put(b"trigger2".to_vec(), b"x".to_vec()).unwrap();
    let after = db.validate().unwrap();
    assert!(
        after.free_ids > while_old.free_ids,
        "dropping the last old reader should release its parked pages"
    );
}

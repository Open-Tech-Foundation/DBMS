//! Phase 3 exit criterion: copy-on-write root handles are real snapshots — a
//! root captured earlier keeps seeing exactly the state it was captured at, no
//! matter how many inserts/deletes install later roots.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use btree::BTree;
use common::{MemoryBackend, Rng, SeededRng};
use pager::{PageId, Pager};

fn scan(tree: &BTree<MemoryBackend>, root: PageId) -> Vec<(Vec<u8>, Vec<u8>)> {
    tree.range(root, None, None).unwrap().collect_all().unwrap()
}

fn expected(model: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)> {
    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[test]
fn an_old_root_sees_no_later_writes() {
    let pager = Pager::create(MemoryBackend::new()).unwrap();
    let tree = BTree::new(&pager);
    let rng = SeededRng::new(20260609);

    // Build an initial tree of a few hundred entries (deep enough to be multi
    // level), freeing superseded pages as we go.
    let mut root = tree.create().unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for i in 0u32..400 {
        let k = format!("key-{:05}", (rng.next_u64() % 4000)).into_bytes();
        let v = vec![(i % 251) as u8; (rng.next_u64() % 300) as usize];
        let edit = tree.insert(root, &k, &v).unwrap();
        for &id in &edit.freed {
            pager.free(id).unwrap();
        }
        root = edit.new_root;
        model.insert(k, v);
    }

    // Capture the snapshot: root id + the exact contents at this instant.
    let snap_root = root;
    let snap_model = model.clone();
    let snap_view = expected(&snap_model);
    assert!(
        tree.validate(snap_root).unwrap().height >= 2,
        "tree too shallow to be interesting"
    );

    // Now mutate heavily WITHOUT freeing anything — the freed pages are exactly
    // the snapshot's nodes, which must stay readable.
    for _ in 0..1500 {
        match rng.next_u64() % 2 {
            0 => {
                let k = format!("key-{:05}", (rng.next_u64() % 4000)).into_bytes();
                let v = vec![0xAB; (rng.next_u64() % 400) as usize];
                let edit = tree.insert(root, &k, &v).unwrap();
                root = edit.new_root;
                model.insert(k, v);
            }
            _ => {
                if model.is_empty() {
                    continue;
                }
                let nth = (rng.next_u64() as usize) % model.len();
                let k = model.keys().nth(nth).unwrap().clone();
                let edit = tree.delete(root, &k).unwrap();
                root = edit.new_root;
                model.remove(&k);
            }
        }
    }

    // The live root reflects all the churn...
    assert_eq!(
        scan(&tree, root),
        expected(&model),
        "live root drifted from model"
    );
    // ...while the snapshot is byte-for-byte what it was at capture time.
    assert_eq!(
        scan(&tree, snap_root),
        snap_view,
        "snapshot saw later writes"
    );
    assert_eq!(
        tree.validate(snap_root).unwrap().entries,
        snap_model.len() as u64
    );

    // Spot-check point lookups against the snapshot model too.
    for (k, v) in snap_model.iter().take(50) {
        assert_eq!(
            tree.lookup(snap_root, k).unwrap().as_deref(),
            Some(v.as_slice())
        );
    }
}

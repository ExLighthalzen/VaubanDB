//! Writers of several threads over the trees of one instance, and the walk that checks the
//! invariants of a tree after them.

use std::cmp::Ordering;

use vauban_types::{SqlType, TypeInfo, Value};

use super::super::temp::TempDir;
use super::super::{DiskOptions, DiskStorage};
use super::*;

/// An empty instance in a temporary directory, with the guard that removes it.
fn instance(label: &str) -> (TempDir, DiskStorage) {
    let dir = TempDir::created(label);
    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
    (dir, storage)
}

/// A copy of the page `id` of the instance, read through the pool.
fn page_of(storage: &DiskStorage, id: PageId) -> Page {
    let pin = storage.pool.pin(id).expect("pin the page");
    pin.with_page(Page::clone).expect("read the cached page")
}

/// An ascending order of `arity` columns of default collation, which is the order of a tree
/// whose key columns are integers, ascending.
fn ascending_ints(arity: usize) -> KeyOrder {
    let columns: Vec<KeyColumn> = (0..arity)
        .map(|column| KeyColumn {
            column: u16::try_from(column).expect("a small arity"),
            descending: false,
        })
        .collect();
    let types = vec![TypeInfo::new(SqlType::BigInt, true); arity];
    KeyOrder::new(&columns, &types).expect("the order of the tree")
}

/// Walks the tree rooted at `root` and panics at the first broken invariant; answers the
/// number of leaf entries.
///
/// Checked: each page is a leaf or an internal page; the entries of a leaf and the separators
/// of an internal page are in strictly increasing order; each entry sits at or above the
/// separator before its subtree and below the one after it; every leaf is at the same depth;
/// and the `prev`/`next` links of the leaves follow the order of the descent, with no
/// neighbour past the two ends.
fn assert_tree_valid(storage: &DiskStorage, root: PageId, order: &KeyOrder) -> usize {
    let mut leaves = Vec::new();
    let mut depth = None;
    let entries = check_subtree(
        storage,
        order,
        root,
        (None, None),
        0,
        &mut depth,
        &mut leaves,
    );
    for (at, leaf) in leaves.iter().enumerate() {
        let page = page_of(storage, *leaf);
        let before = at.checked_sub(1).map(|at| leaves[at]);
        assert_eq!(leaf_prev(&page), before, "prev of leaf {leaf}");
        assert_eq!(
            leaf_next(&page),
            leaves.get(at + 1).copied(),
            "next of leaf {leaf}"
        );
    }
    entries
}

/// The probe that sits at the entry `separator` names, for the bounds of a subtree.
fn bound_of(separator: &[u8]) -> Probe {
    probe_of(separator).expect("a separator decodes")
}

/// One level of [`assert_tree_valid`]: the entries of `node` sit in `[low, high)`.
fn check_subtree(
    storage: &DiskStorage,
    order: &KeyOrder,
    node: PageId,
    (low, high): (Option<&[u8]>, Option<&[u8]>),
    level: usize,
    depth: &mut Option<usize>,
    leaves: &mut Vec<PageId>,
) -> usize {
    let page = page_of(storage, node);
    let in_range = |entry: &TreeEntry| {
        low.is_none_or(|low| {
            compare_entry(order, entry, &bound_of(low)).expect("compare") != Ordering::Less
        }) && high.is_none_or(|high| {
            compare_entry(order, entry, &bound_of(high)).expect("compare") == Ordering::Less
        })
    };
    match page.kind().expect("the kind of a page of the tree") {
        PageKind::BTreeLeaf => {
            assert_eq!(
                *depth.get_or_insert(level),
                level,
                "leaf {node} at another depth"
            );
            let entries: Vec<TreeEntry> = (0..leaf_len(&page))
                .map(|at| {
                    let (bytes, _) = entry_at(&page, LEAF_DIRECTORY, at).expect("a leaf entry");
                    decode_entry_key(bytes).expect("an entry decodes")
                })
                .collect();
            for pair in entries.windows(2) {
                let next = Probe {
                    values: pair[1].0.clone(),
                    payload: PayloadBound::At(pair[1].1.clone()),
                };
                assert_eq!(
                    compare_entry(order, &pair[0], &next).expect("compare"),
                    Ordering::Less,
                    "leaf {node} out of order"
                );
            }
            assert!(
                entries.iter().all(in_range),
                "leaf {node} outside its bounds"
            );
            leaves.push(node);
            entries.len()
        }
        PageKind::BTreeInternal => {
            let keys: Vec<Vec<u8>> = (0..internal_len(&page))
                .map(|at| internal_key(&page, at).expect("a separator").to_vec())
                .collect();
            let children: Vec<PageId> = (0..=internal_len(&page))
                .map(|at| internal_child(&page, at).expect("a child"))
                .collect();
            for pair in keys.windows(2) {
                let (left, _) = decode_entry_key(&pair[0]).expect("a separator decodes");
                let left = (left, probe_payload(&pair[0]));
                assert_eq!(
                    compare_entry(order, &left, &bound_of(&pair[1])).expect("compare"),
                    Ordering::Less,
                    "separators of page {node} out of order"
                );
            }
            let mut total = 0;
            for (at, child) in children.iter().enumerate() {
                let below = if at == 0 {
                    low
                } else {
                    Some(keys[at - 1].as_slice())
                };
                let above = keys.get(at).map(Vec::as_slice).or(high);
                total += check_subtree(
                    storage,
                    order,
                    *child,
                    (below, above),
                    level + 1,
                    depth,
                    leaves,
                );
            }
            total
        }
        other => panic!("page {node} of the tree is a {other:?} page"),
    }
}

/// The payload a separator carries.
fn probe_payload(separator: &[u8]) -> Vec<u8> {
    decode_entry_key(separator).expect("a separator decodes").1
}

/// Eight threads insert overlapping keys into a heap table with a `unique` index and into a
/// clustered table with an index of its own: each key goes in once, every refusal is 2601, and
/// the trees the catalogue names hold one entry per row and keep their invariants. Their roots
/// moved, which is what a writer that opened a stale root would have lost.
#[test]
fn eight_writers_leave_valid_trees() {
    use crate::{IndexShape, Row, Snapshot, Storage, TableShape, TxnId};
    const THREADS: u64 = 8;
    const KEYS: i32 = 2_000;
    const PER_THREAD: i32 = 4_000;
    const PER_TXN: i32 = 200;
    let (_dir, storage) = instance("btree-eight-writers");
    let db = storage.create_database("d").expect("create the database");
    let types = vec![TypeInfo::new(SqlType::Int, true); 2];
    let column = |column: u16| KeyColumn {
        column,
        descending: false,
    };
    let heap = storage
        .create_table(
            db,
            &TableShape {
                columns: types.clone(),
                clustered_key: None,
            },
        )
        .expect("create the heap table");
    let unique = IndexShape {
        columns: vec![column(0)],
        unique: true,
        included: vec![],
    };
    let heap_index = storage
        .create_index(heap, &unique)
        .expect("create the unique index");
    let clustered = storage
        .create_table(
            db,
            &TableShape {
                columns: types.clone(),
                clustered_key: Some(vec![column(0)]),
            },
        )
        .expect("create the clustered table");
    storage
        .create_index(clustered, &unique)
        .expect("the unique clustered key");
    let second = IndexShape {
        columns: vec![column(1)],
        unique: false,
        included: vec![],
    };
    let clustered_index = storage
        .create_index(clustered, &second)
        .expect("create the index");
    let roots_before = index_root(&storage, heap_index);

    let inserted = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|thread| {
                let storage = &storage;
                scope.spawn(move || {
                    let mut done = 0;
                    let offset = i32::try_from(thread).expect("a thread number") * 251;
                    for batch in 0..PER_THREAD / PER_TXN {
                        let txn = TxnId(1 + thread * 1_000 + u64::try_from(batch).expect("n"));
                        for step in 0..PER_TXN {
                            let key = (offset + (batch * PER_TXN + step) * 7) % KEYS;
                            let row = Row(vec![Value::I32(key), Value::I32(key)]);
                            for table in [heap, clustered] {
                                match storage.insert(txn, table, &row) {
                                    Ok(_) => done += 1,
                                    Err(err) => assert_eq!(err.number, 2601, "{}", err.message),
                                }
                            }
                        }
                        storage.commit(txn).expect("commit");
                    }
                    done
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("a writer"))
            .sum::<usize>()
    });
    let keys = usize::try_from(KEYS).expect("a count");
    assert_eq!(
        inserted,
        2 * keys,
        "one insert per key and per table went through"
    );

    let all = Snapshot {
        xmin: TxnId(u64::MAX),
        xmax: TxnId(u64::MAX),
        active: vec![],
        own: TxnId(0),
    };
    for table in [heap, clustered] {
        assert_eq!(storage.scan(&all, table).expect("scan").count(), keys);
    }
    let heap_root = index_root(&storage, heap_index);
    assert_ne!(
        heap_root, roots_before,
        "a split moved the root of the index"
    );
    for (root, arity) in [(heap_root, 1), (index_root(&storage, clustered_index), 1)] {
        let order = KeyOrder::new(&[column(0)], &types[..arity]).expect("the order");
        assert_eq!(
            assert_tree_valid(&storage, root, &order),
            keys,
            "the tree at {root}"
        );
    }
    let entry = storage
        .lock_catalogue()
        .expect("the catalogue")
        .table(clustered)
        .cloned();
    let entry = entry.expect("the clustered table");
    let tree = entry.tree_root.expect("the tree of the versions");
    let tree_arity = first_entry_arity(&storage, tree);
    assert_eq!(
        assert_tree_valid(&storage, tree, &ascending_ints(tree_arity)),
        keys
    );
    let directory = entry.directory_root.expect("the directory");
    assert_eq!(
        assert_tree_valid(&storage, directory, &ascending_ints(1)),
        keys
    );
}

/// The root of the tree of `index`, as the catalogue holds it.
fn index_root(storage: &DiskStorage, index: crate::IndexId) -> PageId {
    let catalogue = storage.lock_catalogue().expect("the catalogue");
    catalogue
        .index(index)
        .and_then(|entry| entry.root)
        .expect("an index with a tree")
}

/// The number of key values of the entries of the tree rooted at `root`, read from its
/// leftmost leaf.
fn first_entry_arity(storage: &DiskStorage, root: PageId) -> usize {
    let tree = BTree::open(
        storage,
        root,
        &[KeyColumn {
            column: 0,
            descending: false,
        }],
        &[TypeInfo::new(SqlType::Int, true)],
    )
    .expect("open the tree");
    let leaf = tree.edge_leaf(true).expect("the leftmost leaf");
    let page = page_of(storage, leaf);
    let (bytes, _) = entry_at(&page, LEAF_DIRECTORY, 0).expect("an entry");
    decode_entry_key(bytes).expect("an entry decodes").0.len()
}

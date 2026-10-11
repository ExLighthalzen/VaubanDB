//! Tests of the B+tree page layer and the tree that walks it: leaf and internal page
//! layout, the split that turns a full page into two, runs of equal keys, the typed
//! order entries compare by, `insert`/`delete`/`seek`, and the ceiling on entries a
//! page holds.

use vauban_types::SqlType;

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

/// An empty leaf page, not tied to an instance.
fn leaf(id: u64) -> Page {
    let mut page = Page::empty(PageKind::Free, PageId(id));
    init_leaf(&mut page);
    page
}

/// The entries of a leaf, copied out for a comparison.
fn entries_of(page: &Page) -> Vec<Entry> {
    leaf_iter(page)
        .expect("the leaf reads back")
        .map(|(key, payload)| (key.to_vec(), payload.to_vec()))
        .collect()
}

/// The keys of a leaf, in the order its directory holds them.
fn keys_of(page: &Page) -> Vec<Vec<u8>> {
    entries_of(page).into_iter().map(|(key, _)| key).collect()
}

/// The keys of an internal page, in the order its directory holds them.
fn internal_keys(page: &Page) -> Vec<Vec<u8>> {
    (0..internal_len(page))
        .map(|index| {
            internal_key(page, index)
                .expect("the internal page reads back")
                .to_vec()
        })
        .collect()
}

/// The children of an internal page, `P0` first.
fn internal_children(page: &Page) -> Vec<PageId> {
    (0..=internal_len(page))
        .map(|index| internal_child(page, index).expect("the child reads back"))
        .collect()
}

/// A copy of the page `id` of the instance, read through the pool.
fn page_of(storage: &DiskStorage, id: PageId) -> Page {
    let pin = storage.pool.pin(id).expect("pin the page");
    pin.with_page(Page::clone).expect("read the cached page")
}

/// Runs `split` on the page `left` through a set that writes no record, and puts the pages it
/// wrote in the pool.
fn split_now(
    storage: &DiskStorage,
    left: PageId,
    split: fn(&mut PageSet<'_>, PageId) -> Result<Split, InternalError>,
) -> Result<Split, InternalError> {
    let mut set = PageSet::new(storage, false);
    let done = split(&mut set, left)?;
    set.commit(|_| Ok(()))?;
    Ok(done)
}

/// A key of a fixed width, so that the byte order of the keys is their numeric order.
fn key(number: usize) -> Vec<u8> {
    format!("k{number:04}").into_bytes()
}

/// A payload of `width` bytes carrying the number of its entry.
fn payload(number: usize, width: usize) -> Vec<u8> {
    let mut bytes = format!("p{number:04}").into_bytes();
    bytes.resize(width, b'.');
    bytes
}

/// An allocated leaf of the instance, filled until it answers [`Put::Full`]; answers its
/// identifier and the number of entries it took.
fn filled_leaf(storage: &DiskStorage) -> (PageId, usize) {
    let id = allocate(storage).expect("allocate a leaf");
    let pin = storage.pool.pin(id).expect("pin the leaf");
    let count = pin
        .with_page_mut(|page| {
            init_leaf(page);
            let mut count = 0;
            while leaf_put(page, &key(count), &payload(count, 100))
                .expect("put an entry in the leaf")
                == Put::Inserted
            {
                count += 1;
            }
            count
        })
        .expect("the test holds the only pin");
    PageWrites::free()
        .mark(storage, id)
        .expect("mark the leaf dirty");
    drop(pin);
    (id, count)
}

#[test]
fn leaf_insert_ordered_slots() {
    let mut page = leaf(1);
    assert_eq!(
        page.kind().expect("kind of a fresh leaf"),
        PageKind::BTreeLeaf
    );
    assert_eq!(leaf_len(&page), 0);
    assert_eq!(page.lower(), LEAF_DIRECTORY as u16);
    assert_eq!(leaf_prev(&page), None);
    assert_eq!(leaf_next(&page), None);

    for (key, payload) in [(b"a", b"1"), (b"c", b"3"), (b"b", b"2")] {
        assert_eq!(
            leaf_put(&mut page, key, payload).expect("put an entry"),
            Put::Inserted
        );
    }

    assert_eq!(leaf_len(&page), 3);
    assert_eq!(
        keys_of(&page),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        entries_of(&page),
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ],
        "the payload follows its key, so a slot that moved took its entry with it"
    );
    assert_eq!(
        leaf_entry(&page, 1).expect("the middle entry"),
        (&b"b"[..], &b"2"[..])
    );
    assert_eq!(
        page.lower(),
        (LEAF_DIRECTORY + 3 * SLOT_SIZE) as u16,
        "three slots of four bytes, after the header and the two links"
    );
    // The entry of `b` was written after that of `c`, so it sits below it in the page: the
    // order of the directory is not the order of the bytes.
    let (offset_b, _) = checked_slot(&page, LEAF_DIRECTORY, 1).expect("the slot of b");
    let (offset_c, _) = checked_slot(&page, LEAF_DIRECTORY, 2).expect("the slot of c");
    assert!(offset_b < offset_c, "{offset_b} < {offset_c}");
}

#[test]
fn duplicate_keys_keep_insertion_order() {
    let mut page = leaf(2);
    for payload in [b"first", b"secnd", b"third"] {
        leaf_put(&mut page, b"dup", payload).expect("put an entry");
    }
    leaf_put(&mut page, b"after", b"zzzzz").expect("put an entry");
    assert_eq!(
        entries_of(&page),
        vec![
            (b"after".to_vec(), b"zzzzz".to_vec()),
            (b"dup".to_vec(), b"first".to_vec()),
            (b"dup".to_vec(), b"secnd".to_vec()),
            (b"dup".to_vec(), b"third".to_vec()),
        ]
    );
}

#[test]
fn entry_too_large_is_bug() {
    let mut page = leaf(3);
    let oversized = vec![b'x'; MAX_ENTRY_BYTES + 1 - 8];
    let err = leaf_put(&mut page, &[b'k'; 8], &oversized)
        .expect_err("eight key bytes and that payload are one byte too many");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("8064"), "{err}");
    assert_eq!(MAX_ENTRY_BYTES, 8064);
    assert_eq!(leaf_len(&page), 0, "the page was left as it was");

    // One byte less goes on the empty page: the limit is the largest entry that fits, not
    // the smallest one that is refused.
    assert_eq!(
        leaf_put(&mut page, &[b'k'; 8], &oversized[..oversized.len() - 1])
            .expect("an entry of exactly MAX_ENTRY_BYTES bytes"),
        Put::Inserted
    );
    assert_eq!(leaf_len(&page), 1);
    assert_eq!(
        leaf_entry(&page, 0).expect("the entry reads back").1.len(),
        MAX_ENTRY_BYTES - 8
    );

    // The same limit guards an internal page, whose payload is the child pointer.
    let mut internal = Page::empty(PageKind::Free, PageId(4));
    init_internal(&mut internal, PageId(1));
    let err = internal_put(&mut internal, &vec![b'k'; MAX_ENTRY_BYTES - 7], PageId(2))
        .expect_err("that key and the eight bytes of the child are one byte too many");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert_eq!(internal_len(&internal), 0);
    assert_eq!(
        internal_put(&mut internal, &vec![b'k'; MAX_ENTRY_BYTES - 8], PageId(2))
            .expect("one byte less fits"),
        Put::Inserted
    );
}

#[test]
fn leaf_put_answers_full_without_changing_the_page() {
    let mut page = leaf(5);
    let mut count = 0;
    while leaf_put(&mut page, &key(count), &payload(count, 100)).expect("put an entry")
        == Put::Inserted
    {
        count += 1;
    }
    assert!(
        count > 1,
        "the page took {count} entries before it was full"
    );
    let before = entries_of(&page);
    assert_eq!(before.len(), count);
    assert_eq!(
        leaf_put(&mut page, &key(count), &payload(count, 100)).expect("the page is full"),
        Put::Full
    );
    assert_eq!(entries_of(&page), before, "a refused put changes nothing");
    assert!(
        free_space(&page, LEAF_DIRECTORY).expect("free space")
            < SLOT_SIZE + ENTRY_OVERHEAD + 5 + 100,
        "the page answered Full because the entry does not fit, not because of a counter"
    );
}

#[test]
fn split_leaf_separator() {
    let (_dir, storage) = instance("split-leaf");
    let (left, count) = filled_leaf(&storage);
    assert!(count >= 2, "{count} entries to share");
    let before = entries_of(&page_of(&storage, left));

    let split = split_now(&storage, left, split_leaf).expect("split the leaf");
    let left_page = page_of(&storage, left);
    let right_page = page_of(&storage, split.right);
    let left_entries = entries_of(&left_page);
    let right_entries = entries_of(&right_page);

    assert_ne!(split.right, left);
    assert_eq!(
        right_page.kind().expect("kind of the right page"),
        PageKind::BTreeLeaf
    );
    // Nothing lost, nothing duplicated, and the two halves are still in order.
    assert_eq!(
        [left_entries.clone(), right_entries.clone()].concat(),
        before
    );
    assert!(!left_entries.is_empty() && !right_entries.is_empty());
    assert!(left_entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert!(right_entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert!(
        left_entries.last().expect("the left half holds an entry").0
            < right_entries
                .first()
                .expect("the right half holds an entry")
                .0,
        "the halves are disjoint"
    );
    // ~50/50: neither half took less than a third of the entries.
    assert!(
        left_entries.len() * 3 >= count && right_entries.len() * 3 >= count,
        "{} and {} out of {count}",
        left_entries.len(),
        right_entries.len()
    );

    assert_eq!(
        split.separator, right_entries[0].0,
        "the separator is the first key of the right leaf"
    );
    assert!(
        right_entries.iter().any(|(key, _)| *key == split.separator),
        "a leaf split copies its separator, it does not move it"
    );
    assert_eq!(leaf_next(&left_page), Some(split.right));
    assert_eq!(leaf_prev(&right_page), Some(left));
    assert_eq!(leaf_prev(&left_page), None);
    assert_eq!(leaf_next(&right_page), None);
}

#[test]
fn split_leaf_relinks_the_old_next_sibling() {
    let (_dir, storage) = instance("split-relink");
    let (first, _) = filled_leaf(&storage);
    let far = split_now(&storage, first, split_leaf)
        .expect("a first split")
        .right;
    // `first` is half empty now; filling it again and splitting it puts a page between
    // `first` and `far`. The keys are above those already there, so the new entries land
    // on the right half.
    let pin = storage.pool.pin(first).expect("pin the left leaf");
    pin.with_page_mut(|page| {
        let mut count = 0;
        while leaf_put(page, &key(10_000 + count), &payload(count, 100)).expect("put an entry")
            == Put::Inserted
        {
            count += 1;
        }
    })
    .expect("the test holds the only pin");
    PageWrites::free()
        .mark(&storage, first)
        .expect("mark the leaf dirty");
    drop(pin);

    let middle = split_now(&storage, first, split_leaf)
        .expect("a second split")
        .right;
    assert_ne!(middle, far);
    assert_eq!(leaf_next(&page_of(&storage, first)), Some(middle));
    assert_eq!(leaf_prev(&page_of(&storage, middle)), Some(first));
    assert_eq!(leaf_next(&page_of(&storage, middle)), Some(far));
    assert_eq!(
        leaf_prev(&page_of(&storage, far)),
        Some(middle),
        "the leaf that was to the right points at the page put in front of it"
    );
}

#[test]
fn split_leaf_refuses_a_page_of_one_entry() {
    let (_dir, storage) = instance("split-too-thin");
    let id = allocate(&storage).expect("allocate a leaf");
    let pin = storage.pool.pin(id).expect("pin the leaf");
    pin.with_page_mut(|page| {
        init_leaf(page);
        leaf_put(page, b"only", b"one").expect("put the single entry");
    })
    .expect("the test holds the only pin");
    drop(pin);

    let err = split_now(&storage, id, split_leaf).expect_err("one entry cannot be shared");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("1 entries"), "{err}");
    assert_eq!(
        storage.control().expect("the control block").next_page_id,
        PageId(1),
        "the refusal came before the allocation"
    );
}

#[test]
fn internal_put_and_split() {
    let (_dir, storage) = instance("split-internal");
    let left = allocate(&storage).expect("allocate an internal page");
    let pin = storage.pool.pin(left).expect("pin the internal page");
    let keys = pin
        .with_page_mut(|page| {
            init_internal(page, PageId(1_000));
            let mut keys = 0;
            // The children are numbered 1001, 1002, … so that a pointer names its key.
            while internal_put(page, &key(keys + 1), PageId(1_001 + keys as u64))
                .expect("put a separator")
                == Put::Inserted
            {
                keys += 1;
            }
            keys
        })
        .expect("the test holds the only pin");
    PageWrites::free()
        .mark(&storage, left)
        .expect("mark the page dirty");
    drop(pin);

    let before = page_of(&storage, left);
    assert!(keys >= 3, "{keys} keys to share");
    assert_eq!(internal_len(&before), keys);
    assert_eq!(
        internal_children(&before).len(),
        keys + 1,
        "one pointer more than the page has keys"
    );
    assert_eq!(internal_child(&before, 0).expect("P0"), PageId(1_000));
    assert_eq!(internal_child(&before, 1).expect("P1"), PageId(1_001));
    let keys_before = internal_keys(&before);
    let children_before = internal_children(&before);

    let split = split_now(&storage, left, split_internal).expect("split the internal page");
    let left_page = page_of(&storage, left);
    let right_page = page_of(&storage, split.right);
    assert_eq!(
        right_page.kind().expect("kind of the right page"),
        PageKind::BTreeInternal
    );

    let left_keys = internal_keys(&left_page);
    let right_keys = internal_keys(&right_page);
    let left_children = internal_children(&left_page);
    let right_children = internal_children(&right_page);
    // Both pages are valid: one pointer more than they have keys, and keys in order.
    assert_eq!(left_children.len(), left_keys.len() + 1);
    assert_eq!(right_children.len(), right_keys.len() + 1);
    assert!(!left_keys.is_empty() && !right_keys.is_empty());
    assert!(left_keys.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(right_keys.windows(2).all(|pair| pair[0] < pair[1]));

    // The promoted key left both pages, and the pointers kept their order.
    assert_eq!(
        [
            left_keys.clone(),
            vec![split.separator.clone()],
            right_keys.clone()
        ]
        .concat(),
        keys_before
    );
    assert_eq!([left_children, right_children].concat(), children_before);
    assert!(
        left_keys.last().expect("a key on the left") < &split.separator,
        "the keys of the left page are below the separator"
    );
    assert!(
        right_keys.first().expect("a key on the right") > &split.separator,
        "the keys of the right page are above the separator that went up"
    );
    assert!(!right_keys.contains(&split.separator));
    assert!(!left_keys.contains(&split.separator));
}

#[test]
fn split_internal_refuses_a_page_of_two_keys() {
    let (_dir, storage) = instance("internal-too-thin");
    let id = allocate(&storage).expect("allocate an internal page");
    let pin = storage.pool.pin(id).expect("pin the internal page");
    pin.with_page_mut(|page| {
        init_internal(page, PageId(7));
        internal_put(page, b"m", PageId(8)).expect("first separator");
        internal_put(page, b"s", PageId(9)).expect("second separator");
    })
    .expect("the test holds the only pin");
    drop(pin);

    let err = split_now(&storage, id, split_internal).expect_err("two keys cannot be shared");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("2 keys"), "{err}");
}

#[test]
fn a_held_write_marks_its_pages_and_hands_them_back() {
    let (_dir, storage) = instance("btree-held-writes");
    let mut tree = int_tree(&storage);
    let root = tree.root();
    storage.pool.flush_all().expect("flush the fresh root");
    assert!(
        tree.take_held_pages().is_empty(),
        "the creation of the tree, left unlogged, held a page"
    );

    tree.hold_writes(Lsn(9));
    tree.insert(&[Value::I32(1)], &rid(1))
        .expect("insert one entry");
    assert_eq!(
        tree.take_held_pages(),
        BTreeSet::from([root]),
        "the leaf the entry landed on is the page handed back"
    );
    assert!(
        tree.take_held_pages().is_empty(),
        "a page was handed out twice"
    );
    assert_eq!(page_of(&storage, root).lsn(), Lsn(9));

    // The flag keeps the leaf in the pool: `flush_all` skips it and `data` still holds the
    // empty root written above.
    storage
        .pool
        .flush_all()
        .expect("flush while the leaf is held");
    assert_eq!(
        storage
            .data
            .read_page(root)
            .expect("read the root from data")
            .slot_count(),
        0
    );

    // The flag lifted, the write-ahead rule is what is left: LSN 9 is past the durable LSN
    // of a journal this test leaves empty.
    storage
        .pool
        .set_in_progress(root, false)
        .expect("lift the flag");
    let err = storage
        .pool
        .flush_all()
        .expect_err("the leaf carries an LSN the journal has not made durable");
    assert!(err.to_string().contains("LSN 9"), "{err}");
}

#[test]
fn a_write_outside_a_transaction_leaves_the_flag_of_a_running_one() {
    let (_dir, storage) = instance("btree-free-write-keeps-the-flag");
    let mut tree = int_tree(&storage);
    let root = tree.root();
    storage.pool.flush_all().expect("flush the fresh root");
    tree.hold_writes(Lsn(0));
    tree.insert(&[Value::I32(1)], &rid(1))
        .expect("insert under a transaction");
    // A second handle on the tree, outside a transaction, as a vacuum writes the leaf.
    let mut other = BTree::open(&storage, root, &ascending(), &int_types()).expect("open");
    other
        .insert(&[Value::I32(2)], &rid(2))
        .expect("insert outside a transaction");
    storage.pool.flush_all().expect("flush");
    assert_eq!(
        storage
            .data
            .read_page(root)
            .expect("read the root from data")
            .slot_count(),
        0,
        "the leaf of the running transaction kept its flag"
    );
    storage
        .pool
        .set_in_progress(root, false)
        .expect("the transaction ends");
    storage.pool.flush_all().expect("flush");
    assert_eq!(
        storage
            .data
            .read_page(root)
            .expect("read the root from data")
            .slot_count(),
        2
    );
}

#[test]
fn a_split_reaches_the_data_file_when_the_pool_is_flushed() {
    let dir = TempDir::created("split-durable");
    let (left, right, separator, left_keys, right_keys) = {
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        let (left, _) = filled_leaf(&storage);
        let split = split_now(&storage, left, split_leaf).expect("split the leaf");
        let left_keys = keys_of(&page_of(&storage, left));
        let right_keys = keys_of(&page_of(&storage, split.right));
        storage.pool.flush_all().expect("flush the pool");
        (left, split.right, split.separator, left_keys, right_keys)
    };

    let reopened =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen the instance");
    let left_page = reopened.data.read_page(left).expect("read the left leaf");
    let right_page = reopened.data.read_page(right).expect("read the right leaf");
    assert_eq!(keys_of(&left_page), left_keys);
    assert_eq!(keys_of(&right_page), right_keys);
    assert_eq!(right_keys[0], separator);
    assert_eq!(leaf_next(&left_page), Some(right));
    assert_eq!(leaf_prev(&right_page), Some(left));
    assert_eq!(left_page.lsn(), Lsn(0), "a split writes no journal record");
    assert_eq!(right_page.lsn(), Lsn(0));
}

#[test]
fn a_page_is_read_as_the_kind_it_carries() {
    let mut page = leaf(11);
    let err = internal_key(&page, 0).expect_err("a leaf is not an internal page");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("BTreeLeaf"), "{err}");

    init_internal(&mut page, PageId(12));
    let err = leaf_put(&mut page, b"k", b"v").expect_err("an internal page is not a leaf");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("BTreeInternal"), "{err}");
    assert_eq!(leftmost_child(&page), PageId(12));
    assert_eq!(internal_len(&page), 0);
}

#[test]
fn a_slot_pointing_outside_the_page_is_corruption() {
    let mut page = leaf(13);
    leaf_put(&mut page, b"k", b"v").expect("put an entry");
    // The slot is moved onto the directory itself, where no entry sits.
    write_u16(&mut page, LEAF_DIRECTORY, LEAF_DIRECTORY).expect("write the slot");
    let err = leaf_entry(&page, 0).expect_err("the slot is inside the directory");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");

    let mut page = leaf(14);
    leaf_put(&mut page, b"k", b"v").expect("put an entry");
    let (offset, _) = checked_slot(&page, LEAF_DIRECTORY, 0).expect("the slot");
    // The declared key length now runs past the end of the entry.
    write_u16(&mut page, offset, 500).expect("write the key length");
    let err = leaf_entry(&page, 0).expect_err("the key does not fit its slot");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
    assert!(err.to_string().contains("500"), "{err}");

    // A slot past the directory is a Bug, not a corruption of the page.
    let err = leaf_entry(&page, 1).expect_err("the page holds one entry");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
}

/// Entries built from their keys, each with a payload naming its rank.
fn entries_from(keys: &[&str]) -> Vec<Entry> {
    keys.iter()
        .enumerate()
        .map(|(rank, key)| (key.as_bytes().to_vec(), format!("p{rank}").into_bytes()))
        .collect()
}

/// A leaf of the instance filled with `runs` entries of one key at a time; answers its
/// identifier and the number of entries it took.
fn leaf_of_runs(storage: &DiskStorage, runs: usize, width: usize) -> (PageId, usize) {
    let id = allocate(storage).expect("allocate a leaf");
    let pin = storage.pool.pin(id).expect("pin the leaf");
    let count = pin
        .with_page_mut(|page| {
            init_leaf(page);
            let mut count = 0;
            while leaf_put(page, &key(count / runs), &payload(count, width))
                .expect("put an entry in the leaf")
                == Put::Inserted
            {
                count += 1;
            }
            count
        })
        .expect("the test holds the only pin");
    PageWrites::free()
        .mark(storage, id)
        .expect("mark the leaf dirty");
    drop(pin);
    (id, count)
}

/// A small deterministic generator, so that the drawn pages are the same at each run.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) % bound as u64) as usize
    }
}

#[test]
fn a_cut_lands_between_two_different_keys() {
    // The cut of a leaf moves to the nearest index where the key changes.
    let run_then_one = entries_from(&["a", "a", "a", "b"]);
    assert_eq!(cut_between_keys(&run_then_one, 2), Some(3));
    let one_then_run = entries_from(&["a", "b", "b", "b"]);
    assert_eq!(cut_between_keys(&one_then_run, 2), Some(1));
    // A cut already between two different keys stays where the byte sizes put it.
    let two_runs = entries_from(&["a", "a", "b", "b"]);
    assert_eq!(cut_between_keys(&two_runs, 2), Some(2));
    // Two candidates at the same distance: the lower index wins.
    let tie = entries_from(&["a", "b", "b", "c"]);
    assert_eq!(cut_between_keys(&tie, 2), Some(1));
    // One key on the page: no cut, and split_leaf turns that into a Bug.
    assert_eq!(
        cut_between_keys(&entries_from(&["a", "a", "a", "a"]), 2),
        None
    );

    // The promoted key of an internal page differs from both of its neighbours.
    assert_eq!(
        key_to_promote(&entries_from(&["a", "b", "c", "d"]), 2),
        Some(2)
    );
    assert_eq!(
        key_to_promote(&entries_from(&["a", "a", "b", "c", "c"]), 2),
        Some(2)
    );
    assert_eq!(
        key_to_promote(&entries_from(&["a", "b", "b", "c", "d"]), 2),
        Some(3)
    );
    // Runs on both sides of the cut: no key of the page can go up alone.
    assert_eq!(key_to_promote(&two_runs, 2), None);
}

#[test]
fn split_leaf_keeps_a_run_of_equal_keys_on_one_side() {
    let (_dir, storage) = instance("split-run");
    let (left, count) = leaf_of_runs(&storage, 8, 100);
    let before = entries_of(&page_of(&storage, left));
    assert_eq!(before.len(), count);
    let balanced = balance_point(&entry_weights(&before));
    assert_eq!(
        before[balanced - 1].0,
        before[balanced].0,
        "the cut the byte sizes pick falls inside a run of equal keys"
    );

    let split = split_now(&storage, left, split_leaf).expect("split the leaf");
    let left_entries = entries_of(&page_of(&storage, left));
    let right_entries = entries_of(&page_of(&storage, split.right));
    assert_eq!(
        [left_entries.clone(), right_entries.clone()].concat(),
        before,
        "nothing lost, nothing duplicated"
    );
    assert_eq!(split.separator, right_entries[0].0);
    assert!(
        left_entries.last().expect("the left leaf holds an entry").0 < split.separator,
        "the highest key of the left leaf is below the separator, not equal to it"
    );
    assert_eq!(
        left_entries
            .iter()
            .filter(|(key, _)| *key == split.separator)
            .count(),
        0,
        "the run of the separator went to the right leaf as a block"
    );
    assert_eq!(
        right_entries
            .iter()
            .filter(|(key, _)| *key == split.separator)
            .count(),
        8,
        "the eight entries of that key are still together"
    );
    // 72 entries, runs of 8: the cut moved from 36 to the run boundary at 32.
    assert_eq!(
        (count, left_entries.len(), right_entries.len()),
        (72, 32, 40)
    );
}

#[test]
fn split_leaf_refuses_a_run_it_cannot_cut() {
    let (_dir, storage) = instance("split-one-key");
    let (left, count) = leaf_of_runs(&storage, usize::MAX, 100);
    let before = entries_of(&page_of(&storage, left));
    assert_eq!(before.len(), count);
    assert!(before.windows(2).all(|pair| pair[0].0 == pair[1].0));

    let err = split_now(&storage, left, split_leaf).expect_err("a page of one key has no cut");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("carry one key"), "{err}");
    assert_eq!(
        entries_of(&page_of(&storage, left)),
        before,
        "the refused split left the leaf as it was"
    );
    assert_eq!(
        storage.control().expect("the control block").next_page_id,
        PageId(1),
        "the refusal came before the allocation, so no page was orphaned"
    );
}

#[test]
fn split_internal_promotes_a_key_out_of_a_run() {
    let (_dir, storage) = instance("internal-run");
    let id = allocate(&storage).expect("allocate an internal page");
    let pin = storage.pool.pin(id).expect("pin the internal page");
    pin.with_page_mut(|page| {
        init_internal(page, PageId(100));
        // A run of four `m` around the middle, one key on the left, two on the right.
        for (rank, key) in ["a", "m", "m", "m", "m", "s", "t"].iter().enumerate() {
            assert_eq!(
                internal_put(page, key.as_bytes(), PageId(101 + rank as u64))
                    .expect("put a separator"),
                Put::Inserted
            );
        }
    })
    .expect("the test holds the only pin");
    PageWrites::free()
        .mark(&storage, id)
        .expect("mark the page dirty");
    drop(pin);

    let before = page_of(&storage, id);
    let keys_before = internal_keys(&before);
    let children_before = internal_children(&before);
    let balanced = balance_point(&entry_weights(
        &owned_entries(&before, INTERNAL_DIRECTORY).expect("the entries read back"),
    ));
    assert_eq!(
        keys_before[balanced],
        b"m".to_vec(),
        "the middle is in the run"
    );

    let split = split_now(&storage, id, split_internal).expect("split the internal page");
    let left_page = page_of(&storage, id);
    let right_page = page_of(&storage, split.right);
    let left_keys = internal_keys(&left_page);
    let right_keys = internal_keys(&right_page);
    assert_eq!(split.separator, b"s".to_vec());
    assert_eq!(
        left_keys,
        [
            b"a".to_vec(),
            b"m".to_vec(),
            b"m".to_vec(),
            b"m".to_vec(),
            b"m".to_vec()
        ]
    );
    assert_eq!(right_keys, [b"t".to_vec()]);
    assert_eq!(
        [left_keys, vec![split.separator.clone()], right_keys].concat(),
        keys_before
    );
    assert_eq!(
        [
            internal_children(&left_page),
            internal_children(&right_page)
        ]
        .concat(),
        children_before
    );
    assert_eq!(
        internal_len(&left_page) + 1,
        internal_children(&left_page).len()
    );
    assert_eq!(
        internal_len(&right_page) + 1,
        internal_children(&right_page).len()
    );
}

#[test]
fn split_internal_refuses_a_page_it_cannot_promote_out_of() {
    let (_dir, storage) = instance("internal-runs");
    let id = allocate(&storage).expect("allocate an internal page");
    let pin = storage.pool.pin(id).expect("pin the internal page");
    pin.with_page_mut(|page| {
        init_internal(page, PageId(200));
        for (rank, key) in ["a", "a", "b", "b"].iter().enumerate() {
            internal_put(page, key.as_bytes(), PageId(201 + rank as u64)).expect("put a separator");
        }
    })
    .expect("the test holds the only pin");
    PageWrites::free()
        .mark(&storage, id)
        .expect("mark the page dirty");
    drop(pin);

    let err = split_now(&storage, id, split_internal).expect_err("each key equals a neighbour");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(err.to_string().contains("4 keys"), "{err}");
    assert_eq!(internal_keys(&page_of(&storage, id)).len(), 4);
}

#[test]
fn equal_keys_stay_on_one_side_of_a_split() {
    let (_dir, storage) = instance("split-drawn");
    let mut rng = Lcg(0x5eed);
    let (mut straddled, mut refused) = (0, 0);
    for round in 0..200 {
        let id = allocate(&storage).expect("allocate a leaf");
        let pin = storage.pool.pin(id).expect("pin the leaf");
        let put = pin
            .with_page_mut(|page| {
                init_leaf(page);
                let mut put: Vec<Entry> = Vec::new();
                loop {
                    // Keys of at most three letters out of four: runs are frequent.
                    let key: Vec<u8> = (0..rng.below(4))
                        .map(|_| b'a' + rng.below(4) as u8)
                        .collect();
                    let payload = vec![(put.len() % 251) as u8; rng.below(300)];
                    if leaf_put(page, &key, &payload).expect("put an entry") != Put::Inserted {
                        break;
                    }
                    put.push((key, payload));
                }
                put
            })
            .expect("the test holds the only pin");
        PageWrites::free()
            .mark(&storage, id)
            .expect("mark the leaf dirty");
        drop(pin);

        let before = entries_of(&page_of(&storage, id));
        let mut sorted = put.clone();
        sorted.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            before, sorted,
            "round {round}: the directory is in key order"
        );
        if before.len() < 2 {
            continue;
        }
        let balanced = balance_point(&entry_weights(&before));
        if before[balanced - 1].0 == before[balanced].0 {
            straddled += 1;
        }
        match split_now(&storage, id, split_leaf) {
            Ok(split) => {
                let left = entries_of(&page_of(&storage, id));
                let right = entries_of(&page_of(&storage, split.right));
                assert_eq!(
                    [left.clone(), right.clone()].concat(),
                    before,
                    "round {round}"
                );
                assert_eq!(split.separator, right[0].0, "round {round}");
                assert!(
                    left.last().expect("an entry on the left").0 < split.separator,
                    "round {round}: a key of the left leaf reaches the separator"
                );
            }
            Err(err) => {
                refused += 1;
                assert!(
                    matches!(err, InternalError::Bug(_)),
                    "round {round}: {err:?}"
                );
                assert!(
                    before.windows(2).all(|pair| pair[0].0 == pair[1].0),
                    "round {round}: a split was refused on a page of several keys"
                );
            }
        }
    }
    // The axis is exercised: the byte-balanced cut fell inside a run in these rounds.
    assert!(
        straddled >= 10,
        "{straddled} rounds out of 200 straddle a run"
    );
    assert_eq!(refused, 0, "{refused} pages of a single key were drawn");
}

#[test]
fn a_slot_count_past_the_page_is_corruption() {
    let mut page = leaf(15);
    leaf_put(&mut page, b"k", b"v").expect("put an entry");
    // 48 + 4 × 4 000 = 16 048 bytes of directory asked for in a page of 8 192.
    page.set_slot_count(4_000);
    for err in [
        leaf_entry(&page, 3_000).expect_err("the directory runs past the page"),
        leaf_iter(&page)
            .err()
            .expect("the iterator reads the directory"),
        leaf_put(&mut page.clone(), b"z", b"v").expect_err("a put reads the free space"),
    ] {
        assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
        assert!(err.to_string().contains("4000"), "{err}");
    }

    // The largest directory a leaf holds: (8 192 − 48) / 4 = 2 036 slots, ending on the
    // last byte of the page.
    page.set_slot_count(2_036);
    assert_eq!(
        directory_end(&page, LEAF_DIRECTORY).expect("2 036 slots fit"),
        PAGE_SIZE
    );
    page.set_slot_count(2_037);
    let err = directory_end(&page, LEAF_DIRECTORY).expect_err("one slot too many");
    assert!(matches!(err, InternalError::Corruption(_)), "{err:?}");
}

#[test]
fn balance_point_keeps_both_halves() {
    assert_eq!(balance_point(&[1, 1]), 1);
    assert_eq!(balance_point(&[1, 1, 1]), 2);
    assert_eq!(balance_point(&[1, 1, 1, 1]), 2);
    // A first entry heavier than the others still stays alone on the left.
    assert_eq!(balance_point(&[100, 1, 1]), 1);
    // A last entry that carries the weight leaves the ones before it on the left.
    assert_eq!(balance_point(&[1, 1, 100]), 2);
}

// ---------------------------------------------------------------------------------------
// The tree over those pages
// ---------------------------------------------------------------------------------------

/// The columns of a table of one nullable `int` column.
fn int_types() -> Vec<TypeInfo> {
    vec![TypeInfo::new(SqlType::Int, true)]
}

/// An ascending key on the column 0.
fn ascending() -> Vec<KeyColumn> {
    vec![KeyColumn {
        column: 0,
        descending: false,
    }]
}

/// An empty tree on one ascending `int` column of the instance.
fn int_tree(storage: &DiskStorage) -> BTree<'_> {
    BTree::create(storage, &ascending(), &int_types()).expect("create a tree")
}

/// The payload of these tests: the eight bytes of a [`crate::RowId`], big-endian so that
/// their byte order is the order of the numbers.
fn rid(number: u64) -> Vec<u8> {
    number.to_be_bytes().to_vec()
}

/// The `int` of each entry, in the order the seek answered them.
fn ints(entries: &[TreeEntry]) -> Vec<i32> {
    entries
        .iter()
        .map(|(values, _)| match values.as_slice() {
            [Value::I32(number)] => *number,
            other => panic!("an entry of the tree carries {other:?}"),
        })
        .collect()
}

/// The payloads of the entries, in the order the seek answered them.
fn payloads(entries: &[TreeEntry]) -> Vec<Vec<u8>> {
    entries.iter().map(|(_, payload)| payload.clone()).collect()
}

/// The whole tree, in key order.
fn all(tree: &BTree<'_>) -> Vec<TreeEntry> {
    tree.seek(&KeyRange::Full, Direction::Forward)
        .expect("seek the whole tree")
}

/// The kind of the page `id` of the instance.
fn kind_of(storage: &DiskStorage, id: PageId) -> PageKind {
    page_of(storage, id)
        .kind()
        .expect("read the kind of the page")
}

#[test]
fn insert_seek_point_int() {
    let (_dir, storage) = instance("btree-point");
    let mut tree = int_tree(&storage);
    for (number, row) in [(1, 10u64), (3, 30), (2, 20)] {
        tree.insert(&[Value::I32(number)], &rid(row))
            .expect("insert an entry");
    }

    let found = tree
        .seek(&KeyRange::Point(vec![Value::I32(2)]), Direction::Forward)
        .expect("seek the point 2");
    assert_eq!(ints(&found), vec![2]);
    assert_eq!(payloads(&found), vec![rid(20)]);
    assert_eq!(ints(&all(&tree)), vec![1, 2, 3]);
}

#[test]
fn seek_between_and_full_both_directions() {
    let (_dir, storage) = instance("btree-between");
    let mut tree = int_tree(&storage);
    for number in [7, 2, 9, 4, 1, 10, 6, 3, 8, 5] {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert an entry");
    }
    let seek = |range: KeyRange, dir| tree.seek(&range, dir).expect("seek the range");

    assert_eq!(
        ints(&seek(KeyRange::Full, Direction::Forward)),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    );
    assert_eq!(
        ints(&seek(KeyRange::Full, Direction::Backward)),
        vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1]
    );

    let closed = KeyRange::Between(
        Bound::Included(vec![Value::I32(3)]),
        Bound::Included(vec![Value::I32(7)]),
    );
    assert_eq!(
        ints(&seek(closed.clone(), Direction::Forward)),
        vec![3, 4, 5, 6, 7]
    );
    assert_eq!(
        ints(&seek(closed, Direction::Backward)),
        vec![7, 6, 5, 4, 3]
    );

    let open = KeyRange::Between(
        Bound::Excluded(vec![Value::I32(3)]),
        Bound::Excluded(vec![Value::I32(7)]),
    );
    assert_eq!(ints(&seek(open.clone(), Direction::Forward)), vec![4, 5, 6]);
    assert_eq!(ints(&seek(open, Direction::Backward)), vec![6, 5, 4]);

    let from = KeyRange::Between(Bound::Included(vec![Value::I32(8)]), Bound::Unbounded);
    assert_eq!(
        ints(&seek(from.clone(), Direction::Forward)),
        vec![8, 9, 10]
    );
    assert_eq!(ints(&seek(from, Direction::Backward)), vec![10, 9, 8]);

    let until = KeyRange::Between(Bound::Unbounded, Bound::Excluded(vec![Value::I32(4)]));
    assert_eq!(
        ints(&seek(until.clone(), Direction::Forward)),
        vec![1, 2, 3]
    );
    assert_eq!(ints(&seek(until, Direction::Backward)), vec![3, 2, 1]);

    let above = KeyRange::Between(Bound::Included(vec![Value::I32(11)]), Bound::Unbounded);
    assert!(seek(above.clone(), Direction::Forward).is_empty());
    assert!(seek(above, Direction::Backward).is_empty());
}

#[test]
fn split_grows_root() {
    let (_dir, storage) = instance("btree-grow");
    let mut tree = int_tree(&storage);
    let leaf_root = tree.root();
    assert_eq!(kind_of(&storage, leaf_root), PageKind::BTreeLeaf);

    for number in 0..300 {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert an entry");
    }

    assert_ne!(tree.root(), leaf_root, "the root moved");
    assert_eq!(kind_of(&storage, tree.root()), PageKind::BTreeInternal);
    assert_eq!(ints(&all(&tree)), (0..300).collect::<Vec<_>>());
    assert_eq!(
        ints(
            &tree
                .seek(&KeyRange::Full, Direction::Backward)
                .expect("seek backwards")
        ),
        (0..300).rev().collect::<Vec<_>>()
    );
}

#[test]
fn a_third_level_appears_when_the_root_splits() {
    let (_dir, storage) = instance("btree-third-level");
    let mut tree = int_tree(&storage);
    let payload = vec![b'.'; 200];
    for number in 0..2_000 {
        tree.insert(&[Value::I32(number)], &payload)
            .expect("insert an entry");
    }

    let root = page_of(&storage, tree.root());
    assert_eq!(
        root.kind().expect("the kind of the root"),
        PageKind::BTreeInternal
    );
    let child = internal_child(&root, 0).expect("the leftmost child of the root");
    assert_eq!(
        kind_of(&storage, child),
        PageKind::BTreeInternal,
        "the children of the root are internal pages, so the tree has three levels"
    );
    assert_eq!(ints(&all(&tree)), (0..2_000).collect::<Vec<_>>());
}

#[test]
fn a_replace_is_one_record() {
    use super::super::wal::{TreeRole, WalRecordKind};
    let (_dir, storage) = instance("btree-replace");
    let page_records = |storage: &DiskStorage| {
        storage
            .wal
            .records()
            .expect("read the journal")
            .iter()
            .filter(|record| record.kind == WalRecordKind::PageWrite)
            .count()
    };
    let mut tree = BTree::create_logged(
        &storage,
        &ascending(),
        &int_types(),
        (crate::TableId(1), TreeRole::Versions),
    )
    .expect("create a journalled tree");
    for number in 0..40 {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert");
    }
    let before = page_records(&storage);
    assert!(
        tree.replace(&[Value::I32(7)], &rid(7), &rid(700))
            .expect("replace")
    );
    assert_eq!(page_records(&storage), before + 1, "one record for the two");
    let found = tree
        .seek(&KeyRange::Point(vec![Value::I32(7)]), Direction::Forward)
        .expect("seek");
    assert_eq!(found, vec![(vec![Value::I32(7)], rid(700))]);
    // An entry the tree does not hold is left alone, and nothing is journalled.
    assert!(
        !tree
            .replace(&[Value::I32(7)], &rid(7), &rid(9))
            .expect("replace")
    );
    assert_eq!(page_records(&storage), before + 1);
}

#[test]
fn delete_then_seek_absent() {
    let (_dir, storage) = instance("btree-delete");
    let mut tree = int_tree(&storage);
    for number in 1..=3 {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert an entry");
    }

    assert!(
        tree.delete(&[Value::I32(2)], &rid(2))
            .expect("delete the entry 2")
    );
    assert!(
        tree.seek(&KeyRange::Point(vec![Value::I32(2)]), Direction::Forward)
            .expect("seek the point 2")
            .is_empty()
    );
    assert_eq!(ints(&all(&tree)), vec![1, 3]);

    assert!(
        !tree
            .delete(&[Value::I32(2)], &rid(2))
            .expect("delete the entry 2 again"),
        "the second delete finds nothing"
    );
    assert!(
        !tree
            .delete(&[Value::I32(9)], &rid(9))
            .expect("delete an entry that was never there")
    );
    assert!(
        !tree
            .delete(&[Value::I32(1)], &rid(7))
            .expect("delete the key 1 under another payload"),
        "the payload is part of what is deleted"
    );
    assert_eq!(ints(&all(&tree)), vec![1, 3]);
}

#[test]
fn nulls_first_on_int_column() {
    let (_dir, storage) = instance("btree-nulls");
    let mut tree = int_tree(&storage);
    tree.insert(&[Value::I32(1)], &rid(1)).expect("insert 1");
    tree.insert(&[Value::Null], &rid(0)).expect("insert NULL");

    let found = all(&tree);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].0, vec![Value::Null], "NULL comes first");
    assert_eq!(found[1].0, vec![Value::I32(1)]);
    assert_eq!(
        payloads(
            &tree
                .seek(&KeyRange::Point(vec![Value::Null]), Direction::Forward)
                .expect("seek the NULL")
        ),
        vec![rid(0)],
        "NULL is equal to NULL"
    );
}

#[test]
fn descending_column_reverses_the_order_nulls_included() {
    let (_dir, storage) = instance("btree-descending");
    let columns = vec![KeyColumn {
        column: 0,
        descending: true,
    }];
    let mut tree =
        BTree::create(&storage, &columns, &int_types()).expect("create a descending tree");
    for number in [2, 1, 3] {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert an entry");
    }
    tree.insert(&[Value::Null], &rid(0)).expect("insert NULL");

    let found = all(&tree);
    assert_eq!(
        found
            .iter()
            .map(|(values, _)| values[0].clone())
            .collect::<Vec<_>>(),
        vec![Value::I32(3), Value::I32(2), Value::I32(1), Value::Null],
        "the descending column reverses the values and puts NULL last"
    );
}

#[test]
fn negative_ints_sort_below_zero_as_compare_orders_them() {
    let (_dir, storage) = instance("btree-negative");
    let mut tree = int_tree(&storage);
    for number in [1, -1, 0] {
        tree.insert(&[Value::I32(number)], &rid(0))
            .expect("insert an entry");
    }
    assert_eq!(ints(&all(&tree)), vec![-1, 0, 1]);

    // The distinguishing vector: read as bytes, the encoded key of -1 sits above that of 1
    // (the encoder is little-endian and untouched by the sign), so a tree that
    // ordered its entries by their bytes would answer 0, 1, -1 above.
    assert!(
        encode_entry_key(&[Value::I32(-1)], &rid(0)) > encode_entry_key(&[Value::I32(1)], &rid(0))
    );
}

#[test]
fn a_page_full_of_one_key_splits_because_the_payloads_differ() {
    let (_dir, storage) = instance("btree-one-key");
    let mut tree = int_tree(&storage);
    for row in 0..400u64 {
        tree.insert(&[Value::I32(7)], &rid(row))
            .expect("insert an entry of the key 7");
    }

    assert_eq!(
        kind_of(&storage, tree.root()),
        PageKind::BTreeInternal,
        "400 entries of one key filled the leaf root and split it"
    );
    let found = tree
        .seek(&KeyRange::Point(vec![Value::I32(7)]), Direction::Forward)
        .expect("seek the point 7");
    assert_eq!(found.len(), 400);
    assert_eq!(ints(&found), vec![7; 400]);
    assert_eq!(
        payloads(&found),
        (0..400u64).map(rid).collect::<Vec<_>>(),
        "the entries of one key come back in payload order"
    );
}

#[test]
fn entries_of_uneven_size_read_back_in_order() {
    let (_dir, storage) = instance("btree-uneven");
    let mut tree = int_tree(&storage);
    // Payloads of 8 to 1 600 bytes, 200 sizes taken in turn: the cut of a split follows the
    // byte sizes (`balance_point`), so it lands away from the middle of the entries.
    let payload = |number: i32| vec![b'x'; 8 + (number % 200) as usize * 8];
    for step in 0..1_200i32 {
        let number = (step * 7) % 1_200;
        tree.insert(&[Value::I32(number)], &payload(number))
            .expect("insert an entry");
    }

    let found = all(&tree);
    assert_eq!(found.len(), 1_200);
    assert_eq!(ints(&found), (0..1_200).collect::<Vec<_>>());
    assert_eq!(
        found
            .iter()
            .map(|(_, bytes)| bytes.len())
            .collect::<Vec<_>>(),
        (0..1_200)
            .map(|number| payload(number).len())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ints(
            &tree
                .seek(&KeyRange::Full, Direction::Backward)
                .expect("seek backwards")
        ),
        (0..1_200).rev().collect::<Vec<_>>()
    );
}

#[test]
fn delete_takes_one_occurrence_of_a_duplicated_key() {
    let (_dir, storage) = instance("btree-duplicates");
    let mut tree = int_tree(&storage);
    for row in [1u64, 2, 3] {
        tree.insert(&[Value::I32(5)], &rid(row))
            .expect("insert an entry of the key 5");
    }

    assert!(
        tree.delete(&[Value::I32(5)], &rid(2))
            .expect("delete one of the three")
    );
    let found = tree
        .seek(&KeyRange::Point(vec![Value::I32(5)]), Direction::Forward)
        .expect("seek the point 5");
    assert_eq!(payloads(&found), vec![rid(1), rid(3)]);
    assert!(
        !tree
            .delete(&[Value::I32(5)], &rid(2))
            .expect("delete the same one again")
    );
}

#[test]
fn an_emptied_leaf_stays_in_the_chain_and_a_seek_steps_over_it() {
    let (_dir, storage) = instance("btree-emptied");
    let mut tree = int_tree(&storage);
    for number in 0..300 {
        tree.insert(&[Value::I32(number)], &rid(number as u64))
            .expect("insert an entry");
    }

    let first = tree.edge_leaf(true).expect("the leftmost leaf");
    let held = leaf_len(&page_of(&storage, first));
    assert!(
        held > 0 && held < 300,
        "the leftmost leaf holds {held} of the 300 entries"
    );
    for number in 0..held as i32 {
        assert!(
            tree.delete(&[Value::I32(number)], &rid(number as u64))
                .expect("delete an entry of the leftmost leaf")
        );
    }

    assert_eq!(leaf_len(&page_of(&storage, first)), 0, "the leaf is empty");
    assert_eq!(
        tree.edge_leaf(true).expect("the leftmost leaf again"),
        first,
        "the empty leaf is still the leftmost one: no merge, no page given back"
    );
    assert_eq!(
        ints(&all(&tree)),
        (held as i32..300).collect::<Vec<_>>(),
        "a forward seek steps over the empty leaf"
    );
    assert_eq!(
        ints(
            &tree
                .seek(&KeyRange::Full, Direction::Backward)
                .expect("seek backwards")
        ),
        (held as i32..300).rev().collect::<Vec<_>>()
    );
}

#[test]
fn a_two_column_key_seeks_by_prefix() {
    let (_dir, storage) = instance("btree-prefix");
    let types = vec![
        TypeInfo::new(SqlType::Int, true),
        TypeInfo::new(SqlType::Int, true),
    ];
    let columns = vec![
        KeyColumn {
            column: 0,
            descending: false,
        },
        KeyColumn {
            column: 1,
            descending: false,
        },
    ];
    let mut tree = BTree::create(&storage, &columns, &types).expect("create a tree");
    for (left, right) in [(1, 1), (1, 2), (2, 1)] {
        tree.insert(&[Value::I32(left), Value::I32(right)], &rid(0))
            .expect("insert an entry");
    }

    let prefix = tree
        .seek(&KeyRange::Point(vec![Value::I32(1)]), Direction::Forward)
        .expect("seek the prefix 1");
    assert_eq!(
        prefix
            .iter()
            .map(|(values, _)| values.clone())
            .collect::<Vec<_>>(),
        vec![
            vec![Value::I32(1), Value::I32(1)],
            vec![Value::I32(1), Value::I32(2)]
        ]
    );
    let whole = tree
        .seek(
            &KeyRange::Point(vec![Value::I32(1), Value::I32(2)]),
            Direction::Forward,
        )
        .expect("seek the whole key");
    assert_eq!(whole.len(), 1);
    let err = tree
        .seek(
            &KeyRange::Point(vec![Value::I32(1), Value::I32(2), Value::I32(3)]),
            Direction::Forward,
        )
        .expect_err("a bound of three values on a key of two columns");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "internal bug: a key bound carries 3 values, the key has 2 columns"
    );
}

#[test]
fn a_seek_answers_a_vec_a_later_insert_does_not_change() {
    let (_dir, storage) = instance("btree-isolation");
    let mut tree = int_tree(&storage);
    for number in 0..10 {
        tree.insert(&[Value::I32(number)], &rid(0))
            .expect("insert an entry");
    }

    let taken = all(&tree);
    tree.insert(&[Value::I32(10)], &rid(0))
        .expect("insert one more");
    assert_eq!(ints(&taken), (0..10).collect::<Vec<_>>());
    assert_eq!(all(&tree).len(), 11);
}

#[test]
fn seek_of_an_empty_tree_answers_nothing() {
    let (_dir, storage) = instance("btree-empty");
    let mut tree = int_tree(&storage);
    for dir in [Direction::Forward, Direction::Backward] {
        assert!(
            tree.seek(&KeyRange::Full, dir)
                .expect("seek an empty tree")
                .is_empty()
        );
        assert!(
            tree.seek(&KeyRange::Point(vec![Value::I32(1)]), dir)
                .expect("seek a point of an empty tree")
                .is_empty()
        );
    }
    assert!(
        !tree
            .delete(&[Value::I32(1)], &rid(0))
            .expect("delete from an empty tree")
    );
}

#[test]
fn a_tree_reopened_by_its_root_reads_its_entries_back() {
    let dir = TempDir::created("btree-reopen");
    let root = {
        let storage =
            DiskStorage::open(dir.path(), DiskOptions::default()).expect("create an instance");
        let mut tree = int_tree(&storage);
        for number in 0..300 {
            tree.insert(&[Value::I32(number)], &rid(number as u64))
                .expect("insert an entry");
        }
        storage.pool.flush_all().expect("flush the pool");
        tree.root()
    };

    let storage =
        DiskStorage::open(dir.path(), DiskOptions::default()).expect("reopen the instance");
    let tree = BTree::open(&storage, root, &ascending(), &int_types()).expect("reopen the tree");
    assert_eq!(tree.root(), root);
    assert_eq!(ints(&all(&tree)), (0..300).collect::<Vec<_>>());
}

#[test]
fn a_leaf_that_lost_slots_takes_entries_again_after_a_compaction() {
    let order = KeyOrder::new(&ascending(), &int_types()).expect("build the order");
    let probe = |number: i32| Probe {
        values: vec![Value::I32(number)],
        payload: PayloadBound::At(rid(number as u64)),
    };
    let entry = |number: i32| encode_entry_key(&[Value::I32(number)], &rid(number as u64));

    let mut page = leaf(1);
    let mut count = 0i32;
    while place_in_leaf(&mut page, &order, &probe(count), &entry(count))
        .expect("put an entry in the leaf")
        == Put::Inserted
    {
        count += 1;
    }
    assert_eq!(count, 262, "a leaf takes 262 entries of this shape");

    // The slots of the 100 lowest keys go. They were written first, at the highest offsets,
    // so the free span between the directory and the entries grows by the four bytes of each
    // freed slot, which is what `taken_raw` reads below; the bytes of the entries
    // themselves come back at the compaction.
    let removed = 100;
    for _ in 0..removed {
        remove_slot(&mut page, LEAF_DIRECTORY, 0).expect("take a slot out");
    }

    let mut raw = page.clone();
    let mut taken_raw = 0i32;
    loop {
        let index = leaf_len(&raw);
        let bytes = entry(count + taken_raw);
        if put_entry(&mut raw, LEAF_DIRECTORY, index, &bytes, &[])
            .expect("a put without compaction")
            == Put::Full
        {
            break;
        }
        taken_raw += 1;
    }

    let mut taken = 0i32;
    while place_in_leaf(
        &mut page,
        &order,
        &probe(count + taken),
        &entry(count + taken),
    )
    .expect("a put that compacts when it finds no room")
        == Put::Inserted
    {
        taken += 1;
    }

    assert_eq!(taken, removed, "the compaction hands the 100 entries back");
    assert_eq!(
        taken_raw, 13,
        "without the compaction, only the four bytes of each freed slot are reused"
    );
    assert_eq!(leaf_len(&page), (count - removed + taken) as usize);
}

/// A pseudo-random source with a seed, so that the sizes a test draws are the same at each
/// run.
struct Draw(u64);

impl Draw {
    /// The next draw.
    fn step(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    /// A draw in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.step() % n as u64) as usize
    }
}

#[test]
fn the_ceiling_of_an_entry_is_a_quarter_of_a_leaf() {
    assert_eq!(MAX_INSERT_BYTES, 2_028);
    assert_eq!(PAGE_SIZE - LEAF_DIRECTORY, 8_144);
    let entry = SLOT_SIZE + ENTRY_OVERHEAD + MAX_INSERT_BYTES;
    assert_eq!(
        entry, 2_036,
        "a leaf entry at the ceiling weighs 2 036 bytes"
    );
    assert_eq!(
        4 * entry,
        PAGE_SIZE - LEAF_DIRECTORY,
        "four of them are exactly what a leaf holds"
    );

    // A separator carries the same key plus the eight bytes of a child; an internal page
    // holds three of those, the number `split_internal` asks for.
    let separator = entry + CHILD_BYTES;
    assert_eq!(separator, 2_044);
    assert!(
        3 * separator <= PAGE_SIZE - INTERNAL_DIRECTORY,
        "three separators of {separator} bytes hold in the {} an internal page leaves",
        PAGE_SIZE - INTERNAL_DIRECTORY
    );
    assert!(
        4 * separator > PAGE_SIZE - INTERNAL_DIRECTORY,
        "and four do not"
    );
}

#[test]
fn two_hundred_entries_at_the_ceiling_read_back_in_order() {
    let (_dir, storage) = instance("btree-ceiling");
    let mut tree = int_tree(&storage);
    let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
    assert_eq!(frame, 15, "an int key and an empty payload take 15 bytes");
    let payload = vec![b'x'; MAX_INSERT_BYTES - frame];
    assert_eq!(
        encode_entry_key(&[Value::I32(0)], &payload).len(),
        MAX_INSERT_BYTES,
        "the entry sits exactly at the ceiling"
    );

    for number in 0..200 {
        tree.insert(&[Value::I32(number)], &payload)
            .expect("insert an entry of the size the ceiling allows");
    }

    let found = all(&tree);
    assert_eq!(found.len(), 200);
    assert_eq!(ints(&found), (0..200).collect::<Vec<_>>());
    assert_eq!(found[199].1.len(), MAX_INSERT_BYTES - frame);
    let root = page_of(&storage, tree.root());
    assert_eq!(
        root.kind().expect("the kind of the root"),
        PageKind::BTreeInternal,
        "the root split"
    );
    let child = internal_child(&root, 0).expect("the leftmost child of the root");
    assert_eq!(
        kind_of(&storage, child),
        PageKind::BTreeInternal,
        "and its children are internal pages: three levels at 200 entries of this size"
    );
}

#[test]
fn an_entry_above_the_ceiling_is_refused_before_any_write() {
    let (_dir, storage) = instance("btree-above-ceiling");
    let mut tree = int_tree(&storage);
    let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
    let pages_before = storage
        .control()
        .expect("read the control block")
        .next_page_id;

    let err = tree
        .insert(&[Value::I32(1)], &vec![b'x'; MAX_INSERT_BYTES - frame + 1])
        .expect_err("one byte above the ceiling");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert!(
        err.to_string().starts_with(&format!(
            "internal bug: an entry of {} bytes",
            MAX_INSERT_BYTES + 1
        )),
        "{err}"
    );
    assert_eq!(
        storage
            .control()
            .expect("read the control block again")
            .next_page_id,
        pages_before,
        "the refusal allocated no page"
    );
    assert_eq!(
        leaf_len(&page_of(&storage, tree.root())),
        0,
        "and wrote nothing in the root leaf"
    );

    // One byte less goes in, so the refusal is the ceiling and not the shape of the entry.
    tree.insert(&[Value::I32(1)], &vec![b'x'; MAX_INSERT_BYTES - frame])
        .expect("an entry at the ceiling");
    assert_eq!(all(&tree).len(), 1);
}

#[test]
fn sizes_drawn_below_the_ceiling_read_back_against_a_model() {
    let (_dir, storage) = instance("btree-drawn-sizes");
    let mut tree = int_tree(&storage);
    let frame = encode_entry_key(&[Value::I32(0)], &[]).len();
    let mut draw = Draw(0x5eed_0018);
    let mut model: Vec<(i32, Vec<u8>)> = Vec::new();
    let mut widest = 0;

    for step in 0..600u64 {
        let key = draw.below(120) as i32 - 40;
        let width = 8 + draw.below(MAX_INSERT_BYTES - frame - 7);
        let mut payload = rid(step);
        payload.resize(width, b'z');
        assert_eq!(
            encode_entry_key(&[Value::I32(key)], &payload).len(),
            width + frame
        );
        widest = widest.max(width);
        tree.insert(&[Value::I32(key)], &payload)
            .expect("insert an entry of a drawn size");
        model.push((key, payload));
    }

    assert_eq!(widest, 2_006, "the widest draw of this seed");
    assert!(widest + frame <= MAX_INSERT_BYTES);
    model.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let got = all(&tree)
        .into_iter()
        .map(|(values, payload)| match values.as_slice() {
            [Value::I32(number)] => (*number, payload),
            other => panic!("an entry of the tree carries {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        got, model,
        "600 entries of drawn sizes, in key then payload order"
    );
    assert_eq!(
        kind_of(&storage, tree.root()),
        PageKind::BTreeInternal,
        "the draws filled more than one page"
    );
}

#[test]
fn five_payloads_around_the_ceiling_are_refused_with_the_tree_intact() {
    let (_dir, storage) = instance("btree-ceiling-payloads");
    let mut tree = int_tree(&storage);
    for (key, width) in [(1, 1_341usize), (2, 1_340)] {
        tree.insert(&[Value::I32(key)], &vec![b'x'; width])
            .expect("a payload below the ceiling");
    }
    let pages_before = storage
        .control()
        .expect("read the control block")
        .next_page_id;
    let before = all(&tree);

    for (key, width) in [(3, 2_686usize), (4, 2_685), (9, 2_686)] {
        let err = tree
            .insert(&[Value::I32(key)], &vec![b'x'; width])
            .expect_err("a payload above the ceiling");
        assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    }

    assert_eq!(
        storage
            .control()
            .expect("read the control block again")
            .next_page_id,
        pages_before,
        "the three refusals allocated no page"
    );
    assert_eq!(all(&tree), before, "and left the entries as they were");

    // An entry put after the refusals reads back with the two that were there, and the
    // refused key 9 is absent: the refusals left no entry behind.
    tree.insert(&[Value::I32(5)], &[b'x'; 10])
        .expect("a small entry after the refusals");
    assert_eq!(ints(&all(&tree)), vec![1, 2, 5]);
    assert!(
        tree.seek(&KeyRange::Point(vec![Value::I32(9)]), Direction::Forward)
            .expect("seek the refused key")
            .is_empty()
    );
}

#[test]
fn a_half_of_a_split_without_room_is_refused_before_the_allocation() {
    let order = KeyOrder::new(&ascending(), &int_types()).expect("build the order");
    let mut page = leaf(7);
    // Two entries heavier than the tree itself takes, put through the byte API of the page
    // layer: the cut then leaves one half nearly full.
    let big = encode_entry_key(&[Value::I32(1)], &vec![b'x'; 6_200]);
    let small = encode_entry_key(&[Value::I32(9)], &vec![b'x'; 1_000]);
    assert_eq!(
        leaf_put(&mut page, &big, &[]).expect("put the heavy entry"),
        Put::Inserted
    );
    assert_eq!(
        leaf_put(&mut page, &small, &[]).expect("put the light entry"),
        Put::Inserted
    );
    let before = page.clone();

    let payload = vec![b'x'; MAX_INSERT_BYTES - 15];
    let probe = Probe {
        values: vec![Value::I32(5)],
        payload: PayloadBound::At(payload.clone()),
    };
    let entry = encode_entry_key(&[Value::I32(5)], &payload);
    let err = leaf_split_fits(&page, &order, &probe, &entry)
        .expect_err("the left half of the cut has no room for the entry");
    assert!(matches!(err, InternalError::Bug(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "internal bug: the left half of the split of leaf 7 would hold 6223 bytes and has \
         no room for the 2036 of the entry that caused the split; this build does not split \
         it again"
    );
    assert_eq!(page.0, before.0, "the check wrote nothing");

    // Counter-check: the same page and the same cut take a lighter entry.
    let light = vec![b'x'; 100];
    let probe = Probe {
        values: vec![Value::I32(5)],
        payload: PayloadBound::At(light.clone()),
    };
    leaf_split_fits(
        &page,
        &order,
        &probe,
        &encode_entry_key(&[Value::I32(5)], &light),
    )
    .expect("a lighter entry fits the half it goes to");
}

#[test]
fn a_key_that_does_not_match_the_columns_is_a_bug() {
    let (_dir, storage) = instance("btree-key-shape");
    let mut tree = int_tree(&storage);
    let err = tree.insert(&[], &rid(0)).expect_err("a key of no value");
    assert_eq!(
        err.to_string(),
        "internal bug: a B+tree entry carries 0 values, the key has 1 columns"
    );
    assert!(
        tree.insert(&[Value::I32(1), Value::I32(2)], &rid(0))
            .is_err(),
        "a key of two values on a key of one column"
    );
    assert!(tree.delete(&[], &rid(0)).is_err());

    assert!(
        KeyOrder::new(&[], &int_types()).is_err(),
        "a key of no column"
    );
    let err = KeyOrder::new(
        &[KeyColumn {
            column: 3,
            descending: false,
        }],
        &int_types(),
    )
    .expect_err("a column outside the table");
    assert_eq!(
        err.to_string(),
        "internal bug: key column 3 is outside the 1 columns of the table"
    );
}

use std::sync::Arc;

use vauban_storage::{MemoryStorage, Storage};
use vauban_txn::{IsolationLevel, TransactionManager};
use vauban_types::{Len, SqlType, TypeInfo, Value};

use super::*;
use crate::bootstrap::{
    bootstrap, internal_table_defs, internal_table_id, registered_internal_tables, registry_shape,
    registry_table, shape_version_row, stored_shape_version,
};
use crate::catalog::Catalog;
use crate::def::{InternalColumnDef, InternalTableDef};

/// Each shape version this crate has shipped, with the fingerprint of its definitions. A
/// change to the internal tables adds a line here with the next number and raises
/// [`CATALOG_SHAPE_VERSION`] to it.
const SHAPE_HISTORY: &[(i64, u64)] = &[(1, 0x41c6_4a70_b780_8024)];

/// A fingerprint of the shape `defs` and `registry` describe: for each table, its name, the
/// name and type of each column in order, and its clustered key.
///
/// The tables are taken in the order of their lowercase names, since the registry finds an
/// internal table by its name and not by its place. The hash is FNV-1a over a text form, so
/// the value does not depend on the hasher of the standard library.
fn shape_fingerprint(defs: &[InternalTableDef], registry: &vauban_storage::TableShape) -> u64 {
    let mut tables: Vec<&InternalTableDef> = defs.iter().collect();
    tables.sort_by_key(|def| def.name.to_ascii_lowercase());
    let mut text = format!("registry {registry:?}\n");
    for def in tables {
        text.push_str(&def.name.to_ascii_lowercase());
        for column in &def.columns {
            text.push_str(&format!(" {}:{:?}", column.name, column.ty));
        }
        text.push_str(&format!(" key {:?}\n", def.clustered_key));
    }
    fnv1a(text.as_bytes())
}

/// FNV-1a, 64 bits.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

fn current_fingerprint() -> u64 {
    shape_fingerprint(&internal_table_defs().unwrap(), &registry_shape())
}

/// A storage and a catalogue bootstrapped on it.
fn bootstrapped() -> (Arc<dyn Storage>, Catalog) {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let txn = Arc::new(TransactionManager::new(Arc::clone(&storage)));
    let catalog = bootstrap(Arc::clone(&storage), txn).expect("bootstrap of a fresh storage");
    (storage, catalog)
}

/// Opens the catalogue again on `storage`, as a restart does.
fn reopen(storage: &Arc<dyn Storage>) -> Result<Catalog, InternalError> {
    let txn = Arc::new(TransactionManager::new(Arc::clone(storage)));
    txn.bump_next_id_above_storage();
    bootstrap(Arc::clone(storage), txn)
}

/// Rewrites the row of the shape version: `Some(version)` sets its number, `None` deletes it.
fn rewrite_shape_version(catalog: &Catalog, version: Option<i64>) {
    let registry = registry_table(catalog).unwrap().expect("a registry");
    let handle = catalog.txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = catalog.txn.statement_snapshot(&handle);
    let mut target = None;
    for row in catalog.storage.scan(&snapshot, registry).unwrap() {
        let (id, values) = row.unwrap();
        if matches!(values.0.first(), Some(Value::String(name)) if name.text == SHAPE_VERSION_ENTRY)
        {
            target = Some(id);
        }
    }
    let target = target.expect("the row of the shape version");
    match version {
        Some(version) => catalog
            .storage
            .update(handle.id, registry, target, &shape_version_row(version))
            .unwrap(),
        None => catalog.storage.delete(handle.id, registry, target).unwrap(),
    }
    catalog.txn.commit(handle).unwrap();
}

#[test]
fn the_shape_version_follows_the_internal_table_definitions() {
    for pair in SHAPE_HISTORY.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "versions follow each other");
        assert_ne!(pair[1].1, pair[0].1, "a new version has a new fingerprint");
    }
    assert_eq!(
        SHAPE_HISTORY.last().copied(),
        Some((CATALOG_SHAPE_VERSION, current_fingerprint())),
        "the internal table definitions changed: add a line to SHAPE_HISTORY with the next \
         version and the new fingerprint, and raise CATALOG_SHAPE_VERSION to it"
    );
}

#[test]
fn a_column_added_or_renamed_changes_the_fingerprint() {
    let defs = internal_table_defs().unwrap();
    let base = shape_fingerprint(&defs, &registry_shape());

    let mut added = defs.clone();
    added[0].columns.push(InternalColumnDef {
        name: "extra".to_owned(),
        ty: TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true),
    });
    assert_ne!(shape_fingerprint(&added, &registry_shape()), base);

    let mut renamed = defs.clone();
    renamed[0].columns[0].name.push('x');
    assert_ne!(shape_fingerprint(&renamed, &registry_shape()), base);

    let mut reordered = defs.clone();
    reordered.reverse();
    assert_eq!(
        shape_fingerprint(&reordered, &registry_shape()),
        base,
        "the order of the descriptions is not part of the shape"
    );
}

#[test]
fn a_different_version_is_refused_with_both_numbers() {
    let err = check(Some(CATALOG_SHAPE_VERSION + 41)).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(&format!("shape version {}", CATALOG_SHAPE_VERSION + 41))
            && message.contains(&format!("expects version {CATALOG_SHAPE_VERSION}")),
        "{message}"
    );
    assert!(check(Some(CATALOG_SHAPE_VERSION)).is_ok());
}

#[test]
fn a_missing_version_is_refused() {
    let message = check(None).unwrap_err().to_string();
    assert!(
        message.contains("shape version none")
            && message.contains(&format!("expects version {CATALOG_SHAPE_VERSION}")),
        "{message}"
    );
}

#[test]
fn a_fresh_storage_carries_the_current_version_and_reopens() {
    let (storage, catalog) = bootstrapped();
    assert_eq!(
        stored_shape_version(&catalog).unwrap(),
        Some(CATALOG_SHAPE_VERSION)
    );
    // The row is not an internal table.
    assert_eq!(
        internal_table_id(&catalog, SHAPE_VERSION_ENTRY).unwrap(),
        None
    );
    assert!(
        registered_internal_tables(&catalog)
            .unwrap()
            .iter()
            .all(|(name, _)| name != SHAPE_VERSION_ENTRY)
    );
    let reopened = reopen(&storage).expect("a restart on the same shape");
    assert_eq!(
        stored_shape_version(&reopened).unwrap(),
        Some(CATALOG_SHAPE_VERSION)
    );
}

#[test]
fn a_restart_on_another_shape_version_is_refused() {
    let (storage, catalog) = bootstrapped();
    rewrite_shape_version(&catalog, Some(CATALOG_SHAPE_VERSION + 1));
    let Err(err) = reopen(&storage) else {
        panic!("a storage of another shape version opens");
    };
    let message = err.to_string();
    assert!(
        message.contains(&format!("shape version {}", CATALOG_SHAPE_VERSION + 1))
            && message.contains(&format!("expects version {CATALOG_SHAPE_VERSION}")),
        "{message}"
    );
}

#[test]
fn a_restart_without_a_shape_version_is_refused() {
    let (storage, catalog) = bootstrapped();
    rewrite_shape_version(&catalog, None);
    let Err(err) = reopen(&storage) else {
        panic!("a storage without a shape version opens");
    };
    assert!(err.to_string().contains("shape version none"), "{err}");
}

//! The system views: one file per group of views, each one carrying the internal table its
//! views read.
//!
//! A view is a `SELECT` over one **denormalised** internal table, without a join, so a file
//! describes the shape of those tables, the rows to put in them and the T-SQL text of the
//! views on top. The bootstrap creates in `master` the tables the files list in
//! `internal_tables()`. A file carries several views and therefore several tables, which is
//! why the per-file signature is a vector.
//!
//! | File | Views |
//! |---|---|
//! | `sys_core.rs` | `sys.databases`, `sys.schemas`, `sys.types` |
//! | `sys_tables.rs` | `sys.objects`, `sys.tables`, `sys.columns` |
//! | `sys_indexes.rs` | `sys.indexes`, `sys.index_columns`, `sys.key_constraints`, `sys.identity_columns` |
//! | `info_schema.rs` | `INFORMATION_SCHEMA.TABLES`, `COLUMNS`, `SCHEMATA` |
//! | `sys_constraints.rs` | `sys.foreign_keys`, `sys.foreign_key_columns`, `sys.check_constraints`, `sys.default_constraints` |
//! | `sys_extra.rs` | `sys.all_objects`, `sys.views`, `sys.partitions`, `sys.allocation_units`, the file views |

use crate::def::InternalTableDef;

// Each file carries the column positions of its tables; the ones no row writer reads yet
// document the shape.
#[allow(dead_code)]
pub(crate) mod info_schema;
#[allow(dead_code)]
pub(crate) mod sys_constraints;
pub(crate) mod sys_core;
#[allow(dead_code)]
pub(crate) mod sys_extra;
#[allow(dead_code)]
pub(crate) mod sys_indexes;
#[allow(dead_code)]
pub(crate) mod sys_tables;

/// The internal tables to create at bootstrap: those the files describe.
///
/// The bootstrap iterates over the result. The collect below reads `sys_tables` first, then
/// `sys_core`, `sys_indexes`, `info_schema`, `sys_constraints`, `sys_extra`: the table of
/// objects is offered first so that it comes first in `master`. Which `TableId` `storage`
/// then hands to each table is the business of the bootstrap, which creates them. A file
/// that describes nothing adds nothing to the result, and the tables of a file are in the
/// result under their own names (unit test
/// `the_described_internal_tables_hold_the_ones_of_sys_core`, which also checks that no name
/// is described twice).
pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    [
        sys_tables::internal_tables(),
        sys_core::internal_tables(),
        sys_indexes::internal_tables(),
        info_schema::internal_tables(),
        sys_constraints::internal_tables(),
        sys_extra::internal_tables(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_described_internal_tables_hold_the_ones_of_sys_core() {
        // `sys_core.rs` describes the table of the system types; the assertion states a
        // presence rather than the whole list.
        let names: Vec<String> = internal_tables()
            .into_iter()
            .map(|table| table.name)
            .collect();
        for described in sys_core::internal_tables() {
            assert!(names.contains(&described.name), "{names:?}");
        }
        assert!(
            names.contains(&sys_core::TYPES_TABLE.to_owned()),
            "{names:?}"
        );
        let mut once = names.clone();
        once.sort();
        once.dedup();
        assert_eq!(
            once.len(),
            names.len(),
            "a name is described twice: {names:?}"
        );
    }
}

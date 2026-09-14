//! The generic contract suite of the `Storage` trait, run on `MemoryStorage`.

use vauban_storage::{MemoryStorage, storage_contract_suite};

storage_contract_suite!(MemoryStorage::new());

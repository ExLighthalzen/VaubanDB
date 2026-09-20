use super::*;
use crate::{all, lookup};

/// A second [`register_builtins`] leaves every built-in exactly where the first put it.
///
/// What is under test is the `Once` guard: [`crate::register`] panics on a duplicate
/// name, so an unguarded second round would abort this test instead of failing an
/// assertion. The loop then checks the weaker half of idempotency — that the first
/// call's definitions are still reachable, and are still the *same* `&'static`
/// definitions rather than fresh copies.
///
/// It compares an identity, not a total. The global registry belongs to the whole
/// test binary, not to this test: `registry::tests::global_register_then_lookup` adds
/// a name of its own, on another thread, at a moment nothing orders. Reading
/// `all().len()` on both sides of the second call would count that name on one side
/// and not the other. A snapshot of definitions is immune to it: a concurrent
/// registration adds an entry this test does not look at.
#[test]
fn register_builtins_is_idempotent() {
    register_builtins();
    let first = all();
    assert!(!first.is_empty(), "register_builtins registered nothing");

    register_builtins();

    for def in &first {
        let found = lookup(def.name).expect("a built-in must still be registered");
        assert!(
            std::ptr::eq(found, *def),
            "`{}` no longer resolves to the definition of the first call",
            def.name
        );
    }
}

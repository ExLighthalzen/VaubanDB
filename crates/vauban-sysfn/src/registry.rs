//! Definition of a built-in function and the case-insensitive registry.
//!
//! The registry is a process-wide singleton filled once at start-up (`register`) and then
//! only read (`lookup`, `all`). Registration is a programming step, not a query step:
//! registering a name twice is a bug and panics, which the conventions allow at
//! initialisation.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};

use vauban_errors::SqlResult;
use vauban_types::{TypeInfo, Value};

use crate::context::EvalContext;

/// Whether a function computes one value per row or one value per group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    /// Scalar function: one result per evaluation (`LEN`, `GETDATE`, `@@SPID`).
    Scalar,
    /// Aggregate function: one result per group of rows (`COUNT`, `SUM`, `MAX`).
    Aggregate,
}

/// Number of arguments a function accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arity {
    /// Exactly `n` arguments (`LEN(s)`: `Exact(1)`).
    Exact(u8),
    /// Between `min` and `max` arguments, both included (`ROUND(x, n[, f])`: `Range(2, 3)`).
    Range(u8, u8),
    /// At least `min` arguments, no upper bound (`COALESCE(a, b, ...)`: `Variadic(2)`).
    Variadic(u8),
}

impl Arity {
    /// Returns `true` when a call with `n` arguments matches this arity.
    pub fn accepts(&self, n: usize) -> bool {
        match *self {
            Arity::Exact(k) => n == usize::from(k),
            Arity::Range(min, max) => (usize::from(min)..=usize::from(max)).contains(&n),
            Arity::Variadic(min) => n >= usize::from(min),
        }
    }
}

/// Arguments of one call, as the `executor` knows them at evaluation time.
///
/// The three fields describe the same call: `values[i]` is the already computed value of
/// the `i`-th argument and `types[i]` its declared type, in the order written in the
/// query. `values.len() == types.len()` is a **precondition of the caller**, not an error
/// case: an `eval` may read both slices up to the arity it declared without checking their
/// lengths. `result` is exactly the [`TypeInfo`] `check_call` returned for this same call,
/// so a function can trim or pad its result to the type the call was bound to.
///
/// The types travel with the values because a [`Value`] does not carry them:
/// `Value::String` says nothing about the declared length, the collation, or the
/// `varchar`/`nvarchar` family, which `DATALENGTH`, the truncation of `ISNULL` and
/// collation-aware searching all need. A structure rather than three parameters, so that a
/// later field costs nothing to the functions that ignore it.
#[derive(Debug, Clone, Copy)]
pub struct EvalArgs<'a> {
    /// Value of each argument, in call order.
    pub values: &'a [Value],
    /// Declared type of each argument: same length and same order as `values`.
    pub types: &'a [TypeInfo],
    /// Result type of the call, as `check_call` computed it from `types`.
    pub result: &'a TypeInfo,
}

/// Builds the accumulator of an aggregate from the type of its aggregated argument.
///
/// A plain function pointer, so that a [`FunctionDef`] stays `Copy` and usable in `const`.
/// One [`AggregateState`] is built per group.
pub type AggregateFactory = fn(&TypeInfo) -> SqlResult<Box<dyn AggregateState>>;

/// A built-in function as the `binder` and the `executor` see it.
///
/// `return_type` and `eval` are plain function pointers, not closures, so that definitions
/// can live in `static` items. The registry hands out `&'static FunctionDef`.
#[derive(Debug, Clone, Copy)]
pub struct FunctionDef {
    /// Name as written in T-SQL, `@@` prefix included for `@@VERSION`-style functions.
    /// Lookup is case-insensitive; this field keeps the canonical spelling.
    pub name: &'static str,
    /// Scalar or aggregate.
    pub kind: FunctionKind,
    /// `true` when the same arguments always produce the same result (Microsoft Learn,
    /// "Deterministic and nondeterministic functions"). `GETDATE`, `RAND`, `NEWID` are not.
    pub deterministic: bool,
    /// Accepted number of arguments.
    pub arity: Arity,
    /// Computes the result type from the argument types, at bind time.
    pub return_type: fn(&[TypeInfo]) -> SqlResult<TypeInfo>,
    /// Evaluates the function on one call's arguments, at execution time.
    pub eval: fn(&EvalArgs<'_>, &dyn EvalContext) -> SqlResult<Value>,
    /// Accumulator factory, `Some` exactly when `kind` is [`FunctionKind::Aggregate`].
    ///
    /// A scalar function leaves it `None` and is computed by `eval`; an aggregate builds
    /// one [`AggregateState`] per group through this factory.
    pub aggregate: Option<AggregateFactory>,
}

/// Accumulator of an aggregate over the rows of one group.
///
/// Built by an [`AggregateFactory`], fed one value per row by `step`, then consumed by
/// `finish`.
pub trait AggregateState {
    /// Feeds one row's value to the accumulator.
    fn step(&mut self, v: &Value) -> SqlResult<()>;
    /// Produces the aggregate result once every row of the group has been fed.
    fn finish(self: Box<Self>) -> SqlResult<Value>;
    /// `true` when the accumulator ignored at least one `NULL` value, which makes the
    /// session emit the informational message 8153. `false` by default: an aggregate that
    /// never skips `NULL` (`COUNT(*)`) has nothing to redefine.
    fn null_eliminated(&self) -> bool {
        false
    }
}

/// Normalises a function name to its registry key: ASCII upper case, nothing else.
///
/// `@@version` and `@@VERSION` share a key; `dbo.fn` is not resolved (schema resolution is
/// the `binder`'s job and never reaches this crate).
fn key(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// The registry data structure, separated from the global instance so that it can be
/// tested in isolation (the global one is shared by every test of the process).
#[derive(Debug, Default)]
pub(crate) struct Registry {
    by_key: HashMap<String, &'static FunctionDef>,
}

impl Registry {
    /// Creates an empty registry.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Stores `def` under its upper-cased name and returns the leaked `'static` reference.
    ///
    /// # Panics
    ///
    /// When a definition with the same name (any case) is already registered. The check
    /// happens before any mutation, so a panicking call leaves the registry unchanged.
    pub(crate) fn register(&mut self, def: FunctionDef) -> &'static FunctionDef {
        let key = key(def.name);
        if let Some(existing) = self.by_key.get(&key) {
            // Programming error at initialisation.
            panic!(
                "built-in function `{}` is already registered (as `{}`)",
                def.name, existing.name
            );
        }
        // Registration is finite and happens at start-up: leaking is the intended way to
        // obtain the `'static` lifetime the interfaces promise.
        let leaked: &'static FunctionDef = Box::leak(Box::new(def));
        self.by_key.insert(key, leaked);
        leaked
    }

    /// Finds a definition by name, ASCII case-insensitively.
    pub(crate) fn lookup(&self, name: &str) -> Option<&'static FunctionDef> {
        self.by_key.get(&key(name)).copied()
    }

    /// Every registered definition, sorted by upper-cased name for a stable order.
    pub(crate) fn all(&self) -> Vec<&'static FunctionDef> {
        let mut defs: Vec<&'static FunctionDef> = self.by_key.values().copied().collect();
        defs.sort_by_key(|def| key(def.name));
        defs
    }
}

/// The process-wide registry, created empty on first access.
fn global() -> &'static RwLock<Registry> {
    static GLOBAL: OnceLock<RwLock<Registry>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(Registry::new()))
}

/// Registers a built-in function in the global registry.
///
/// Meant to be called at start-up, by `register_builtins` and by `compat`
/// (`@@VERSION`, `SERVERPROPERTY`). Takes ownership of `def` and keeps it for the life of
/// the process.
///
/// # Panics
///
/// When a function with the same name (any case) is already registered: this is a
/// programming error detected at initialisation.
pub fn register(def: FunctionDef) {
    // A poisoned lock only means an earlier duplicate registration panicked while holding
    // it; the registry itself was left untouched by that call, so it is safe to reuse.
    global()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .register(def);
}

/// Finds a built-in function by name, ASCII case-insensitively.
///
/// Accepts ordinary names (`serverproperty`) and `@@`-prefixed ones (`@@version`). Does no
/// schema resolution: `dbo.fn` is not this crate's problem.
pub fn lookup(name: &str) -> Option<&'static FunctionDef> {
    global()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .lookup(name)
}

/// Every registered built-in function, sorted by name. For catalogue views and tests.
pub fn all() -> Vec<&'static FunctionDef> {
    global()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::StaticContext;
    use vauban_types::{Len, SqlString, SqlType};

    fn int_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
        Ok(TypeInfo::new(SqlType::Int, false))
    }

    fn eval_null(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
        Ok(Value::Null)
    }

    fn eval_spid(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
        Ok(Value::I16(ctx.spid()))
    }

    /// Writes a declared type the way T-SQL spells it. Local to these tests: rendering a
    /// type belongs to the `types` crate.
    fn declaration(info: &TypeInfo) -> String {
        match info.ty {
            SqlType::VarChar(Len::Fixed(n)) => format!("varchar({n})"),
            SqlType::NVarChar(Len::Fixed(n)) => format!("nvarchar({n})"),
            SqlType::Int => "int".to_owned(),
            other => format!("{other:?}"),
        }
    }

    /// Returns the declared type of the first argument: proves `eval` sees the types.
    fn eval_first_declaration(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
        Ok(Value::String(SqlString {
            text: declaration(&args.types[0]),
        }))
    }

    /// Returns the result type of the call: proves `eval` sees `check_call`'s answer.
    fn eval_result_declaration(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
        Ok(Value::String(SqlString {
            text: declaration(args.result),
        }))
    }

    fn text(value: &Value) -> String {
        match value {
            Value::String(s) => s.text.clone(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn def(name: &'static str) -> FunctionDef {
        FunctionDef {
            name,
            kind: FunctionKind::Scalar,
            deterministic: true,
            arity: Arity::Exact(0),
            return_type: int_type,
            eval: eval_null,
            aggregate: None,
        }
    }

    /// A definition written as a `const`, as `compat` and `builtins` write theirs.
    const CONST_DEF: FunctionDef = FunctionDef {
        name: "TEST_Const",
        kind: FunctionKind::Scalar,
        deterministic: true,
        arity: Arity::Exact(0),
        return_type: int_type,
        eval: eval_null,
        aggregate: None,
    };

    /// Sums the `I32` values of a group; the test implementation of [`AggregateState`].
    #[derive(Debug, Default)]
    struct SumState {
        total: i32,
    }

    impl AggregateState for SumState {
        fn step(&mut self, v: &Value) -> SqlResult<()> {
            if let Value::I32(n) = v {
                self.total += *n;
            }
            Ok(())
        }

        fn finish(self: Box<Self>) -> SqlResult<Value> {
            Ok(Value::I32(self.total))
        }
    }

    fn sum_factory(_arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
        Ok(Box::new(SumState::default()))
    }

    #[test]
    fn isolated_lookup_is_case_insensitive() {
        let mut reg = Registry::new();
        let registered = reg.register(def("MyFn"));
        for name in ["MYFN", "myfn", "MyFn"] {
            let found = reg.lookup(name).expect("registered function must be found");
            assert!(std::ptr::eq(found, registered), "lookup({name}) differs");
            assert_eq!(found.name, "MyFn");
        }
        assert!(reg.lookup("other").is_none());
    }

    #[test]
    #[should_panic(expected = "myfn")]
    fn isolated_duplicate_name_panics_with_the_name() {
        let mut reg = Registry::new();
        reg.register(def("MyFn"));
        reg.register(def("myfn"));
    }

    #[test]
    fn isolated_duplicate_panic_leaves_registry_unchanged() {
        let mut reg = Registry::new();
        reg.register(def("MyFn"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reg.register(def("MYFN"));
        }));
        assert!(result.is_err());
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.lookup("myfn").map(|d| d.name), Some("MyFn"));
    }

    #[test]
    fn isolated_lookup_accepts_double_at_names() {
        let mut reg = Registry::new();
        reg.register(def("@@VERSION"));
        let found = reg.lookup("@@version").expect("@@version must be found");
        assert_eq!(found.name, "@@VERSION");
        assert!(reg.lookup("VERSION").is_none());
    }

    #[test]
    fn isolated_all_is_sorted_by_name() {
        let mut reg = Registry::new();
        reg.register(def("zeta"));
        reg.register(def("Alpha"));
        reg.register(def("@@mid"));
        let names: Vec<&str> = reg.all().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["@@mid", "Alpha", "zeta"]);
    }

    #[test]
    fn arity_accepts() {
        assert!(Arity::Exact(1).accepts(1));
        assert!(!Arity::Exact(1).accepts(2));
        assert!(!Arity::Exact(1).accepts(0));

        assert!(!Arity::Range(1, 3).accepts(0));
        assert!(Arity::Range(1, 3).accepts(1));
        assert!(Arity::Range(1, 3).accepts(2));
        assert!(Arity::Range(1, 3).accepts(3));
        assert!(!Arity::Range(1, 3).accepts(4));

        assert!(!Arity::Variadic(2).accepts(1));
        assert!(Arity::Variadic(2).accepts(2));
        assert!(Arity::Variadic(2).accepts(50));
    }

    #[test]
    fn eval_reads_the_context() {
        let spid_fn = FunctionDef {
            name: "TEST_SPID",
            kind: FunctionKind::Scalar,
            deterministic: false,
            arity: Arity::Exact(0),
            return_type: int_type,
            eval: eval_spid,
            aggregate: None,
        };
        let ctx = StaticContext {
            spid: 57,
            ..StaticContext::default()
        };
        let int = TypeInfo::new(SqlType::Int, false);
        let args = EvalArgs {
            values: &[],
            types: &[],
            result: &int,
        };
        let result = (spid_fn.eval)(&args, &ctx).expect("eval must succeed");
        assert_eq!(result, Value::I16(57));
        let ty = (spid_fn.return_type)(&[]).expect("return_type must succeed");
        assert_eq!(ty.ty, SqlType::Int);
    }

    #[test]
    fn eval_receives_the_argument_types() {
        let type_fn = FunctionDef {
            eval: eval_first_declaration,
            ..def("TEST_ArgType")
        };
        let varchar3 = TypeInfo::new(SqlType::VarChar(Len::Fixed(3)), true);
        let args = EvalArgs {
            values: &[Value::Null],
            types: std::slice::from_ref(&varchar3),
            result: &varchar3,
        };
        let value = (type_fn.eval)(&args, &StaticContext::default()).expect("eval must succeed");
        assert_eq!(text(&value), "varchar(3)");
    }

    #[test]
    fn eval_receives_the_result_type() {
        let result_fn = FunctionDef {
            eval: eval_result_declaration,
            ..def("TEST_ResultType")
        };
        let varchar3 = TypeInfo::new(SqlType::VarChar(Len::Fixed(3)), true);
        let result = TypeInfo::new(SqlType::NVarChar(Len::Fixed(10)), true);
        let args = EvalArgs {
            values: &[Value::Null],
            types: std::slice::from_ref(&varchar3),
            result: &result,
        };
        let value = (result_fn.eval)(&args, &StaticContext::default()).expect("eval must succeed");
        // The result type, not the argument type: the two differ on purpose.
        assert_eq!(text(&value), declaration(&result));
        assert_eq!(text(&value), "nvarchar(10)");
    }

    #[test]
    fn function_def_is_still_const() {
        let mut reg = Registry::new();
        let registered = reg.register(CONST_DEF);
        assert_eq!(registered.name, "TEST_Const");
        assert_eq!(reg.lookup("test_const").map(|d| d.name), Some("TEST_Const"));
    }

    #[test]
    fn aggregate_defaults_to_none() {
        assert!(def("TEST_Scalar").aggregate.is_none());
        assert!(CONST_DEF.aggregate.is_none());
    }

    #[test]
    fn aggregate_factory_builds_a_state() {
        let sum_fn = FunctionDef {
            name: "TEST_Sum",
            kind: FunctionKind::Aggregate,
            deterministic: true,
            arity: Arity::Exact(1),
            return_type: int_type,
            eval: eval_null,
            aggregate: Some(sum_factory),
        };
        let factory = sum_fn
            .aggregate
            .expect("an aggregate definition must expose its factory");
        let mut state =
            factory(&TypeInfo::new(SqlType::Int, false)).expect("the factory must succeed");
        state.step(&Value::I32(2)).expect("step must succeed");
        state.step(&Value::I32(40)).expect("step must succeed");
        assert!(!state.null_eliminated(), "the default is false");
        assert_eq!(state.finish().expect("finish must succeed"), Value::I32(42));
    }

    #[test]
    fn global_registry_test_names_are_prefixed() {
        // Other tests of this binary register `TEST_`-prefixed names into the global
        // registry, possibly concurrently. The built-in functions live there too
        // (registered by `register_builtins`), so the invariant checked here is that each
        // name is a valid identifier: no empty name, no whitespace.
        //
        // The assertion is per name, so a registration that lands between the call to
        // `all()` and the loop changes nothing: this test never reads a total. That is the
        // rule the whole binary follows, see `global_register_then_lookup`.
        for name in all().into_iter().map(|d| d.name) {
            assert!(
                !name.is_empty() && !name.contains(char::is_whitespace),
                "bad name {name:?}"
            );
        }
    }

    /// `register` writes where `lookup` and `all` read: one process-wide registry.
    ///
    /// The three functions here are the global wrappers, not the isolated [`Registry`] the
    /// `isolated_*` tests above build. Proving they agree means writing into the registry
    /// the whole test binary shares, which this test is allowed to do, and which every
    /// other test must expect.
    ///
    /// Hence the rule of this crate: **no test compares a global count**
    /// (`builtins::tests::register_builtins_is_idempotent` compares identities). Names
    /// are added to this registry by tests running on other threads, at a moment nothing
    /// orders, so a total read twice is two different numbers. Assert on names, on
    /// identities, on a per-entry invariant, not on `all().len()`.
    #[test]
    fn global_register_then_lookup() {
        register(def("TEST_Global"));
        let found = lookup("test_global").expect("global lookup must find it");
        assert_eq!(found.name, "TEST_Global");
        assert!(all().iter().any(|d| std::ptr::eq(*d, found)));
    }

    #[test]
    fn global_lookup_unknown_is_none() {
        assert!(lookup("TEST_NeverRegistered").is_none());
    }
}

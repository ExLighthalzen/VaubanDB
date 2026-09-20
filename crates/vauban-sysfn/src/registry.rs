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
mod tests;

//! Call frames: the state of a level of call, set aside while a callee runs and given
//! back when it returns.
//!
//! # What a level holds
//!
//! The running level keeps its state where the statements read it: its variables in
//! `ExecSession::variables` and `ExecSession::variable_types`, its `SET` options in
//! [`ExecContext::options`] and in the fields of `ExecSession` that hold one
//! (`XACT_ABORT`, the isolation level, the lock timeout, `NOCOUNT`), its depth in
//! `ExecSession::nest_level`. Its labels are the ones of the [`Program`](super::Program)
//! it runs.
//!
//! # A call
//!
//! When an `EXECUTE` of a program calls, [`Frame::enter`] moves the variables of the
//! caller into a [`Frame`] and copies its options there, so the callee starts with an
//! empty set of variables, the options of its caller, and a level one deeper. A variable
//! of the caller is not visible to the callee, and one the callee declares is gone once it
//! returns; a name declared on both sides is two variables
//! (`interp::frame::tests::a_variable_declared_by_a_callee_is_not_visible_to_its_caller`).
//! The parameters of a callee are the first variables its handler enters.
//!
//! When the callee returns, however it ended, [`Frame::leave`] puts the variables and the
//! options of the caller back, and the interpreter hands the caller's current database to
//! [`CallHandler::restore_database`](super::CallHandler::restore_database), or to
//! [`Host::leave_call`](super::Host::leave_call) for a program run through a host. A `SET` option the callee changed is thus back at
//! the value of its caller (`interp::frame::tests::nocount_set_by_a_callee_is_restored_on_return`,
//! `interp::frame::tests::the_options_a_callee_changes_are_restored_on_return`). The
//! options the session holds itself (`QUOTED_IDENTIFIER`, `DATEFORMAT`, `LANGUAGE`,
//! `TEXTSIZE` and their neighbours) are the session's to give back when a call returns.
//!
//! `ExecSession::max_nest_level` bounds the depth: a call that would go past it is
//! refused before the callee runs, by an internal error that ends the program which made
//! it (`interp::frame::tests::a_call_past_the_nesting_limit_is_refused`).

use std::collections::HashMap;
use std::mem;

use vauban_binder::{BatchVariables, SessionOptions};
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_txn::{IsolationLevel, LockTimeout};
use vauban_types::{TypeInfo, Value};

use crate::context::ExecContext;

/// Values keyed by variable name, each at the position its name took when it was entered.
///
/// A name is found through its [`BatchVariables::name_key`], hashed to its position, so a
/// read does not walk the names entered before it, and `@x` and `@X` are one entry
/// (`interp::frame::tests::names_fold_as_the_binder_folds_them`). The spelling kept is
/// the one the name was first entered with.
#[derive(Debug, Clone)]
pub struct NameMap<V> {
    /// `(name as first entered, value)`; the index is the position of the name.
    slots: Vec<(String, V)>,
    /// The position of each name, keyed by [`BatchVariables::name_key`].
    positions: HashMap<String, usize>,
}

impl<V> Default for NameMap<V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            positions: HashMap::new(),
        }
    }
}

impl<V> NameMap<V> {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many names the map holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the map holds a name.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Forgets the names it holds.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.positions.clear();
    }

    /// The position of `name`, `None` when the map does not hold it.
    #[must_use]
    pub fn position(&self, name: &str) -> Option<usize> {
        self.positions
            .get(BatchVariables::name_key(name).as_ref())
            .copied()
    }

    /// The value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&V> {
        self.slots.get(self.position(name)?).map(|(_, value)| value)
    }

    /// The value of `name`, to change it in place.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut V> {
        let position = self.position(name)?;
        self.slots.get_mut(position).map(|(_, value)| value)
    }

    /// Whether the map holds `name`.
    #[must_use]
    pub fn contains_key(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// Stores `value` under `name`: in place, under the spelling already held, when the
    /// map holds the name, at the next position otherwise. Answers the value replaced.
    pub fn insert(&mut self, name: String, value: V) -> Option<V> {
        match self.get_mut(&name) {
            Some(held) => Some(mem::replace(held, value)),
            None => {
                self.push(name, value);
                None
            }
        }
    }

    /// The entry of `name`, to fill it when the map does not hold it yet.
    pub fn entry(&mut self, name: String) -> NameEntry<'_, V> {
        NameEntry { map: self, name }
    }

    /// The names and their values, in the order of their positions.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.slots.iter().map(|(name, value)| (name, value))
    }

    /// The names, in the order of their positions.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.slots.iter().map(|(name, _)| name)
    }

    /// Enters `name`, which the map does not hold, at the next position.
    fn push(&mut self, name: String, value: V) -> &mut V {
        let position = self.slots.len();
        self.positions
            .insert(BatchVariables::name_key(&name).into_owned(), position);
        self.slots.push((name, value));
        &mut self.slots[position].1
    }
}

/// The entry of a name in a [`NameMap`], from [`NameMap::entry`].
#[derive(Debug)]
pub struct NameEntry<'m, V> {
    map: &'m mut NameMap<V>,
    name: String,
}

impl<'m, V> NameEntry<'m, V> {
    /// The value held under the name, or `default` entered at the next position.
    pub fn or_insert(self, default: V) -> &'m mut V {
        self.or_insert_with(|| default)
    }

    /// The value held under the name, or the value `make` builds entered at the next
    /// position.
    pub fn or_insert_with(self, make: impl FnOnce() -> V) -> &'m mut V {
        match self.map.position(&self.name) {
            Some(position) => &mut self.map.slots[position].1,
            None => self.map.push(self.name, make()),
        }
    }
}

/// A level of call set aside while its callee runs: what it gets back when the callee
/// returns (module documentation).
#[derive(Debug)]
pub(crate) struct Frame {
    /// The variables of the caller.
    variables: NameMap<Value>,
    /// The declared types of the caller's variables.
    variable_types: HashMap<String, TypeInfo>,
    /// The options of the caller that change evaluation.
    options: SessionOptions,
    /// `SET XACT_ABORT` of the caller.
    xact_abort: bool,
    /// `SET TRANSACTION ISOLATION LEVEL` of the caller.
    isolation: IsolationLevel,
    /// `SET LOCK_TIMEOUT` of the caller.
    lock_timeout: LockTimeout,
    /// `SET NOCOUNT` of the caller.
    nocount: bool,
    /// The current database of the caller.
    database: String,
}

impl Frame {
    /// Sets the running level aside and opens the level of its callee (module
    /// documentation, "A call").
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the session state is missing, or when the callee
    /// would run deeper than `ExecSession::max_nest_level`; nothing is set aside then.
    pub(crate) fn enter(ctx: &mut ExecContext<'_>) -> SqlResult<Self> {
        let options = ctx.options;
        let database = ctx.eval.current_database().to_owned();
        let session = ctx.session()?;
        if let Some(limit) = session.max_nest_level
            && session.nest_level >= limit
        {
            return Err(SqlError::from(InternalError::Bug(format!(
                "a call would nest deeper than the limit of {limit} levels"
            ))));
        }
        session.nest_level += 1;
        Ok(Self {
            variables: mem::take(&mut session.variables),
            variable_types: mem::take(&mut session.variable_types),
            options,
            xact_abort: session.xact_abort,
            isolation: session.isolation,
            lock_timeout: session.lock_timeout,
            nocount: session.nocount,
            database,
        })
    }

    /// Closes the level of the callee and gives the caller its state back (module
    /// documentation, "A call"). Answers the current database of the caller, which the
    /// interpreter hands to the host to make current again.
    ///
    /// # Errors
    ///
    /// The internal error 50000 when the session state is missing; the options are back
    /// then.
    pub(crate) fn leave(self, ctx: &mut ExecContext<'_>) -> SqlResult<String> {
        ctx.options = self.options;
        let session = ctx.session()?;
        session.variables = self.variables;
        session.variable_types = self.variable_types;
        session.xact_abort = self.xact_abort;
        session.isolation = self.isolation;
        session.lock_timeout = self.lock_timeout;
        session.nocount = self.nocount;
        session.nest_level = session.nest_level.saturating_sub(1);
        Ok(self.database)
    }
}

#[cfg(test)]
mod tests;

//! Where the results of execution go: a protocol-neutral sink.
//!
//! The executor knows nothing of the wire. What a statement produces, a result set (its
//! columns, then its rows), the end of the statement with its row count, informational
//! messages, errors and changes of the session environment, is handed to a
//! [`StatementSink`] in the order it happens. The types the sink receives are the
//! engine's own: [`ResultColumn`] carries a [`TypeInfo`], a name and a nullability,
//! [`EnvironmentChange`] names what changed in the session, [`SqlError`] and
//! [`InfoMessage`] come from `vauban-errors`.
//!
//! # Who implements it
//!
//! * `session` adapts it to the tokens of its client connection. What depends on the
//!   position of a statement in what the client sent (whether more results follow, whether
//!   the statement runs inside a procedure) is the adapter's to decide, not this trait's.
//! * [`CollectSink`] keeps everything in memory, in order: the tests read it, and a caller
//!   with no client connection can run statements into it.
//!
//! # The order of the calls
//!
//! For one statement: [`StatementSink::columns`] once before the first row of a result
//! set, [`StatementSink::row`] per row, the messages and errors as they arise, then
//! [`StatementSink::done`]. The executor itself calls `columns`, `row` and `info`; the
//! caller that drives a sequence of statements closes each with `done`, and reports the
//! error a statement answered through `error` (`sink::tests::a_batch_runs_into_the_collect_sink_without_a_session`).

use vauban_binder::{OutputColumn, OutputSchema};
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_types::{TypeInfo, Value};

use crate::row::Row;

/// One column of a result set, as the receiver of the results sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    /// The name of the column, empty for an expression without an alias (`SELECT 1`).
    pub name: String,
    /// The type of the column, collation included.
    pub ty: TypeInfo,
    /// Whether the column may hold `NULL`.
    pub nullable: bool,
}

impl ResultColumn {
    /// The column a bound output column describes: its name, its type, and the
    /// nullability that type carries.
    #[must_use]
    pub fn from_output(column: &OutputColumn) -> Self {
        Self {
            name: column.name.clone(),
            ty: column.ty.clone(),
            nullable: column.ty.nullable,
        }
    }

    /// The columns of `schema`, in order.
    #[must_use]
    pub fn of_schema(schema: &OutputSchema) -> Vec<Self> {
        schema.columns.iter().map(Self::from_output).collect()
    }

    /// The bound output column this column describes back; the nullability goes into
    /// the type.
    #[must_use]
    pub fn to_output(&self) -> OutputColumn {
        let mut ty = self.ty.clone();
        ty.nullable = self.nullable;
        OutputColumn {
            name: self.name.clone(),
            ty,
        }
    }
}

/// A change of the session environment a statement caused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentChange {
    /// The current database changed (`USE`).
    Database {
        /// The database the session was in.
        old: String,
        /// The database the session is in now.
        new: String,
    },
    /// The session language changed.
    Language {
        /// The previous language.
        old: String,
        /// The new language.
        new: String,
    },
    /// A transaction began; carries its descriptor.
    TransactionBegun(u64),
    /// A transaction committed; carries the descriptor of the ended transaction.
    TransactionCommitted(u64),
    /// A transaction rolled back; carries the descriptor of the ended transaction.
    TransactionRolledBack(u64),
}

/// Where the results of execution go (module documentation).
pub trait StatementSink {
    /// A result set starts: its columns, sent once before its first row.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the metadata; the statement stops
    /// there.
    fn columns(&mut self, columns: &[ResultColumn]) -> SqlResult<()>;

    /// One row of the current result set, one value per column announced by
    /// [`StatementSink::columns`].
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the row; the statement stops there.
    fn row(&mut self, row: &[Value]) -> SqlResult<()>;

    /// A statement ends. `rowcount` is `Some` when the count of rows it affected or
    /// returned is meaningful for the receiver, `None` otherwise.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the end of the statement.
    fn done(&mut self, rowcount: Option<u64>) -> SqlResult<()>;

    /// An informational message the statement produced (`PRINT`, a warning).
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the message.
    fn info(&mut self, message: &InfoMessage) -> SqlResult<()>;

    /// An error the statement answered, for the receiver of the results.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take this one.
    fn error(&mut self, error: &SqlError) -> SqlResult<()>;

    /// A change of the session environment.
    ///
    /// # Errors
    ///
    /// The error the sink raises when it cannot take the change.
    fn env_change(&mut self, change: &EnvironmentChange) -> SqlResult<()>;
}

/// One call received by a [`CollectSink`], in the order it was made.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkEvent {
    /// [`StatementSink::columns`].
    Columns(Vec<ResultColumn>),
    /// [`StatementSink::row`].
    Row(Row),
    /// [`StatementSink::done`].
    Done(Option<u64>),
    /// [`StatementSink::info`].
    Info(InfoMessage),
    /// [`StatementSink::error`].
    Error(SqlError),
    /// [`StatementSink::env_change`].
    EnvChange(EnvironmentChange),
}

/// A [`StatementSink`] that keeps what it is given in memory, for the tests, for
/// [`execute_collect`](crate::execute_collect), and for a caller that runs statements
/// without a client connection.
///
/// A row handed over before any columns is refused with the internal error 50000: the
/// order of the calls is part of what a test reads on this sink
/// (`sink::tests::a_row_before_the_columns_is_refused`).
#[derive(Debug, Clone, Default)]
pub struct CollectSink {
    /// The columns announced last by [`StatementSink::columns`], `None` until then.
    pub columns: Option<Vec<ResultColumn>>,
    /// The rows, in the order they were handed over, across result sets.
    pub rows: Vec<Row>,
    /// The messages, in the order they were handed over.
    pub infos: Vec<InfoMessage>,
    /// The calls received, in order.
    pub events: Vec<SinkEvent>,
}

impl CollectSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The columns announced last, as a bound output schema; `None` until a call of
    /// [`StatementSink::columns`].
    #[must_use]
    pub fn schema(&self) -> Option<OutputSchema> {
        self.columns.as_ref().map(|columns| OutputSchema {
            columns: columns.iter().map(ResultColumn::to_output).collect(),
        })
    }

    /// The errors received, in order.
    #[must_use]
    pub fn errors(&self) -> Vec<&SqlError> {
        self.events
            .iter()
            .filter_map(|event| match event {
                SinkEvent::Error(error) => Some(error),
                _ => None,
            })
            .collect()
    }
}

impl StatementSink for CollectSink {
    fn columns(&mut self, columns: &[ResultColumn]) -> SqlResult<()> {
        self.columns = Some(columns.to_vec());
        self.events.push(SinkEvent::Columns(columns.to_vec()));
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        if self.columns.is_none() {
            return Err(SqlError::from(InternalError::Bug(
                "CollectSink: a row was handed over before the columns".to_owned(),
            )));
        }
        self.rows.push(row.to_vec());
        self.events.push(SinkEvent::Row(row.to_vec()));
        Ok(())
    }

    fn done(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.events.push(SinkEvent::Done(rowcount));
        Ok(())
    }

    fn info(&mut self, message: &InfoMessage) -> SqlResult<()> {
        self.infos.push(message.clone());
        self.events.push(SinkEvent::Info(message.clone()));
        Ok(())
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.events.push(SinkEvent::Error(error.clone()));
        Ok(())
    }

    fn env_change(&mut self, change: &EnvironmentChange) -> SqlResult<()> {
        self.events.push(SinkEvent::EnvChange(change.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests;

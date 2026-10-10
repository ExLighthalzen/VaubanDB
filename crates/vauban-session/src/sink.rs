//! `ResultSink` trait and its TDS implementation, fed through a channel from the blocking
//! pool.
//!
//! The engine is synchronous and runs on the blocking pool of tokio; the connection task
//! is asynchronous. `TdsSink` is the bridge: every call of the trait becomes one `Token`
//! pushed into a bounded `mpsc` channel with `blocking_send`, and the connection task
//! drains the channel into the `TdsStream`. When the client reads slowly the channel fills
//! up and `blocking_send` blocks the pool thread, never the async loop: that is the
//! intended back-pressure.
//!
//! # The adapter between [`StatementSink`] and [`ResultSink`]
//!
//! The executor hands its results to a [`StatementSink`], a trait of `vauban-executor`
//! that knows nothing of the wire. [`StatementSinkAdapter`] implements it on top of a
//! `&mut dyn ResultSink` and turns each call into the matching token: the neutral columns
//! into COLMETADATA, an [`EnvironmentChange`] into an ENVCHANGE, and the end of a
//! statement into DONE, or DONEINPROC inside a nested batch. Whether that end carries
//! `more` depends on where the statement sits in what the client sent: the adapter holds
//! it ([`StatementSinkAdapter::set_more`]), set by the caller that knows it.

use tokio::sync::mpsc;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_executor::{EnvironmentChange, ResultColumn, StatementSink};
use vauban_tds::{ColumnFlags, ColumnMeta, DoneStatus, EnvChange, Token};
use vauban_types::{TypeInfo, Value};

/// Capacity of the token channel between the blocking pool and the connection task.
pub(crate) const CHANNEL_CAPACITY: usize = 256;

/// `CurCmd` of the DONE token that closes a SELECT, as SQL Server sends it.
pub(crate) const CUR_CMD_SELECT: u16 = 0xC1;

/// `CurCmd` of the closing DONEPROC of a nested batch or an RPC.
pub(crate) const CUR_CMD_PROC: u16 = 0xE0;

/// `CurCmd` of the DONE that closes a `SELECT … INTO`.
pub(crate) const CUR_CMD_SELECT_INTO: u16 = 0xC2;

/// `CurCmd` of the DONE that closes an `INSERT`.
pub(crate) const CUR_CMD_INSERT: u16 = 0xC3;

/// `CurCmd` of the DONE that closes a `DELETE`.
pub(crate) const CUR_CMD_DELETE: u16 = 0xC4;

/// `CurCmd` of the DONE that closes an `UPDATE`.
pub(crate) const CUR_CMD_UPDATE: u16 = 0xC5;

/// Where the results of a statement go: implemented for TDS here, and by the recording
/// sinks of the tests.
///
/// Rule of the DONE: `done(rowcount, more)` does not carry the error flag. A call to
/// `error()` since the last `done()` makes the TDS implementation emit
/// `DoneStatus::ERROR`; `cur_cmd` is the one [`ResultSink::next_cur_cmd`] named since the
/// last `done()`, else 0xC1 (SELECT) when `columns()` was emitted since then, 0 otherwise.
pub trait ResultSink {
    /// A result set starts: the metadata of its columns ([MS-TDS] 2.2.7 COLMETADATA).
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()>;
    /// One row of the current result set, one value per column ([MS-TDS] 2.2.7 ROW).
    fn row(&mut self, row: &[Value]) -> SqlResult<()>;
    /// A statement ends: `rowcount` when the count is meaningful, `more` when another
    /// statement of the same batch follows ([MS-TDS] 2.2.7 DONE).
    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()>;
    /// An informational message ([MS-TDS] 2.2.7 INFO).
    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()>;
    /// An error destined to the client ([MS-TDS] 2.2.7 ERROR).
    fn error(&mut self, err: &SqlError) -> SqlResult<()>;
    /// A change of the session environment ([MS-TDS] 2.2.7 ENVCHANGE).
    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()>;
    /// The value of an OUTPUT parameter of an RPC ([MS-TDS] 2.2.7 RETURNVALUE).
    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()>;
    /// The return status of a stored procedure ([MS-TDS] 2.2.7 RETURNSTATUS).
    fn return_status(&mut self, status: i32) -> SqlResult<()>;
    /// A statement ends inside a nested batch or a procedure ([MS-TDS] 2.2.7 DONEINPROC).
    /// Nested batches emit one DONEINPROC per executed statement in the nested text;
    /// plain SQL batches use [`done`] instead. The default delegates to [`done`].
    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.done(rowcount, more)
    }
    /// The nested batch or procedure ends: DONEPROC after the last DONEINPROC
    /// ([MS-TDS] 2.2.7 DONEPROC). The default delegates to [`done`].
    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        self.done(rowcount, false)
    }
    /// The `CurCmd` of the next `done` or `done_in_proc`, which closes a statement of that
    /// kind ([MS-TDS] 2.2.7 DONE). A driver reads the count of a DONE whose `CurCmd` is
    /// `SELECT` (0xC1) as the size of a result set rather than as rows affected, which is
    /// why `SET @v` closes with 0xC1 (`tests/row_counts.rs`). A sink that wraps another
    /// passes it on; the default drops it.
    fn next_cur_cmd(&mut self, cur_cmd: u16) -> SqlResult<()> {
        let _ = cur_cmd;
        Ok(())
    }
}

/// Adapts the executor's [`StatementSink`] to a [`ResultSink`] (module documentation).
///
/// Built per statement, wraps the caller's sink and counts the rows it receives.
pub(crate) struct StatementSinkAdapter<'a> {
    sink: &'a mut dyn ResultSink,
    row_count: u64,
    /// The end of the statement carries `more`: another result follows in the response.
    more: bool,
    /// The end of the statement is a DONEINPROC: the statement runs in a nested batch.
    in_proc: bool,
}

impl<'a> StatementSinkAdapter<'a> {
    /// Wraps `sink` for a statement of a top-level batch: the end of the statement is a
    /// DONE, without `more` until [`StatementSinkAdapter::set_more`] says otherwise.
    pub(crate) fn new(sink: &'a mut dyn ResultSink) -> Self {
        Self {
            sink,
            row_count: 0,
            more: false,
            in_proc: false,
        }
    }

    /// Wraps `sink` for a statement of a nested batch: the end of the statement is a
    /// DONEINPROC, with `more`, since the DONEPROC that closes the nested batch follows it.
    pub(crate) fn in_proc(sink: &'a mut dyn ResultSink) -> Self {
        Self {
            more: true,
            in_proc: true,
            ..Self::new(sink)
        }
    }

    /// Whether the next end of statement carries `more`.
    pub(crate) fn set_more(&mut self, more: bool) {
        self.more = more;
    }

    /// The wrapped sink, for the tokens that are not results of the statement (the
    /// procedure tokens, the end of a batch).
    pub(crate) fn tds(&mut self) -> &mut dyn ResultSink {
        &mut *self.sink
    }

    /// How many rows the sink received since the adapter was built.
    ///
    /// Kept for assertions in tests; the batch flow reads the count from
    /// [`ExecOutcome::Rows`](vauban_executor::ExecOutcome::Rows).
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn row_count(&self) -> u64 {
        self.row_count
    }
}

/// The ENVCHANGE that announces `change` ([MS-TDS] 2.2.7 ENVCHANGE).
pub(crate) fn env_change_token(change: &EnvironmentChange) -> EnvChange {
    match change {
        EnvironmentChange::Database { old, new } => EnvChange::Database {
            old: old.clone(),
            new: new.clone(),
        },
        EnvironmentChange::Language { old, new } => EnvChange::Language {
            old: old.clone(),
            new: new.clone(),
        },
        EnvironmentChange::TransactionBegun(descriptor) => EnvChange::BeginTransaction(*descriptor),
        EnvironmentChange::TransactionCommitted(descriptor) => {
            EnvChange::CommitTransaction(*descriptor)
        }
        EnvironmentChange::TransactionRolledBack(descriptor) => {
            EnvChange::RollbackTransaction(*descriptor)
        }
    }
}

/// The COLMETADATA entry of `column` ([MS-TDS] 2.2.7 COLMETADATA): `fNullable` follows
/// the nullability of the column, the other flags are clear
/// (`tests::the_adapter_turns_a_statement_into_batch_tokens`).
pub(crate) fn column_meta(column: &ResultColumn) -> ColumnMeta {
    ColumnMeta {
        name: column.name.clone(),
        ty: column.ty.clone(),
        flags: ColumnFlags {
            nullable: column.nullable,
            ..ColumnFlags::default()
        },
    }
}

impl StatementSink for StatementSinkAdapter<'_> {
    fn columns(&mut self, columns: &[ResultColumn]) -> SqlResult<()> {
        let cols: Vec<ColumnMeta> = columns.iter().map(column_meta).collect();
        self.sink.columns(&cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.row_count += 1;
        self.sink.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        if self.in_proc {
            self.sink.done_in_proc(rowcount, self.more)
        } else {
            self.sink.done(rowcount, self.more)
        }
    }

    fn info(&mut self, message: &InfoMessage) -> SqlResult<()> {
        self.sink.info(message)
    }

    fn error(&mut self, error: &SqlError) -> SqlResult<()> {
        self.sink.error(error)
    }

    fn env_change(&mut self, change: &EnvironmentChange) -> SqlResult<()> {
        self.sink.env_change(&env_change_token(change))
    }
}

/// The `ResultSink` of a TDS connection: turns every call into a `Token` and sends it to
/// the connection task through a bounded channel.
///
/// The sender is owned by the sink and dropped with it, so the receiver sees the end of
/// the channel exactly when the request is over: the connection task needs no other
/// signal.
pub struct TdsSink {
    tx: mpsc::Sender<Token>,
    /// `true` for an RPC: `done()` emits DONEPROC instead of DONE.
    rpc: bool,
    /// An `error()` was emitted since the last `done()`.
    error_since_done: bool,
    /// A `columns()` was emitted since the last `done()`.
    columns_since_done: bool,
    /// The `CurCmd` [`ResultSink::next_cur_cmd`] named since the last `done()`.
    next_cur_cmd: Option<u16>,
}

impl TdsSink {
    /// A sink for a SQL batch: `done()` emits `Token::Done`.
    pub fn new(tx: mpsc::Sender<Token>) -> Self {
        Self {
            tx,
            rpc: false,
            error_since_done: false,
            columns_since_done: false,
            next_cur_cmd: None,
        }
    }

    /// A sink for an RPC: `done()` emits `Token::DoneProc` with the same fields.
    pub(crate) fn for_rpc(tx: mpsc::Sender<Token>) -> Self {
        Self {
            rpc: true,
            ..Self::new(tx)
        }
    }

    /// Pushes one token, blocking the pool thread while the channel is full. A closed
    /// channel means the connection task is gone (client left, write failed): an
    /// internal error, reported as such by `run_batch`.
    fn send(&self, token: Token) -> SqlResult<()> {
        self.tx
            .blocking_send(token)
            .map_err(|_| InternalError::Bug("result channel closed by the connection task".into()))
            .map_err(SqlError::from)
    }

    fn done_fields(
        &self,
        rowcount: Option<u64>,
        more: bool,
        proc_close: bool,
    ) -> (DoneStatus, u16) {
        let mut status = DoneStatus::FINAL;
        if more {
            status |= DoneStatus::MORE;
        }
        if rowcount.is_some() {
            status |= DoneStatus::COUNT;
        }
        if self.error_since_done {
            status |= DoneStatus::ERROR;
        }
        let cur_cmd = if proc_close {
            CUR_CMD_PROC
        } else if let Some(cur_cmd) = self.next_cur_cmd {
            cur_cmd
        } else if self.columns_since_done {
            CUR_CMD_SELECT
        } else {
            0
        };
        (status, cur_cmd)
    }
}

impl ResultSink for TdsSink {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.columns_since_done = true;
        self.send(Token::ColMetaData(cols.to_vec()))
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.send(Token::Row(row.to_vec()))
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        let (status, cur_cmd) = self.done_fields(rowcount, more, false);
        self.error_since_done = false;
        self.columns_since_done = false;
        self.next_cur_cmd = None;
        let token = if self.rpc {
            Token::DoneProc {
                status,
                cur_cmd,
                row_count: rowcount,
            }
        } else {
            Token::Done {
                status,
                cur_cmd,
                row_count: rowcount,
            }
        };
        self.send(token)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        let (status, cur_cmd) = self.done_fields(rowcount, more, false);
        self.error_since_done = false;
        self.columns_since_done = false;
        self.next_cur_cmd = None;
        self.send(Token::DoneInProc {
            status,
            cur_cmd,
            row_count: rowcount,
        })
    }

    fn done_proc(&mut self, rowcount: Option<u64>) -> SqlResult<()> {
        let (status, cur_cmd) = self.done_fields(rowcount, false, true);
        self.error_since_done = false;
        self.columns_since_done = false;
        self.next_cur_cmd = None;
        self.send(Token::DoneProc {
            status,
            cur_cmd,
            row_count: rowcount,
        })
    }

    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.send(Token::Info(msg.clone()))
    }

    fn next_cur_cmd(&mut self, cur_cmd: u16) -> SqlResult<()> {
        self.next_cur_cmd = Some(cur_cmd);
        Ok(())
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.error_since_done = true;
        self.send(Token::Error(err.clone()))
    }

    fn env_change(&mut self, change: &EnvChange) -> SqlResult<()> {
        self.send(Token::EnvChange(change.clone()))
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.send(Token::ReturnValue {
            name: name.to_owned(),
            // Ordinal of the parameter among the OUTPUT ones; not numbered yet.
            ordinal: 0,
            ty: ty.clone(),
            value: value.clone(),
        })
    }

    fn return_status(&mut self, status: i32) -> SqlResult<()> {
        self.send(Token::ReturnStatus(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vauban_tds::ColumnFlags;
    use vauban_types::SqlType;

    /// A sink and the receiver that sees its tokens. Plain `#[test]`: `blocking_send`
    /// must not run inside a tokio runtime.
    fn sink() -> (TdsSink, mpsc::Receiver<Token>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (TdsSink::new(tx), rx)
    }

    fn int_column() -> ColumnMeta {
        ColumnMeta {
            name: String::new(),
            ty: TypeInfo::new(SqlType::Int, false),
            flags: ColumnFlags::default(),
        }
    }

    /// Every token sent so far, in order.
    fn drain(rx: &mut mpsc::Receiver<Token>) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Ok(token) = rx.try_recv() {
            tokens.push(token);
        }
        tokens
    }

    #[test]
    fn select_sequence_gets_final_count_and_cur_cmd_select() {
        let (mut sink, mut rx) = sink();
        sink.columns(&[int_column()]).unwrap();
        sink.row(&[Value::I32(1)]).unwrap();
        sink.done(Some(1), false).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![
                Token::ColMetaData(vec![int_column()]),
                Token::Row(vec![Value::I32(1)]),
                Token::Done {
                    status: DoneStatus::FINAL | DoneStatus::COUNT,
                    cur_cmd: CUR_CMD_SELECT,
                    row_count: Some(1),
                },
            ]
        );
    }

    #[test]
    fn error_then_done_gets_error_flag_and_cur_cmd_zero() {
        let (mut sink, mut rx) = sink();
        let err = SqlError::incorrect_syntax_near("SELEC", 1);
        sink.error(&err).unwrap();
        sink.done(None, false).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![
                Token::Error(err),
                Token::Done {
                    status: DoneStatus::FINAL | DoneStatus::ERROR,
                    cur_cmd: 0,
                    row_count: None,
                },
            ]
        );
    }

    #[test]
    fn done_with_more_gets_more_without_error() {
        let (mut sink, mut rx) = sink();
        sink.done(None, true).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![Token::Done {
                status: DoneStatus::MORE,
                cur_cmd: 0,
                row_count: None,
            }]
        );
    }

    #[test]
    fn error_flag_does_not_survive_a_done() {
        let (mut sink, mut rx) = sink();
        sink.error(&SqlError::incorrect_syntax_near("x", 1))
            .unwrap();
        sink.done(None, true).unwrap();
        sink.done(None, false).unwrap();
        let tokens = drain(&mut rx);
        assert_eq!(
            tokens[1],
            Token::Done {
                status: DoneStatus::MORE | DoneStatus::ERROR,
                cur_cmd: 0,
                row_count: None,
            }
        );
        assert_eq!(
            tokens[2],
            Token::Done {
                status: DoneStatus::FINAL,
                cur_cmd: 0,
                row_count: None,
            }
        );
    }

    #[test]
    fn columns_flag_does_not_survive_a_done() {
        let (mut sink, mut rx) = sink();
        sink.columns(&[int_column()]).unwrap();
        sink.done(Some(0), true).unwrap();
        sink.done(None, false).unwrap();
        let tokens = drain(&mut rx);
        assert!(matches!(
            tokens[1],
            Token::Done {
                cur_cmd: CUR_CMD_SELECT,
                ..
            }
        ));
        assert!(matches!(tokens[2], Token::Done { cur_cmd: 0, .. }));
    }

    #[test]
    fn rpc_sink_emits_doneproc() {
        let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
        let mut sink = TdsSink::for_rpc(tx);
        sink.error(&SqlError::procedure_not_found("sp_x")).unwrap();
        sink.done(None, false).unwrap();
        assert_eq!(
            drain(&mut rx)[1],
            Token::DoneProc {
                status: DoneStatus::FINAL | DoneStatus::ERROR,
                cur_cmd: 0,
                row_count: None,
            }
        );
    }

    #[test]
    fn other_calls_translate_directly() {
        let (mut sink, mut rx) = sink();
        let info = InfoMessage {
            number: 5701,
            severity: 0,
            state: 2,
            message: "Database context is now 'master'.".into(),
            line: 1,
        };
        let change = EnvChange::Language {
            old: String::new(),
            new: "us_english".into(),
        };
        let ty = TypeInfo::new(SqlType::Int, true);
        sink.info(&info).unwrap();
        sink.env_change(&change).unwrap();
        sink.return_value("@p", &ty, &Value::I32(7)).unwrap();
        sink.return_status(0).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![
                Token::Info(info),
                Token::EnvChange(change),
                Token::ReturnValue {
                    name: "@p".into(),
                    ordinal: 0,
                    ty,
                    value: Value::I32(7),
                },
                Token::ReturnStatus(0),
            ]
        );
    }

    #[test]
    fn done_in_proc_and_done_proc_match_procedure_shape() {
        let (mut sink, mut rx) = sink();
        sink.columns(&[int_column()]).unwrap();
        sink.row(&[Value::I32(42)]).unwrap();
        sink.done_in_proc(Some(1), true).unwrap();
        sink.return_status(0).unwrap();
        sink.done_proc(None).unwrap();
        let tokens = drain(&mut rx);
        assert!(matches!(
            tokens[2],
            Token::DoneInProc {
                status,
                cur_cmd: CUR_CMD_SELECT,
                row_count: Some(1),
            } if status.contains(DoneStatus::MORE | DoneStatus::COUNT)
        ));
        assert_eq!(tokens[3], Token::ReturnStatus(0));
        assert!(matches!(
            tokens[4],
            Token::DoneProc {
                status: DoneStatus::FINAL,
                cur_cmd: CUR_CMD_PROC,
                row_count: None,
            }
        ));
    }

    #[test]
    fn closed_channel_is_an_internal_error() {
        let (mut sink, rx) = sink();
        drop(rx);
        let err = sink.done(None, false).unwrap_err();
        assert_eq!(err.number, 50000);
        assert!(err.message.starts_with("Internal error: "));
    }

    /// The adapter turns the neutral calls into the tokens of a statement of a batch: the
    /// nullability of a column into `fNullable`, the end into DONE with the `more` it was
    /// given, and counts the rows.
    #[test]
    fn the_adapter_turns_a_statement_into_batch_tokens() {
        let (mut sink, mut rx) = sink();
        let column = ResultColumn {
            name: "a".into(),
            ty: TypeInfo::new(SqlType::Int, false),
            nullable: true,
        };
        let mut adapter = StatementSinkAdapter::new(&mut sink);
        adapter.columns(std::slice::from_ref(&column)).unwrap();
        adapter.row(&[Value::I32(1)]).unwrap();
        adapter.set_more(true);
        adapter.done(Some(1)).unwrap();
        assert_eq!(adapter.row_count(), 1);
        let tokens = drain(&mut rx);
        assert_eq!(
            tokens[0],
            Token::ColMetaData(vec![ColumnMeta {
                name: "a".into(),
                ty: TypeInfo::new(SqlType::Int, false),
                flags: ColumnFlags {
                    nullable: true,
                    ..ColumnFlags::default()
                },
            }])
        );
        assert_eq!(
            tokens[2],
            Token::Done {
                status: DoneStatus::MORE | DoneStatus::COUNT,
                cur_cmd: CUR_CMD_SELECT,
                row_count: Some(1),
            }
        );
    }

    /// In a nested batch the end of a statement is a DONEINPROC with `more`.
    #[test]
    fn the_adapter_ends_a_nested_statement_with_doneinproc() {
        let (mut sink, mut rx) = sink();
        let mut adapter = StatementSinkAdapter::in_proc(&mut sink);
        adapter.done(None).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![Token::DoneInProc {
                status: DoneStatus::MORE,
                cur_cmd: 0,
                row_count: None,
            }]
        );
    }

    /// An error and an environment change go out as ERROR and ENVCHANGE, and the error
    /// flags the DONE that follows.
    #[test]
    fn the_adapter_sends_errors_and_environment_changes() {
        let (mut sink, mut rx) = sink();
        let err = SqlError::incorrect_syntax_near("x", 1);
        let mut adapter = StatementSinkAdapter::new(&mut sink);
        adapter
            .env_change(&EnvironmentChange::Database {
                old: "master".into(),
                new: "tempdb".into(),
            })
            .unwrap();
        adapter
            .env_change(&EnvironmentChange::TransactionBegun(3))
            .unwrap();
        adapter
            .env_change(&EnvironmentChange::TransactionCommitted(3))
            .unwrap();
        adapter
            .env_change(&EnvironmentChange::TransactionRolledBack(4))
            .unwrap();
        adapter
            .env_change(&EnvironmentChange::Language {
                old: String::new(),
                new: "us_english".into(),
            })
            .unwrap();
        adapter.error(&err).unwrap();
        adapter.done(None).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![
                Token::EnvChange(EnvChange::Database {
                    old: "master".into(),
                    new: "tempdb".into(),
                }),
                Token::EnvChange(EnvChange::BeginTransaction(3)),
                Token::EnvChange(EnvChange::CommitTransaction(3)),
                Token::EnvChange(EnvChange::RollbackTransaction(4)),
                Token::EnvChange(EnvChange::Language {
                    old: String::new(),
                    new: "us_english".into(),
                }),
                Token::Error(err),
                Token::Done {
                    status: DoneStatus::FINAL | DoneStatus::ERROR,
                    cur_cmd: 0,
                    row_count: None,
                },
            ]
        );
    }
}

//! `ResultSink` trait and its TDS implementation, fed through a channel from the blocking
//! pool.
//!
//! The engine is synchronous and runs on the blocking pool of tokio; the connection task
//! is asynchronous. `TdsSink` is the bridge: every call of the trait becomes one `Token`
//! pushed into a bounded `mpsc` channel with `blocking_send`, and the connection task
//! drains the channel into the `TdsStream`. When the client reads slowly the channel fills
//! up and `blocking_send` blocks the pool thread, never the async loop: that is the
//! intended back-pressure.

use tokio::sync::mpsc;
use vauban_errors::{InfoMessage, InternalError, SqlError, SqlResult};
use vauban_tds::{ColumnMeta, DoneStatus, EnvChange, Token};
use vauban_types::{TypeInfo, Value};

/// Capacity of the token channel between the blocking pool and the connection task.
pub(crate) const CHANNEL_CAPACITY: usize = 256;

/// `CurCmd` of the DONE token that closes a SELECT, as SQL Server sends it.
pub(crate) const CUR_CMD_SELECT: u16 = 0xC1;

/// Where the results of a statement go: implemented for TDS here, and by the recording
/// sinks of the tests.
///
/// Rule of the DONE: `done(rowcount, more)` does not carry the error flag. A call to
/// `error()` since the last `done()` makes the TDS implementation emit
/// `DoneStatus::ERROR`; `cur_cmd` is 0xC1 (SELECT) when `columns()` was emitted since the
/// last `done()`, 0 otherwise.
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
}

/// The `ResultSink` of a TDS connection: turns every call into a `Token` and sends it to
/// the connection task through a bounded channel.
///
/// The sender is owned by the sink and dropped with it, so the receiver sees the end of
/// the channel exactly when the request is over: the connection task needs no other
/// signal.
pub(crate) struct TdsSink {
    tx: mpsc::Sender<Token>,
    /// `true` for an RPC: `done()` emits DONEPROC instead of DONE.
    rpc: bool,
    /// An `error()` was emitted since the last `done()`.
    error_since_done: bool,
    /// A `columns()` was emitted since the last `done()`.
    columns_since_done: bool,
}

impl TdsSink {
    /// A sink for a SQL batch: `done()` emits `Token::Done`.
    pub(crate) fn new(tx: mpsc::Sender<Token>) -> Self {
        Self {
            tx,
            rpc: false,
            error_since_done: false,
            columns_since_done: false,
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
        let cur_cmd = if self.columns_since_done {
            CUR_CMD_SELECT
        } else {
            0
        };
        self.error_since_done = false;
        self.columns_since_done = false;
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

    fn info(&mut self, msg: &InfoMessage) -> SqlResult<()> {
        self.send(Token::Info(msg.clone()))
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
    fn closed_channel_is_an_internal_error() {
        let (mut sink, rx) = sink();
        drop(rx);
        let err = sink.done(None, false).unwrap_err();
        assert_eq!(err.number, 50000);
        assert!(err.message.starts_with("Internal error: "));
    }
}

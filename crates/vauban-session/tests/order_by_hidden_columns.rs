//! `ORDER BY` through `Session::run_batch`: a key the select list does not return is a
//! hidden column computed once per row, and the `ORDER BY` of a grouped query reads the
//! columns of its aggregation.
//!
//! The table of the grouped tests is `dbo.s (c int, x int)`, six rows: three of `c = 3`,
//! two of `c = 1`, one of `c = 2`.

use std::cmp::Ordering;
use std::sync::Arc;

use vauban_errors::{InfoMessage, SqlError, SqlResult};
use vauban_session::{Engine, ResultSink, Session, SessionState};
use vauban_storage::MemoryStorage;
use vauban_tds::{ColumnMeta, EnvChange};
use vauban_types::{Collation, TypeInfo, Value, compare};

/// What the sink keeps: the rows of the last result set, and the errors.
#[derive(Default)]
struct Recording {
    columns: Vec<Vec<ColumnMeta>>,
    rows: Vec<Vec<Value>>,
    errors: Vec<SqlError>,
}

impl ResultSink for Recording {
    fn columns(&mut self, cols: &[ColumnMeta]) -> SqlResult<()> {
        self.columns.push(cols.to_vec());
        self.rows.clear();
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }
    fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
        Ok(())
    }
    fn info(&mut self, _msg: &InfoMessage) -> SqlResult<()> {
        Ok(())
    }
    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.errors.push(err.clone());
        Ok(())
    }
    fn env_change(&mut self, _change: &EnvChange) -> SqlResult<()> {
        Ok(())
    }
    fn return_value(&mut self, _name: &str, _ty: &TypeInfo, _value: &Value) -> SqlResult<()> {
        Ok(())
    }
    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
}

/// A session on an in-memory engine, with the built-in functions registered.
fn session() -> Session {
    vauban_sysfn::register_builtins();
    Session::new(
        Arc::new(Engine::new(Arc::new(MemoryStorage::new()))),
        SessionState::new(51),
    )
}

/// Runs `text` on `session` and returns what the sink kept.
fn run(session: &mut Session, text: &str) -> Recording {
    let mut sink = Recording::default();
    session
        .run_batch(text, &mut sink)
        .expect("a batch reports its errors through the sink");
    sink
}

/// Runs `text`, which raises no error, and returns the rows of its last result set.
fn rows(session: &mut Session, text: &str) -> Vec<Vec<Value>> {
    let recording = run(session, text);
    assert!(
        recording.errors.is_empty(),
        "{text}: {:?}",
        recording.errors
    );
    recording.rows
}

/// The single `int` column of `rows`.
fn ints(rows: &[Vec<Value>]) -> Vec<i32> {
    rows.iter()
        .map(|row| match row.as_slice() {
            [Value::I32(n)] => *n,
            other => panic!("one int, got {other:?}"),
        })
        .collect()
}

/// A session holding `dbo.s`.
fn grouped_session() -> Session {
    let mut session = session();
    rows(
        &mut session,
        "CREATE TABLE dbo.s (c int, x int); \
         INSERT dbo.s VALUES (1, 10), (1, 20), (2, 5), (3, 1), (3, 2), (3, 3);",
    );
    session
}

/// A session holding `dbo.t (a int)` with forty rows.
fn forty_rows() -> Session {
    let mut session = session();
    rows(
        &mut session,
        "CREATE TABLE dbo.t (a int); DECLARE @i int = 0; \
         WHILE @i < 40 BEGIN SET @i = @i + 1; INSERT dbo.t VALUES (@i); END;",
    );
    session
}

/// Whether each value of the single column of `rows` is ordered against the next one as
/// `wanted` asks, equality allowed.
fn ordered(rows: &[Vec<Value>], wanted: Ordering) -> bool {
    rows.windows(2).all(|pair| {
        let order = compare(&pair[0][0], &pair[1][0], &Collation::DEFAULT)
            .expect("two values of one type compare")
            .expect("no NULL");
        order == wanted || order == Ordering::Equal
    })
}

#[test]
fn an_alias_key_is_evaluated_once_per_row() {
    let mut session = forty_rows();
    for (text, wanted) in [
        ("SELECT NEWID() AS g FROM dbo.t ORDER BY g", Ordering::Less),
        (
            "SELECT NEWID() AS g FROM dbo.t ORDER BY g DESC",
            Ordering::Greater,
        ),
        ("SELECT NEWID() AS g FROM dbo.t ORDER BY 1", Ordering::Less),
        (
            "SELECT TOP 10 CAST(NEWID() AS varchar(36)) AS g FROM dbo.t ORDER BY g",
            Ordering::Less,
        ),
    ] {
        let returned = rows(&mut session, text);
        assert!(returned.len() >= 10, "{text}: {}", returned.len());
        assert!(
            ordered(&returned, wanted),
            "{text}: the rows follow the values returned: {returned:?}"
        );
    }
}

#[test]
fn a_hidden_key_is_not_returned() {
    let mut session = grouped_session();
    let recording = run(&mut session, "SELECT c FROM dbo.s ORDER BY x DESC");
    assert!(recording.errors.is_empty(), "{:?}", recording.errors);
    assert_eq!(recording.columns.last().map(Vec::len), Some(1));
    assert_eq!(ints(&recording.rows), [1, 1, 2, 3, 3, 3]);
    // A key holding a correlated subquery is computed with the rows it reads.
    let correlated = rows(
        &mut session,
        "SELECT c FROM dbo.s AS o \
         ORDER BY (SELECT MIN(i.x) FROM dbo.s AS i WHERE i.c = o.c) DESC, x",
    );
    assert_eq!(ints(&correlated), [1, 1, 2, 3, 3, 3]);
}

#[test]
fn a_grouped_query_orders_its_groups() {
    let mut session = grouped_session();
    assert_eq!(
        rows(
            &mut session,
            "SELECT c, COUNT(*) n FROM dbo.s GROUP BY c ORDER BY n DESC, c"
        ),
        [
            vec![Value::I32(3), Value::I32(3)],
            vec![Value::I32(1), Value::I32(2)],
            vec![Value::I32(2), Value::I32(1)],
        ]
    );
    for (text, wanted) in [
        (
            "SELECT c FROM dbo.s GROUP BY c ORDER BY COUNT(*)",
            [2, 1, 3],
        ),
        (
            "SELECT c FROM dbo.s GROUP BY c ORDER BY COUNT(*) DESC",
            [3, 1, 2],
        ),
        (
            "SELECT c FROM dbo.s GROUP BY c ORDER BY c * 2 DESC",
            [3, 2, 1],
        ),
        ("SELECT c FROM dbo.s GROUP BY c ORDER BY SUM(x)", [2, 3, 1]),
        (
            "SELECT c FROM dbo.s WHERE x > 1 GROUP BY c HAVING COUNT(*) >= 1 ORDER BY MAX(x)",
            [3, 2, 1],
        ),
    ] {
        assert_eq!(ints(&rows(&mut session, text)), wanted, "{text}");
    }
    assert_eq!(
        ints(&rows(
            &mut session,
            "SELECT TOP 1 WITH TIES c FROM dbo.s GROUP BY c ORDER BY COUNT(*) DESC"
        )),
        [3]
    );
    assert_eq!(
        rows(&mut session, "SELECT 1 FROM dbo.s ORDER BY COUNT(*)"),
        [vec![Value::I32(1)]]
    );
}

#[test]
fn a_column_outside_the_group_by_in_the_order_by_is_8127() {
    let mut session = grouped_session();
    let recording = run(&mut session, "SELECT c FROM dbo.s GROUP BY c ORDER BY x");
    let [error] = recording.errors.as_slice() else {
        panic!("one error: {:?}", recording.errors)
    };
    assert_eq!((error.number, error.severity, error.state), (8127, 16, 1));
    assert!(recording.rows.is_empty());
}

/// A session holding `dbo.k (a int, b int, t varchar(5))`: `b` runs opposite to `a`, and
/// `t` follows `a` from `a` to `e`. The rows are inserted out of order.
fn accumulating_session() -> Session {
    let mut session = session();
    rows(
        &mut session,
        "CREATE TABLE dbo.k (a int, b int, t varchar(5)); \
         INSERT dbo.k VALUES (3, 3, 'c'), (1, 5, 'a'), (5, 1, 'e'), (2, 4, 'b'), (4, 2, 'd');",
    );
    session
}

#[test]
fn an_assignment_accumulates_in_key_order() {
    let mut session = accumulating_session();
    for (text, wanted) in [
        (
            "DECLARE @s varchar(50) = ''; SELECT @s = @s + t FROM dbo.k ORDER BY t; SELECT @s;",
            "abcde",
        ),
        (
            "DECLARE @s varchar(50) = ''; SELECT @s = @s + t FROM dbo.k ORDER BY t DESC; \
             SELECT @s;",
            "edcba",
        ),
        // A key the assignment does not read, ascending and descending.
        (
            "DECLARE @s varchar(50) = ''; SELECT @s = @s + t FROM dbo.k ORDER BY b; SELECT @s;",
            "edcba",
        ),
        (
            "DECLARE @s varchar(50) = ''; SELECT @s = @s + t FROM dbo.k ORDER BY b DESC; \
             SELECT @s;",
            "abcde",
        ),
        (
            "DECLARE @s varchar(50) = ''; SELECT TOP 3 @s = @s + t + ',' FROM dbo.k \
             ORDER BY b; SELECT @s;",
            "e,d,c,",
        ),
    ] {
        let returned = rows(&mut session, text);
        assert_eq!(
            returned,
            [vec![Value::String(vauban_types::SqlString {
                text: wanted.to_owned()
            })]],
            "{text}"
        );
    }
    for (text, wanted) in [
        (
            "DECLARE @v int = 0; SELECT @v = @v * 10 + a FROM dbo.k ORDER BY a DESC; SELECT @v;",
            54321,
        ),
        (
            "DECLARE @v int = 0; SELECT @v = @v * 10 + a FROM dbo.k ORDER BY b; SELECT @v;",
            54321,
        ),
        (
            "DECLARE @v int = 0; SELECT @v = @v * 10 + a FROM dbo.k ORDER BY t; SELECT @v;",
            12345,
        ),
    ] {
        assert_eq!(ints(&rows(&mut session, text)), [wanted], "{text}");
    }
}

//! Named constructors for the errors other modules raise most often.
//!
//! Each constructor fills number, severity (from the catalogue), state and the final
//! message (template with arguments substituted by `format::from_catalog`). Parameter
//! count and order follow the catalogue template.
//!
//! States are the ones SQL Server sends for the same situation; the rustdoc of each
//! constructor says which shape sends which state. Default state: 1.
//!
//! Extending the list: a module that needs another catalogued error adds its constructor
//! here, with the tests that pin its state.

use crate::SqlError;
use crate::format::{Arg, from_catalog, from_catalog_with_severity};

/// The width, in bytes, error 168 prints for a floating-point value: a `float` is a
/// double, so `SELECT 1e400;` prints `8 bytes`.
const FLOAT_BYTES: i64 = 8;

/// The state of an overflow whose (source, target) pair has no row of its own.
const DEFAULT_OVERFLOW_STATE: u8 = 2;

/// The states of error 220, keyed by (source, target).
///
/// The state indexes the conversion routine, not the target: `int` and `money` towards
/// the same `smallint` send 1 and 7 (`SELECT CAST(CAST(<v> AS <source>) AS <target>);`).
/// A pair that is not here takes [`DEFAULT_OVERFLOW_STATE`].
const OVERFLOW_220_STATES: &[(&str, &str, u8)] = &[
    ("int", "tinyint", 2),
    ("smallint", "tinyint", 2),
    ("int", "smallint", 1),
    ("smallmoney", "smallint", 5),
    ("money", "smallint", 7),
];

/// The states of error 232, keyed by (source, target).
///
/// Same reading as [`OVERFLOW_220_STATES`]: `float` and `money` towards the same `tinyint`
/// send 1 and 11. `float` and `real` share their states.
const OVERFLOW_232_STATES: &[(&str, &str, u8)] = &[
    ("float", "tinyint", 1),
    ("real", "tinyint", 1),
    ("float", "smallint", 2),
    ("real", "smallint", 2),
    ("float", "int", 3),
    ("real", "int", 3),
    ("float", "smallmoney", 2),
    ("real", "smallmoney", 2),
    ("float", "money", 2),
    ("float", "real", 2),
    ("money", "tinyint", 11),
];

/// The state of error 220 or 232 for a (source, target) pair, [`DEFAULT_OVERFLOW_STATE`]
/// when the pair has no row.
fn overflow_state(table: &[(&str, &str, u8)], from: &str, to: &str) -> u8 {
    table
        .iter()
        .find(|(source, target, _)| *source == from && *target == to)
        .map_or(DEFAULT_OVERFLOW_STATE, |(_, _, state)| *state)
}

/// The state of error 8115 for a (source, target) pair.
///
/// Four families, and [`DEFAULT_OVERFLOW_STATE`] for the rest:
/// - towards `numeric`: 6 from `float` or `real`, 8 from another source
///   (`SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));` and
///   `SELECT CAST('1234.5' AS decimal(5,2));`);
/// - from `numeric` towards `money` or `smallmoney`: 4
///   (`SELECT CAST(CAST(99999999999999999 AS numeric(20,0)) AS money);`);
/// - from `numeric` towards the narrow character family: 5
///   (`DECLARE @n numeric(20,0) = 99999999999999999; SELECT CAST(@n AS varchar(3));`, and
///   the same towards `char(3)`); the wide family keeps 2 (`... AS nvarchar(3));`);
/// - anything else: 2, where the source prints `expression`
///   (`SELECT CAST(CAST(3000000000 AS bigint) AS int);`).
fn overflow_8115_state(from: &str, to: &str) -> u8 {
    match (from, to) {
        ("float" | "real", "numeric") => 6,
        (_, "numeric") => 8,
        ("numeric", "money" | "smallmoney") => 4,
        ("numeric", "varchar" | "char") => 5,
        _ => DEFAULT_OVERFLOW_STATE,
    }
}

/// The states of error 8114 for a conversion written in an expression, keyed by
/// (source, target).
///
/// - a character source towards `datetimeoffset` sends 31, at scale 7 as at scale 0,
///   from a `CAST`, a `CONVERT`, a `DECLARE` initialiser or an `INSERT`
///   (`SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));`); the value has
///   to be out of range to reach 8114, a malformed string raises 241;
/// - the other targets (`bigint`, `float`, `real`, `numeric`, `varbinary`, and the
///   integer and money types a parameter reaches as 8114 where a plain `CAST` answers 245
///   or 8115) send [`DEFAULT_CONVERTING_STATE`].
///
/// The path counts as well as the pair: a value bound by the caller to a parameter
/// declared `datetimeoffset` (`EXEC sp_executesql N'SELECT @a', N'@a datetimeoffset(7)',
/// @a = '0001-01-01T01:59:59+02:00';`, or `EXEC #p @a = '...';`) sends
/// [`DEFAULT_CONVERTING_STATE`], with the same message; a parameter left to its default
/// value, or a `CAST` inside the text of `sp_executesql`, is an expression and sends 31.
/// This table covers the conversions written in an expression, which `vauban-types`
/// performs; a caller that converts a bound parameter value needs its own state.
///
/// Among the date and time targets, `datetimeoffset` is the one that reaches 8114:
/// `date`, `time`, `datetime` and `datetime2` answer 241 or 242, `smalldatetime` answers
/// 295 or 242.
///
/// The fixed and variable narrow forms share a row because the message prints the
/// variable name on the expression path: `CAST(CAST('...' AS char(30)) AS datetimeoffset(7))`
/// names `varchar`, and an `nchar` source names `nvarchar`. The table carries `char` and
/// `nchar` because the caller passes a bare type name, the same reason
/// [`overflow_8115_state`] matches `"varchar" | "char"`. On the parameter path the server
/// prints the fixed-length name as written (`char`, `nchar`), with state 5.
///
/// A pair that is not here takes [`DEFAULT_CONVERTING_STATE`].
const CONVERTING_8114_STATES: &[(&str, &str, u8)] = &[
    ("varchar", "datetimeoffset", 31),
    ("nvarchar", "datetimeoffset", 31),
    ("char", "datetimeoffset", 31),
    ("nchar", "datetimeoffset", 31),
];

/// The state error 8114 carries for a (source, target) pair without a row of its own.
const DEFAULT_CONVERTING_STATE: u8 = 5;

/// The state of error 8114 for a (source, target) pair converted in an expression,
/// [`DEFAULT_CONVERTING_STATE`] when the pair has no row.
fn converting_state(from: &str, to: &str) -> u8 {
    CONVERTING_8114_STATES
        .iter()
        .find(|(source, target, _)| *source == from && *target == to)
        .map_or(DEFAULT_CONVERTING_STATE, |(_, _, state)| *state)
}

/// The states of error 536, keyed by the function name.
///
/// The state is a property of the calling function, not of the number: `LEFT` and `RIGHT`
/// send 6, `SUBSTRING` sends 8 (`SELECT LEFT('abc', -1);`, `SELECT RIGHT('abc', -1);`,
/// `SELECT SUBSTRING('abc', 1, -1);`). A function that is not here takes
/// [`DEFAULT_LENGTH_PARAMETER_STATE`].
const LENGTH_PARAMETER_536_STATES: &[(&str, u8)] = &[("left", 6), ("right", 6), ("substring", 8)];

/// The state error 536 carries for a function without a row of its own.
const DEFAULT_LENGTH_PARAMETER_STATE: u8 = 1;

/// The state of error 536 for `function`, [`DEFAULT_LENGTH_PARAMETER_STATE`] when the
/// function has no row.
///
/// The comparison ignores the case: the message prints the function in lower case, so
/// `LEFT` and `left` are the same code path.
fn length_parameter_state(function: &str) -> u8 {
    LENGTH_PARAMETER_536_STATES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(function))
        .map_or(DEFAULT_LENGTH_PARAMETER_STATE, |(_, state)| *state)
}

/// The states of error 237, keyed by the target type.
///
/// Unlike the states of 220, 232 and 8115, this one is a function of the target alone,
/// the source being `money` (`SELECT CAST(CAST(300000 AS money) AS <target>);`, and
/// `99999999999` towards `int`). A target that is not here takes
/// [`DEFAULT_RESULT_SPACE_STATE`].
const RESULT_SPACE_237_STATES: &[(&str, u8)] = &[
    ("int", 1),
    ("smallint", 2),
    ("tinyint", 3),
    ("smallmoney", 3),
];

/// The state error 237 carries for a target without a row of its own.
const DEFAULT_RESULT_SPACE_STATE: u8 = 1;

/// The state of error 237 for `to`, [`DEFAULT_RESULT_SPACE_STATE`] when the target has no
/// row.
fn result_space_state(to: &str) -> u8 {
    RESULT_SPACE_237_STATES
        .iter()
        .find(|(target, _)| *target == to)
        .map_or(DEFAULT_RESULT_SPACE_STATE, |(_, state)| *state)
}

/// The states of error 137, keyed by the role the undeclared variable plays in the
/// statement.
///
/// The state is a property of the form, not of the number, the way the state of 536 is a
/// property of the calling function: a statement that assigns the variable sends 1
/// (`SELECT @x = 1;`, `SET @x = 1;`), one that reads it sends 2 (`SELECT @x;`,
/// `PRINT @x;`, `SELECT 1 WHERE @x = 1;`).
///
/// The role is the variable's, not the statement's: `SELECT @x = @y;` names `@y` and sends
/// state 2, because `@y` is read there. When a statement both reads and assigns, the read is
/// diagnosed first: `SET @x += 1;` sends 1 but `SET @x = @x + 1;` sends 2.
const SCALAR_VARIABLE_137_STATES: &[(ScalarVariableRole, u8)] = &[
    (ScalarVariableRole::Assigned, 1),
    (ScalarVariableRole::Read, 2),
];

/// The role an undeclared variable plays in the statement that names it, which fixes the
/// state of error 137 ([`SCALAR_VARIABLE_137_STATES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarVariableRole {
    /// An assignment clause stores into the variable (`SELECT @x = 1;`, `SET @x = 1;`,
    /// `UPDATE #t SET @x = c;`). Narrower than "the statement assigns it": `EXEC @x = sp_who;`,
    /// `FETCH NEXT FROM c INTO @x;` and `@o = @x OUTPUT` store into `@x` and send state 2;
    /// an assignment clause sends 1.
    Assigned,
    /// The statement reads the variable's value (`SELECT @x;`, `PRINT @x;`).
    Read,
}

/// The state of error 137 for `role`, from [`SCALAR_VARIABLE_137_STATES`].
///
/// Both roles have a row, so the lookup finds one; an unreachable default would be dead
/// code, hence the `Read` state as the fallback of the search.
fn scalar_variable_state(role: ScalarVariableRole) -> u8 {
    SCALAR_VARIABLE_137_STATES
        .iter()
        .find(|(known, _)| *known == role)
        .map_or(2, |(_, state)| *state)
}

/// The states of error 506, keyed by the width of the `LIKE` predicate that carries the
/// invalid `ESCAPE`.
///
/// The state is a property of the whole predicate, not of the operand the message quotes:
/// a predicate whose three operands are narrow sends 1, and one where one of the three is
/// Unicode sends 2:
///
/// | query                                                           | state |
/// |-----------------------------------------------------------------|:-----:|
/// | `SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';`                        | 1 |
/// | `SELECT 1 WHERE N'a' LIKE 'a' ESCAPE 'ab';`                       | 2 |
/// | `SELECT 1 WHERE 'a' LIKE N'a' ESCAPE 'ab';`                       | 2 |
/// | `SELECT 1 WHERE 'a' LIKE 'a' ESCAPE N'ab';`                       | 2 |
///
/// The escape operand alone flips the state while the value and the pattern stay narrow,
/// so the rule is "one of the three operands is Unicode", not "the value or the pattern
/// is"; `CAST(N'!' AS nchar(3))` as the escape sends 2 against 1 for `CAST('!' AS char(3))`,
/// and a Unicode value read from a source sends 2 as well. `NOT LIKE` sends the same states
/// as `LIKE`, and an empty `ESCAPE ''` follows the same rule (1 narrow, 2 Unicode).
const ESCAPE_506_STATES: &[(LikePredicateWidth, u8)] = &[
    (LikePredicateWidth::Narrow, 1),
    (LikePredicateWidth::Unicode, 2),
];

/// The width of a `LIKE` predicate, which fixes the state of error 506
/// ([`ESCAPE_506_STATES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikePredicateWidth {
    /// The value, the pattern and the escape operand are all non-Unicode.
    Narrow,
    /// At least one of the three operands is Unicode.
    Unicode,
}

/// The state of error 506 for `width`, from [`ESCAPE_506_STATES`].
///
/// Both widths have a row, so the lookup finds one; an unreachable default would be dead
/// code, hence the narrow state as the fallback of the search.
fn escape_state(width: LikePredicateWidth) -> u8 {
    ESCAPE_506_STATES
        .iter()
        .find(|(known, _)| *known == width)
        .map_or(1, |(_, state)| *state)
}

/// The number of prefixes a name may carry, which error 117 prints in its `%d`.
///
/// SQL Server counts `server.database.schema` and refuses a fourth prefix: the `%d`
/// prints 3 for `SELECT a.b.c.d.*;` and for `EXEC a.b.c.d.e;` alike.
const MAX_NAME_PREFIXES: i64 = 3;

/// The severity error 1062 travels with, where the catalogue holds 16:
/// `SELECT TOP (1) WITH TIES 1 AS c;` answers severity 15, state 1.
const WITH_TIES_1062_SEVERITY: u8 = 15;

/// The state of error 131 for a `%S_MSG` filling.
///
/// `type` sends 3 (`DECLARE @v varchar(9000);`) and `column` sends 2
/// (`CREATE TABLE #t(c varchar(9000));`), both with severity 15. The third filling,
/// `convert specification`, has its own constructor
/// ([`SqlError::convert_specification_size_out_of_range`]) because its severity differs
/// too. An unknown filling takes the `type` state.
fn size_out_of_range_state(kind: &str) -> u8 {
    match kind {
        "column" => 2,
        _ => 3,
    }
}

// -------------------------------------------------------------------------------------
// The states of the DDL errors, keyed by the filling of their `%S_MSG` or `%ls`.
// -------------------------------------------------------------------------------------

/// The state a DDL filling without a row of its own takes: the catalogue default.
///
/// A caller that meets another filling adds its row to [`CANNOT_DROP_3701_STATES`] or
/// [`NOT_ALLOWED_IN_TRANSACTION_226_STATES`].
const UNKNOWN_DDL_STATE: u8 = 1;

/// The state error 1801 sends when `CREATE DATABASE` names a database that is already
/// there.
///
/// Severity 16, state 3, for a system database (`CREATE DATABASE master;`, delimited or
/// not, delimiters removed from the message) as for a user database created twice in a
/// row (`database_already_exists_1801`).
const DATABASE_EXISTS_1801_STATE: u8 = 3;

/// The state error 2714 sends for the `CREATE TABLE` of a **permanent** table.
///
/// The state follows the creating statement, not the object already in place:
/// `CREATE TABLE t` over an existing table or an existing view sends 6, `CREATE VIEW t`
/// or `CREATE PROCEDURE t` over an existing table sends 3, and a temporary table
/// (`CREATE TABLE #tt`, `##g`) sends 1. A caller that creates a view, a procedure or a
/// temporary table needs its own state, the way 131 and 2717 have one constructor per
/// context.
const OBJECT_EXISTS_2714_TABLE_STATE: u8 = 6;

/// The states of error 3701, keyed by the `%S_MSG` that names the object kind.
///
/// Severity 11 for each kind; the state follows the kind: table, view, procedure,
/// function, trigger, sequence and synonym send 5, a database sends 1, an index over an
/// existing table sends 7.
///
/// The `%.*ls` of an index carries the table and the index name:
/// `DROP INDEX ix_nope ON t3;` prints `'t3.ix_nope'`, `DROP INDEX ix_nope ON dbo.t6;`
/// prints `'dbo.t6.ix_nope'`. For the other kinds it is the object name as written, with
/// its delimiters removed, and a `DROP` of several names reports the first that misses.
///
/// For an index the table counts too: a `DROP INDEX` whose table is missing
/// (`DROP INDEX ix_nope ON nope_table;`, the legacy `DROP INDEX t4.ix_nope;`) sends 6
/// instead of 7, so a caller that drops an index from a table it did not find needs its
/// own state. The table is keyed on the kind because that is what
/// [`SqlError::cannot_drop`] receives.
///
/// Two kinds share the wording without sharing the number: `DROP SCHEMA nope_s;` answers
/// 15151 severity 16, and `DROP TYPE nope_ty;` answers 218 with another sentence. Neither
/// is catalogued, so a caller that needs one does not reuse this constructor.
///
/// A system database is a neighbour with a number and a sentence of its own:
/// `DROP DATABASE master;` answers 3708, not 3701
/// ([`SqlError::cannot_drop_system_database`]). A system object stays on 3701 state 5
/// (`DROP TABLE sys.objects;`, `DROP VIEW sys.tables;`, `DROP PROCEDURE sys.sp_executesql;`).
const CANNOT_DROP_3701_STATES: &[(&str, u8)] = &[
    ("table", 5),
    ("view", 5),
    ("procedure", 5),
    ("function", 5),
    ("trigger", 5),
    ("sequence", 5),
    ("synonym", 5),
    ("database", 1),
    ("index", 7),
];

/// The states of error 226, keyed by the `%ls` statement name.
///
/// `CREATE DATABASE` sends 5 under a plain `BEGIN TRANSACTION`, under two nested ones and
/// under `BEGIN TRANSACTION tx_name` alike; `ALTER DATABASE ... SET RECOVERY SIMPLE` sends
/// 6 and `ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 1` sends 7, each printing its
/// own statement name in the `%ls`.
///
/// `DROP DATABASE` is not a filling of 226: inside a transaction it answers 574 severity
/// 16 state 0, on an existing database and on a missing one alike, and so does
/// `CREATE FULLTEXT CATALOG` ([`SqlError::drop_database_in_transaction`]). `CREATE SCHEMA`
/// inside a transaction succeeds, so these states are not a blanket rule about DDL in a
/// transaction.
const NOT_ALLOWED_IN_TRANSACTION_226_STATES: &[(&str, u8)] = &[
    ("CREATE DATABASE", 5),
    ("ALTER DATABASE", 6),
    ("ALTER DATABASE SCOPED CONFIGURATION", 7),
];

/// The state error 3708 sends when a `DROP DATABASE` names a system database.
///
/// Severity 16, state 4, for `master`, `[MaStEr]` and `MASTER` alike; a list reports the
/// system name it meets
/// (`DROP DATABASE vauban_a, master;` names `master`).
const CANNOT_DROP_SYSTEM_DATABASE_3708_STATE: u8 = 4;

/// The state error 574 sends for a statement written inside a user transaction.
///
/// Severity 16, state 0, not the catalogue default of 1: `DROP DATABASE` over a missing
/// database, over an existing one and over `master`, under a plain, a named or a nested
/// `BEGIN TRANSACTION`, and `CREATE FULLTEXT CATALOG` alike.
const DROP_DATABASE_IN_TRANSACTION_574_STATE: u8 = 0;

/// The state error 2705 sends when a `CREATE TABLE` names the same column twice.
///
/// Severity 16, state 3: `CREATE TABLE t_dup (a int, a int);`, with a column between the
/// two copies, with three copies, with a delimited copy (`[a] int, a int`), with a copy in
/// another case (`a int, A int`), for a qualified, delimited or temporary table, for a
/// table variable (`DECLARE @t TABLE (a int, a int);`), and when the repeated column
/// also names a type that does not exist.
///
/// The state follows the statement: `ALTER TABLE t_alter ADD a int;` over a table that
/// already has `a` sends the same sentence at state 4, so a caller that adds a column
/// needs its own constructor, the way 131 and 2717 have one per context.
const DUPLICATE_COLUMN_2705_CREATE_TABLE_STATE: u8 = 3;

/// The state error 2715 sends when the type is unknown in the column list of a table.
///
/// Severity 16, state 6: `CREATE TABLE t_unknown (a foo);`, `(a int, b foo)` (which
/// prints `#2`), `(a dbo.foo)` (which prints the qualified name),
/// `DECLARE @t TABLE (a foo);`, and a column list where the unknown type comes before a
/// repeated column.
///
/// It is the column list that moves the state, not the `CREATE`: `DECLARE @x foo;` and a
/// procedure parameter `@a foo` send 3, the state of
/// [`SqlError::cannot_find_data_type`]. A table variable is on the 6 side and a procedure
/// parameter on the 3 side.
const CANNOT_FIND_DATA_TYPE_2715_COLUMN_LIST_STATE: u8 = 6;

/// The state error 2715 sends for a scalar declaration or a routine parameter
/// (`DECLARE @x foo;`, `CREATE PROCEDURE p (@a foo) AS SELECT 1;`): severity 16, state 3.
const CANNOT_FIND_DATA_TYPE_2715_DECLARATION_STATE: u8 = 3;

/// The state of `key` in `table`, [`UNKNOWN_DDL_STATE`] for a filling without a row.
fn ddl_state(table: &[(&str, u8)], key: &str) -> u8 {
    table
        .iter()
        .find(|(known, _)| *known == key)
        .map_or(UNKNOWN_DDL_STATE, |(_, state)| *state)
}

// -------------------------------------------------------------------------------------
// DML, flow-control, transaction and concurrency errors.
// -------------------------------------------------------------------------------------

/// The severity error 116 travels with, where the catalogue holds 15.
///
/// `SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);` and
/// `SELECT a FROM dbo.t2 WHERE a IN (SELECT a, b FROM dbo.t2);` send severity 16 state 1;
/// `WHERE EXISTS (SELECT a, b FROM dbo.t2)` returns rows instead, the exception the
/// sentence itself names.
const ONLY_ONE_EXPRESSION_116_SEVERITY: u8 = 16;

/// The state error 1222 sends while it waits for a lock on a row.
///
/// Severity 16, state 51, for `SELECT v FROM dbo.s WHERE k = 1;` and
/// `UPDATE dbo.s SET v = v + 5 WHERE k = 1;` under `SET LOCK_TIMEOUT 2000;` while another
/// session holds an uncommitted `UPDATE` of the same row.
const LOCK_TIMEOUT_1222_ROW_STATE: u8 = 51;

/// The state error 1222 sends while it waits for a lock on the object.
///
/// Severity 16, state 56, for `ALTER TABLE dbo.s ADD zz int NULL;`, `DROP TABLE dbo.s;`
/// and `SELECT v FROM dbo.s WITH (TABLOCKX) WHERE k = 2;` against the same holder. The
/// granularity of the wait moves the state, same sentence and same severity, so 1222 gets
/// one constructor per granularity, the way 131 and 2717 have one per context.
const LOCK_TIMEOUT_1222_OBJECT_STATE: u8 = 56;

// -------------------------------------------------------------------------------------
// Index, constraint, identifier and name resolution errors.
// -------------------------------------------------------------------------------------

/// The states error 3723 sends, by the kind of constraint the dropped index enforces.
///
/// `DROP INDEX pk ON dbo.t;` over the index of a `PRIMARY KEY` sends severity 16 state 4,
/// and the same statement over the index of a `UNIQUE` constraint sends state 5, with
/// `UNIQUE KEY` where the first prints `PRIMARY KEY`. The kind the caller passes is the
/// one the message prints, so the state is read from it, the way 536 reads its state from
/// the function name.
const DROP_INDEX_3723_STATES: &[(&str, u8)] = &[("PRIMARY KEY", 4), ("UNIQUE KEY", 5)];

/// The state error 1909 sends when the repeated column is in the key list.
///
/// Severity 16, state 1: `CREATE INDEX ix ON dbo.t (a, a);`, the same index written
/// `(a ASC, A DESC)` (which prints the last spelling, `'A'`), a `PRIMARY KEY (a, a)` or
/// a `UNIQUE (b, b)` of a `CREATE TABLE`, and
/// `ALTER TABLE dbo.t ADD CONSTRAINT pk PRIMARY KEY (a, a);`.
const DUPLICATE_COLUMN_1909_KEY_LIST_STATE: u8 = 1;

/// The state error 1909 sends when the repeated column is in the `INCLUDE` list.
///
/// Severity 16, state 2, with the same sentence as the key list:
/// `CREATE INDEX ix ON dbo.t (a) INCLUDE (a);` and `INCLUDE (b, b)`. The list the
/// repetition sits in moves the state, so 1909 gets one constructor per list, the way
/// 1222 has one per lock granularity
/// (`tests::duplicate_column_1909_has_one_state_per_column_list`).
const DUPLICATE_COLUMN_1909_INCLUDE_LIST_STATE: u8 = 2;

/// The state error 8168 sends when one statement creates two constraints of one name.
///
/// Severity 16, state 0: a `CREATE TABLE` holding `CONSTRAINT c1 PRIMARY KEY` beside
/// `CONSTRAINT c1 UNIQUE`, a `CREATE TABLE` holding two `CONSTRAINT c1 CHECK` clauses,
/// and the `ALTER TABLE dbo.t ADD` of those same pairs.
const DUPLICATE_NAME_8168_CONSTRAINT_STATE: u8 = 0;

/// The state error 8168 sends when one `CREATE TABLE` holds two indexes of one name.
///
/// Severity 16, state 1, with the sentence and the argument the constraint state prints:
/// `CREATE TABLE dbo.t2 (a int NOT NULL INDEX ix (a), b int NOT NULL, INDEX ix (b));`,
/// with both indexes written in the column definitions, or both at table level. The kind
/// of object the repeated name designates moves the state, so 8168 gets one constructor
/// per kind, the way 1909 has one per column list
/// (`tests::duplicate_name_8168_has_one_state_per_context`).
const DUPLICATE_NAME_8168_INDEX_STATE: u8 = 1;

/// The state error 103 sends for an identifier longer than its maximum.
///
/// Severity 15, state 4, with `identifier` as the `%S_MSG`: `SELECT 1 AS "<129 b>` left
/// unclosed, `SELECT 1 AS [<300 b>` left unclosed, the closed `SELECT 1 AS "<129 b>";`,
/// and `SELECT 1 FROM dbo.<200 c>;` on a name that is not an alias. The message prints
/// the first 128 characters of the token and the maximum, 128.
const IDENTIFIER_TOO_LONG_103_STATE: u8 = 4;

/// The state error 103 sends for a number literal longer than its maximum:
/// `SELECT 1 x <200 digits>;` sends severity 15 state 5 with `number` as the `%S_MSG`.
const NUMBER_TOO_LONG_103_STATE: u8 = 5;

/// The state error 103 sends for a money literal longer than its maximum.
///
/// `SELECT 1 x $<130 digits>;` sends severity 15 state 6, with the same `number`
/// `%S_MSG` and the same sentence as the plain literal of [`NUMBER_TOO_LONG_103_STATE`]:
/// the `$` is the difference between the two literals, and the state is the field that
/// separates them (`tests::identifier_too_long_103_has_one_state_per_shape`).
const MONEY_LITERAL_TOO_LONG_103_STATE: u8 = 6;

/// The state error 447 sends for a `COLLATE` written in a column definition.
///
/// Severity 16, state 1: `CREATE TABLE dbo.t (a int COLLATE Latin1_General_CI_AS NOT NULL);`,
/// the same column declared `date`, and
/// `ALTER TABLE dbo.t ALTER COLUMN a int COLLATE Latin1_General_CI_AS NOT NULL;`. A
/// `COLLATE` written in an expression keeps the state 0 of
/// [`SqlError::collate_on_non_string`]: `SELECT 1 COLLATE Latin1_General_CI_AS;`, the same
/// over a declared variable, over a `CAST(... AS date)` and over a column read from a
/// table. The context moves the state, so the definition gets its own constructor
/// (`tests::collate_on_non_string_has_one_constructor_per_context`).
const COLLATE_ON_NON_STRING_447_COLUMN_STATE: u8 = 1;

/// The state error 208 sends for a name that resolves to an object of another type.
///
/// Severity 16, state 3: `SELECT 1 FROM sys.sp_executesql;`, `SELECT 1 FROM dbo.p;` over
/// a stored procedure and `SELECT 1 FROM dbo.f;` over a scalar function. A name that
/// resolves to nothing keeps the state 1 of [`SqlError::invalid_object_name`]
/// (`SELECT 1 FROM nosuchdb.dbo.t;`), and a temporary `#nosuch` sends 0.
const INVALID_OBJECT_NAME_208_OTHER_TYPE_STATE: u8 = 3;

/// The severity error 1088 travels with, where the catalogue holds 15.
///
/// `CREATE INDEX ix ON dbo.nosuch (a);`, the same as `CREATE UNIQUE INDEX`, and the same
/// over a view name that does not exist either, send severity 16 state 12.
const CANNOT_FIND_OBJECT_1088_SEVERITY: u8 = 16;

/// The severity errors 1011 and 1012 travel with, where the catalogue holds 15.
///
/// `SELECT 1 FROM dbo.t AS x, dbo.t AS x;` and `SELECT 1 FROM dbo.t AS x JOIN dbo.u AS x
/// ON 1 = 1;` send 1011 with severity 16 state 1; `SELECT 1 FROM dbo.t AS u JOIN dbo.u ON
/// 1 = 1;` sends 1012 with severity 16 state 1.
const DUPLICATE_CORRELATION_NAME_SEVERITY: u8 = 16;

/// The severity error 1013 travels with, where the catalogue holds 15.
///
/// `SELECT 1 FROM dbo.t, dbo.t;`, the same pair written as a `JOIN ... ON 1 = 1`, and
/// `SELECT t.a FROM dbo.t, s.t;`, where the two objects differ by their schema and share
/// their exposed name, send severity 16 state 1.
const SAME_EXPOSED_NAMES_1013_SEVERITY: u8 = 16;

/// The severity error 8154 travels with, where the catalogue holds 15.
///
/// A target that matches two sources of the same `FROM`
/// (`UPDATE t SET a = 1 FROM dbo.t AS x, dbo.t AS y;`,
/// `DELETE t FROM dbo.t AS x, dbo.t AS y;`) sends severity 16 state 1.
const AMBIGUOUS_TABLE_8154_SEVERITY: u8 = 16;

/// The severity error 8155 travels with, where the catalogue holds 15.
///
/// Severity 16, state 2, for the first as for the second unnamed column:
/// `SELECT * FROM (SELECT a, a + 1 FROM dbo.t) AS d;` prints column 2,
/// `SELECT * FROM (SELECT a + 1, a FROM dbo.t) AS d;` column 1.
const NO_COLUMN_NAME_8155_SEVERITY: u8 = 16;

/// The severity error 1750 travels with, where the catalogue holds 10.
///
/// The catalogued class would make it an informational message; the token sent carries
/// the class of an error: severity 16 state 0, behind the error that explains it (8111
/// for a `PRIMARY KEY` over a nullable column, 1909 for a repeated key column).
const COULD_NOT_CREATE_CONSTRAINT_1750_SEVERITY: u8 = 16;

impl SqlError {
    /// Error 102, severity 15, state 1: the parser met an
    /// unexpected token. `line` is the 1-based line of the token in the batch.
    ///
    /// ```text
    /// Syntax error near 'SELEC'.
    /// ```
    pub fn incorrect_syntax_near(token: &str, line: u32) -> Self {
        from_catalog(102, 1, &[Arg::Str(token)]).with_line(line)
    }

    /// Error 208, severity 16, state 1: a table, view or
    /// other object referenced by a statement does not exist. `name` is the name as the
    /// user wrote it (e.g. `dbo.t`).
    ///
    /// ```text
    /// Unknown object name 'dbo.t'.
    /// ```
    pub fn invalid_object_name(name: &str) -> Self {
        from_catalog(208, 1, &[Arg::Str(name)])
    }

    /// Error 215, severity 16, state 1: arguments were supplied to an object that is not
    /// a function. `name` is the object name as written, with delimiters removed:
    /// `SELECT * FROM dbo.T (1);` prints `dbo.T` and keeps its case.
    ///
    /// ```text
    /// Object 'dbo.T' is not a function and takes no parameters; a table hint needs the WITH keyword.
    /// ```
    pub fn parameters_supplied_to_non_function(name: &str) -> Self {
        from_catalog(215, 1, &[Arg::Str(name)])
    }

    /// Error 207, severity 16, state 1: a column referenced
    /// by a statement does not exist in any table in scope.
    ///
    /// ```text
    /// Unknown column name 'c'.
    /// ```
    pub fn invalid_column_name(name: &str) -> Self {
        from_catalog(207, 1, &[Arg::Str(name)])
    }

    /// Error 515, severity 16, state 2: an `INSERT` or
    /// `UPDATE` tries to store `NULL` in a `NOT NULL` column.
    ///
    /// `table` is the three-part name SQL Server prints (`db.schema.table`);
    /// `statement` is the failing statement's name, `INSERT` or `UPDATE`.
    ///
    /// The template has no separate database placeholder: the database is part of
    /// `table`, and the third placeholder is the statement name.
    ///
    /// ```text
    /// Column 'c' of table 'db.dbo.t' does not accept NULL; INSERT fails.
    /// ```
    pub fn cannot_insert_null(col: &str, table: &str, statement: &str) -> Self {
        from_catalog(
            515,
            2,
            &[Arg::Str(col), Arg::Str(table), Arg::Str(statement)],
        )
    }

    /// Error 2627, severity 14, state 1: an `INSERT` or
    /// `UPDATE` duplicates a key protected by a `PRIMARY KEY` or `UNIQUE` constraint.
    ///
    /// `constraint_kind` is `PRIMARY KEY` or `UNIQUE KEY` (the template's first `%ls`);
    /// `key` is the duplicate value as SQL Server displays it, parentheses included,
    /// e.g. `(1)`.
    ///
    /// The template has a leading placeholder for the constraint kind.
    ///
    /// ```text
    /// The PRIMARY KEY constraint 'PK_t' rejects a duplicate key in object 'dbo.t': the value (1) exists already.
    /// ```
    pub fn unique_violation(
        constraint_kind: &str,
        constraint: &str,
        table: &str,
        key: &str,
    ) -> Self {
        from_catalog(
            2627,
            1,
            &[
                Arg::Str(constraint_kind),
                Arg::Str(constraint),
                Arg::Str(table),
                Arg::Str(key),
            ],
        )
    }

    /// Error 2601, severity 14, state 1: an `INSERT` or
    /// `UPDATE` duplicates a key of a unique index that is not a constraint.
    ///
    /// `key` is the duplicate value as SQL Server displays it, parentheses included.
    ///
    /// ```text
    /// Duplicate key in object 'dbo.t' for unique index 'IX_t': the value (1) exists already.
    /// ```
    pub fn duplicate_key_index(table: &str, index: &str, key: &str) -> Self {
        from_catalog(2601, 1, &[Arg::Str(table), Arg::Str(index), Arg::Str(key)])
    }

    /// Error 547, severity 16, state 0: a DML statement
    /// violates a `FOREIGN KEY`, `REFERENCE` or `CHECK` constraint.
    ///
    /// - `statement`: `INSERT`, `UPDATE`, `DELETE`, or `MERGE`;
    /// - `constraint_kind`: `FOREIGN KEY`, `REFERENCE` or `CHECK`;
    /// - `db` and `table`: where the conflict occurred (the referenced table for a
    ///   `FOREIGN KEY`, the referencing table for a `REFERENCE`);
    /// - `column`: the conflicting column when SQL Server names one (`FOREIGN KEY` and
    ///   `REFERENCE` conflicts), `None` otherwise (`CHECK` constraints).
    ///
    /// The template ends with `%ls%.*ls%ls`: with `Some(c)` they become `, column '`, `c`
    /// and `'`; with `None` they become three empty strings.
    ///
    /// ```text
    /// INSERT violates the FOREIGN KEY constraint "FK_child_parent" in database "db", table "dbo.parent", column 'id'.
    /// INSERT violates the CHECK constraint "CK_t_positive" in database "db", table "dbo.t".
    /// ```
    pub fn fk_violation(
        statement: &str,
        constraint_kind: &str,
        constraint: &str,
        db: &str,
        table: &str,
        column: Option<&str>,
    ) -> Self {
        let (prefix, column, suffix) = match column {
            Some(column) => (", column '", column, "'"),
            None => ("", "", ""),
        };
        from_catalog(
            547,
            0,
            &[
                Arg::Str(statement),
                Arg::Str(constraint_kind),
                Arg::Str(constraint),
                Arg::Str(db),
                Arg::Str(table),
                Arg::Str(prefix),
                Arg::Str(column),
                Arg::Str(suffix),
            ],
        )
    }

    /// Error 18456, severity 14, state 1: authentication
    /// failed for `user`. The template's two trailing `%.*ls` carry server-side detail
    /// that SQL Server does not send to the client; they are substituted with empty
    /// strings so the client sees the message below.
    ///
    /// ```text
    /// Login refused for user 'sa'.
    /// ```
    pub fn login_failed(user: &str) -> Self {
        from_catalog(18456, 1, &[Arg::Str(user), Arg::Str(""), Arg::Str("")])
    }

    /// Error 911, severity 16, state 1: `USE <name>` names a
    /// database that does not exist. For a database refused at login time, see
    /// [`cannot_open_database`](Self::cannot_open_database).
    ///
    /// ```text
    /// No database named 'nope' exists; check the spelling of the name.
    /// ```
    pub fn database_not_found(name: &str) -> Self {
        from_catalog(911, 1, &[Arg::Str(name)])
    }

    /// Error 4060, severity 11, state 1: the database
    /// requested in the LOGIN7 packet cannot be opened (does not exist, offline, no
    /// access). Emitted during connection, before any batch; for `USE` see
    /// [`database_not_found`](Self::database_not_found).
    ///
    /// ```text
    /// The database "nope" requested at login could not be opened; login refused.
    /// ```
    pub fn cannot_open_database(db: &str) -> Self {
        from_catalog(4060, 1, &[Arg::Str(db)])
    }

    /// Error 2812, severity 16, state 62: `EXEC` or an RPC names a stored procedure that
    /// does not exist. Used by `session` for unknown RPCs.
    ///
    /// ```text
    /// Unknown stored procedure 'dbo.p'.
    /// ```
    pub fn procedure_not_found(name: &str) -> Self {
        from_catalog(2812, 62, &[Arg::Str(name)])
    }

    /// Error 201, severity 16, state 10: `EXEC` or an RPC calls `procedure` without the
    /// `parameter` it requires.
    ///
    /// ```text
    /// Procedure 'sp_executesql' was called without the parameter '@statement' it requires.
    /// ```
    pub fn procedure_expects_parameter(procedure: &str, parameter: &str) -> Self {
        from_catalog(201, 10, &[Arg::Str(procedure), Arg::Str(parameter)])
    }

    /// Error 8144, severity 16, state 2: the call gives more arguments than the procedure
    /// declares. The name is empty when the call goes through `sp_executesql`.
    ///
    /// ```text
    /// Procedure sp_who was called with more arguments than it declares.
    /// ```
    pub fn too_many_arguments(procedure: &str) -> Self {
        from_catalog(8144, 2, &[Arg::Str(procedure)])
    }

    /// Error 8145, severity 16, state 1: `parameter` is not a parameter of `procedure`.
    ///
    /// ```text
    /// @x is not a parameter declared by procedure sp_who.
    /// ```
    pub fn not_a_parameter(parameter: &str, procedure: &str) -> Self {
        from_catalog(8145, 1, &[Arg::Str(parameter), Arg::Str(procedure)])
    }

    /// Error 8146, severity 16, state 1: a procedure that declares no parameter was called
    /// with arguments.
    ///
    /// ```text
    /// Procedure  declares no parameter and was called with arguments.
    /// ```
    pub fn no_parameter_but_arguments(procedure: &str) -> Self {
        from_catalog(8146, 1, &[Arg::Str(procedure)])
    }

    /// Error 119, severity 15, state 1: a positional argument follows a named one, at
    /// `position` in the argument list, which is 1-based.
    ///
    /// ```text
    /// Parameter number 2 and the ones after it have to use the '@name = value' form; once that form has been used, a positional argument may not follow it.
    /// ```
    pub fn positional_after_named(position: i64) -> Self {
        from_catalog(119, 1, &[Arg::Int(position)])
    }

    /// Error 179, severity 15, state 1: `OUTPUT` was written on a constant.
    ///
    /// ```text
    /// The OUTPUT option cannot be used on a constant argument of a procedure.
    /// ```
    pub fn output_on_a_constant() -> Self {
        from_catalog(179, 1, &[])
    }

    /// Error 214, severity 16, state 2: `procedure` was passed `parameter`, whose type is not
    /// `ty`.
    ///
    /// ```text
    /// The parameter '@statement' has to be of type 'ntext/nchar/nvarchar' for this procedure.
    /// ```
    pub fn procedure_expects_type(parameter: &str, ty: &str) -> Self {
        from_catalog(214, 2, &[Arg::Str(parameter), Arg::Str(ty)])
    }

    /// Error 8178, severity 16, state 1: the parameter list of `sp_executesql` declares
    /// `parameter` and the call does not supply it. `query` is the parameterised text.
    ///
    /// ```text
    /// The parameterized query '(@x int)SELECT @x' was called without its parameter '@x'.
    /// ```
    pub fn parameter_not_supplied(query: &str, parameter: &str) -> Self {
        from_catalog(8178, 1, &[Arg::Str(query), Arg::Str(parameter)])
    }

    /// Error 8179, severity 16, state 4: `handle` names no prepared statement of this session.
    ///
    /// ```text
    /// No prepared statement of this session has the handle 123456.
    /// ```
    pub fn prepared_statement_not_found(handle: i64) -> Self {
        from_catalog(8179, 4, &[Arg::Int(handle)])
    }

    /// Error 8180, severity 16, state 1: the statement given to `sp_prepare` or
    /// `sp_executesql` could not be prepared. It comes after the compile error that caused
    /// it, as a second message of the batch.
    ///
    /// ```text
    /// The statement could not be prepared.
    /// ```
    pub fn statement_could_not_be_prepared() -> Self {
        from_catalog(8180, 1, &[])
    }

    /// Error 15009, severity 16, state 1: `object` does not exist in `database`.
    ///
    /// ```text
    /// The object 'nosuchobj' is not in database 'master', or this operation does not accept it.
    /// ```
    pub fn object_missing_in_database(object: &str, database: &str) -> Self {
        from_catalog(15009, 1, &[Arg::Str(object), Arg::Str(database)])
    }

    /// Error 15010, severity 16, state 1: `sp_helpdb` names a database that does not exist.
    ///
    /// ```text
    /// No database named 'nosuchdb' exists; give a valid database name.
    /// ```
    pub fn help_database_not_found(database: &str) -> Self {
        from_catalog(15010, 1, &[Arg::Str(database)])
    }

    /// Error 245, severity 16, state 1: an implicit or
    /// explicit conversion of `value` (of type `from`) to type `to` failed.
    ///
    /// Parameters follow the template order, `from`, `value`, `to`. For a conversion that
    /// does not quote the value, see
    /// [`error_converting_data_type`](Self::error_converting_data_type); for a date or
    /// time target, [`conversion_failed_datetime`](Self::conversion_failed_datetime).
    ///
    /// ```text
    /// The varchar value 'abc' could not be converted to data type int.
    /// ```
    pub fn conversion_failed(from: &str, value: &str, to: &str) -> Self {
        from_catalog(245, 1, &[Arg::Str(from), Arg::Str(value), Arg::Str(to)])
    }

    /// Error 8114, severity 16 (`SELECT CAST('abc' AS bigint);`): a conversion between two
    /// data types failed without SQL Server quoting the value (typically parameters and
    /// `numeric`/`decimal` sources).
    ///
    /// The state follows the (source, target) pair of a conversion written in an
    /// expression, [`CONVERTING_8114_STATES`]: 5, except towards `datetimeoffset`, which
    /// sends 31 (`SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));`). The
    /// pair does not settle it alone, the same pair bound to an `sp_executesql` parameter
    /// sends 5, so this constructor is for the conversions `vauban-types` writes, which
    /// are expressions.
    ///
    /// `from` is the source type as the message prints it: on the expression path `char`
    /// prints as `varchar` and `nchar` as `nvarchar`, with the state of the fixed-length
    /// form, so the table carries the four narrow names. On the parameter path the
    /// fixed-length name prints as written (`char`, `nchar`), at state 5.
    ///
    /// ```text
    /// Data type varchar could not be converted to numeric.
    /// ```
    pub fn error_converting_data_type(from: &str, to: &str) -> Self {
        from_catalog(
            8114,
            converting_state(from, to),
            &[Arg::Str(from), Arg::Str(to)],
        )
    }

    /// Error 241, severity 16, state 1: a character string
    /// could not be converted to a date and/or time type.
    ///
    /// ```text
    /// The character string could not be converted to a date or time.
    /// ```
    pub fn conversion_failed_datetime() -> Self {
        from_catalog(241, 1, &[])
    }

    /// Error 210, severity 16, state 1: a binary value is out of range for a legacy date
    /// target. The converter refuses a tick count of 25 920 000 or more, a minute of
    /// 1 440 or more, or a day outside the calendar of the target, with the same four
    /// fields on both axes:
    ///
    /// * out of the clock, day inside the calendar:
    ///   `SELECT CAST(0x00000000018B8200 AS datetime);` (25 920 000 ticks) and
    ///   `SELECT CAST(0x000005A0 AS smalldatetime);` (minute 1 440);
    /// * out of the calendar, clock at zero:
    ///   `SELECT CAST(0xFFFF2E4500000000 AS datetime);` (the day before 1753-01-01) and
    ///   `SELECT CAST(0x002D248000000000 AS datetime);` (the day after 9999-12-31).
    ///
    /// The calendar is an axis of its own because the neighbours of those two values are
    /// accepted: `CAST(0xFFFF2E4600000000 AS datetime)` is `Jan  1 1753 12:00AM` and
    /// `CAST(0x002D247F018B81FF AS datetime)` is `Dec 31 9999 11:59PM`. On
    /// `smalldatetime` the day count is two unsigned bytes whose largest value is still a
    /// valid day (`CAST(0xFFFF0000 AS smalldatetime)` = `Jun  6 2079 12:00AM`), so that
    /// target has no calendar case. `0xFFFFFFFFFFFFFFFF` raises 210 on both targets too,
    /// through the clock: its day count is -1 for `datetime`, accepted on its own
    /// (`CAST(0xFFFFFFFF00000000 AS datetime)` = `Dec 31 1899 12:00AM`), and 0xFFFF for
    /// `smalldatetime`, the last valid day of the type. The message names `datetime` for
    /// both targets.
    ///
    /// ```text
    /// A binary or varbinary value could not be converted to datetime.
    /// ```
    pub fn converting_datetime_from_binary() -> Self {
        from_catalog(210, 1, &[])
    }

    /// Error 8115, severity 16, state 2: a value of type
    /// `from` does not fit in type `to`.
    ///
    /// The template has two `%ls`, `from` then `to`.
    ///
    /// ```text
    /// Converting expression to data type int overflowed.
    /// ```
    pub fn arithmetic_overflow(from: &str, to: &str) -> Self {
        from_catalog(8115, 2, &[Arg::Str(from), Arg::Str(to)])
    }

    /// Error 8134, severity 16, state 1: a division whose
    /// divisor evaluates to zero. No argument.
    ///
    /// ```text
    /// Division by zero.
    /// ```
    pub fn divide_by_zero() -> Self {
        from_catalog(8134, 1, &[])
    }

    // ---------------------------------------------------------------------------------
    // Grouped by the module that raises them; each rustdoc quotes a query that raises
    // the error with the state given.
    // ---------------------------------------------------------------------------------

    // parser

    /// Error 105, severity 15, state 1 (`SELECT 'abc;`): a character string literal is not
    /// closed before the end of the batch. `text` is the run of characters that follows the
    /// opening quote, as SQL Server echoes it. SQL Server also raises 102 right after; the
    /// parser decides whether to send both.
    ///
    /// ```text
    /// Quotation mark left open after the string 'abc;'.
    /// ```
    pub fn unclosed_quotation_mark(text: &str, line: u32) -> Self {
        from_catalog(105, 1, &[Arg::Str(text)]).with_line(line)
    }

    /// Error 113, severity 15, state 1 (`SELECT 1 /* oops`): a block comment is not closed
    /// before the end of the batch. No argument.
    ///
    /// ```text
    /// Comment is not closed: '*/' expected.
    /// ```
    pub fn missing_end_comment_mark(line: u32) -> Self {
        from_catalog(113, 1, &[]).with_line(line)
    }

    /// Error 156, severity 15, state 1 (`SELECT FROM t;`): the parser met a reserved
    /// keyword where it expected something else. `keyword` is echoed as written.
    ///
    /// ```text
    /// Syntax error near the keyword 'FROM'.
    /// ```
    pub fn incorrect_syntax_near_keyword(keyword: &str, line: u32) -> Self {
        from_catalog(156, 1, &[Arg::Str(keyword)]).with_line(line)
    }

    /// Error 191, severity 15, state 1 (`SELECT ((…1016 parentheses…1…));`): the batch
    /// nests one level deeper than the parser will descend. No argument.
    ///
    /// ```text
    /// The statement is nested too deeply; split it into smaller queries.
    /// ```
    ///
    /// # Which of the three numbers applies
    ///
    /// Deep nesting has three answers and they are not interchangeable:
    ///
    /// - 191, severity 15, state 1: a recursive shape one level past the limit the server
    ///   counts for it. One number for many shapes, but no single limit: 1016
    ///   parentheses, 1014 nested calls, 510 nested `BEGIN...END`, 169 scalar subqueries,
    ///   101 nested derived tables, 84 nested `EXISTS` and 1016 prefix operators each
    ///   answer 191 at that depth and parse one level below it. This constructor.
    /// - 125, severity 15, state 4: the `CASE`, which has a limit of its own two orders of
    ///   magnitude below the others: an eleventh nested `CASE` answers 125, a tenth
    ///   succeeds. Not catalogued here.
    /// - 8631, severity 17, state 1: a flat chain, which no nesting counter sees: a chain
    ///   of 20 000 `+` answers 8631, a chain of 20 000 `OR` succeeds. It comes out of the
    ///   binder, not the parse ([`SqlError::stack_limit_reached`]).
    ///
    /// Do not read this constructor as covering the family: a caller that meets a `CASE`
    /// or a flat chain sends a different number and a different severity.
    ///
    /// # Why the state is fixed at 1
    ///
    /// Neither the shape (the seven above) nor the path into the parser (a plain batch,
    /// `EXEC('...')`, `EXEC sp_executesql N'...'`, a `CREATE PROCEDURE` body, a
    /// `CREATE VIEW` body) moves the state: each combination sends 1
    /// (`tests::nested_too_deeply_is_191_severity_15_state_1`).
    pub fn nested_too_deeply(line: u32) -> Self {
        from_catalog(191, 1, &[]).with_line(line)
    }

    // types

    /// Error 242, severity 16, state 3
    /// (`SELECT CAST(CAST('9999-12-31' AS date) AS smalldatetime);`): the value is a valid
    /// `from` but falls outside the range of `to`. Both names are SQL type names.
    ///
    /// ```text
    /// Converting date to smalldatetime produced a value outside the target range.
    /// ```
    pub fn out_of_range_conversion(from: &str, to: &str) -> Self {
        from_catalog(242, 3, &[Arg::Str(from), Arg::Str(to)])
    }

    /// Error 220, severity 16, state 2 (`SELECT CAST(300 AS tinyint);`): an integral value
    /// does not fit in `ty`. The template prints the value with `%ld`, in decimal.
    ///
    /// ```text
    /// Value out of range for data type tinyint: 300.
    /// ```
    pub fn overflow_for_data_type(ty: &str, value: i64) -> Self {
        from_catalog(220, 2, &[Arg::Str(ty), Arg::Int(value)])
    }

    /// Error 232, severity 16, state 2 (`SELECT CAST(1e40 AS real);`): a floating-point
    /// value does not fit in `ty`. The template prints the value with `%f`, whose exact
    /// form is documented on `format::Arg::Float`: six decimals, no exponent, at most 17
    /// significant digits.
    ///
    /// ```text
    /// Value out of range for type real: 10000000000000000000000000000000000000000.000000.
    /// ```
    pub fn overflow_for_type(ty: &str, value: f64) -> Self {
        from_catalog(232, 2, &[Arg::Str(ty), Arg::Float(value)])
    }

    /// Error 448, severity 16, state 1 (`SELECT 'a' COLLATE Klingon_CI_AS;`): the collation
    /// name is unknown to the server.
    ///
    /// ```text
    /// Unknown collation 'Klingon_CI_AS'.
    /// ```
    pub fn invalid_collation(name: &str) -> Self {
        from_catalog(448, 1, &[Arg::Str(name)])
    }

    /// Error 8169, severity 16, state 2 (`SELECT CAST('a' AS uniqueidentifier);`): a
    /// character string is not a GUID. No argument: SQL Server does not echo the value.
    ///
    /// ```text
    /// The character string could not be converted to uniqueidentifier.
    /// ```
    pub fn conversion_failed_guid() -> Self {
        from_catalog(8169, 2, &[])
    }

    /// Error 9809, severity 16, state 1
    /// (`SELECT CONVERT(varbinary(10), CAST('abc' AS varchar(10)), 5);`): the `CONVERT`
    /// style number means nothing for that pair of types.
    ///
    /// ```text
    /// Style 5 is not defined for converting varchar to varbinary.
    /// ```
    pub fn unsupported_convert_style(style: i32, from: &str, to: &str) -> Self {
        from_catalog(
            9809,
            1,
            &[Arg::Int(i64::from(style)), Arg::Str(from), Arg::Str(to)],
        )
    }

    /// Error 8152, severity 16, state 17: a temporal value cannot fit the binary
    /// conversion target (`SELECT CONVERT(binary(2), CAST('13:05:06' AS time(3)));`, and
    /// the same from `datetime2(3)` and `datetimeoffset(3)`); `date` to `binary(2)`
    /// truncates without an error.
    ///
    /// This state is scoped to those conversions: an `INSERT` of a value too long for a
    /// `varchar(2)`, `nvarchar(2)` or `varbinary(2)` column answers 2628 state 1 instead.
    ///
    /// ```text
    /// Data too long: the string or binary value would be cut.
    /// ```
    pub fn string_or_binary_truncated() -> Self {
        from_catalog(8152, 17, &[])
    }

    /// Error 2628, severity 16, state 1: an `INSERT` or `UPDATE` assigns a value that
    /// does not fit a fixed `varchar`, `nvarchar` or `varbinary` column.
    ///
    /// `table` is the three-part name; `column` the column name; `truncated_value` is the
    /// portion of the value that would be stored (empty for `varbinary` sources).
    ///
    /// ```text
    /// Data too long for table 'db.dbo.t', column 'a': the value 'ab' would be cut.
    /// ```
    pub fn string_or_binary_data_truncated(
        table: &str,
        column: &str,
        truncated_value: &str,
    ) -> Self {
        from_catalog(
            2628,
            1,
            &[Arg::Str(table), Arg::Str(column), Arg::Str(truncated_value)],
        )
    }

    /// Error 529, severity 16, state 1
    /// (`SELECT CAST(GETDATE() AS uniqueidentifier);`): the pair of types has no explicit
    /// conversion at all, so even `CAST` and `CONVERT` refuse it. For a conversion that is
    /// allowed but fails on the value, see [`conversion_failed`](Self::conversion_failed).
    ///
    /// ```text
    /// No explicit conversion exists from datetime to uniqueidentifier.
    /// ```
    pub fn explicit_conversion_not_allowed(from: &str, to: &str) -> Self {
        from_catalog(529, 1, &[Arg::Str(from), Arg::Str(to)])
    }

    /// Error 257, severity 16, state 3
    /// (`DECLARE @x xml = '<a/>'; DECLARE @v varchar(10); SET @v = @x;`): the pair of types
    /// has an explicit conversion but no implicit one, so the user must write `CONVERT`.
    ///
    /// ```text
    /// No implicit conversion from xml to varchar; use CONVERT explicitly.
    /// ```
    pub fn implicit_conversion_not_allowed(from: &str, to: &str) -> Self {
        from_catalog(257, 3, &[Arg::Str(from), Arg::Str(to)])
    }

    /// Error 206, severity 16, state 2 (`SELECT CAST(GETDATE() AS date) + 1;`): two
    /// operands of an expression or an assignment have types SQL Server refuses to
    /// reconcile.
    ///
    /// ```text
    /// Type mismatch: date cannot be combined with int.
    /// ```
    pub fn operand_type_clash(left: &str, right: &str) -> Self {
        from_catalog(206, 2, &[Arg::Str(left), Arg::Str(right)])
    }

    // sysfn

    /// Error 174, severity 15, state 1 (`SELECT LEN();`): a built-in function was called
    /// with the wrong number of arguments and accepts exactly `n`. SQL Server echoes the
    /// name as the user wrote it, lower-cased by its own parser; `sysfn` passes the name it
    /// wants printed.
    ///
    /// ```text
    /// The function LEN takes exactly 1 argument(s).
    /// ```
    pub fn function_arg_count(name: &str, n: u8) -> Self {
        from_catalog(174, 1, &[Arg::Str(name), Arg::Int(i64::from(n))])
    }

    /// Error 189, severity 15, state 1 (`SELECT ROUND(1);`): same as
    /// [`function_arg_count`](Self::function_arg_count) for a function that accepts a
    /// range.
    ///
    /// ```text
    /// The function ROUND takes between 2 and 3 arguments.
    /// ```
    pub fn function_arg_count_range(name: &str, min: u8, max: u8) -> Self {
        from_catalog(
            189,
            1,
            &[
                Arg::Str(name),
                Arg::Int(i64::from(min)),
                Arg::Int(i64::from(max)),
            ],
        )
    }

    /// Error 8116, severity 16, state 1 (`SELECT LEN(CAST('1' AS xml));`): the argument at
    /// `position` (1-based) has a type the function does not accept.
    ///
    /// ```text
    /// Data type xml is not accepted for argument 1 of the len function.
    /// ```
    pub fn invalid_argument_type(ty: &str, position: u8, function: &str) -> Self {
        from_catalog(
            8116,
            1,
            &[
                Arg::Str(ty),
                Arg::Int(i64::from(position)),
                Arg::Str(function),
            ],
        )
    }

    /// Error 536, severity 16: a length or a start position passed to a string function is
    /// negative or otherwise out of range, raised during compilation for
    /// `SELECT LEFT('abc', -1);`, `SELECT RIGHT('abc', -1);` and
    /// `SELECT SUBSTRING('abc', 1, -1);`.
    ///
    /// The state follows the calling function (`LENGTH_PARAMETER_536_STATES` in this
    /// module): 6 for `left` and `right`, 8 for `substring`, and 1 for another function.
    /// `function` is printed as given, in lower case.
    ///
    /// ```text
    /// The length given to the left function is not valid.
    /// ```
    pub fn invalid_length_parameter(function: &str) -> Self {
        from_catalog(536, length_parameter_state(function), &[Arg::Str(function)])
    }

    /// The run-time form of the length check, reached when the length is not a folded
    /// `int`: `LEFT` and `SUBSTRING` raise 537 with state 2 on a narrow string and 3 on a
    /// wide one; `RIGHT` raises 536 with states 2 and 4. `unicode` selects the wide
    /// character result family, not the function spelling.
    pub fn runtime_length_parameter(function: &str, unicode: bool) -> Self {
        if function.eq_ignore_ascii_case("right") {
            from_catalog(536, if unicode { 4 } else { 2 }, &[Arg::Str("RIGHT")])
        } else {
            from_catalog(537, if unicode { 3 } else { 2 }, &[])
        }
    }

    /// Error 517, severity 16, for a `DATEADD` overflow at the bound of the final type: 1
    /// for `datetime`, 2 for `smalldatetime`, 3 for `date`, `datetime2` and
    /// `datetimeoffset` (`SELECT DATEADD(day, 2147483647, CAST('9999-12-31' AS date));`,
    /// `DATEADD(day, 1, ...)` from 2079-06-06 for `smalldatetime`). The intermediate
    /// `datetime` overflow of `smalldatetime` has a constructor of its own, `time` wraps
    /// rather than overflowing, and `EOMONTH` has its own constructor.
    ///
    /// ```text
    /// The addition overflowed the 'datetime' column.
    /// ```
    pub fn datetime_overflow(ty: &str) -> Self {
        let state = match ty {
            "smalldatetime" => 2,
            "date" | "datetime2" | "datetimeoffset" => 3,
            _ => 1,
        };
        from_catalog(517, state, &[Arg::Str(ty)])
    }

    /// Error 517, severity 16, state 1 on the intermediate overflow in
    /// `SELECT DATEADD(day, 2147483647, CAST('2079-06-06' AS smalldatetime));`, as with
    /// `year` or `hour` for that amount and `day` for -2147483648. A `day` of +1 reaches
    /// the `smalldatetime` bound instead and gives state 2.
    pub fn smalldatetime_intermediate_overflow() -> Self {
        from_catalog(517, 1, &[Arg::Str("smalldatetime")])
    }

    /// Error 517, severity 16, state 1 on `SELECT EOMONTH('9999-12-31', 1);`
    /// and `SELECT EOMONTH('0001-01-01', -1);`, where `DATEADD` on `date` uses 3.
    pub fn eomonth_overflow() -> Self {
        from_catalog(517, 1, &[Arg::Str("date")])
    }

    /// Error 1023, severity 15, state 1 on `SELECT DATEPART('year', GETDATE());`
    /// and `SELECT DATEPART(user, GETDATE());`.
    pub fn invalid_function_parameter(position: u8, function: &str) -> Self {
        from_catalog(
            1023,
            1,
            &[Arg::Int(i64::from(position)), Arg::Str(function)],
        )
    }

    /// Error 9806, severity 16, state 0 on `SELECT DATEDIFF(iso_week,
    /// CAST('2020-03-04' AS date), CAST('2020-03-04' AS date));`. Unlike 9810, this
    /// message has no type argument. Across the six temporal types and `varchar` on both
    /// sides of `iso_week`, the four pairs made from `datetime` and `smalldatetime` give
    /// state 2, the other pairs give 0.
    pub fn datediff_datepart_not_supported(datepart: &str, start: &str, end: &str) -> Self {
        let legacy = |ty: &str| matches!(ty, "datetime" | "smalldatetime");
        let state = if legacy(start) && legacy(end) { 2 } else { 0 };
        from_catalog(9806, state, &[Arg::Str(datepart), Arg::Str("datediff")])
    }

    /// Error 535, severity 16, state 0
    /// (`SELECT DATEDIFF(nanosecond, '1900-01-01', '2100-01-01');`): the number of
    /// dateparts between the two instants does not fit in the result type. The template
    /// names the function twice with `%.*ls`; both are filled with `datediff`. No argument.
    ///
    /// ```text
    /// datediff overflowed: too many dateparts separate the two instants. Call datediff with a coarser datepart.
    /// ```
    pub fn datediff_overflow() -> Self {
        from_catalog(535, 0, &[Arg::Str("datediff"), Arg::Str("datediff")])
    }

    /// Error 9810, severity 16 (`SELECT DATEADD(hour, 1, CAST('2020-01-01' AS date));`):
    /// the datepart exists but the function refuses it for that data type. The state
    /// depends on the function, the type and sometimes the part: `DATEADD(date, iso_week)`
    /// is 2, `DATEADD(date, hour)` is 1. `DATETRUNC` has its rows although VaubanDB does
    /// not register it. A triple without a row takes 1.
    ///
    /// ```text
    /// Datepart hour cannot be used with the date function dateadd on data type date.
    /// ```
    pub fn datepart_not_supported(datepart: &str, function: &str, ty: &str) -> Self {
        let state = match (function, ty, datepart) {
            ("dateadd", "datetime", _) => 0,
            ("dateadd", "smalldatetime", _) => 3,
            ("dateadd", "datetime2" | "datetimeoffset", _) | ("dateadd", "date", "iso_week") => 2,
            ("datepart", "date", _) => 2,
            ("datepart", "time", _) => 3,
            ("datepart", "datetime" | "smalldatetime", _) => 6,
            ("datename", "date", _) => 4,
            ("datename", "time", _) => 5,
            ("datename", "datetime" | "smalldatetime", _) => 7,
            ("datetrunc", "smalldatetime", _) => 8,
            ("datetrunc", "datetime", _) => 9,
            ("datetrunc", "date", "weekday")
            | ("datetrunc", "time", "nanosecond")
            | ("datetrunc", "datetime2" | "datetimeoffset", _) => 11,
            ("datetrunc", "date" | "time", _) => 10,
            _ => 1,
        };
        from_catalog(
            9810,
            state,
            &[Arg::Str(datepart), Arg::Str(function), Arg::Str(ty)],
        )
    }

    /// Error 155, severity 15, state 1 (`SELECT DATEPART(bogus, GETDATE());`): a keyword
    /// argument is not one of the options the construct accepts. `option_kind` is the word
    /// SQL Server puts before `option`, e.g. `datepart`.
    ///
    /// ```text
    /// 'bogus' is not a known datepart option.
    /// ```
    pub fn not_a_recognized_option(value: &str, option_kind: &str) -> Self {
        from_catalog(155, 1, &[Arg::Str(value), Arg::Str(option_kind)])
    }

    /// Error 3623, severity 16, state 1 (`SELECT LOG(-1);`): a mathematical function was
    /// given an argument outside its domain. No argument.
    ///
    /// ```text
    /// The floating point operation is undefined.
    /// ```
    pub fn invalid_floating_point_operation() -> Self {
        from_catalog(3623, 1, &[])
    }

    /// Error 4127, severity 16, state 1 (`SELECT COALESCE(NULL, NULL);`): every argument of
    /// `COALESCE` is the literal `NULL`, so the expression has no type. No argument.
    ///
    /// ```text
    /// COALESCE needs at least one argument other than the NULL constant.
    /// ```
    pub fn coalesce_all_null() -> Self {
        from_catalog(4127, 1, &[])
    }

    /// Error 289, severity 16, state 1 for `date` (`SELECT DATEFROMPARTS(2020,99,1);`),
    /// state 3 for `datetime` (`SELECT DATETIMEFROMPARTS(2020, 99, 99, 0, 0, 0, 0);`): a
    /// `*FROMPARTS` function got parts that do not form a valid value of `ty`.
    ///
    /// ```text
    /// Data type datetime cannot be built from these arguments: at least one value is out of range.
    /// ```
    pub fn cannot_construct_type(ty: &str) -> Self {
        from_catalog(289, if ty == "date" { 1 } else { 3 }, &[Arg::Str(ty)])
    }

    // binder

    /// Error 195, severity 15, state 10 (`SELECT NO_SUCH_FN(1);`): a name used as a
    /// function, a procedure or an object is unknown.
    ///
    /// The template ends with `%S_MSG.`, filled with `built-in function name`, the
    /// trailing `name` included. `kind` is therefore the kind alone (`built-in function`,
    /// `function`, `object`); the constructor appends ` name`.
    ///
    /// ```text
    /// 'NO_SUCH_FN' is not a known built-in function name.
    /// ```
    pub fn not_a_recognized_name(name: &str, kind: &str) -> Self {
        let kind = format!("{kind} name");
        from_catalog(195, 10, &[Arg::Str(name), Arg::Str(&kind)])
    }

    /// Error 8117, severity 16, state 1 (`SELECT ~CAST(1.5 AS float);`): an operator has no
    /// meaning for that operand type. `operator` is printed as given — SQL Server quotes
    /// the symbolic ones (`'~'`) but not the named ones.
    ///
    /// ```text
    /// Data type float is not accepted by the '~' operator.
    /// ```
    pub fn invalid_operand_type(ty: &str, operator: &str) -> Self {
        from_catalog(8117, 1, &[Arg::Str(ty), Arg::Str(operator)])
    }

    /// Error 135, severity 15, state 1 (`BREAK;` outside a `WHILE`): a BREAK statement
    /// written where no WHILE encloses it.
    ///
    /// ```text
    /// BREAK is written where no WHILE loop encloses it.
    /// ```
    pub fn break_without_while() -> Self {
        from_catalog(135, 1, &[])
    }

    /// Error 136, severity 15, state 1 (`CONTINUE;` outside a `WHILE`): a CONTINUE
    /// statement written where no WHILE encloses it.
    ///
    /// ```text
    /// CONTINUE is written where no WHILE loop encloses it.
    /// ```
    pub fn continue_without_while() -> Self {
        from_catalog(136, 1, &[])
    }

    /// Error 4145, severity 15, state 1 (`IF 1 SELECT 1;`): a place that requires a
    /// predicate (`IF`, `WHERE`, `WHEN`) got an expression that is not one. `near` is the
    /// token that follows the expression, as SQL Server echoes it.
    ///
    /// ```text
    /// A condition is expected near 'SELECT', but the expression is not boolean.
    /// ```
    pub fn non_boolean_expression(near: &str) -> Self {
        from_catalog(4145, 1, &[Arg::Str(near)])
    }

    /// Error 178, severity 15, state 1 (`RETURN 1;` outside a procedure): a RETURN
    /// statement written with a value where the context does not accept one.
    ///
    /// ```text
    /// A return value needs a procedure to return from; this statement is not inside one.
    /// ```
    pub fn return_with_value_outside_procedure() -> Self {
        from_catalog(178, 1, &[])
    }

    /// Error 2715, severity 16, state 3 (`DECLARE @x foo;`): a declaration names a type
    /// that does not exist. `position` is the 1-based rank of the column, parameter or
    /// variable in the statement.
    ///
    /// This is the state of a scalar declaration and of a routine parameter
    /// ([`CANNOT_FIND_DATA_TYPE_2715_DECLARATION_STATE`]); the column list of a table
    /// sends 6 and has [`SqlError::cannot_find_data_type_in_table`].
    ///
    /// ```text
    /// Column, parameter or variable #1: unknown data type foo.
    /// ```
    pub fn cannot_find_data_type(position: u32, name: &str) -> Self {
        from_catalog(
            2715,
            CANNOT_FIND_DATA_TYPE_2715_DECLARATION_STATE,
            &[Arg::Int(i64::from(position)), Arg::Str(name)],
        )
    }

    /// Error 2715, severity 16, state 6 (`CREATE TABLE t_unknown (a foo);`): the column
    /// list of a table names a type that does not exist. `position` is the 1-based rank of
    /// the column in the list, and `name` the type name as written, its qualifier kept
    /// (`CREATE TABLE t_unknown (a dbo.foo);` prints `dbo.foo`).
    ///
    /// Same number and same sentence as [`SqlError::cannot_find_data_type`], another
    /// state: see [`CANNOT_FIND_DATA_TYPE_2715_COLUMN_LIST_STATE`] for the shapes that
    /// carry the 6 and those that carry the 3.
    ///
    /// ```text
    /// Column, parameter or variable #1: unknown data type foo.
    /// ```
    pub fn cannot_find_data_type_in_table(position: u32, name: &str) -> Self {
        from_catalog(
            2715,
            CANNOT_FIND_DATA_TYPE_2715_COLUMN_LIST_STATE,
            &[Arg::Int(i64::from(position)), Arg::Str(name)],
        )
    }

    /// Error 137, severity 15, state 2 (`SELECT @x;`): a batch **reads** a variable it
    /// never declared. The template's double quotes are part of the message.
    ///
    /// The state follows the role the variable plays, not the number
    /// (`SCALAR_VARIABLE_137_STATES` in this module): a read sends 2, and a statement that
    /// assigns the variable sends 1 — see
    /// [`SqlError::must_declare_scalar_variable_assigned`]. `SELECT @x = @y;` names `@y`
    /// and takes *this* constructor, because `@y` is read there.
    ///
    /// ```text
    /// The scalar variable "@x" is not declared.
    /// ```
    pub fn must_declare_scalar_variable(name: &str) -> Self {
        from_catalog(
            137,
            scalar_variable_state(ScalarVariableRole::Read),
            &[Arg::Str(name)],
        )
    }

    /// Error 134, severity 15, state 1 (`DECLARE @x int; DECLARE @x int;`): a batch
    /// declares a variable twice. `DECLARE @x int, @x int;` and a second `DECLARE` that
    /// spells the name in another case (`@X` after `@x`) answer the same number; the name
    /// quoted is the one of the second declaration, as written.
    ///
    /// ```text
    /// The variable '@x' is already declared; a batch or a procedure declares each name once.
    /// ```
    pub fn variable_already_declared(name: &str) -> Self {
        from_catalog(134, 1, &[Arg::Str(name)])
    }

    /// Error 137, severity 15, state 1 (`SELECT @x = 1;`): a batch **assigns** a variable
    /// it never declared. Same number, same text and same severity as
    /// [`SqlError::must_declare_scalar_variable`], one state apart.
    ///
    /// `SELECT @x = 1;`, `SET @x = 1;` and `SELECT @x = 1, @y = 2;` answer state 1, where
    /// the same variable read (`SELECT @x;`) answers state 2.
    ///
    /// ```text
    /// The scalar variable "@x" is not declared.
    /// ```
    pub fn must_declare_scalar_variable_assigned(name: &str) -> Self {
        from_catalog(
            137,
            scalar_variable_state(ScalarVariableRole::Assigned),
            &[Arg::Str(name)],
        )
    }

    /// Error 8631, severity 17, state 1 (`SELECT 1 + 1 + ... ;`, 20 000 terms): a flat
    /// chain of operators, which no nesting counter sees, builds a tree deep enough to
    /// exhaust the stack of the thread compiling it. No argument.
    ///
    /// ```text
    /// Internal error: the server ran out of stack; the query is probably nested too deeply and needs simplifying.
    /// ```
    ///
    /// Carries **no line**: SQL Server reports the line of the *statement*, not of the node
    /// the descent gave up on, and the binder only knows it further up
    /// (`vauban_binder::depth::at_statement`). Use [`SqlError::with_line`] there.
    ///
    /// # Which of the three numbers applies
    ///
    /// Deep expressions have three answers, not interchangeable, which differ in severity
    /// as well as in number:
    ///
    /// - 8631, severity 17, state 1: a flat chain. The text nests nothing, so neither the
    ///   parser's counter nor the `CASE` counter sees it coming, and the server runs out
    ///   of stack instead. This constructor.
    /// - 191, severity 15, state 1: a shape the parse recurses through: parentheses,
    ///   nested calls, scalar subqueries, `BEGIN...END`, derived tables, `EXISTS`, and
    ///   also a flat chain of prefix operators, which is flat text but recursive parsing
    ///   (20 000 `-` or 20 000 `NOT`). [`SqlError::nested_too_deeply`].
    /// - 125, severity 15, state 4: the `CASE`, which has a limit of its own two orders
    ///   of magnitude below the others. Not catalogued here.
    ///
    /// # The limit follows the operator, not the length
    ///
    /// At 20 000 terms, `+`, `-`, `*`, `%`, `&`, `|`, `^`, string `+`, a mixed `+`/`*`
    /// text and `AND` answer 8631, while `OR` answers nothing, at 200 000 terms or over a
    /// variable as well. A caller does not read "long chain" as "8631": the operator
    /// decides.
    ///
    /// # Why the state is fixed at 1
    ///
    /// Neither the shape (the ten operators above, in a `SELECT` list, a `WHERE`, an
    /// `ORDER BY`, a `DECLARE` initialiser, a `SET`, an `INSERT ... VALUES`, a `PRINT`, a
    /// function argument or an `IF` condition) nor the path (a plain batch, `EXEC('...')`,
    /// `EXEC sp_executesql N'...'`, a `CREATE PROCEDURE` body, a `CREATE VIEW` body, an
    /// `sp_executesql` that itself `EXEC`s) moves the state
    /// (`tests::stack_limit_reached_is_8631_severity_17_state_1`).
    ///
    /// The axis that splits the state of 8114 into 31 and 5, a value bound by the caller
    /// to a declared parameter, is unreachable here: T-SQL refuses an expression in that
    /// position, be it two terms or twenty thousand: `EXEC sp_executesql N'SELECT @p;', N'@p int',
    /// @p = 1 + 1;` answers 102.
    pub fn stack_limit_reached() -> Self {
        from_catalog(8631, 1, &[])
    }

    // types, continued

    /// Error 237, severity 16 (`SELECT CAST(CAST(300000 AS money) AS smallmoney);`): a
    /// `money` value has no room in the target type. `to` is the target's name.
    ///
    /// The state follows the target alone: 1 towards `int`, 2 towards `smallint`, 3
    /// towards `tinyint` and `smallmoney` (table `RESULT_SPACE_237_STATES`). `money`
    /// towards `decimal(3,0)` raises 8115 instead, and a target without a row takes the
    /// default state, 1.
    ///
    /// ```text
    /// A money value does not fit in the result type smallmoney.
    /// ```
    pub fn insufficient_result_space_money(to: &str) -> Self {
        from_catalog(237, result_space_state(to), &[Arg::Str(to)])
    }

    /// Error 1007, severity 15, state 1: a numeric literal needs more than the 38 digits
    /// of precision `numeric` offers. `text` is the literal as the user wrote it.
    ///
    /// The template opens with `%S_MSG`, rendered `number`; the constructor supplies it,
    /// so the caller passes the literal alone
    /// (`SELECT CAST(SQL_VARIANT_PROPERTY(123456789012345678901234567890123456789,
    /// 'BaseType') AS varchar(30));`).
    ///
    /// ```text
    /// The number '123456789012345678901234567890123456789' exceeds the numeric range (precision is limited to 38).
    /// ```
    pub fn number_out_of_numeric_range(text: &str) -> Self {
        from_catalog(1007, 1, &[Arg::Str("number"), Arg::Str(text)])
    }

    /// Error 168, severity 15, state 1
    /// (`SELECT CAST(SQL_VARIANT_PROPERTY(1e400, 'BaseType') AS varchar(30));`): a float
    /// literal falls outside a double. `text` is the literal as the user wrote it.
    ///
    /// The template's `%d` is the width of the representation, 8, the size of a `float`;
    /// the constructor supplies it.
    ///
    /// ```text
    /// The floating point literal '1e400' cannot be represented in 8 bytes.
    /// ```
    pub fn float_out_of_range(text: &str) -> Self {
        from_catalog(168, 1, &[Arg::Str(text), Arg::Int(FLOAT_BYTES)])
    }

    /// Error 151, severity 15, state 1
    /// (`SELECT CAST(SQL_VARIANT_PROPERTY($99999999999999999999, 'BaseType') AS varchar(30));`):
    /// a `money` literal falls outside the `money` range. `text` is the literal as the
    /// user wrote it, currency sign included.
    ///
    /// ```text
    /// '$99999999999999999999' cannot be read as a money value.
    /// ```
    pub fn invalid_money_value(text: &str) -> Self {
        from_catalog(151, 1, &[Arg::Str(text)])
    }

    /// Error 248, severity 16, state 1 (`SELECT CAST('99999999999' AS int);`): a value of
    /// type `from` is a well-formed integer but too large for `int`. `text` is the value
    /// as SQL Server echoes it.
    ///
    /// `int` is hard-coded in the template: [`SqlError::conversion_overflowed_small_int`]
    /// is the `tinyint` and `smallint` twin.
    ///
    /// ```text
    /// The varchar value '99999999999' does not fit in an int column.
    /// ```
    pub fn conversion_overflowed_int(from: &str, text: &str) -> Self {
        from_catalog(248, 1, &[Arg::Str(from), Arg::Str(text)])
    }

    /// Error 244, severity 16: a value of type `from` is a well-formed integer but too
    /// large for `tinyint` or `smallint`. `text` is the value as SQL Server echoes it.
    ///
    /// `column` is the internal name SQL Server prints for the target, `INT1` for
    /// `tinyint` or `INT2` for `smallint`, and it drives the state: 1 for `INT1`
    /// (`SELECT CAST('300' AS tinyint);`), 2 for `INT2`
    /// (`SELECT CAST('99999' AS smallint);`).
    ///
    /// ```text
    /// The varchar value '300' does not fit in an INT1 column; a wider integer type is needed.
    /// ```
    pub fn conversion_overflowed_small_int(from: &str, text: &str, column: &str) -> Self {
        let state = if column == "INT2" { 2 } else { 1 };
        from_catalog(
            244,
            state,
            &[Arg::Str(from), Arg::Str(text), Arg::Str(column)],
        )
    }

    /// Error 235, severity 16, state 0 (`SELECT CAST('abc' AS money);`): a character value
    /// towards `money` is not a money literal. No argument: the template names neither the
    /// value nor the source type.
    ///
    /// ```text
    /// The character value is not a valid money literal and could not be converted.
    /// ```
    pub fn char_to_money_syntax() -> Self {
        from_catalog(235, 0, &[])
    }

    /// Error 8115, severity 16, towards `numeric`: a value of type `from` has an integer
    /// part too large for the target precision and scale.
    ///
    /// The named variant of [`SqlError::arithmetic_overflow`] for a `numeric` target,
    /// which SQL Server sends with another state: 8 from a character, integer, `numeric`
    /// or `money` source (`SELECT CAST('1234.5' AS decimal(5,2));`) and 6 from `float` or
    /// `real` (`SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));`), where the two-type
    /// constructor sends state 2. The target prints `numeric` for `decimal(5,2)` as for
    /// `numeric(5,2)`, so it is hard-coded.
    ///
    /// ```text
    /// Converting varchar to data type numeric overflowed.
    /// ```
    pub fn arithmetic_overflow_to_numeric(from: &str) -> Self {
        Self::arithmetic_overflow_from(from, "numeric")
    }

    // binder and parser

    /// Error 447, severity 16, state 0
    /// (`SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;`): a `COLLATE` clause applies
    /// to an expression that is not a character string. `ty` is the expression's type.
    ///
    /// ```text
    /// COLLATE cannot apply to an expression of type int.
    /// ```
    pub fn collate_on_non_string(ty: &str) -> Self {
        from_catalog(447, 0, &[Arg::Str(ty)])
    }

    /// Error 127, severity 15, state 1 (`SELECT TOP (-1) 1;`): a `TOP` or `FETCH` row
    /// count is negative. No argument.
    ///
    /// ```text
    /// The row count of TOP or FETCH cannot be negative.
    /// ```
    pub fn top_negative() -> Self {
        from_catalog(127, 1, &[])
    }

    /// Error 1060, severity 15, state 1 (`SELECT TOP (NULL) 1;`): a `TOP` or `FETCH` row
    /// count is `NULL` or is not an integer. No argument.
    ///
    /// ```text
    /// The row count of TOP or FETCH has to be an integer.
    /// ```
    pub fn top_null() -> Self {
        from_catalog(1060, 1, &[])
    }

    /// Error 263, severity 16, state 1 (`SELECT *;`): a `SELECT *` has no `FROM` clause.
    /// No argument.
    ///
    /// ```text
    /// No table to select from.
    /// ```
    pub fn select_star_without_from() -> Self {
        from_catalog(263, 1, &[])
    }

    /// Error 131, severity 15: the size given to a type or to a column exceeds what any
    /// data type allows.
    ///
    /// The arguments follow the template: `size` as written, `kind` for the `%S_MSG`,
    /// `name`, and `maximum` for the ceiling (8000 for `varchar` and `varbinary`). Two
    /// fillings of the `%S_MSG` share this severity, and the state tells them apart
    /// (`size_out_of_range_state` in this module):
    ///
    /// - `type`, state 3, `name` being the type's name (`DECLARE @v varchar(9000);`);
    /// - `column`, state 2, `name` being the column's name
    ///   (`CREATE TABLE #t(c varchar(9000));`, which prints `'c'`, not `'varchar'`).
    ///
    /// The third filling, `convert specification`, raises severity 16 state 1 and has its
    /// own constructor ([`SqlError::convert_specification_size_out_of_range`]).
    ///
    /// ```text
    /// Size 9000 of the type 'varchar' is larger than any data type allows (8000).
    /// Size 9000 of the column 'c' is larger than any data type allows (8000).
    /// ```
    pub fn size_out_of_range(size: u32, kind: &str, name: &str, maximum: u32) -> Self {
        from_catalog(
            131,
            size_out_of_range_state(kind),
            &[
                Arg::Int(i64::from(size)),
                Arg::Str(kind),
                Arg::Str(name),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 506, severity 16, state 1
    /// (`SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';`): an `ESCAPE` clause holds something
    /// else than exactly one character, in a predicate whose three operands are
    /// non-Unicode. `escape` is echoed between double quotes by the template, `predicate`
    /// names the predicate, `LIKE`.
    ///
    /// A predicate where the value, the pattern or the escape operand is Unicode sends
    /// state 2 and has its own constructor ([`SqlError::invalid_escape_unicode`]), the way
    /// 137 has one per form: the caller chooses, it does not patch the state afterwards
    /// ([`ESCAPE_506_STATES`]).
    ///
    /// An empty `ESCAPE ''` raises the same error, with an empty `escape`.
    ///
    /// ```text
    /// The escape character "ab" of the LIKE predicate must be a single character.
    /// ```
    pub fn invalid_escape(escape: &str, predicate: &str) -> Self {
        from_catalog(
            506,
            escape_state(LikePredicateWidth::Narrow),
            &[Arg::Str(escape), Arg::Str(predicate)],
        )
    }

    // types and binder

    /// Error 402, severity 16, state 1 (`SELECT CAST(1 AS bit) + CAST(1 AS bit);`): an
    /// operator has no meaning for the pair of operand types it was given.
    ///
    /// `operator` is the name SQL Server prints, which is the operation, not the symbol:
    /// `add` for `+` (`DECLARE @d date = '2020-01-01'; DECLARE @t datetime = @d;
    /// SELECT @d + @t;`), `subtract` for `-` (`SELECT @v - @w;` on two `varchar`), and the
    /// quoted symbol for a bitwise operator, `'&'` (`DECLARE @f float = 1; SELECT @f & 1;`).
    /// The state is 1 for each of those pairs.
    ///
    /// The template's three placeholders are `%s`, not `%ls`; that changes nothing for the
    /// caller, both consume one string.
    ///
    /// ```text
    /// The types bit and bit cannot be combined by the add operator.
    /// ```
    pub fn incompatible_types_for_operator(left: &str, right: &str, operator: &str) -> Self {
        from_catalog(
            402,
            1,
            &[Arg::Str(left), Arg::Str(right), Arg::Str(operator)],
        )
    }

    /// Error 234, severity 16, state 2
    /// (`DECLARE @m money = 922337203685477.58; SELECT CAST(@m AS varchar(3));`): a `money`
    /// value has no room in the target **character** type. `to` is the target's name.
    ///
    /// Same text as 237, another number: 237 is the numeric target
    /// ([`SqlError::insufficient_result_space_money`]), 234 the character one. The target
    /// prints its variable-length name whatever the declared type: `varchar` for `char(3)`
    /// as for `varchar(3)`, `nvarchar` for `nvarchar(3)`. The state is 2 for both.
    ///
    /// ```text
    /// A money value does not fit in the result type varchar.
    /// ```
    pub fn insufficient_result_space_money_to(to: &str) -> Self {
        from_catalog(234, 2, &[Arg::Str(to)])
    }

    /// Error 292, severity 16, state 2
    /// (`DECLARE @s smallmoney = 214748.3647; SELECT CAST(@s AS varchar(3));`): a
    /// `smallmoney` value has no room in the target character type.
    ///
    /// The `smallmoney` twin of [`SqlError::insufficient_result_space_money_to`], with the
    /// same rule on the target's name: `varchar` for `char(3)` as for `varchar(3)`.
    ///
    /// ```text
    /// A smallmoney value does not fit in the result type varchar.
    /// ```
    pub fn insufficient_result_space_smallmoney_to(to: &str) -> Self {
        from_catalog(292, 2, &[Arg::Str(to)])
    }

    /// Error 8170, severity 16, state 2
    /// (`DECLARE @g uniqueidentifier = NEWID(); SELECT CAST(@g AS varchar(35));`): a GUID
    /// needs 36 characters and the target character type is shorter. No argument: the
    /// template names neither the target nor its length.
    ///
    /// The narrow character family raises it, `varchar(35)` as `char(10)`; the same cast
    /// towards `nvarchar(35)` raises 8115 state 2 instead
    /// ([`SqlError::arithmetic_overflow`]).
    ///
    /// ```text
    /// A uniqueidentifier value does not fit in the char result.
    /// ```
    pub fn insufficient_result_space_guid() -> Self {
        from_catalog(8170, 2, &[])
    }

    // sysfn

    /// Error 4151, severity 16, state 1 (`SELECT NULLIF(NULL, 1);`): the first argument of
    /// `NULLIF` is the `NULL` constant, whose type nothing can infer. No argument.
    ///
    /// `NULLIF(CAST(NULL AS int), 1)` is accepted: the constant alone is refused.
    ///
    /// ```text
    /// The first argument of NULLIF cannot be the NULL constant: its type has to be known.
    /// ```
    pub fn nullif_first_argument_null() -> Self {
        from_catalog(4151, 1, &[])
    }

    // types: the state-aware twins of the three overflow constructors

    /// Error 8115, severity 16, with the state SQL Server sends for the (`from`, `to`)
    /// pair: see `overflow_8115_state` (4, 6, 8, and 2 by default).
    ///
    /// Same number, severity and text as [`SqlError::arithmetic_overflow`], which keeps
    /// state 2 for each pair. `from` is what the message prints, `expression` when
    /// SQL Server does not name the source type.
    ///
    /// `to` likewise is what the message prints, which is the family rather than the
    /// declared type: a `numeric` towards `char(3)` prints `varchar` and sends state 5,
    /// like the same cast towards `varchar(3)`.
    ///
    /// ```text
    /// Converting numeric to data type money overflowed.
    /// ```
    pub fn arithmetic_overflow_from(from: &str, to: &str) -> Self {
        from_catalog(
            8115,
            overflow_8115_state(from, to),
            &[Arg::Str(from), Arg::Str(to)],
        )
    }

    /// Error 220, severity 16, with the state SQL Server sends for the (`from`, `ty`)
    /// pair: see `OVERFLOW_220_STATES` (1, 2, 5, 7, and 2 by default).
    ///
    /// Same number, severity and text as [`SqlError::overflow_for_data_type`], which keeps
    /// state 2 for each pair. `from` is the source type's name; it does not appear in
    /// the message, it selects the state.
    ///
    /// `value` is what SQL Server prints, which is the source's internal integer, not the
    /// number the user wrote: `SELECT CAST(CAST(40000 AS money) AS smallint);` prints
    /// `400000000`, the `money` scaled by 10000.
    ///
    /// ```text
    /// Value out of range for data type smallint: 400000000.
    /// ```
    pub fn overflow_for_data_type_from(from: &str, ty: &str, value: i64) -> Self {
        from_catalog(
            220,
            overflow_state(OVERFLOW_220_STATES, from, ty),
            &[Arg::Str(ty), Arg::Int(value)],
        )
    }

    /// Error 232, severity 16, with the state SQL Server sends for the (`from`, `ty`)
    /// pair: see `OVERFLOW_232_STATES` (1, 2, 3, 11, and 2 by default).
    ///
    /// Same number, severity and text as [`SqlError::overflow_for_type`], which keeps
    /// state 2 for each pair. `from` is the source type's name; it does not appear in
    /// the message, it selects the state. `value` is printed with `%f`, whose exact form
    /// is documented on `format::Arg::Float`.
    ///
    /// ```text
    /// Value out of range for type tinyint: 300.000000.
    /// ```
    pub fn overflow_for_type_from(from: &str, ty: &str, value: f64) -> Self {
        from_catalog(
            232,
            overflow_state(OVERFLOW_232_STATES, from, ty),
            &[Arg::Str(ty), Arg::Float(value)],
        )
    }

    // ---------------------------------------------------------------------------------
    // What a type declaration refuses. Five of these numbers travel with a severity that
    // is not the catalogue's; the rustdoc says so where it happens.
    // ---------------------------------------------------------------------------------

    /// Error 131, severity 16, state 1 (`SELECT CAST(1 AS nvarchar(5000));`): the size
    /// given to the target of a `CAST` or a `CONVERT` exceeds what any data type allows.
    ///
    /// The `CAST` twin of [`SqlError::size_out_of_range`]: same template, and the `%S_MSG`
    /// prints `convert specification` instead of `type`. Severity and state follow the
    /// filling, 16 and 1 here against 15 and 3 there, where the catalogue holds 15.
    /// `maximum` is the ceiling of the family, 4000 for
    /// `nvarchar` and `nchar` (`SELECT CONVERT(nvarchar(5000), 1);` and
    /// `SELECT CAST(1 AS nchar(5000));` print the same message).
    ///
    /// ```text
    /// Size 5000 of the convert specification 'nvarchar' is larger than any data type allows (4000).
    /// ```
    pub fn convert_specification_size_out_of_range(size: u32, name: &str, maximum: u32) -> Self {
        from_catalog_with_severity(
            131,
            16,
            1,
            &[
                Arg::Int(i64::from(size)),
                Arg::Str("convert specification"),
                Arg::Str(name),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 192, severity 15, state 1 (`DECLARE @v decimal(5,6);`): a `decimal` or
    /// `numeric` declares a scale larger than its precision. No argument: the template
    /// names neither the type nor the two numbers.
    ///
    /// The catalogue holds severity 16 for the number and the server sends 15.
    /// `SELECT CAST(1 AS decimal(5,6));` raises the same error, same state.
    ///
    /// ```text
    /// Scale cannot exceed precision.
    /// ```
    pub fn scale_greater_than_precision() -> Self {
        from_catalog_with_severity(192, 15, 1, &[])
    }

    /// Error 1001, severity 15, state 1 (`DECLARE @v varchar(0);`): a length or a
    /// precision is not a legal one, zero for instance (`float(0)` and `decimal(0,0)`
    /// raise it too, as does `SELECT CAST(1 AS varchar(0));`).
    ///
    /// The template opens with the batch line, which the message repeats and the error
    /// carries: a `DECLARE @v varchar(0);` on the second line prints `Line 2:` and sets
    /// [`SqlError::line`] to 2. `specification` is the offending number.
    ///
    /// The catalogue holds severity 16 for the number and the server sends 15.
    ///
    /// ```text
    /// Line 1: the length or precision 0 is not valid.
    /// ```
    pub fn invalid_length_or_precision(line: u32, specification: u32) -> Self {
        from_catalog_with_severity(
            1001,
            15,
            1,
            &[
                Arg::Int(i64::from(line)),
                Arg::Int(i64::from(specification)),
            ],
        )
        .with_line(line)
    }

    /// Error 1002, severity 15, state 1 (`DECLARE @v time(8);`): the scale of a
    /// `time`, `datetime2` or `datetimeoffset` is out of range.
    ///
    /// The `line` argument reads like [`SqlError::invalid_length_or_precision`]'s: the
    /// message repeats the batch line and the error carries it.
    ///
    /// The catalogue holds severity 16 for the number and the server sends 15.
    ///
    /// ```text
    /// Line 1: the scale 8 is not valid.
    /// ```
    pub fn invalid_scale(line: u32, scale: u32) -> Self {
        from_catalog_with_severity(
            1002,
            15,
            1,
            &[Arg::Int(i64::from(line)), Arg::Int(i64::from(scale))],
        )
        .with_line(line)
    }

    /// Error 2717, severity 16, state 1 (`DECLARE @v decimal(39,2);`): the size given to
    /// a type exceeds that type's own maximum, `maximum` (38 for `decimal`), while
    /// staying within what a data type may hold (error 131 is the other side).
    ///
    /// `name` is the type's name, `%S_MSG` prints `type`, and the catalogue holds
    /// severity 15 for the number where the server sends 16.
    /// `SELECT CAST(1 AS decimal(39,2));` raises the same error, same state.
    ///
    /// ```text
    /// Size 39 of the type 'decimal' is larger than the maximum (38).
    /// ```
    pub fn type_size_out_of_range(size: u32, name: &str, maximum: u32) -> Self {
        from_catalog_with_severity(
            2717,
            16,
            1,
            &[
                Arg::Int(i64::from(size)),
                Arg::Str("type"),
                Arg::Str(name),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 2717, severity 16, state 2 (`DECLARE @v nvarchar(5000);`): same ceiling as
    /// [`SqlError::type_size_out_of_range`], but the message names the declared object
    /// rather than the type, `%S_MSG` printing `parameter`.
    ///
    /// `name` is the variable's name, `@v` at signal included, or the column's name for a
    /// `CREATE TABLE #t(c nvarchar(5000));`, which prints `parameter` as well. `maximum`
    /// is 4000 for the `nvarchar` and `nchar` family.
    ///
    /// ```text
    /// Size 5000 of the parameter '@v' is larger than the maximum (4000).
    /// ```
    pub fn parameter_size_out_of_range(size: u32, name: &str, maximum: u32) -> Self {
        from_catalog_with_severity(
            2717,
            16,
            2,
            &[
                Arg::Int(i64::from(size)),
                Arg::Str("parameter"),
                Arg::Str(name),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 2750, severity 16, state 1 (`DECLARE @v float(54);`): the precision of a
    /// column, parameter or variable is greater than `maximum` (53 for `float`, 38 for
    /// `decimal` in a `CREATE TABLE #t(c decimal(39,2));`).
    ///
    /// `position` is the 1-based rank of the variable or column in the batch, the way
    /// [`SqlError::cannot_find_data_type`] counts: a third `DECLARE` prints `#3`, and two
    /// offending variables of one `DECLARE` raise the error twice, `#1` then `#2`.
    ///
    /// ```text
    /// Column or parameter #1: precision 54 exceeds the maximum of 53.
    /// ```
    pub fn precision_greater_than_maximum(position: u32, precision: u32, maximum: u32) -> Self {
        from_catalog(
            2750,
            1,
            &[
                Arg::Int(i64::from(position)),
                Arg::Int(i64::from(precision)),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 243, severity 16, state 1 (`SELECT CAST(1 AS foo);`): the target of a `CAST`
    /// or a `CONVERT` names a type that does not exist. `name` is that name.
    ///
    /// A declaration of the same unknown type raises 2715 instead
    /// (`DECLARE @v foo;` → [`SqlError::cannot_find_data_type`]).
    ///
    /// ```text
    /// foo is not a known system type.
    /// ```
    pub fn not_a_defined_system_type(name: &str) -> Self {
        from_catalog(243, 1, &[Arg::Str(name)])
    }

    /// Error 295, severity 16, state 3
    /// (`SELECT CAST('not a date' AS smalldatetime);`): a character string towards
    /// `smalldatetime` is not a date at all. No argument.
    ///
    /// The `smalldatetime` twin of [`SqlError::conversion_failed_datetime`] (241, from a
    /// `datetime` target). A string that *is* a date but falls outside the
    /// `smalldatetime` range raises 242 state 3 instead
    /// (`SELECT CAST('1899-01-01' AS smalldatetime);`,
    /// [`SqlError::out_of_range_conversion`]).
    ///
    /// A fraction of more than three digits takes the same road, and the target decides
    /// the number: `SELECT CAST('2020-01-01 00:00:00.1234' AS smalldatetime);` raises 295
    /// state 3 and the same string towards `datetime` raises 241 state 1.
    ///
    /// ```text
    /// The character string could not be converted to smalldatetime.
    /// ```
    pub fn conversion_failed_smalldatetime() -> Self {
        from_catalog(295, 3, &[])
    }

    /// Error 4121, severity 16, state 1 (`SELECT dbo.LEN(1);`): a call names a function
    /// with a qualified name, and no user-defined function or aggregate answers to it.
    ///
    /// A qualified name raises it, under a system schema as under a user one:
    /// `SELECT dbo.LEN(1);`, `SELECT sys.LEN(1);` and `SELECT foo.bar(1);`, because a
    /// built-in function is not reached through a schema. An unqualified unknown name
    /// raises 195 instead ([`SqlError::not_a_recognized_name`], `SELECT bar(1);`).
    ///
    /// `name` is the qualified name with its identifiers already unquoted, case as the
    /// user wrote it (`SELECT [dbo].[LEN](1);` prints `dbo.LEN`). The template holds two
    /// placeholders for that one name: the leading part, which the server reads as a
    /// possible column, then the whole name: `SELECT dbo.foo.bar(1);` prints `"dbo"` and
    /// `"dbo.foo.bar"`. This constructor derives the leading part, the text before the
    /// first `.`, so the caller passes the name once.
    ///
    /// ```text
    /// Neither a column "dbo" nor a user-defined function or aggregate "dbo.LEN" was found, or the name is ambiguous.
    /// ```
    pub fn cannot_find_column_or_function(name: &str) -> Self {
        let leading = name.split('.').next().unwrap_or(name);
        from_catalog(4121, 1, &[Arg::Str(leading), Arg::Str(name)])
    }

    /// Error 4104, severity 16, state 1 (`SELECT t.c;`): a column is reached through a
    /// qualifier no source of the query answers to.
    ///
    /// `name` is the **whole** multi-part name, its identifiers already unquoted and the
    /// case as the user wrote it: the template holds a single `%.*ls` and the server puts
    /// the full name in it, however many parts it has: `SELECT a.b.c.d;` prints
    /// `"a.b.c.d"` and `SELECT [dbo].[t].[c];` prints `"dbo.t.c"`. The state stays 1
    /// from two parts to four, and a qualifier that misses an existing source
    /// (`SELECT x.y FROM (SELECT 1 AS c) AS z;`) sends 1 too.
    ///
    /// An *unqualified* unknown column raises 207 instead
    /// ([`SqlError::invalid_column_name`]).
    ///
    /// ```text
    /// The qualified name "t.c" matches nothing in scope.
    /// ```
    pub fn multi_part_identifier(name: &str) -> Self {
        from_catalog(4104, 1, &[Arg::Str(name)])
    }

    /// Error 1062, severity 15, state 1 (`SELECT TOP (1) WITH TIES 1 AS c;`): a
    /// `TOP … WITH TIES` has no `ORDER BY` to break its ties against. No argument.
    ///
    /// The severity is the sent one: the catalogue holds 16 and the server sends 15, so
    /// the constructor overrides the catalogue (`WITH_TIES_1062_SEVERITY`,
    /// `format::from_catalog_with_severity`). The row count plays no part: `TOP (0)`,
    /// `TOP 1` and `TOP (1) PERCENT` raise it, and adding an `ORDER BY` makes each of them
    /// succeed.
    ///
    /// ```text
    /// TOP WITH TIES requires an ORDER BY clause.
    /// ```
    pub fn top_with_ties_without_order_by() -> Self {
        from_catalog_with_severity(1062, WITH_TIES_1062_SEVERITY, 1, &[])
    }

    /// Error 107, severity 15, state 1 (`SELECT t.*;`): a qualified wildcard names a
    /// prefix no source of the query answers to.
    ///
    /// `prefix` is the whole prefix, its identifiers already unquoted and the trailing `*`
    /// left out: `SELECT [dbo].[t].*;` prints `'dbo.t'` and `SELECT db1.dbo.t.*;` prints
    /// `'db1.dbo.t'`. A prefix of four parts does not reach this error, 117 winning
    /// before the lookup ([`SqlError::too_many_column_prefixes`]), and the same name
    /// without its `.*` raises 4104 ([`SqlError::multi_part_identifier`]).
    ///
    /// ```text
    /// The column prefix 't' matches no table or alias of the query.
    /// ```
    pub fn column_prefix_does_not_match(prefix: &str) -> Self {
        from_catalog(107, 1, &[Arg::Str(prefix)])
    }

    /// Error 117, severity 15, state 1 (`SELECT a.b.c.d.*;`): a column name carries more
    /// than the three prefixes SQL Server allows.
    ///
    /// `name` is the whole name, its identifiers already unquoted and the trailing `*` left
    /// out (`SELECT [a].[b].[c].[d].*;` prints `'a.b.c.d'`). The constructor supplies the
    /// two other placeholders: the `%S_MSG`, `column` for this context, and the maximum,
    /// [`MAX_NAME_PREFIXES`]. Two other fillings exist, `object` for
    /// `SELECT * FROM a.b.c.d.e;` and `procedure` for `EXEC a.b.c.d.e;`, both severity 15
    /// state 1; each gets its own constructor when a caller needs it, the way 131 and 2717
    /// have one per context.
    ///
    /// ```text
    /// The column name 'a.b.c.d' has too many prefixes; at most 3 are allowed.
    /// ```
    pub fn too_many_column_prefixes(name: &str) -> Self {
        from_catalog(
            117,
            1,
            &[
                Arg::Str("column"),
                Arg::Str(name),
                Arg::Int(MAX_NAME_PREFIXES),
            ],
        )
    }

    /// Error 281, severity 16, state 1
    /// (`SELECT CONVERT(varchar(30), CAST('2020-01-01' AS date), 999);`): a `CONVERT`
    /// towards a character type names a style that type has no reading for.
    ///
    /// `from` is the source type, the one the value is converted from: the target is not
    /// named, the template spells it out as `a character string`. Six sources under one
    /// target print six different words (`date`, `time`, `datetime`, `smalldatetime`,
    /// `datetime2`, `datetimeoffset`), and four targets under one source print the same
    /// one: `CONVERT(nvarchar(30), ...)`, `CONVERT(char(30), ...)` and
    /// `CONVERT(nchar(30), ...)` print `from date`.
    ///
    /// `style` is printed as written, a negative value included (`..., -1);` prints `-1`).
    /// The date and time types raise it; a `varbinary` source raises 9809
    /// ([`SqlError::unsupported_convert_style`]) and `int`, `float`, `money` and
    /// `uniqueidentifier` accept style 999 without a word.
    ///
    /// ```text
    /// Style 999 is not defined for converting date to a character string.
    /// ```
    pub fn invalid_style_number(style: i32, from: &str) -> Self {
        from_catalog(281, 1, &[Arg::Int(i64::from(style)), Arg::Str(from)])
    }

    /// Error 9807, severity 16, state 0
    /// (`SELECT CONVERT(date, '2020-01-01', 100);`): a character string does not have the
    /// shape the requested style demands.
    ///
    /// The four types introduced with SQL Server 2008 raise it, `date`, `time`,
    /// `datetime2` and `datetimeoffset`, for the seventeen strict styles:
    ///
    /// > 6, 7, 8, 9, 12, 13, 14, 24, 100, 106, 107, 108, 109, 112, 113, 114, 130
    ///
    /// Outside the seventeen, a style still has to be supported on those four targets.
    /// The lenient reading, 241 when the string cannot be read, holds for a style the
    /// pair accepts; an unsupported style is refused before the string is read, by 9809
    /// ([`SqlError::unsupported_convert_style`], severity 16 state 1). Over
    /// `SELECT CONVERT(date, 'zzz', N);` for N in 0 to 255, 42 styles answer 241 and 214
    /// answer 9809: the seventeen strict styles are among the 42, and the other lenient
    /// styles are 0 to 5, 10, 11, 20 to 23, 25, 101 to 105, 110, 111, 120, 121, 126, 127
    /// and 131.
    ///
    /// `time`, `datetime2` and `datetimeoffset` answer what `date` answers, style by
    /// style and string by string; `datetime` and `smalldatetime` answer neither 9809 nor
    /// 9807, from style -1 to 256. On those two the style selects a grammar instead of
    /// being validated, and the string is refused by the style-less error, 241 towards
    /// `datetime`, 295 towards `smalldatetime`
    /// ([`SqlError::conversion_failed_smalldatetime`]), or by 242 when its shape is the one
    /// the style reads and a piece falls out of range. Four vectors tell the two families
    /// apart:
    ///
    /// | vector | answer |
    /// |---|---|
    /// | `CONVERT(smalldatetime, '20000102', 77)` | `Jan  2 2000 12:00AM` |
    /// | `CONVERT(datetime, '2000-01-02', 77)` | 241 |
    /// | `CONVERT(date, '2000-01-02', 77)` | 9809 |
    /// | `CONVERT(smalldatetime, '2000-01-02 13:05:29.999', 112)` | 295 |
    ///
    /// The grammar each style selects on a legacy target is described by
    /// `vauban_types::convert::datetime::legacy_style`.
    ///
    /// A caller that validates a style therefore reaches for
    /// [`unsupported_convert_style`](Self::unsupported_convert_style) first
    /// (`SELECT CONVERT(date, 'zzz', 200);` answers 9809, as do 15, 128 and 255, while 100
    /// and 126 answer 241), and for this constructor once the style is known to be
    /// supported and strict.
    ///
    /// ```text
    /// The input string does not match style 100; change the string or the style.
    /// ```
    pub fn input_does_not_follow_style(style: i32) -> Self {
        from_catalog(9807, 0, &[Arg::Int(i64::from(style))])
    }

    /// Error 1031, severity 15, state 1 (`SELECT TOP (101) PERCENT 1;`): the percentage of
    /// a `TOP … PERCENT` falls outside `0..=100`. No argument.
    ///
    /// The severity is 15, the catalogue's, so no override. `SELECT TOP 200 PERCENT 1;`,
    /// `SELECT TOP (-1) PERCENT 1;` and `SELECT TOP (1e40) PERCENT 1;` send severity 15
    /// state 1 as well; the bounds themselves are accepted, `TOP (100) PERCENT` and
    /// `TOP (0.5) PERCENT` returning rows.
    ///
    /// ```text
    /// A percentage has to lie between 0 and 100.
    /// ```
    pub fn percent_out_of_range() -> Self {
        from_catalog(1031, 1, &[])
    }

    /// Error 1014, severity 15, state 1 (`SELECT TOP (NULL) PERCENT 1;`): a
    /// `TOP … PERCENT` was given a value it cannot read as a percentage. No argument.
    ///
    /// The `PERCENT` is what tells this error from 1060: the very same `TOP (NULL)`
    /// without it raises 1060 ([`SqlError::top_null`]), severity 15 state 1 as well.
    /// Severity 15 is the catalogue's and the sent value, so no override.
    ///
    /// ```text
    /// The value of the TOP or FETCH clause is not valid.
    /// ```
    pub fn top_invalid_value() -> Self {
        from_catalog(1014, 1, &[])
    }

    /// Error 506, severity 16, state 2
    /// (`SELECT 1 WHERE 'a' LIKE 'a' ESCAPE N'ab';`): the Unicode twin of
    /// [`SqlError::invalid_escape`].
    ///
    /// The state follows the **predicate**, not the operand the message quotes: one
    /// Unicode operand among the value, the pattern and the escape is enough
    /// ([`ESCAPE_506_STATES`]). The message itself is the same text as the narrow form;
    /// the state differs, which is why this is a second constructor rather than a flag.
    ///
    /// ```text
    /// The escape character "ab" of the LIKE predicate must be a single character.
    /// ```
    pub fn invalid_escape_unicode(escape: &str, predicate: &str) -> Self {
        from_catalog(
            506,
            escape_state(LikePredicateWidth::Unicode),
            &[Arg::Str(escape), Arg::Str(predicate)],
        )
    }

    // ---------------------------------------------------------------------------------
    // DDL errors. Raising them is left to the catalogue, the binder and the executor.
    // ---------------------------------------------------------------------------------

    /// Error 1801, severity 16, state 3 (`CREATE DATABASE master;`): `CREATE DATABASE`
    /// names a database the instance already holds.
    ///
    /// `name` is the database name as written, its delimiters removed:
    /// `CREATE DATABASE [master];` prints `'master'`. See [`DATABASE_EXISTS_1801_STATE`].
    ///
    /// ```text
    /// A database named 'd' exists already; pick another name.
    /// ```
    pub fn database_already_exists(name: &str) -> Self {
        from_catalog(1801, DATABASE_EXISTS_1801_STATE, &[Arg::Str(name)])
    }

    /// Error 2714, severity 16, state 6 (`CREATE TABLE t (a int);` twice): a
    /// `CREATE TABLE` names an object the database already holds, be it a table or a
    /// view: both send 6.
    ///
    /// `name` is the object name as written, its delimiters and its schema removed:
    /// `CREATE TABLE dbo.t2 (a int);` prints `'t2'`.
    ///
    /// This is the state of a permanent `CREATE TABLE`. The other statements send the
    /// same text at another state and get their own constructor when a caller needs one
    /// ([`OBJECT_EXISTS_2714_TABLE_STATE`]). A duplicate index is a different error:
    /// `CREATE INDEX ix5 ON t5 (a);` twice answers 1913
    /// (catalogued without a constructor).
    ///
    /// ```text
    /// An object named 't' exists already in the database.
    /// ```
    pub fn object_already_exists(name: &str) -> Self {
        from_catalog(2714, OBJECT_EXISTS_2714_TABLE_STATE, &[Arg::Str(name)])
    }

    /// Error 2705, severity 16, state 3 (`CREATE TABLE t_dup (a int, a int);`): a
    /// `CREATE TABLE` names the same column twice.
    ///
    /// `column` is the name of the later copy, as written and delimiters removed
    /// (`CREATE TABLE t_dup (a int, A int);` prints `'A'`), and `table` the table name
    /// without its schema: `CREATE TABLE dbo.t_dup (a int, a int);` prints `'t_dup'`,
    /// `CREATE TABLE [t dup] ...` prints `'t dup'`, `CREATE TABLE #t_dup ...` prints
    /// `'#t_dup'` and `DECLARE @t TABLE (a int, a int);` prints `'@t'`.
    ///
    /// When the same statement also names an unknown type, the first fault in the column
    /// list wins: `CREATE TABLE t_both (a int, a foo);` answers 2705 state 3, while
    /// `CREATE TABLE t_both (a foo, b int, b int);` answers 2715 state 6. The
    /// `ALTER TABLE ... ADD` path sends state 4 and needs its own constructor
    /// ([`DUPLICATE_COLUMN_2705_CREATE_TABLE_STATE`]).
    ///
    /// ```text
    /// Column 'a' of table 't_dup' is defined twice; column names have to be unique.
    /// ```
    pub fn duplicate_column_name(column: &str, table: &str) -> Self {
        from_catalog(
            2705,
            DUPLICATE_COLUMN_2705_CREATE_TABLE_STATE,
            &[Arg::Str(column), Arg::Str(table)],
        )
    }

    /// Error 3701, severity 11 (`DROP TABLE nope;`): a `DROP` names an object that is not
    /// there, or that the login may not see.
    ///
    /// The template holds two `%S_MSG`:
    /// - `action`: `drop` ([`CANNOT_DROP_3701_STATES`]). `ALTER PROCEDURE nope_p`,
    ///   `ALTER VIEW nope_v` and `ALTER FUNCTION nope_f` answer 208 instead of an `alter`
    ///   filling of 3701, so the parameter stays open rather than being fixed here;
    /// - `kind`: `table`, `database`, `index`, and also `view`, `procedure`, `function`
    ///   and `trigger`. It is the key of the state table.
    ///
    /// `name` is the object name as written, its delimiters removed; for an index it is
    /// the table and the index joined by a dot (`DROP INDEX ix_nope ON t3;` prints
    /// `'t3.ix_nope'`). The state follows `kind` and, for an index whose table is
    /// missing, would be 6 rather than the 7 of this table: see
    /// [`CANNOT_DROP_3701_STATES`].
    ///
    /// ```text
    /// Unable to drop the table 'nope': it does not exist or is not accessible.
    /// ```
    pub fn cannot_drop(action: &str, kind: &str, name: &str) -> Self {
        from_catalog(
            3701,
            ddl_state(CANNOT_DROP_3701_STATES, kind),
            &[Arg::Str(action), Arg::Str(kind), Arg::Str(name)],
        )
    }

    /// Error 3708, severity 16, state 4 (`DROP DATABASE master;`): a `DROP DATABASE` names
    /// one of the databases the server owns.
    ///
    /// The template holds three `%S_MSG` around its `%.*ls`, filled `drop`, `database`,
    /// the name, then `database` again; this constructor writes the three fillings itself
    /// and takes the name alone. `name` is the database name as written, delimiters
    /// removed and case kept: `DROP DATABASE MASTER;` prints `'MASTER'` and
    /// `DROP DATABASE [MaStEr];` prints `'MaStEr'`.
    ///
    /// The four system databases answer it, `master`, `tempdb`, `model`, `msdb`, while
    /// `DROP DATABASE nope;` answers 3701 severity 11 state 1 and `DROP TABLE sys.objects;`
    /// answers 3701 state 5: the number follows the system database, not the `sys` prefix
    /// nor the absence of the object. Inside a transaction the number changes again:
    /// `BEGIN TRANSACTION; DROP DATABASE master;` answers 574
    /// ([`SqlError::drop_database_in_transaction`]), so 574 is raised before 3708 on that
    /// path.
    ///
    /// ```text
    /// Unable to drop the database 'master': it is a system database.
    /// ```
    pub fn cannot_drop_system_database(name: &str) -> Self {
        from_catalog(
            3708,
            CANNOT_DROP_SYSTEM_DATABASE_3708_STATE,
            &[
                Arg::Str("drop"),
                Arg::Str("database"),
                Arg::Str(name),
                Arg::Str("database"),
            ],
        )
    }

    /// Error 226, severity 16, state 5 for `CREATE DATABASE`
    /// (`BEGIN TRANSACTION; CREATE DATABASE d;`): a statement that cannot take part in a
    /// user transaction was written inside one.
    ///
    /// `statement` is the statement name in upper case, as the server prints it, and it
    /// is also the key of the state table: `CREATE DATABASE` 5, `ALTER DATABASE` 6,
    /// `ALTER DATABASE SCOPED CONFIGURATION` 7
    /// ([`NOT_ALLOWED_IN_TRANSACTION_226_STATES`]). `DROP DATABASE` is not one of them:
    /// it answers 574, whose constructor is [`SqlError::drop_database_in_transaction`].
    ///
    /// ```text
    /// CREATE DATABASE is not allowed inside a multi-statement transaction.
    /// ```
    pub fn statement_not_allowed_in_transaction(statement: &str) -> Self {
        from_catalog(
            226,
            ddl_state(NOT_ALLOWED_IN_TRANSACTION_226_STATES, statement),
            &[Arg::Str(statement)],
        )
    }

    /// Error 574, severity 16, state 0 (`BEGIN TRANSACTION; DROP DATABASE d;`): a
    /// `DROP DATABASE` was written inside a user transaction.
    ///
    /// The single `%ls` is the statement name, which this constructor fills with
    /// `DROP DATABASE`. The `%ls` does take other statements
    /// (`BEGIN TRANSACTION; CREATE FULLTEXT CATALOG ft;` prints `CREATE FULLTEXT CATALOG`
    /// at the same severity and the same state), and such a caller adds its own
    /// constructor, the way 131 and 2717 have one per context.
    ///
    /// 574 is not a filling of 226: `CREATE DATABASE` inside a transaction answers 226
    /// ([`SqlError::statement_not_allowed_in_transaction`]) with another sentence. The
    /// state is 0 ([`DROP_DATABASE_IN_TRANSACTION_574_STATE`]), and the number does not
    /// move with the database named: a missing database, an existing one and `master`
    /// each answer 574 rather than 3701 or 3708.
    ///
    /// ```text
    /// DROP DATABASE is not allowed inside a user transaction.
    /// ```
    pub fn drop_database_in_transaction() -> Self {
        from_catalog(
            574,
            DROP_DATABASE_IN_TRANSACTION_574_STATE,
            &[Arg::Str("DROP DATABASE")],
        )
    }

    /// Error 209, severity 16, state 1 (`SELECT c FROM (SELECT 1 AS c) AS x CROSS JOIN
    /// (SELECT 2 AS c) AS y;`): a column name answers to more than one source of the
    /// query.
    ///
    /// `name` is the column name alone, its qualifiers and its delimiters removed and its
    /// case as written: the same query over `[c d]` prints `'c d'`. Severity 16 state 1
    /// for two derived sources, two real tables, three derived sources, an ambiguity
    /// carried by a `WHERE` or by an `ORDER BY` over two identical output aliases, and a
    /// delimited name: the state does not move with the clause that meets the ambiguity,
    /// on those shapes.
    ///
    /// A column no source answers to raises 207 instead
    /// ([`SqlError::invalid_column_name`]), and one reached through an unknown qualifier
    /// raises 4104 ([`SqlError::multi_part_identifier`]).
    ///
    /// ```text
    /// Column name 'a' is ambiguous.
    /// ```
    pub fn ambiguous_column_name(name: &str) -> Self {
        from_catalog(209, 1, &[Arg::Str(name)])
    }

    // ---------------------------------------------------------------------------------
    // DML, flow control, transactions and concurrency; the query that raises each one is
    // quoted in its rustdoc. Raising them is left to the binder, the executor, the
    // catalogue and the session.
    // ---------------------------------------------------------------------------------

    /// Error 116, severity 16, state 1 (`SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);`):
    /// a subquery not introduced with `EXISTS` carries more than one expression in its
    /// select list.
    ///
    /// The sentence takes no argument. The catalogue holds severity 15 and the server
    /// sends 16 ([`ONLY_ONE_EXPRESSION_116_SEVERITY`]).
    ///
    /// ```text
    /// A subquery not introduced by EXISTS must select a single expression.
    /// ```
    pub fn only_one_expression_in_subquery() -> Self {
        from_catalog_with_severity(116, ONLY_ONE_EXPRESSION_116_SEVERITY, 1, &[])
    }

    /// Error 141, severity 15, state 1 (`DECLARE @x int; SELECT @x = a, b FROM dbo.t2;`):
    /// a `SELECT` both assigns a variable and returns a column.
    ///
    /// No argument.
    ///
    /// ```text
    /// A SELECT that assigns variables cannot also return rows.
    /// ```
    pub fn assignment_mixed_with_data_retrieval() -> Self {
        from_catalog(141, 1, &[])
    }

    /// Error 130, severity 15, state 1 (`SELECT SUM(COUNT(*)) FROM dbo.t2;`): an
    /// aggregate is applied to an expression that itself holds an aggregate, whichever
    /// operator sits between the two (`SELECT SUM(1 + MIN(a)) FROM dbo.t2;` raises it as
    /// well). SQL Server reports it on the line of the inner call.
    ///
    /// No argument.
    ///
    /// ```text
    /// An aggregate function cannot be applied to an expression that holds an aggregate or a subquery.
    /// ```
    pub fn nested_aggregate() -> Self {
        from_catalog(130, 1, &[])
    }

    /// Error 144, severity 15, state 1 (`SELECT a FROM dbo.t2 GROUP BY SUM(b);`): a
    /// `GROUP BY` key holds an aggregate. SQL Server reports it on the line of the call.
    ///
    /// No argument.
    ///
    /// ```text
    /// A GROUP BY expression cannot hold an aggregate or a subquery.
    /// ```
    pub fn aggregate_in_group_by() -> Self {
        from_catalog(144, 1, &[])
    }

    /// Error 147, severity 15, state 1 (`SELECT a FROM dbo.t2 WHERE COUNT(*) > 1;`): a
    /// `WHERE` holds an aggregate, with or without a `GROUP BY` after it, and with or
    /// without a `FROM` (`SELECT 1 WHERE COUNT(*) > 0;`). SQL Server reports it on the
    /// line of the statement, not on the line of the call.
    ///
    /// No argument.
    ///
    /// ```text
    /// An aggregate cannot appear in a WHERE clause, except inside a subquery of a HAVING clause or a select list, aggregating an outer reference.
    /// ```
    pub fn aggregate_in_where() -> Self {
        from_catalog(147, 1, &[])
    }

    /// Error 145, severity 15, state 1 (`SELECT DISTINCT a FROM dbo.t2 ORDER BY b;`): an
    /// `ORDER BY` item is absent from the select list of a `SELECT DISTINCT`.
    ///
    /// No argument.
    ///
    /// ```text
    /// With SELECT DISTINCT, each ORDER BY item has to be in the select list.
    /// ```
    pub fn order_by_item_not_in_distinct_select_list() -> Self {
        from_catalog(145, 1, &[])
    }

    /// Error 205, severity 16, state 1
    /// (`SELECT a FROM dbo.t2 UNION SELECT a, b FROM dbo.t2;`): two branches of a set
    /// operator do not carry the same number of expressions.
    ///
    /// No argument: the sentence names `UNION`, `INTERSECT` and `EXCEPT` itself; the batch
    /// above wrote `UNION` and printed the three names.
    ///
    /// ```text
    /// Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions.
    /// ```
    pub fn set_operator_column_count_mismatch() -> Self {
        from_catalog(205, 1, &[])
    }

    /// Error 266, severity 16, state 2
    /// (`EXEC('CREATE PROCEDURE dbo.p266 AS BEGIN TRANSACTION;'); EXEC dbo.p266;`): a
    /// procedure left the transaction count somewhere else than it found it.
    ///
    /// `previous` and `current` are the two `%ld` of the template, in that order: the
    /// count before the `EXECUTE` and the count after it. A procedure opening one
    /// transaction printed `Previous count = 0, current count = 1.` and one opening two
    /// printed `current count = 2.`, both at severity 16 state 2. The server sends the
    /// error with line 0, which the caller may replace through
    /// [`SqlError::with_line`].
    ///
    /// ```text
    /// The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before 0, after 1).
    /// ```
    pub fn transaction_count_after_execute(previous: i64, current: i64) -> Self {
        from_catalog(266, 2, &[Arg::Int(previous), Arg::Int(current)])
    }

    /// Error 512, severity 16, state 1 (`SELECT (SELECT a FROM dbo.tv);` over two rows): a
    /// scalar subquery returned more than one row.
    ///
    /// No argument.
    ///
    /// ```text
    /// The subquery returned several values, which is not allowed after a comparison operator or as an expression.
    /// ```
    pub fn subquery_returned_more_than_one_value() -> Self {
        from_catalog(512, 1, &[])
    }

    /// Error 1047, severity 15, state 1
    /// (`SELECT * FROM dbo.lone WITH (NOLOCK, TABLOCKX);`): a table hint list holds two
    /// hints that contradict each other.
    ///
    /// No argument: the sentence names neither hint.
    ///
    /// ```text
    /// The locking hints contradict each other.
    /// ```
    pub fn conflicting_locking_hints() -> Self {
        from_catalog(1047, 1, &[])
    }

    /// Error 1065, severity 15, state 1 (`UPDATE dbo.lone WITH (NOLOCK) SET a = 1;`):
    /// `NOLOCK` or `READUNCOMMITTED` sits on the target of a data-modification statement.
    ///
    /// No argument: the sentence lists the four statements itself.
    ///
    /// ```text
    /// NOLOCK and READUNCOMMITTED cannot be applied to the target table of INSERT, UPDATE, DELETE or MERGE.
    /// ```
    pub fn nolock_not_allowed_on_target() -> Self {
        from_catalog(1065, 1, &[])
    }

    /// Error 1769, severity 16, state 1
    /// (`ALTER TABLE dbo.lone ADD CONSTRAINT fk_bad FOREIGN KEY (nosuch) REFERENCES
    /// dbo.parent(id);`): a `FOREIGN KEY` names a column the referencing table has not.
    ///
    /// The three `%.*ls` are, in the order of the template, the constraint name, the
    /// unknown column and the referencing table. The batch above prints the table
    /// unqualified (`'lone'`) where 1776 prints its referenced table qualified
    /// (`'dbo.nopk'`), so each constructor takes the name as its message prints it and
    /// does not qualify anything itself.
    ///
    /// ```text
    /// Foreign key 'fk_bad' names the column 'nosuch', which the referencing table 'lone' does not have.
    /// ```
    pub fn foreign_key_references_invalid_column(
        constraint: &str,
        column: &str,
        table: &str,
    ) -> Self {
        from_catalog(
            1769,
            1,
            &[Arg::Str(constraint), Arg::Str(column), Arg::Str(table)],
        )
    }

    /// Error 1776, severity 16, state 0
    /// (`ALTER TABLE dbo.lone ADD CONSTRAINT fk_nopk FOREIGN KEY (a) REFERENCES
    /// dbo.nopk(id);`): the referenced table holds no key matching the referencing column
    /// list.
    ///
    /// The two `%.*ls` are the referenced table then the constraint. State 0, not the
    /// default 1, for the statement quoted above as for a two-column variant
    /// (`FOREIGN KEY (a, c) REFERENCES dbo.nopk(id, other)`).
    ///
    /// ```text
    /// The referenced table 'dbo.nopk' has no primary or unique key matching the columns of foreign key 'fk_nopk'.
    /// ```
    pub fn no_matching_key_in_referenced_table(referenced_table: &str, constraint: &str) -> Self {
        from_catalog(1776, 0, &[Arg::Str(referenced_table), Arg::Str(constraint)])
    }

    /// Error 3726, severity 16, state 1 (`DROP TABLE dbo.parent;` while another table
    /// references it): a `DROP` targets an object a `FOREIGN KEY` points at.
    ///
    /// `name` is the object name as the server printed it, schema included
    /// (`'dbo.parent'`). A `TRUNCATE` of the same table answers 4712 instead
    /// ([`SqlError::cannot_truncate_referenced_table`]).
    ///
    /// ```text
    /// Object 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be dropped.
    /// ```
    pub fn cannot_drop_referenced_object(name: &str) -> Self {
        from_catalog(3726, 1, &[Arg::Str(name)])
    }

    /// Error 3728, severity 16, state 1 (`ALTER TABLE dbo.t DROP CONSTRAINT nosuch;`
    /// when `nosuch` is not a constraint on `t`).
    ///
    /// ```text
    /// 'nosuch' is not the name of a constraint here.
    /// ```
    pub fn constraint_not_on_table(name: &str) -> Self {
        from_catalog(3728, 1, &[Arg::Str(name)])
    }

    /// Error 3952, severity 16, state 1 (`SET TRANSACTION ISOLATION LEVEL SNAPSHOT;
    /// BEGIN TRANSACTION; SELECT v FROM dbo.s WHERE k = 1;` in a database whose
    /// `snapshot_isolation_state` is 0): a snapshot transaction reached a database that
    /// does not allow snapshot isolation.
    ///
    /// `database` is the database name, undelimited. The database setting, not the
    /// isolation level alone, is what raises 3952: the same batch in a database whose
    /// `snapshot_isolation_state` is 1 returns rows instead.
    ///
    /// ```text
    /// Database 'snapdb' does not allow snapshot isolation; enable it with ALTER DATABASE.
    /// ```
    pub fn snapshot_isolation_not_allowed(database: &str) -> Self {
        from_catalog(3952, 1, &[Arg::Str(database)])
    }

    /// Error 4712, severity 16, state 1 (`TRUNCATE TABLE dbo.parent;` while another table
    /// references it): a `TRUNCATE` targets a table a `FOREIGN KEY` points at.
    ///
    /// `name` is the table name as the server printed it, schema included
    /// (`'dbo.parent'`). The `DROP` of the same table answers 3726
    /// ([`SqlError::cannot_drop_referenced_object`]).
    ///
    /// ```text
    /// Table 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be truncated.
    /// ```
    pub fn cannot_truncate_referenced_table(name: &str) -> Self {
        from_catalog(4712, 1, &[Arg::Str(name)])
    }

    /// Error 4901, severity 16, state 1 (`ALTER TABLE dbo.notempty ADD b int NOT NULL;`
    /// on a table holding one row): a column that admits no null and carries no default
    /// is added to a table that is not empty.
    ///
    /// The two `%.*ls` are the column then the table; the batch above printed the table
    /// unqualified (`'notempty'`).
    ///
    /// ```text
    /// A column added to a non-empty table has to be nullable, have a DEFAULT, or be an identity or timestamp column. Column 'b' cannot be added to table 'notempty'.
    /// ```
    pub fn cannot_add_column_to_non_empty_table(column: &str, table: &str) -> Self {
        from_catalog(4901, 1, &[Arg::Str(column), Arg::Str(table)])
    }

    /// Error 4902, severity 16, state 1 (`ALTER TABLE dbo.nosuch ADD c int NULL;` when
    /// `nosuch` is not in the catalogue).
    ///
    /// `name` is the qualified name the server prints (`'dbo.nosuch'`).
    ///
    /// ```text
    /// Object "dbo.nosuch" was not found: it does not exist or this login lacks permissions.
    /// ```
    pub fn cannot_find_object_to_alter_table(name: &str) -> Self {
        from_catalog(4902, 1, &[Arg::Str(name)])
    }

    /// Error 4924, severity 16, state 1 (`ALTER TABLE dbo.t DROP COLUMN nosuch;` when
    /// `nosuch` is not a column of `t`).
    ///
    /// ```text
    /// ALTER TABLE DROP COLUMN could not run: column 'nosuch' is missing from table 't'.
    /// ```
    pub fn alter_table_drop_column_missing(column: &str, table: &str) -> Self {
        from_catalog(4924, 1, &[Arg::Str(column), Arg::Str(table)])
    }

    /// Error 5074, severity 16, state 1 (`ALTER TABLE dbo.ix_t DROP COLUMN b;` after
    /// `CREATE INDEX ix ON dbo.ix_t(b);`): an index or another object still references
    /// the column.
    ///
    /// The two `%S_MSG`/`%.*ls` pairs name the dependent object then the column. An index
    /// prints `index` and `column`; a `CHECK` or `DEFAULT` prints `object` and `column`.
    ///
    /// ```text
    /// index 'ix' still depends on column 'b'.
    /// ```
    pub fn object_depends_on_column(object_kind: &str, object: &str, column: &str) -> Self {
        from_catalog(
            5074,
            1,
            &[
                Arg::Str(object_kind),
                Arg::Str(object),
                Arg::Str("column"),
                Arg::Str(column),
            ],
        )
    }

    /// Error 8101, severity 16, state 1 (`INSERT INTO dbo.ident VALUES (5, 1);` on a table
    /// whose first column is an `IDENTITY`): an explicit value reaches an identity column
    /// through an `INSERT` without a column list.
    ///
    /// `table` is the table name as the server printed it, schema included
    /// (`'dbo.ident'`). The same `INSERT` with a column list answers 544 instead
    /// ([`SqlError::identity_insert_is_off`]): `INSERT INTO dbo.ident (id, v) VALUES (1, 1);`
    /// prints 544 with the table unqualified, `'ident'`.
    ///
    /// ```text
    /// An explicit value for the identity column of table 'dbo.ident' requires a column list and IDENTITY_INSERT ON.
    /// ```
    pub fn identity_insert_requires_column_list(table: &str) -> Self {
        from_catalog(8101, 1, &[Arg::Str(table)])
    }

    /// Error 8102, severity 16, state 1 (`UPDATE dbo.ident SET id = 2;` where `id` is an
    /// `IDENTITY`): an `UPDATE` writes an identity column.
    ///
    /// `column` is the column name alone, undelimited and unqualified (`'id'`).
    ///
    /// ```text
    /// Identity column 'id' cannot be updated.
    /// ```
    pub fn cannot_update_identity_column(column: &str) -> Self {
        from_catalog(8102, 1, &[Arg::Str(column)])
    }

    /// Error 8107, severity 16, state 1
    /// (`SET IDENTITY_INSERT dbo.t1 ON; SET IDENTITY_INSERT dbo.t2 ON;`): a second table
    /// is asked to accept explicit identity values while another already does.
    ///
    /// The first three `%.*ls` are the open table as `database.schema.object`
    /// (`'master.dbo.t1'`). The fourth is the refused table as written (`'dbo.t2'`, or
    /// `'t2'` when the statement was unqualified). After the error, the first table stays
    /// open (`SET IDENTITY_INSERT dbo.t1 ON; SET IDENTITY_INSERT dbo.t2 ON;` then
    /// `INSERT INTO dbo.t1 (id, v) VALUES (52, 1);` writes the row).
    ///
    /// ```text
    /// A session holds IDENTITY_INSERT for one table at a time; 'master.dbo.t1' already has it, and 'dbo.t2' is refused.
    /// ```
    pub fn identity_insert_already_on(
        database: &str,
        schema: &str,
        table: &str,
        refused: &str,
    ) -> Self {
        from_catalog(
            8107,
            1,
            &[
                Arg::Str(database),
                Arg::Str(schema),
                Arg::Str(table),
                Arg::Str(refused),
            ],
        )
    }

    /// Error 8106, severity 16, state 1 (`SET IDENTITY_INSERT dbo.plain ON` on a table
    /// without an identity column): the setting cannot open on that table.
    ///
    /// `name` is the qualified name the server prints, usually `schema.object`.
    ///
    /// ```text
    /// Table 'dbo.plain' lacks an IDENTITY column; SET IDENTITY_INSERT cannot run on it.
    /// ```
    pub fn identity_insert_table_has_no_identity(name: &str) -> Self {
        from_catalog(8106, 1, &[Arg::Str(name)])
    }

    /// Error 1088, severity 16, state 11 (`SET IDENTITY_INSERT dbo.nosuch ON` when the
    /// name resolves to nothing).
    ///
    /// `name` is the qualified name the server prints, usually `schema.object`.
    ///
    /// ```text
    /// Object "dbo.nosuch" was not found: it does not exist or is not accessible.
    /// ```
    pub fn cannot_find_object_for_identity_insert(name: &str) -> Self {
        from_catalog_with_severity(
            1088,
            CANNOT_FIND_OBJECT_1088_SEVERITY,
            11,
            &[Arg::Str(name)],
        )
    }

    /// Error 271, severity 16, state 1 (`UPDATE dbo.tc SET c = 1;` where `c` is a computed
    /// column): an `UPDATE` writes a computed column.
    ///
    /// `column` is the column name as the catalogue spells it, unqualified (`"c"`): the
    /// template puts it between double quotes, where 8102 uses single ones.
    ///
    /// ```text
    /// The column "c" cannot be modified: it is computed, or it comes out of a UNION.
    /// ```
    pub fn cannot_update_computed_column(column: &str) -> Self {
        from_catalog(271, 1, &[Arg::Str(column)])
    }

    /// Error 8121, severity 16, state 1
    /// (`SELECT a FROM dbo.t2 GROUP BY a HAVING b > 1;`): a `HAVING` names a column that
    /// is neither grouped nor aggregated.
    ///
    /// The two `%.*ls` are the table then the column, and the template puts a dot between
    /// them: `'dbo.t2'` and `'b'` print `'dbo.t2.b'`. The first argument is the base
    /// table, schema included, not the alias written in the query:
    /// `SELECT x.a FROM dbo.t2 AS x GROUP BY x.a HAVING x.b > 1;` prints `'dbo.t2.b'`
    /// too. The same fault in the select list answers 8120
    /// ([`SqlError::column_invalid_in_select_list`]).
    ///
    /// ```text
    /// Column 'dbo.t2.b' of the HAVING clause is neither aggregated nor part of GROUP BY.
    /// ```
    pub fn column_invalid_in_having(table: &str, column: &str) -> Self {
        from_catalog(8121, 1, &[Arg::Str(table), Arg::Str(column)])
    }

    /// Error 213, severity 16, state 1 (`INSERT INTO dbo.t2 VALUES (1);` on a two-column
    /// table): the values supplied to an `INSERT` do not match the table definition.
    ///
    /// No argument.
    ///
    /// ```text
    /// The supplied values do not match the columns of the table.
    /// ```
    pub fn column_count_does_not_match_table() -> Self {
        from_catalog(213, 1, &[])
    }

    /// Error 544, severity 16, state 1
    /// (`INSERT INTO dbo.ident (id, v) VALUES (1, 1);` with `IDENTITY_INSERT` off): an
    /// explicit value reaches an identity column named in the column list.
    ///
    /// `table` is the table name as the server prints it, unqualified (`'ident'`) where
    /// 8101 prints `'dbo.ident'` for the same table.
    ///
    /// ```text
    /// An explicit value for the identity column of table 'ident' requires IDENTITY_INSERT ON.
    /// ```
    pub fn identity_insert_is_off(table: &str) -> Self {
        from_catalog(544, 1, &[Arg::Str(table)])
    }

    /// Error 109, severity 15, state 1 (`INSERT INTO dbo.t2 (a, b) VALUES (1);`): the
    /// column list of an `INSERT` names more columns than a `VALUES` row supplies.
    ///
    /// No argument. The opposite count is 110 ([`SqlError::more_values_than_columns`]);
    /// the same `INSERT` written without a column list answers 213
    /// ([`SqlError::column_count_does_not_match_table`]).
    ///
    /// ```text
    /// The INSERT column list names more columns than the VALUES row supplies; the two counts have to match.
    /// ```
    pub fn more_columns_than_values() -> Self {
        from_catalog(109, 1, &[])
    }

    /// Error 110, severity 15, state 1 (`INSERT INTO dbo.t2 (a) VALUES (1, 2);`): the
    /// column list of an `INSERT` names fewer columns than a `VALUES` row supplies.
    ///
    /// No argument.
    ///
    /// ```text
    /// The INSERT column list names fewer columns than the VALUES row supplies; the two counts have to match.
    /// ```
    pub fn more_values_than_columns() -> Self {
        from_catalog(110, 1, &[])
    }

    /// Error 120, severity 15, state 1 (`INSERT INTO dbo.t2 (a, b) SELECT 1;`): the
    /// select list of an `INSERT … SELECT` supplies fewer items than the column list names.
    ///
    /// No argument. The opposite count is 121 ([`SqlError::select_list_longer_than_insert_list`]).
    ///
    /// ```text
    /// The select list of the INSERT supplies fewer items than its column list; the two counts have to match.
    /// ```
    pub fn select_list_shorter_than_insert_list() -> Self {
        from_catalog(120, 1, &[])
    }

    /// Error 121, severity 15, state 1 (`INSERT INTO dbo.t2 (a) SELECT 1, 2;`): the select
    /// list of an `INSERT … SELECT` supplies more items than the column list names.
    ///
    /// No argument.
    ///
    /// ```text
    /// The select list of the INSERT supplies more items than its column list; the two counts have to match.
    /// ```
    pub fn select_list_longer_than_insert_list() -> Self {
        from_catalog(121, 1, &[])
    }

    /// Error 264, severity 16, state 1 (`INSERT INTO dbo.t2 (a, a) VALUES (1, 2);`): a
    /// column is named twice in the column list of an `INSERT` or the `SET` clause of an
    /// `UPDATE`.
    ///
    /// `column` is the name as the catalogue spells it, unqualified (`'a'`): the list
    /// `(A, a)` over a column `a` prints `'a'`.
    ///
    /// ```text
    /// Column 'a' is named more than once in the column list of the INSERT or the SET clause of the UPDATE; a column takes one value per statement.
    /// ```
    pub fn column_specified_more_than_once(column: &str) -> Self {
        from_catalog(264, 1, &[Arg::Str(column)])
    }

    /// Error 339, severity 16, state 1 (`INSERT INTO dbo.ident (id, v) VALUES (NULL, 1);`
    /// and the same row with `DEFAULT`): a `NULL` or a `DEFAULT` is written for an identity
    /// column named in the column list.
    ///
    /// No argument.
    ///
    /// ```text
    /// DEFAULT and NULL cannot be given as explicit identity values.
    /// ```
    pub fn default_or_null_as_identity_value() -> Self {
        from_catalog(339, 1, &[])
    }

    /// Error 10709, severity 16, state 1 (`INSERT INTO dbo.t2 (a, b) VALUES (1, 2), (3);`):
    /// two rows of one `VALUES` constructor do not supply the same number of columns.
    ///
    /// No argument. Raised whether or not a column list was written, and before the count
    /// of a row is compared with that list (109, 110) or with the table (213).
    ///
    /// ```text
    /// The rows of a table value constructor have to supply the same number of columns.
    /// ```
    pub fn table_value_constructor_rows_differ() -> Self {
        from_catalog(10709, 1, &[])
    }

    /// Error 1205, severity 13, state 51: this session was picked as the victim of a
    /// deadlock.
    ///
    /// `process_id` fills the `%d` and `resource` the `%.*ls`: two sessions updating the
    /// same two rows in opposite order send severity 13 state 51 to the victim, with the
    /// word `lock` as the resource kind. Which session is picked is the transaction
    /// manager's business; severity 13 is the catalogue's, so 1205 takes no override.
    ///
    /// ```text
    /// Process 67 was chosen as the victim of a deadlock on lock resources; run the transaction again.
    /// ```
    pub fn deadlock_victim(process_id: i64, resource: &str) -> Self {
        from_catalog(1205, 51, &[Arg::Int(process_id), Arg::Str(resource)])
    }

    /// Error 1222, severity 16, state 51: a wait for a **row** lock hit `LOCK_TIMEOUT`.
    ///
    /// No argument. See [`LOCK_TIMEOUT_1222_ROW_STATE`]; a wait on the object sends 56
    /// and has its own constructor, [`SqlError::lock_request_timeout_on_object`].
    ///
    /// ```text
    /// The lock could not be acquired before the timeout.
    /// ```
    pub fn lock_request_timeout_on_row() -> Self {
        from_catalog(1222, LOCK_TIMEOUT_1222_ROW_STATE, &[])
    }

    /// Error 1222, severity 16, state 56: a wait for an **object** lock hit
    /// `LOCK_TIMEOUT`.
    ///
    /// No argument, and the same sentence as [`SqlError::lock_request_timeout_on_row`]:
    /// see [`LOCK_TIMEOUT_1222_OBJECT_STATE`].
    ///
    /// ```text
    /// The lock could not be acquired before the timeout.
    /// ```
    pub fn lock_request_timeout_on_object() -> Self {
        from_catalog(1222, LOCK_TIMEOUT_1222_OBJECT_STATE, &[])
    }

    /// Error 3902, severity 16, state 1 (`COMMIT TRANSACTION;` outside any transaction):
    /// a `COMMIT` has no `BEGIN TRANSACTION` to close.
    ///
    /// No argument.
    ///
    /// ```text
    /// COMMIT TRANSACTION without a matching BEGIN TRANSACTION.
    /// ```
    pub fn commit_without_begin() -> Self {
        from_catalog(3902, 1, &[])
    }

    /// Error 3903, severity 16, state 1 (`ROLLBACK TRANSACTION;` outside any
    /// transaction): a `ROLLBACK` has no `BEGIN TRANSACTION` to undo.
    ///
    /// No argument.
    ///
    /// ```text
    /// ROLLBACK TRANSACTION without a matching BEGIN TRANSACTION.
    /// ```
    pub fn rollback_without_begin() -> Self {
        from_catalog(3903, 1, &[])
    }

    /// Error 3971, severity 16, state 1: a batch carries a transaction descriptor the
    /// session no longer owns.
    ///
    /// `descriptor` is printed in lowercase hexadecimal, as SQL Server does for `%I64x`.
    ///
    /// ```text
    /// Could not resume the transaction. Desc:3400000021.
    /// ```
    pub fn failed_to_resume_transaction(descriptor: u64) -> Self {
        from_catalog(3971, 1, &[Arg::Str(&format!("{descriptor:x}"))])
    }

    /// Error 3989, severity 16, state 1: a request arrived without the transaction
    /// descriptor the open session transaction requires.
    ///
    /// ```text
    /// The request cannot start without a valid transaction descriptor.
    /// ```
    pub fn invalid_transaction_descriptor() -> Self {
        from_catalog(3989, 1, &[])
    }

    /// Error 3960, severity 16, state 2: a snapshot transaction met a row another
    /// transaction had committed since it started.
    ///
    /// The two `%.*ls` are the table then the database. In a database set
    /// `ALLOW_SNAPSHOT_ISOLATION ON`, a session that opened a snapshot transaction and
    /// read a row, then runs `UPDATE dbo.s SET v = v + 100 WHERE k = 1;` after another
    /// session updated and committed that row, receives severity 16 state 2.
    ///
    /// ```text
    /// Update conflict under snapshot isolation: a row of table 'dbo.s' in database 'cc' was changed by another transaction. Retry or change the isolation level.
    /// ```
    pub fn snapshot_update_conflict(table: &str, database: &str) -> Self {
        from_catalog(3960, 2, &[Arg::Str(table), Arg::Str(database)])
    }

    /// Error 8120, severity 16, state 1 (`SELECT a, b FROM dbo.t2 GROUP BY a;`): a select
    /// list names a column that is neither grouped nor aggregated.
    ///
    /// Same two arguments as [`SqlError::column_invalid_in_having`], table then column,
    /// with the dot supplied by the template; the base table is what the server prints,
    /// `SELECT x.a, x.b FROM dbo.t2 AS x GROUP BY x.a;` printing `'dbo.t2.b'` as well.
    ///
    /// ```text
    /// Column 'dbo.t2.b' of the select list is neither aggregated nor part of GROUP BY.
    /// ```
    pub fn column_invalid_in_select_list(table: &str, column: &str) -> Self {
        from_catalog(8120, 1, &[Arg::Str(table), Arg::Str(column)])
    }

    // ---------------------------------------------------------------------------------
    // Index and constraint DDL.
    // ---------------------------------------------------------------------------------

    /// Error 8110, severity 16, state 0: a `CREATE TABLE` declares two `PRIMARY KEY`
    /// constraints. `table` is the qualified name the server prints.
    ///
    /// `CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY, b int NOT NULL PRIMARY KEY);` and
    /// the same table with two named `CONSTRAINT ... PRIMARY KEY` clauses both print the
    /// qualified name. A second `PRIMARY KEY` added by `ALTER TABLE ... ADD CONSTRAINT`
    /// is another number, 1779, not catalogued.
    ///
    /// ```text
    /// Table 'dbo.t' can have a single PRIMARY KEY constraint.
    /// ```
    pub fn multiple_primary_keys(table: &str) -> Self {
        from_catalog(8110, 0, &[Arg::Str(table)])
    }

    /// Error 8111, severity 16, state 1: a `PRIMARY KEY` names a column that is declared
    /// nullable. `table` is the name the server prints, without its schema:
    /// `CREATE TABLE dbo.t (a int NULL, CONSTRAINT pk PRIMARY KEY (a));` and
    /// `ALTER TABLE dbo.t ADD CONSTRAINT pk PRIMARY KEY (a);` over `a int NULL`
    /// both print `'t'`. Error 1750 follows it in the same batch
    /// ([`SqlError::could_not_create_constraint_or_index`]).
    ///
    /// ```text
    /// A PRIMARY KEY of table 't' cannot include a nullable column.
    /// ```
    pub fn primary_key_on_nullable_column(table: &str) -> Self {
        from_catalog(8111, 1, &[Arg::Str(table)])
    }

    /// Error 8112, severity 16, state 0: one `CREATE TABLE` declares two clustered
    /// constraints. `table` is the qualified name the server prints:
    /// `CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY CLUSTERED, b int NOT NULL UNIQUE
    /// CLUSTERED);`, or the same pair written as two named constraints. A second
    /// clustered index created afterwards is [`SqlError::more_than_one_clustered_index`].
    ///
    /// ```text
    /// Constraints of table 'dbo.t' can create a single clustered index.
    /// ```
    pub fn multiple_clustered_index_constraints(table: &str) -> Self {
        from_catalog(8112, 0, &[Arg::Str(table)])
    }

    /// Error 1902, severity 16, state 3: a `CREATE CLUSTERED INDEX` meets a clustered
    /// index the table already carries.
    ///
    /// The `%S_MSG` is filled with `table`; `table_name` is the qualified table and
    /// `existing_index` the index to drop first, the generated name when the existing
    /// index is a constraint's: `CREATE CLUSTERED INDEX ix2 ON dbo.t (b);` after
    /// `CREATE CLUSTERED INDEX ix1 ON dbo.t (a);` prints `'ix1'`, and the same statement
    /// after a `PRIMARY KEY CLUSTERED` prints the generated `'PK__t__...'`.
    ///
    /// ```text
    /// The table 'dbo.t' can have a single clustered index; drop 'ix_a' first.
    /// ```
    pub fn more_than_one_clustered_index(table_name: &str, existing_index: &str) -> Self {
        from_catalog(
            1902,
            3,
            &[
                Arg::Str("table"),
                Arg::Str(table_name),
                Arg::Str(existing_index),
            ],
        )
    }

    /// Error 1911, severity 16, state 1: an index or a key names a column the target does
    /// not have. `column` is the column name as written:
    /// `CREATE INDEX ix ON dbo.t (nosuchcolumn);`,
    /// `CREATE TABLE dbo.t (a int NOT NULL, b int NOT NULL, PRIMARY KEY (z));` and the
    /// same table with `UNIQUE (z)`. The first is raised at execution, the other two while
    /// the statement is compiled, with the same four fields.
    ///
    /// ```text
    /// The target table or view has no column named 'nosuchcolumn'.
    /// ```
    pub fn column_does_not_exist_in_target(column: &str) -> Self {
        from_catalog(1911, 1, &[Arg::Str(column)])
    }

    /// Error 1913, severity 16, state 1: a `CREATE INDEX` reuses a name the table already
    /// carries. The `%S_MSG` is filled with `table`.
    ///
    /// `CREATE INDEX ix ON dbo.t (b);` after the same index name was created on `(a)`,
    /// and `CREATE INDEX pk ON dbo.t (b);` over a table whose `PRIMARY KEY` is named
    /// `pk`: an index name taken by a constraint answers 1913 as well. Two constraints of
    /// one name inside a single statement answer 8168 instead
    /// ([`SqlError::duplicate_name_in_this_context`]).
    ///
    /// ```text
    /// An index or statistics named 'ix_t' exists already on table 'dbo.t'.
    /// ```
    pub fn index_name_already_exists(index: &str, table: &str) -> Self {
        from_catalog(
            1913,
            1,
            &[Arg::Str(index), Arg::Str("table"), Arg::Str(table)],
        )
    }

    /// Error 1909, severity 16, state 1: the **key** list of an index or of a key
    /// constraint names the same column twice.
    ///
    /// The `%S_MSG` is filled with `index`, for a constraint as for an index; `column` is
    /// the spelling of the last occurrence, `(a, A)` printing `'A'`. See
    /// [`DUPLICATE_COLUMN_1909_KEY_LIST_STATE`].
    ///
    /// ```text
    /// The index cannot repeat a column: 'a' is listed twice.
    /// ```
    pub fn duplicate_column_in_index(column: &str) -> Self {
        from_catalog(
            1909,
            DUPLICATE_COLUMN_1909_KEY_LIST_STATE,
            &[Arg::Str("index"), Arg::Str(column)],
        )
    }

    /// Error 1909, severity 16, state 2: the `INCLUDE` list of an index repeats a column
    /// of its key, or repeats one of its own.
    ///
    /// Same sentence and same arguments as [`SqlError::duplicate_column_in_index`]; see
    /// [`DUPLICATE_COLUMN_1909_INCLUDE_LIST_STATE`].
    ///
    /// ```text
    /// The index cannot repeat a column: 'a' is listed twice.
    /// ```
    pub fn duplicate_column_in_included_columns(column: &str) -> Self {
        from_catalog(
            1909,
            DUPLICATE_COLUMN_1909_INCLUDE_LIST_STATE,
            &[Arg::Str("index"), Arg::Str(column)],
        )
    }

    /// Error 1750, severity 16, state 0: the summary the server appends after the error
    /// that made a constraint or an index impossible.
    ///
    /// No argument. The catalogue holds severity 10 and the token sent carries 16: the
    /// constructor overrides the catalogue ([`COULD_NOT_CREATE_CONSTRAINT_1750_SEVERITY`]).
    ///
    /// ```text
    /// The constraint or index was not created; the previous errors say why.
    /// ```
    pub fn could_not_create_constraint_or_index() -> Self {
        from_catalog_with_severity(1750, COULD_NOT_CREATE_CONSTRAINT_1750_SEVERITY, 0, &[])
    }

    /// Error 1754, severity 16, state 0 (`CREATE TABLE dbo.t (a int IDENTITY DEFAULT 1);`):
    /// a `DEFAULT` is written on an `IDENTITY` column.
    ///
    /// `table` is the table name without its schema (`CREATE TABLE dbo.t …` prints `'t'`),
    /// `column` the column name as written (`CREATE TABLE dbo.t ([Ab] int IDENTITY DEFAULT 1);`
    /// prints `'Ab'`). A named `DEFAULT` on the same column answers 1754 as well
    /// (`CREATE TABLE dbo.t (a int IDENTITY CONSTRAINT d DEFAULT 1);`).
    ///
    /// A column that also carries two `DEFAULT` clauses answers 8148
    /// ([`SqlError::multiple_column_defaults`]) instead of 1754.
    ///
    /// ```text
    /// A DEFAULT cannot sit on an IDENTITY column. Table 't', column 'a'.
    /// ```
    pub fn default_on_identity_column(table: &str, column: &str) -> Self {
        from_catalog(1754, 0, &[Arg::Str(table), Arg::Str(column)])
    }

    /// Error 8168, severity 16, state 0: one statement creates two constraints of the
    /// name `name` ([`DUPLICATE_NAME_8168_CONSTRAINT_STATE`]): a `CREATE TABLE` holding
    /// two `CONSTRAINT c1` clauses, one `PRIMARY KEY` and one `UNIQUE`, a `CREATE TABLE`
    /// holding two `CONSTRAINT c1 CHECK` clauses, or the `ALTER TABLE dbo.t ADD` of each
    /// pair.
    ///
    /// Two other shapes the sentence names answer otherwise: two indexes of one name
    /// inside a `CREATE TABLE` send 8168 state 1
    /// ([`SqlError::duplicate_index_name_in_this_context`]), and two columns of one name
    /// answer 2705 state 3 (`CREATE TABLE dbo.t (a int NULL, a int NULL);`,
    /// `ALTER TABLE dbo.t ADD c int NULL, c int NULL;`, the number
    /// [`SqlError::duplicate_column_name`] builds). A `CREATE INDEX` that reuses a name
    /// already stored answers 1913 ([`SqlError::index_name_already_exists`]).
    ///
    /// ```text
    /// The name 'c1' is used more than once for a constraint, column, index or trigger here; names have to be unique.
    /// ```
    pub fn duplicate_name_in_this_context(name: &str) -> Self {
        from_catalog(
            8168,
            DUPLICATE_NAME_8168_CONSTRAINT_STATE,
            &[Arg::Str(name)],
        )
    }

    /// Error 8168, severity 16, state 1: one `CREATE TABLE` creates two indexes of the
    /// name `name`.
    ///
    /// Same sentence and same argument as [`SqlError::duplicate_name_in_this_context`];
    /// see [`DUPLICATE_NAME_8168_INDEX_STATE`].
    ///
    /// ```text
    /// The name 'ix_t' is used more than once for a constraint, column, index or trigger here; names have to be unique.
    /// ```
    pub fn duplicate_index_name_in_this_context(name: &str) -> Self {
        from_catalog(8168, DUPLICATE_NAME_8168_INDEX_STATE, &[Arg::Str(name)])
    }

    /// Error 3723, severity 16, state 4 or 5: a `DROP INDEX` names the index a key
    /// constraint enforces.
    ///
    /// `index` is the `table.index` name the server prints, schema included
    /// (`'dbo.t.pk'`); `constraint_kind` fills the `%ls` and chooses the state through
    /// [`DROP_INDEX_3723_STATES`], `PRIMARY KEY` giving 4 and `UNIQUE KEY` giving 5. A
    /// kind outside that table takes the catalogue default, [`UNKNOWN_DDL_STATE`]. A
    /// `DROP INDEX` over a name the table does not carry is another number, 3701 state 7.
    ///
    /// ```text
    /// Index 'dbo.t.pk_t' enforces a PRIMARY KEY constraint and cannot be dropped directly.
    /// ```
    pub fn cannot_drop_constraint_index(index: &str, constraint_kind: &str) -> Self {
        from_catalog(
            3723,
            ddl_state(DROP_INDEX_3723_STATES, constraint_kind),
            &[Arg::Str(index), Arg::Str(constraint_kind)],
        )
    }

    /// Error 1939, severity 16, state 1: a `CREATE INDEX` targets a view that is not
    /// schema bound.
    ///
    /// The `%S_MSG` is filled with `index`; `view` is the name without its schema:
    /// `CREATE INDEX ix ON dbo.v (a);` and `CREATE UNIQUE CLUSTERED INDEX ix ON dbo.v2 (a);`
    /// over a view created without `WITH SCHEMABINDING`.
    ///
    /// ```text
    /// The index requires the view 'v' to be schema bound.
    /// ```
    pub fn cannot_create_index_on_view(view: &str) -> Self {
        from_catalog(1939, 1, &[Arg::Str("index"), Arg::Str(view)])
    }

    /// Error 1088, severity 16, state 12: a `CREATE INDEX` names a table or a view that
    /// does not exist. `name` is the qualified name the server prints.
    ///
    /// The catalogue holds severity 15 and the token sent carries 16
    /// ([`CANNOT_FIND_OBJECT_1088_SEVERITY`]). The same sentence under another statement
    /// carries another number: `TRUNCATE TABLE` answers 4701
    /// ([`SqlError::cannot_find_object_to_truncate`]) and `ALTER TABLE ... ADD` answers
    /// 4902.
    ///
    /// ```text
    /// Object "dbo.nosuch" was not found: it does not exist or is not accessible.
    /// ```
    pub fn cannot_find_object_to_index(name: &str) -> Self {
        from_catalog_with_severity(
            1088,
            CANNOT_FIND_OBJECT_1088_SEVERITY,
            12,
            &[Arg::Str(name)],
        )
    }

    /// Error 2749, severity 16, state 2: an `IDENTITY` column is declared with a type
    /// that cannot carry it (`CREATE TABLE dbo.t (a decimal(9,2) IDENTITY(1,1) NOT NULL);`,
    /// or `varchar(10)`). `column` is the column name, without its table. An `IDENTITY`
    /// on a nullable column of an accepted type is another number, 8147
    /// ([`SqlError::identity_on_nullable_column`]), although the sentence of 2749 names
    /// nullability too.
    ///
    /// ```text
    /// Identity column 'a' has to be a non-nullable int, bigint, smallint, tinyint, or decimal or numeric of scale 0.
    /// ```
    pub fn invalid_identity_column_type(column: &str) -> Self {
        from_catalog(2749, 2, &[Arg::Str(column)])
    }

    /// Error 8147, severity 16, state 1: an `IDENTITY` column is declared `NULL`.
    ///
    /// `column` is the column name and `table` the qualified table, in the order of the
    /// template (`CREATE TABLE dbo.t (a int IDENTITY(1,1) NULL);`, or
    /// `bigint NULL IDENTITY(1,1)`).
    ///
    /// ```text
    /// Column 'a' of table 'dbo.t' is nullable and cannot be an IDENTITY column.
    /// ```
    pub fn identity_on_nullable_column(column: &str, table: &str) -> Self {
        from_catalog(8147, 1, &[Arg::Str(column), Arg::Str(table)])
    }

    /// Error 8148, severity 16, state 0
    /// (`CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2);`): a column is given
    /// two `DEFAULT` clauses.
    ///
    /// The `%ls` and `%S_MSG` are filled `DEFAULT` and `constraint`. `column` is the column
    /// name as written (`CREATE TABLE dbo.t ([A col] int DEFAULT 1 CONSTRAINT c DEFAULT 2);`
    /// prints `'A col'`). `table` is the table as written, schema included when it was
    /// written (`CREATE TABLE dbo.t …` prints `'dbo.t'`, `CREATE TABLE t …` prints `'t'`).
    ///
    /// `CREATE TABLE dbo.t (a int DEFAULT 1 DEFAULT 2)`,
    /// `CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2)`,
    /// `CREATE TABLE dbo.t (a int CONSTRAINT c DEFAULT 2 DEFAULT 1)` and
    /// `CREATE TABLE dbo.t (a int CONSTRAINT c1 DEFAULT 1 CONSTRAINT c2 DEFAULT 2)` each
    /// answer 8148. `CREATE TABLE dbo.t (a int CONSTRAINT da DEFAULT 1, b int CONSTRAINT db
    /// DEFAULT 2)` does not: each column has one default
    /// (`crates/vauban-catalog/tests/sys_constraints.rs`).
    ///
    /// ```text
    /// A second DEFAULT constraint is refused for column 'a' of table 'dbo.t'.
    /// ```
    pub fn multiple_column_defaults(column: &str, table: &str) -> Self {
        from_catalog(
            8148,
            0,
            &[
                Arg::Str("DEFAULT"),
                Arg::Str("constraint"),
                Arg::Str(column),
                Arg::Str(table),
            ],
        )
    }

    // ---------------------------------------------------------------------------------
    // The names a `FROM` clause and a batch of the parser resolve.
    // ---------------------------------------------------------------------------------

    /// Error 208, severity 16, state 3: a name resolves to an object that is not a table
    /// or a view. `name` is the name as written.
    ///
    /// The state is what separates this form from [`SqlError::invalid_object_name`],
    /// whose message is the same sentence ([`INVALID_OBJECT_NAME_208_OTHER_TYPE_STATE`]).
    ///
    /// ```text
    /// Unknown object name 'sys.sp_executesql'.
    /// ```
    pub fn invalid_object_name_of_another_type(name: &str) -> Self {
        from_catalog(
            208,
            INVALID_OBJECT_NAME_208_OTHER_TYPE_STATE,
            &[Arg::Str(name)],
        )
    }

    /// Error 447, severity 16, state 1: a `COLLATE` clause written in a column definition
    /// applies to a column that is not a character string. `ty` is the declared type.
    ///
    /// Same sentence as [`SqlError::collate_on_non_string`], state 0 on an expression;
    /// see [`COLLATE_ON_NON_STRING_447_COLUMN_STATE`].
    ///
    /// ```text
    /// COLLATE cannot apply to an expression of type int.
    /// ```
    pub fn collate_on_non_string_column(ty: &str) -> Self {
        from_catalog(447, COLLATE_ON_NON_STRING_447_COLUMN_STATE, &[Arg::Str(ty)])
    }

    /// Error 1087, severity 15, state 2: a statement reads or writes a table variable
    /// that no `DECLARE` introduced (`SELECT 1 FROM @tv;`,
    /// `INSERT INTO @tv2 (a) VALUES (1);`). `name` is the variable with its `@`.
    ///
    /// ```text
    /// The table variable "@tv" is not declared.
    /// ```
    pub fn must_declare_table_variable(name: &str) -> Self {
        from_catalog(1087, 2, &[Arg::Str(name)])
    }

    /// Error 7202, severity 11, state 2: the first part of a four-part name is not a
    /// linked server (`SELECT 1 FROM nosuchserver.somedb.dbo.t;`, or an `INSERT INTO`
    /// whose target carries the unknown server). `server` is that first part.
    ///
    /// ```text
    /// Server 'nosuchserver' is not registered in sys.servers; check the name or add it with sp_addlinkedserver.
    /// ```
    pub fn linked_server_not_found(server: &str) -> Self {
        from_catalog(7202, 2, &[Arg::Str(server)])
    }

    /// Error 1011, severity 16, state 1: two sources of one `FROM` clause carry the same
    /// alias. `name` is the alias as written on the source that came second
    /// (`FROM dbo.t AS x JOIN dbo.u AS X ON 1 = 1` prints `'X'`).
    ///
    /// The catalogue holds severity 15 and the token sent carries 16
    /// ([`DUPLICATE_CORRELATION_NAME_SEVERITY`]).
    ///
    /// ```text
    /// Correlation name 'x' is used more than once in the FROM clause.
    /// ```
    pub fn duplicate_correlation_name(name: &str) -> Self {
        from_catalog_with_severity(
            1011,
            DUPLICATE_CORRELATION_NAME_SEVERITY,
            1,
            &[Arg::Str(name)],
        )
    }

    /// Error 1012, severity 16, state 1: the alias of one source of a `FROM` clause is the
    /// exposed name of a table of the same clause, written without an alias. `alias` is
    /// the alias as written and `table` the name of the table as written, whichever of
    /// the two came first (`FROM dbo.t AS u JOIN dbo.u ON 1 = 1` prints `'u'` and
    /// `'dbo.u'`).
    ///
    /// The catalogue holds severity 15 and the token sent carries 16
    /// ([`DUPLICATE_CORRELATION_NAME_SEVERITY`]).
    ///
    /// ```text
    /// Correlation name 'u' is also the exposed name of table 'dbo.u'; give one of them another alias.
    /// ```
    pub fn correlation_name_is_a_table_name(alias: &str, table: &str) -> Self {
        from_catalog_with_severity(
            1012,
            DUPLICATE_CORRELATION_NAME_SEVERITY,
            1,
            &[Arg::Str(alias), Arg::Str(table)],
        )
    }

    /// Error 1013, severity 16, state 1: two sources of one `FROM` clause expose the same
    /// name. The two `%.*ls` are the sources in the order the server prints them.
    ///
    /// The catalogue holds severity 15 and the token sent carries 16
    /// ([`SAME_EXPOSED_NAMES_1013_SEVERITY`]). The order is not the `FROM` order:
    /// `SELECT t.a FROM dbo.t, s.t;` prints `"s.t"` first and `"dbo.t"` second.
    ///
    /// ```text
    /// "dbo.t" and "dbo.t" expose the same name in the FROM clause; give them distinct aliases.
    /// ```
    pub fn same_exposed_names(first: &str, second: &str) -> Self {
        from_catalog_with_severity(
            1013,
            SAME_EXPOSED_NAMES_1013_SEVERITY,
            1,
            &[Arg::Str(first), Arg::Str(second)],
        )
    }

    /// Error 8154, severity 16, state 1: the target of an `UPDATE` or of a `DELETE`
    /// matches more than one source of its `FROM`. `name` is the target as written.
    ///
    /// The catalogue holds severity 15 and the token sent carries 16
    /// ([`AMBIGUOUS_TABLE_8154_SEVERITY`]). Two sources that share an alias answer 1011
    /// instead, and two sources that share an exposed name without a target naming them
    /// answer 1013.
    ///
    /// ```text
    /// The reference to table 't' is ambiguous.
    /// ```
    pub fn table_is_ambiguous(name: &str) -> Self {
        from_catalog_with_severity(8154, AMBIGUOUS_TABLE_8154_SEVERITY, 1, &[Arg::Str(name)])
    }

    /// Error 8155, severity 16, state 2: a column of a derived table has no name.
    ///
    /// `position` is the 1-based position of the column and `name` the derived table's
    /// alias, in the order of the template. The catalogue holds severity 15 and the token
    /// sent carries 16 ([`NO_COLUMN_NAME_8155_SEVERITY`]). The same omission in a
    /// `CREATE VIEW` answers 4511 and in a `SELECT ... INTO` answers 1038 state 5.
    ///
    /// ```text
    /// Column 2 of 'd' has no name.
    /// ```
    pub fn no_column_name_in_derived_table(position: i64, name: &str) -> Self {
        from_catalog_with_severity(
            8155,
            NO_COLUMN_NAME_8155_SEVERITY,
            2,
            &[Arg::Int(position), Arg::Str(name)],
        )
    }

    /// Error 1038, severity 15, state 4: a name or an alias is empty, or a delimiter is
    /// left open (`SELECT 1 AS [];`, `SELECT 1 AS "";`, `SELECT [` and `SELECT "` left
    /// unclosed at the end of the batch). No argument.
    /// `SELECT a + 1 INTO dbo.u FROM dbo.t;` sends the same sentence at state 5, a
    /// filling without a constructor.
    ///
    /// ```text
    /// An object or column name is empty: each SELECT INTO column needs a name, and an alias written "" or [] is not allowed.
    /// ```
    pub fn object_or_column_name_missing() -> Self {
        from_catalog(1038, 4, &[])
    }

    /// Error 103, severity 15, state 4: an identifier is longer than the maximum.
    ///
    /// `start` is what the message prints, the first `maximum` characters of the token
    /// (the caller passes the truncated text), and `maximum` fills the `%d`, 128
    /// ([`IDENTIFIER_TOO_LONG_103_STATE`]). The `%S_MSG` is filled with `identifier`.
    ///
    /// ```text
    /// The identifier beginning with 'bbbb...' exceeds the maximum length of 128.
    /// ```
    pub fn identifier_too_long(start: &str, maximum: u32) -> Self {
        from_catalog(
            103,
            IDENTIFIER_TOO_LONG_103_STATE,
            &[
                Arg::Str("identifier"),
                Arg::Str(start),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 103, severity 15, state 5: a number literal is longer than the maximum.
    ///
    /// Same arguments as [`SqlError::identifier_too_long`], with `number` as the `%S_MSG`
    /// ([`NUMBER_TOO_LONG_103_STATE`]).
    ///
    /// ```text
    /// The number beginning with '9999...' exceeds the maximum length of 128.
    /// ```
    pub fn number_too_long(start: &str, maximum: u32) -> Self {
        from_catalog(
            103,
            NUMBER_TOO_LONG_103_STATE,
            &[
                Arg::Str("number"),
                Arg::Str(start),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    /// Error 103, severity 15, state 6: a money literal is longer than the maximum.
    ///
    /// Same arguments and same `number` `%S_MSG` as [`SqlError::number_too_long`], the
    /// `$` included in `start` ([`MONEY_LITERAL_TOO_LONG_103_STATE`]).
    ///
    /// ```text
    /// The number beginning with '$111...' exceeds the maximum length of 128.
    /// ```
    pub fn money_literal_too_long(start: &str, maximum: u32) -> Self {
        from_catalog(
            103,
            MONEY_LITERAL_TOO_LONG_103_STATE,
            &[
                Arg::Str("number"),
                Arg::Str(start),
                Arg::Int(i64::from(maximum)),
            ],
        )
    }

    // ---------------------------------------------------------------------------------
    // The database a statement names and the one a login asks for.
    // ---------------------------------------------------------------------------------

    /// Error 2702, severity 16, state 2: a statement names a database that does not exist
    /// (`CREATE TABLE nosuchdb.dbo.t (a int NOT NULL);`). `name` is the first part of the
    /// qualified name. A `SELECT` through the same three-part shape answers 208 state 1
    /// instead (`SELECT 1 FROM nosuchdb.dbo.t;`), so the number follows the statement.
    ///
    /// ```text
    /// No database named 'nosuchdb' exists.
    /// ```
    pub fn database_does_not_exist(name: &str) -> Self {
        from_catalog(2702, 2, &[Arg::Str(name)])
    }

    /// Error 4701, severity 16, state 1: a `TRUNCATE TABLE` names a table that does not
    /// exist.
    ///
    /// `name` is the object part alone: `TRUNCATE TABLE dbo.nosuch;` prints `"nosuch"`
    /// and `TRUNCATE TABLE s2.nosuch3;` prints `"nosuch3"`. The same sentence under a
    /// `CREATE INDEX` is number 1088 ([`SqlError::cannot_find_object_to_index`]), which
    /// prints the qualified name.
    ///
    /// ```text
    /// Object "nosuch" was not found: it does not exist or is not accessible.
    /// ```
    pub fn cannot_find_object_to_truncate(name: &str) -> Self {
        from_catalog(4701, 1, &[Arg::Str(name)])
    }

    /// Error 4063, severity 11, state 1: the login asked for a database it cannot open and
    /// the session falls back on the user's default one.
    ///
    /// `requested` is the database the login asked for and `default_database` the one the
    /// session opened, in the order of the template. The ERROR token travels with line 1,
    /// before the ENVCHANGE and the LOGINACK of the session opened in `master`.
    ///
    /// ```text
    /// The database "nosuchdb" requested at login could not be opened; the default database "master" is used instead.
    /// ```
    pub fn cannot_open_login_database(requested: &str, default_database: &str) -> Self {
        from_catalog(4063, 1, &[Arg::Str(requested), Arg::Str(default_database)])
    }

    /// Error 8158, severity 16, state 1 (`SELECT 1 FROM (SELECT 1 AS c, 2 AS d) AS t(x);`):
    /// a derived table has more columns than its column list names.
    ///
    /// ```text
    /// The alias list of the derived table '%.*ls' names fewer columns than the query inside it produces.
    /// ```
    pub fn derived_table_more_columns_than_column_list(alias: &str) -> Self {
        from_catalog(8158, 1, &[Arg::Str(alias)])
    }

    /// Error 8159, severity 16, state 1 (`SELECT 1 FROM (SELECT 1 AS c) AS t(x, y);`):
    /// a derived table has fewer columns than its column list names.
    ///
    /// ```text
    /// The alias list of the derived table '%.*ls' names more columns than the query inside it produces.
    /// ```
    pub fn derived_table_fewer_columns_than_column_list(alias: &str) -> Self {
        from_catalog(8159, 1, &[Arg::Str(alias)])
    }
}

#[cfg(test)]
mod tests {
    use super::UNKNOWN_DDL_STATE;
    use crate::{SqlError, message_template};

    /// Every constructor of this module, called with dummy arguments.
    fn all_constructors() -> Vec<SqlError> {
        vec![
            SqlError::incorrect_syntax_near("tok", 1),
            SqlError::invalid_object_name("dbo.t"),
            SqlError::invalid_column_name("c"),
            SqlError::cannot_insert_null("c", "db.dbo.t", "INSERT"),
            SqlError::unique_violation("PRIMARY KEY", "PK_t", "dbo.t", "(1)"),
            SqlError::duplicate_key_index("dbo.t", "IX_t", "(1)"),
            SqlError::fk_violation("INSERT", "FOREIGN KEY", "FK_x", "db", "dbo.p", Some("id")),
            SqlError::fk_violation("INSERT", "CHECK", "CK_x", "db", "dbo.t", None),
            SqlError::login_failed("sa"),
            SqlError::database_not_found("nope"),
            SqlError::cannot_open_database("nope"),
            SqlError::procedure_not_found("dbo.p"),
            SqlError::procedure_expects_parameter("sp_executesql", "@statement"),
            SqlError::too_many_arguments("sp_who"),
            SqlError::not_a_parameter("@x", "sp_who"),
            SqlError::no_parameter_but_arguments(""),
            SqlError::positional_after_named(2),
            SqlError::output_on_a_constant(),
            SqlError::procedure_expects_type("@statement", "ntext/nchar/nvarchar"),
            SqlError::parameter_not_supplied("(@x int)SELECT @x", "@x"),
            SqlError::prepared_statement_not_found(123456),
            SqlError::statement_could_not_be_prepared(),
            SqlError::object_missing_in_database("nosuchobj", "master"),
            SqlError::help_database_not_found("nosuchdb"),
            SqlError::conversion_failed("varchar", "abc", "int"),
            SqlError::error_converting_data_type("varchar", "numeric"),
            SqlError::conversion_failed_datetime(),
            SqlError::converting_datetime_from_binary(),
            SqlError::arithmetic_overflow("expression", "int"),
            SqlError::divide_by_zero(),
            SqlError::unclosed_quotation_mark("abc;", 4),
            SqlError::missing_end_comment_mark(2),
            SqlError::incorrect_syntax_near_keyword("FROM", 3),
            SqlError::out_of_range_conversion("date", "datetime"),
            SqlError::overflow_for_data_type("tinyint", 300),
            SqlError::overflow_for_type("real", 1e40),
            SqlError::invalid_collation("Klingon_CI_AS"),
            SqlError::conversion_failed_guid(),
            SqlError::unsupported_convert_style(112, "date", "datetimeoffset"),
            SqlError::string_or_binary_truncated(),
            SqlError::explicit_conversion_not_allowed("date", "int"),
            SqlError::implicit_conversion_not_allowed("date", "int"),
            SqlError::operand_type_clash("date", "int"),
            SqlError::function_arg_count("LEN", 1),
            SqlError::function_arg_count_range("ROUND", 2, 3),
            SqlError::invalid_argument_type("uniqueidentifier", 1, "len"),
            SqlError::invalid_length_parameter("left"),
            SqlError::datetime_overflow("datetime"),
            SqlError::eomonth_overflow(),
            SqlError::smalldatetime_intermediate_overflow(),
            SqlError::invalid_function_parameter(1, "datepart"),
            SqlError::datediff_datepart_not_supported("iso_week", "date", "date"),
            SqlError::datediff_overflow(),
            SqlError::datepart_not_supported("hour", "dateadd", "date"),
            SqlError::not_a_recognized_option("bogus", "datepart"),
            SqlError::invalid_floating_point_operation(),
            SqlError::coalesce_all_null(),
            SqlError::cannot_construct_type("datetime"),
            SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function"),
            SqlError::invalid_operand_type("uniqueidentifier", "add"),
            SqlError::non_boolean_expression("1"),
            SqlError::cannot_find_data_type(1, "foo"),
            SqlError::must_declare_scalar_variable("@x"),
            SqlError::insufficient_result_space_money("smallmoney"),
            SqlError::insufficient_result_space_money("int"),
            SqlError::number_out_of_numeric_range("1234567890123456789012345678901234567.89"),
            SqlError::float_out_of_range("1e400"),
            SqlError::invalid_money_value("$99999999999999999999"),
            SqlError::conversion_overflowed_int("varchar", "99999999999"),
            SqlError::conversion_overflowed_small_int("varchar", "300", "INT1"),
            SqlError::conversion_overflowed_small_int("varchar", "99999", "INT2"),
            SqlError::char_to_money_syntax(),
            SqlError::arithmetic_overflow_to_numeric("varchar"),
            SqlError::arithmetic_overflow_to_numeric("float"),
            SqlError::collate_on_non_string("int"),
            SqlError::top_negative(),
            SqlError::top_null(),
            SqlError::select_star_without_from(),
            SqlError::size_out_of_range(9000, "type", "varchar", 8000),
            SqlError::invalid_escape("ab", "LIKE"),
            SqlError::incompatible_types_for_operator("bit", "bit", "add"),
            SqlError::insufficient_result_space_money_to("varchar"),
            SqlError::insufficient_result_space_smallmoney_to("varchar"),
            SqlError::insufficient_result_space_guid(),
            SqlError::nullif_first_argument_null(),
            SqlError::arithmetic_overflow_from("numeric", "money"),
            SqlError::overflow_for_data_type_from("money", "smallint", 400_000_000),
            SqlError::overflow_for_type_from("money", "tinyint", 300.0),
            SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000),
            SqlError::scale_greater_than_precision(),
            SqlError::invalid_length_or_precision(1, 0),
            SqlError::invalid_scale(1, 8),
            SqlError::type_size_out_of_range(39, "decimal", 38),
            SqlError::parameter_size_out_of_range(5000, "@v", 4000),
            SqlError::precision_greater_than_maximum(1, 54, 53),
            SqlError::not_a_defined_system_type("foo"),
            SqlError::conversion_failed_smalldatetime(),
            SqlError::invalid_length_parameter("substring"),
            SqlError::size_out_of_range(9000, "column", "c", 8000),
            SqlError::cannot_find_column_or_function("dbo.LEN"),
            SqlError::multi_part_identifier("t.c"),
            SqlError::must_declare_scalar_variable_assigned("@x"),
            SqlError::top_with_ties_without_order_by(),
            SqlError::column_prefix_does_not_match("t"),
            SqlError::too_many_column_prefixes("a.b.c.d"),
            SqlError::invalid_style_number(999, "date"),
            SqlError::input_does_not_follow_style(100),
            SqlError::percent_out_of_range(),
            SqlError::top_invalid_value(),
            SqlError::invalid_escape_unicode("ab", "LIKE"),
            SqlError::error_converting_data_type("varchar", "datetimeoffset"),
            SqlError::nested_too_deeply(1),
            SqlError::stack_limit_reached(),
            SqlError::parameters_supplied_to_non_function("dbo.T"),
            SqlError::database_already_exists("d"),
            SqlError::object_already_exists("t"),
            SqlError::cannot_drop("drop", "table", "nope"),
            SqlError::cannot_drop("drop", "database", "nope"),
            SqlError::cannot_drop("drop", "index", "t3.ix_nope"),
            SqlError::statement_not_allowed_in_transaction("CREATE DATABASE"),
            SqlError::ambiguous_column_name("a"),
            SqlError::cannot_drop("drop", "sequence", "nope_sequence"),
            SqlError::cannot_drop("drop", "synonym", "nope_synonym"),
            SqlError::cannot_drop_system_database("master"),
            SqlError::drop_database_in_transaction(),
            SqlError::duplicate_column_name("a", "t_dup"),
            SqlError::cannot_find_data_type_in_table(1, "foo"),
            SqlError::only_one_expression_in_subquery(),
            SqlError::assignment_mixed_with_data_retrieval(),
            SqlError::order_by_item_not_in_distinct_select_list(),
            SqlError::set_operator_column_count_mismatch(),
            SqlError::transaction_count_after_execute(0, 1),
            SqlError::subquery_returned_more_than_one_value(),
            SqlError::conflicting_locking_hints(),
            SqlError::nolock_not_allowed_on_target(),
            SqlError::foreign_key_references_invalid_column("fk_bad", "nosuch", "lone"),
            SqlError::no_matching_key_in_referenced_table("dbo.nopk", "fk_nopk"),
            SqlError::cannot_drop_referenced_object("dbo.parent"),
            SqlError::snapshot_isolation_not_allowed("snapdb"),
            SqlError::cannot_truncate_referenced_table("dbo.parent"),
            SqlError::cannot_add_column_to_non_empty_table("b", "notempty"),
            SqlError::identity_insert_requires_column_list("dbo.ident"),
            SqlError::cannot_update_identity_column("id"),
            SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2"),
            SqlError::identity_insert_table_has_no_identity("dbo.plain"),
            SqlError::cannot_find_object_for_identity_insert("dbo.nosuch"),
            SqlError::cannot_update_computed_column("c"),
            SqlError::column_invalid_in_having("dbo.t2", "b"),
            SqlError::column_count_does_not_match_table(),
            SqlError::identity_insert_is_off("ident"),
            SqlError::deadlock_victim(60, "lock"),
            SqlError::lock_request_timeout_on_row(),
            SqlError::lock_request_timeout_on_object(),
            SqlError::commit_without_begin(),
            SqlError::rollback_without_begin(),
            SqlError::snapshot_update_conflict("dbo.s", "cc"),
            SqlError::column_invalid_in_select_list("dbo.t2", "b"),
            SqlError::multiple_primary_keys("dbo.t"),
            SqlError::primary_key_on_nullable_column("t"),
            SqlError::multiple_clustered_index_constraints("dbo.t"),
            SqlError::more_than_one_clustered_index("dbo.t", "ix_a"),
            SqlError::column_does_not_exist_in_target("nosuchcolumn"),
            SqlError::index_name_already_exists("ix_t", "dbo.t"),
            SqlError::duplicate_column_in_index("a"),
            SqlError::duplicate_column_in_included_columns("a"),
            SqlError::could_not_create_constraint_or_index(),
            SqlError::default_on_identity_column("t", "a"),
            SqlError::duplicate_name_in_this_context("c1"),
            SqlError::duplicate_index_name_in_this_context("ix_t"),
            SqlError::cannot_drop_constraint_index("dbo.t.pk_t", "PRIMARY KEY"),
            SqlError::cannot_drop_constraint_index("dbo.t.uq_t", "UNIQUE KEY"),
            SqlError::cannot_create_index_on_view("v"),
            SqlError::cannot_find_object_to_index("dbo.nosuch"),
            SqlError::invalid_identity_column_type("a"),
            SqlError::identity_on_nullable_column("a", "dbo.t"),
            SqlError::multiple_column_defaults("a", "dbo.t"),
            SqlError::invalid_object_name_of_another_type("sys.sp_executesql"),
            SqlError::collate_on_non_string_column("int"),
            SqlError::must_declare_table_variable("@tv"),
            SqlError::linked_server_not_found("nosuchserver"),
            SqlError::duplicate_correlation_name("x"),
            SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
            SqlError::same_exposed_names("dbo.t", "dbo.t"),
            SqlError::table_is_ambiguous("t"),
            SqlError::no_column_name_in_derived_table(2, "d"),
            SqlError::object_or_column_name_missing(),
            SqlError::identifier_too_long("abc", 128),
            SqlError::number_too_long("123", 128),
            SqlError::money_literal_too_long("$123", 128),
            SqlError::database_does_not_exist("nosuchdb"),
            SqlError::cannot_find_object_to_truncate("nosuch"),
            SqlError::cannot_open_login_database("nosuchdb", "master"),
        ]
    }

    #[test]
    fn datepart_not_supported_states_follow_function_type_and_part() {
        // One representative per function/type/state, DATETRUNC included although the
        // function is not registered by VaubanDB yet.
        for (part, function, ty, state) in [
            ("hour", "dateadd", "date", 1),
            ("iso_week", "dateadd", "date", 2),
            ("iso_week", "dateadd", "datetime2", 2),
            ("iso_week", "dateadd", "datetime", 0),
            ("iso_week", "dateadd", "datetimeoffset", 2),
            ("iso_week", "dateadd", "smalldatetime", 3),
            ("day", "dateadd", "time", 1),
            ("hour", "datename", "date", 4),
            ("tzoffset", "datename", "datetime", 7),
            ("tzoffset", "datename", "smalldatetime", 7),
            ("day", "datename", "time", 5),
            ("hour", "datepart", "date", 2),
            ("tzoffset", "datepart", "datetime", 6),
            ("tzoffset", "datepart", "smalldatetime", 6),
            ("day", "datepart", "time", 3),
            ("hour", "datetrunc", "date", 10),
            ("weekday", "datetrunc", "date", 11),
            ("nanosecond", "datetrunc", "datetime2", 11),
            ("microsecond", "datetrunc", "datetime", 9),
            ("nanosecond", "datetrunc", "datetimeoffset", 11),
            ("microsecond", "datetrunc", "smalldatetime", 8),
            ("day", "datetrunc", "time", 10),
            ("nanosecond", "datetrunc", "time", 11),
        ] {
            let error = SqlError::datepart_not_supported(part, function, ty);
            assert_eq!(
                (error.number, error.severity, error.state),
                (9810, 16, state),
                "{function}/{ty}/{part}"
            );
            assert_eq!(
                error.message,
                format!(
                    "Datepart {part} cannot be used with the date function {function} on data type {ty}."
                )
            );
        }
        assert_eq!(
            SqlError::datediff_datepart_not_supported("iso_week", "datetime", "smalldatetime")
                .state,
            2
        );
        assert_eq!(
            SqlError::datediff_datepart_not_supported("iso_week", "datetime", "date").state,
            0
        );
        assert_eq!(SqlError::smalldatetime_intermediate_overflow().state, 1);
    }

    #[test]
    fn date_function_constructors_carry_their_states() {
        assert_eq!(
            message_template(1023).unwrap().template,
            "Parameter %d of %ls is not valid."
        );
        assert_eq!(
            message_template(9806).unwrap().template,
            "Datepart %.*ls cannot be used with the date function %.*ls."
        );
        for (ty, state) in [
            ("datetime", 1),
            ("smalldatetime", 2),
            ("date", 3),
            ("datetime2", 3),
            ("datetimeoffset", 3),
        ] {
            let error = SqlError::datetime_overflow(ty);
            assert_eq!(
                (error.number, error.severity, error.state),
                (517, 16, state)
            );
            assert_eq!(
                error.message,
                format!("The addition overflowed the '{ty}' column.")
            );
        }
        assert_eq!(SqlError::eomonth_overflow().state, 1);
        assert_eq!(SqlError::cannot_construct_type("date").state, 1);
        assert_eq!(SqlError::cannot_construct_type("datetime").state, 3);
        let error = SqlError::invalid_function_parameter(1, "datepart");
        assert_eq!((error.number, error.severity, error.state), (1023, 15, 1));
        assert_eq!(error.message, "Parameter 1 of datepart is not valid.");
        let error = SqlError::datediff_datepart_not_supported("iso_week", "date", "date");
        assert_eq!((error.number, error.severity, error.state), (9806, 16, 0));
        assert_eq!(
            error.message,
            "Datepart iso_week cannot be used with the date function datediff."
        );
    }

    #[test]
    fn parameters_supplied_to_non_function_is_215() {
        let error = SqlError::parameters_supplied_to_non_function("dbo.T");
        assert_eq!(error.number, 215);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
        assert_eq!(
            error.message,
            "Object 'dbo.T' is not a function and takes no parameters; a table hint needs the WITH keyword."
        );
    }

    /// The constructors the server raises with a severity other than the catalogue's:
    /// number, severity sent on the wire, severity of the catalogue, with a query that
    /// raises it in the comment.
    const SEVERITY_OVERRIDES: &[(u32, u8, u8)] = &[
        // SELECT CAST(1 AS nvarchar(5000)); — the `type` filling of 131 keeps 15.
        (131, 16, 15),
        // DECLARE @v decimal(5,6);
        (192, 15, 16),
        // DECLARE @v varchar(0);
        (1001, 15, 16),
        // DECLARE @v time(8);
        (1002, 15, 16),
        // DECLARE @v decimal(39,2); and DECLARE @v nvarchar(5000);
        (2717, 16, 15),
        // SELECT TOP (1) WITH TIES 1 AS c;
        (1062, 15, 16),
        // SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);
        (116, 16, 15),
        // SELECT 1 FROM dbo.t AS x, dbo.t AS x;
        (1011, 16, 15),
        // SELECT 1 FROM dbo.t AS u JOIN dbo.u ON 1 = 1;
        (1012, 16, 15),
        // SELECT 1 FROM dbo.t, dbo.t;
        (1013, 16, 15),
        // CREATE INDEX ix_t ON dbo.nosuch (a);
        (1088, 16, 15),
        // CREATE TABLE dbo.t (a int NULL, CONSTRAINT pk_t PRIMARY KEY (a));
        // catalogued 10, sent 16.
        (1750, 16, 10),
        // UPDATE t SET a = 1 FROM dbo.t AS x, dbo.t AS y;
        (8154, 16, 15),
        // SELECT * FROM (SELECT a, a + 1 FROM dbo.t) AS d;
        (8155, 16, 15),
    ];

    #[test]
    fn every_constructor_number_is_in_the_catalog() {
        for err in all_constructors() {
            let def = message_template(err.number)
                .unwrap_or_else(|| panic!("error {} is not in the catalog", err.number));
            let overridden = SEVERITY_OVERRIDES.contains(&(err.number, err.severity, def.severity));
            assert!(
                err.severity == def.severity || overridden,
                "severity {} of error {} is neither the catalog's {} nor a known override",
                err.severity,
                err.number,
                def.severity
            );
            assert!(
                !err.message.contains('%'),
                "error {} still contains a specifier: {}",
                err.number,
                err.message
            );
            assert_eq!(err.procedure, None);
        }
    }

    /// The rendered message of each literal and conversion constructor, with a query
    /// that raises it quoted next to it; state and severity go with the query.
    #[test]
    fn literal_and_conversion_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // SELECT CAST(CAST(300000 AS money) AS smallmoney);
                SqlError::insufficient_result_space_money("smallmoney"),
                237,
                16,
                3,
                "A money value does not fit in the result type smallmoney.",
            ),
            (
                // SELECT CAST(CAST(99999999999 AS money) AS int);
                SqlError::insufficient_result_space_money("int"),
                237,
                16,
                1,
                "A money value does not fit in the result type int.",
            ),
            (
                // SELECT CAST(SQL_VARIANT_PROPERTY(123456789012345678901234567890123456789, 'BaseType') AS varchar(30));
                SqlError::number_out_of_numeric_range("123456789012345678901234567890123456789"),
                1007,
                15,
                1,
                "The number '123456789012345678901234567890123456789' exceeds the numeric range (precision is limited to 38).",
            ),
            (
                // SELECT CAST(SQL_VARIANT_PROPERTY(1e400, 'BaseType') AS varchar(30));
                SqlError::float_out_of_range("1e400"),
                168,
                15,
                1,
                "The floating point literal '1e400' cannot be represented in 8 bytes.",
            ),
            (
                // SELECT CAST(SQL_VARIANT_PROPERTY($99999999999999999999, 'BaseType') AS varchar(30));
                SqlError::invalid_money_value("$99999999999999999999"),
                151,
                15,
                1,
                "'$99999999999999999999' cannot be read as a money value.",
            ),
            (
                // SELECT CAST('99999999999' AS int);
                SqlError::conversion_overflowed_int("varchar", "99999999999"),
                248,
                16,
                1,
                "The varchar value '99999999999' does not fit in an int column.",
            ),
            (
                // SELECT CAST('300' AS tinyint);
                SqlError::conversion_overflowed_small_int("varchar", "300", "INT1"),
                244,
                16,
                1,
                "The varchar value '300' does not fit in an INT1 column; a wider integer type is needed.",
            ),
            (
                // SELECT CAST('99999' AS smallint);
                SqlError::conversion_overflowed_small_int("varchar", "99999", "INT2"),
                244,
                16,
                2,
                "The varchar value '99999' does not fit in an INT2 column; a wider integer type is needed.",
            ),
            (
                // SELECT CAST('abc' AS money);
                SqlError::char_to_money_syntax(),
                235,
                16,
                0,
                "The character value is not a valid money literal and could not be converted.",
            ),
            (
                // SELECT CAST(1 AS int) COLLATE Latin1_General_CI_AS;
                SqlError::collate_on_non_string("int"),
                447,
                16,
                0,
                "COLLATE cannot apply to an expression of type int.",
            ),
            (
                // SELECT TOP (-1) 1;
                SqlError::top_negative(),
                127,
                15,
                1,
                "The row count of TOP or FETCH cannot be negative.",
            ),
            (
                // SELECT TOP (NULL) 1;
                SqlError::top_null(),
                1060,
                15,
                1,
                "The row count of TOP or FETCH has to be an integer.",
            ),
            (
                // SELECT *;
                SqlError::select_star_without_from(),
                263,
                16,
                1,
                "No table to select from.",
            ),
            (
                // DECLARE @v varchar(9000);
                SqlError::size_out_of_range(9000, "type", "varchar", 8000),
                131,
                15,
                3,
                "Size 9000 of the type 'varchar' is larger than any data type allows (8000).",
            ),
            (
                // DECLARE @v varbinary(9000);
                SqlError::size_out_of_range(9000, "type", "varbinary", 8000),
                131,
                15,
                3,
                "Size 9000 of the type 'varbinary' is larger than any data type allows (8000).",
            ),
            (
                // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';
                SqlError::invalid_escape("ab", "LIKE"),
                506,
                16,
                1,
                "The escape character \"ab\" of the LIKE predicate must be a single character.",
            ),
            (
                // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE '';
                SqlError::invalid_escape("", "LIKE"),
                506,
                16,
                1,
                "The escape character \"\" of the LIKE predicate must be a single character.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of {number}");
            assert_eq!(err.state, *state, "state of {number}");
            assert_eq!(&err.message, message, "message of {number}");
            assert_eq!(err.line, 0);
        }
    }

    /// The message, severity and state of each procedure-call constructor, with the form
    /// that raises it quoted next to it.
    #[test]
    fn procedure_call_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // EXEC sp_executesql;
                SqlError::procedure_expects_parameter("sp_executesql", "@statement"),
                201,
                16,
                10,
                "Procedure 'sp_executesql' was called without the parameter '@statement' it requires.",
            ),
            (
                // EXEC sp_who 'sa', 'x';
                SqlError::too_many_arguments("sp_who"),
                8144,
                16,
                2,
                "Procedure sp_who was called with more arguments than it declares.",
            ),
            (
                // EXEC sp_who @x = 1;
                SqlError::not_a_parameter("@x", "sp_who"),
                8145,
                16,
                1,
                "@x is not a parameter declared by procedure sp_who.",
            ),
            (
                // EXEC sp_executesql N'SELECT 1', N'', N'', N'';
                SqlError::no_parameter_but_arguments(""),
                8146,
                16,
                1,
                "Procedure  declares no parameter and was called with arguments.",
            ),
            (
                // EXEC sp_executesql @stmt = N'SELECT 1', 1;
                SqlError::positional_after_named(2),
                119,
                15,
                1,
                "Parameter number 2 and the ones after it have to use the '@name = value' form; once that form has been used, a positional argument may not follow it.",
            ),
            (
                // EXEC sp_executesql N'SELECT @x OUTPUT', N'@x int', 1 OUTPUT;
                SqlError::output_on_a_constant(),
                179,
                15,
                1,
                "The OUTPUT option cannot be used on a constant argument of a procedure.",
            ),
            (
                // EXEC sp_executesql 123;
                SqlError::procedure_expects_type("@statement", "ntext/nchar/nvarchar"),
                214,
                16,
                2,
                "The parameter '@statement' has to be of type 'ntext/nchar/nvarchar' for this procedure.",
            ),
            (
                // EXEC sp_executesql N'SELECT @x', N'@x int';
                SqlError::parameter_not_supplied("(@x int)SELECT @x", "@x"),
                8178,
                16,
                1,
                "The parameterized query '(@x int)SELECT @x' was called without its parameter '@x'.",
            ),
            (
                // EXEC sp_execute 123456;
                SqlError::prepared_statement_not_found(123456),
                8179,
                16,
                4,
                "No prepared statement of this session has the handle 123456.",
            ),
            (
                // DECLARE @p int; EXEC sp_prepare @p OUTPUT, N'@x int, @x int', N'SELECT @x';
                SqlError::statement_could_not_be_prepared(),
                8180,
                16,
                1,
                "The statement could not be prepared.",
            ),
            (
                // EXEC sp_help 'nosuchobj';
                SqlError::object_missing_in_database("nosuchobj", "master"),
                15009,
                16,
                1,
                "The object 'nosuchobj' is not in database 'master', or this operation does not accept it.",
            ),
            (
                // EXEC sp_helpdb N'nosuchdb';
                SqlError::help_database_not_found("nosuchdb"),
                15010,
                16,
                1,
                "No database named 'nosuchdb' exists; give a valid database name.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of {number}");
            assert_eq!(err.state, *state, "state of {number}");
            assert_eq!(&err.message, message, "message of {number}");
            assert_eq!(err.line, 0);
        }
    }

    /// The rendered message of each operator and result-space constructor, with a query
    /// that raises it quoted next to it; state and severity go with the query.
    #[test]
    fn operator_and_result_space_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // SELECT CAST(1 AS bit) + CAST(1 AS bit);
                SqlError::incompatible_types_for_operator("bit", "bit", "add"),
                402,
                16,
                1,
                "The types bit and bit cannot be combined by the add operator.",
            ),
            (
                // DECLARE @d date = '2020-01-01'; DECLARE @t datetime = '2020-01-01'; SELECT @d + @t;
                SqlError::incompatible_types_for_operator("date", "datetime", "add"),
                402,
                16,
                1,
                "The types date and datetime cannot be combined by the add operator.",
            ),
            (
                // DECLARE @v varchar(1)='a'; DECLARE @b varbinary(1)=0x41; SELECT @v + @b;
                SqlError::incompatible_types_for_operator("varchar", "varbinary", "add"),
                402,
                16,
                1,
                "The types varchar and varbinary cannot be combined by the add operator.",
            ),
            (
                // DECLARE @v varchar(1)='a'; DECLARE @w varchar(1)='b'; SELECT @v - @w;
                SqlError::incompatible_types_for_operator("varchar", "varchar", "subtract"),
                402,
                16,
                1,
                "The types varchar and varchar cannot be combined by the subtract operator.",
            ),
            (
                // DECLARE @f float = 1; SELECT @f & 1;
                SqlError::incompatible_types_for_operator("float", "int", "'&'"),
                402,
                16,
                1,
                "The types float and int cannot be combined by the '&' operator.",
            ),
            (
                // SELECT NULLIF(NULL, 1);
                SqlError::nullif_first_argument_null(),
                4151,
                16,
                1,
                "The first argument of NULLIF cannot be the NULL constant: its type has to be known.",
            ),
            (
                // DECLARE @m money = 922337203685477.58; SELECT CAST(@m AS varchar(3));
                SqlError::insufficient_result_space_money_to("varchar"),
                234,
                16,
                2,
                "A money value does not fit in the result type varchar.",
            ),
            (
                // DECLARE @m money = 922337203685477.58; SELECT CAST(@m AS nvarchar(3));
                SqlError::insufficient_result_space_money_to("nvarchar"),
                234,
                16,
                2,
                "A money value does not fit in the result type nvarchar.",
            ),
            (
                // DECLARE @s smallmoney = 214748.3647; SELECT CAST(@s AS varchar(3));
                SqlError::insufficient_result_space_smallmoney_to("varchar"),
                292,
                16,
                2,
                "A smallmoney value does not fit in the result type varchar.",
            ),
            (
                // DECLARE @g uniqueidentifier = NEWID(); SELECT CAST(@g AS varchar(35));
                SqlError::insufficient_result_space_guid(),
                8170,
                16,
                2,
                "A uniqueidentifier value does not fit in the char result.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of {number}");
            assert_eq!(err.state, *state, "state of {number}");
            assert_eq!(&err.message, message, "message of {number}");
            assert_eq!(err.line, 0);
        }
    }

    /// The rendered message of each type declaration constructor, with a query that
    /// raises it quoted next to it; severity and state go with the query, the catalogue
    /// notwithstanding.
    #[test]
    fn type_declaration_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // SELECT CAST(1 AS nvarchar(5000));
                SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000),
                131,
                16,
                1,
                "Size 5000 of the convert specification 'nvarchar' is larger than any data type allows (4000).",
            ),
            (
                // SELECT CAST(1 AS nchar(5000));
                SqlError::convert_specification_size_out_of_range(5000, "nchar", 4000),
                131,
                16,
                1,
                "Size 5000 of the convert specification 'nchar' is larger than any data type allows (4000).",
            ),
            (
                // DECLARE @v decimal(5,6);
                SqlError::scale_greater_than_precision(),
                192,
                15,
                1,
                "Scale cannot exceed precision.",
            ),
            (
                // DECLARE @v varchar(0);
                SqlError::invalid_length_or_precision(1, 0),
                1001,
                15,
                1,
                "Line 1: the length or precision 0 is not valid.",
            ),
            (
                // DECLARE @v time(8);
                SqlError::invalid_scale(1, 8),
                1002,
                15,
                1,
                "Line 1: the scale 8 is not valid.",
            ),
            (
                // DECLARE @v decimal(39,2);
                SqlError::type_size_out_of_range(39, "decimal", 38),
                2717,
                16,
                1,
                "Size 39 of the type 'decimal' is larger than the maximum (38).",
            ),
            (
                // DECLARE @v nvarchar(5000);
                SqlError::parameter_size_out_of_range(5000, "@v", 4000),
                2717,
                16,
                2,
                "Size 5000 of the parameter '@v' is larger than the maximum (4000).",
            ),
            (
                // CREATE TABLE #t(c nvarchar(5000));
                SqlError::parameter_size_out_of_range(5000, "c", 4000),
                2717,
                16,
                2,
                "Size 5000 of the parameter 'c' is larger than the maximum (4000).",
            ),
            (
                // DECLARE @v float(54);
                SqlError::precision_greater_than_maximum(1, 54, 53),
                2750,
                16,
                1,
                "Column or parameter #1: precision 54 exceeds the maximum of 53.",
            ),
            (
                // CREATE TABLE #t(c decimal(39,2));
                SqlError::precision_greater_than_maximum(1, 39, 38),
                2750,
                16,
                1,
                "Column or parameter #1: precision 39 exceeds the maximum of 38.",
            ),
            (
                // SELECT CAST(1 AS foo);
                SqlError::not_a_defined_system_type("foo"),
                243,
                16,
                1,
                "foo is not a known system type.",
            ),
            (
                // SELECT CAST('not a date' AS smalldatetime);
                SqlError::conversion_failed_smalldatetime(),
                295,
                16,
                3,
                "The character string could not be converted to smalldatetime.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of {number}");
            assert_eq!(err.state, *state, "state of {number}");
            assert_eq!(&err.message, message, "message of {number}");
        }
    }

    /// The rendered message of the length and qualified-name constructors, with a query
    /// that raises it quoted next to it; severity and state go with the query.
    #[test]
    fn qualified_function_name_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // SELECT LEFT('abc', -1);
                SqlError::invalid_length_parameter("left"),
                536,
                16,
                6,
                "The length given to the left function is not valid.",
            ),
            (
                // SELECT RIGHT('abc', -1);
                SqlError::invalid_length_parameter("right"),
                536,
                16,
                6,
                "The length given to the right function is not valid.",
            ),
            (
                // SELECT SUBSTRING('abc', 1, -1);
                SqlError::invalid_length_parameter("substring"),
                536,
                16,
                8,
                "The length given to the substring function is not valid.",
            ),
            (
                // CREATE TABLE #t(c varchar(9000));
                SqlError::size_out_of_range(9000, "column", "c", 8000),
                131,
                15,
                2,
                "Size 9000 of the column 'c' is larger than any data type allows (8000).",
            ),
            (
                // SELECT dbo.LEN(1);
                SqlError::cannot_find_column_or_function("dbo.LEN"),
                4121,
                16,
                1,
                "Neither a column \"dbo\" nor a user-defined function or aggregate \"dbo.LEN\" was found, or the name is ambiguous.",
            ),
            (
                // SELECT foo.bar(1);
                SqlError::cannot_find_column_or_function("foo.bar"),
                4121,
                16,
                1,
                "Neither a column \"foo\" nor a user-defined function or aggregate \"foo.bar\" was found, or the name is ambiguous.",
            ),
            (
                // SELECT sys.LEN(1);
                SqlError::cannot_find_column_or_function("sys.LEN"),
                4121,
                16,
                1,
                "Neither a column \"sys\" nor a user-defined function or aggregate \"sys.LEN\" was found, or the name is ambiguous.",
            ),
            (
                // SELECT dbo.foo.bar(1);
                SqlError::cannot_find_column_or_function("dbo.foo.bar"),
                4121,
                16,
                1,
                "Neither a column \"dbo\" nor a user-defined function or aggregate \"dbo.foo.bar\" was found, or the name is ambiguous.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of {number}");
            assert_eq!(err.state, *state, "state of {number}");
            assert_eq!(&err.message, message, "message of {number}");
            assert_eq!(err.line, 0);
        }
    }

    /// 4121 fills its two placeholders from one name: the leading part, then the whole
    /// name. A name without a dot repeats itself, which is what a caller that qualified
    /// nothing would print; `binder` sends 195 in that case (`SELECT bar(1);` raises 195
    /// state 10).
    #[test]
    fn cannot_find_column_or_function_splits_on_the_first_dot() {
        let one_part = SqlError::cannot_find_column_or_function("bar");
        assert_eq!(
            one_part.message,
            "Neither a column \"bar\" nor a user-defined function or aggregate \"bar\" was found, or the name is ambiguous."
        );

        let three_parts = SqlError::cannot_find_column_or_function("db1.dbo.bar");
        assert!(three_parts.message.contains("column \"db1\""));
        assert!(three_parts.message.contains("aggregate \"db1.dbo.bar\""));
    }

    /// Error 4104 for each form of a qualified column name: one `%.*ls` and the whole
    /// name in it, state 1 throughout, with the query in the comment.
    #[test]
    fn multi_part_identifier_is_4104() {
        let expected: &[(SqlError, &str)] = &[
            (
                // SELECT t.c;
                SqlError::multi_part_identifier("t.c"),
                "The qualified name \"t.c\" matches nothing in scope.",
            ),
            (
                // SELECT a.b.c.d;
                SqlError::multi_part_identifier("a.b.c.d"),
                "The qualified name \"a.b.c.d\" matches nothing in scope.",
            ),
            (
                // SELECT [dbo].[t].[c]; — the server prints the name brackets stripped.
                SqlError::multi_part_identifier("dbo.t.c"),
                "The qualified name \"dbo.t.c\" matches nothing in scope.",
            ),
            (
                // SELECT x.y FROM (SELECT 1 AS c) AS z;
                SqlError::multi_part_identifier("x.y"),
                "The qualified name \"x.y\" matches nothing in scope.",
            ),
        ];
        for (err, message) in expected {
            assert_eq!(err.number, 4104);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, 1, "state of {message}");
            assert_eq!(&err.message, message);
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }
    }

    /// The state of 237 follows the target on the four targets that raise it, and the
    /// message is the same text throughout (`SELECT CAST(CAST(300000 AS money) AS
    /// <target>);`, except `int`, which needs a larger value).
    #[test]
    fn insufficient_result_space_money_states() {
        let expected: &[(&str, u8)] = &[
            // SELECT CAST(CAST(99999999999 AS money) AS int);
            ("int", 1),
            // SELECT CAST(CAST(300000 AS money) AS smallint);
            ("smallint", 2),
            // SELECT CAST(CAST(300000 AS money) AS tinyint);
            ("tinyint", 3),
            // SELECT CAST(CAST(300000 AS money) AS smallmoney);
            ("smallmoney", 3),
        ];
        for (to, state) in expected {
            let err = SqlError::insufficient_result_space_money(to);
            assert_eq!(err.number, 237);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, *state, "state towards {to}");
            assert_eq!(
                err.message,
                format!("A money value does not fit in the result type {to}.")
            );
        }

        // A target without a row keeps the default state.
        assert_eq!(
            SqlError::insufficient_result_space_money("bigint").state,
            super::DEFAULT_RESULT_SPACE_STATE
        );
    }

    /// 131 and 2717 each carry one template and several constructors: the `%S_MSG`, the
    /// severity and the state are what tells the contexts apart.
    /// `SELECT CAST(1 AS nvarchar(5000));`, `DECLARE @v varchar(9000);` and
    /// `CREATE TABLE #t(c varchar(9000));` for the three fillings of 131,
    /// `DECLARE @v decimal(39,2);` against `DECLARE @v nvarchar(5000);` for 2717.
    #[test]
    fn the_two_size_contexts_of_131_and_2717_differ_by_severity_and_state() {
        let convert = SqlError::convert_specification_size_out_of_range(5000, "nvarchar", 4000);
        let declaration = SqlError::size_out_of_range(9000, "type", "varchar", 8000);
        let column = SqlError::size_out_of_range(9000, "column", "c", 8000);
        assert_eq!(
            (convert.number, convert.severity, convert.state),
            (131, 16, 1)
        );
        assert_eq!(
            (declaration.number, declaration.severity, declaration.state),
            (131, 15, 3)
        );
        assert_eq!((column.number, column.severity, column.state), (131, 15, 2));
        assert!(declaration.message.contains("the type 'varchar'"));
        assert!(column.message.contains("the column 'c'"));

        let ty = SqlError::type_size_out_of_range(39, "decimal", 38);
        let parameter = SqlError::parameter_size_out_of_range(5000, "@v", 4000);
        assert_eq!((ty.number, ty.severity, ty.state), (2717, 16, 1));
        assert_eq!(
            (parameter.number, parameter.severity, parameter.state),
            (2717, 16, 2)
        );
        assert!(ty.message.contains("the type 'decimal'"));
        assert!(parameter.message.contains("the parameter '@v'"));
    }

    /// 1001 and 1002 print the batch line inside the message *and* carry it as the
    /// error's line: `DECLARE @a int;` then `DECLARE @v varchar(0);` prints `Line 2:` and
    /// the server sends line 2.
    #[test]
    fn the_line_of_1001_and_1002_is_in_the_message_and_on_the_error() {
        let length = SqlError::invalid_length_or_precision(2, 0);
        assert_eq!(length.line, 2);
        assert_eq!(
            length.message,
            "Line 2: the length or precision 0 is not valid."
        );

        let scale = SqlError::invalid_scale(2, 8);
        assert_eq!(scale.line, 2);
        assert_eq!(scale.message, "Line 2: the scale 8 is not valid.");
    }

    /// 234 and 237 share their text and differ by number and state: the character target
    /// sends 234 state 2, the numeric one 237 state 1, 2 or 3 depending on the target
    /// (table `RESULT_SPACE_237_STATES`).
    #[test]
    fn error_234_and_237_differ_by_number_and_state() {
        let to_character = SqlError::insufficient_result_space_money_to("varchar");
        let to_numeric = SqlError::insufficient_result_space_money("int");
        assert_eq!(to_character.number, 234);
        assert_eq!(to_numeric.number, 237);
        assert_eq!(to_character.state, 2);
        assert_eq!(to_numeric.state, 1);
        assert_eq!(
            SqlError::insufficient_result_space_money("smallmoney").state,
            3
        );
        assert_eq!(
            to_character.message,
            "A money value does not fit in the result type varchar."
        );
    }

    /// The states of 220, 232 and 8115 index the conversion routine, not the target: the
    /// same target reached from two sources carries two states, with the query in the
    /// comment; a pair without a row keeps state 2, which the stateless constructors send.
    #[test]
    fn overflow_states_follow_the_source_and_target() {
        // SELECT CAST(CAST(99999999999999999 AS numeric(20,0)) AS money); / AS smallmoney);
        for to in ["money", "smallmoney"] {
            let err = SqlError::arithmetic_overflow_from("numeric", to);
            assert_eq!(err.number, 8115);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, 4, "state of numeric -> {to}");
            assert_eq!(
                err.message,
                format!("Converting numeric to data type {to} overflowed.")
            );
        }
        // SELECT CAST('1234.5' AS decimal(5,2)); / SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));
        assert_eq!(
            SqlError::arithmetic_overflow_from("varchar", "numeric").state,
            8
        );
        assert_eq!(
            SqlError::arithmetic_overflow_from("float", "numeric").state,
            6
        );
        // SELECT CAST(CAST(3000000000 AS bigint) AS int);
        assert_eq!(
            SqlError::arithmetic_overflow_from("expression", "int").state,
            2
        );
        // DECLARE @n numeric(20,0) = 99999999999999999; SELECT CAST(@n AS varchar(3)); / AS char(3)); / AS nvarchar(3));
        for to in ["varchar", "char"] {
            let err = SqlError::arithmetic_overflow_from("numeric", to);
            assert_eq!(err.state, 5, "state of numeric -> {to}");
        }
        assert_eq!(
            SqlError::arithmetic_overflow_from("expression", "nvarchar").state,
            2
        );

        // SELECT CAST(CAST(300 AS float) AS tinyint); / AS int); / SELECT CAST(CAST(300 AS money) AS tinyint);
        for (from, to, state) in [
            ("float", "tinyint", 1),
            ("real", "tinyint", 1),
            ("float", "int", 3),
            ("real", "int", 3),
            ("float", "smallint", 2),
            ("float", "smallmoney", 2),
            ("float", "money", 2),
            ("float", "real", 2),
            ("money", "tinyint", 11),
            ("numeric", "tinyint", 2),
        ] {
            let err = SqlError::overflow_for_type_from(from, to, 300.0);
            assert_eq!(err.number, 232);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, state, "state of 232, {from} -> {to}");
            assert_eq!(
                err.message,
                format!("Value out of range for type {to}: 300.000000.")
            );
        }

        // SELECT CAST(CAST(40000 AS money) AS smallint); / AS smallmoney); / CAST(40000 AS int) AS smallint);
        // `DECLARE @t TABLE(c smallint); INSERT @t VALUES(CAST(40000 AS money));` sends the
        // same 220 state 7: the state follows the pair, not the statement.
        for (from, to, state) in [
            ("money", "smallint", 7),
            ("smallmoney", "smallint", 5),
            ("int", "smallint", 1),
            ("int", "tinyint", 2),
            ("smallint", "tinyint", 2),
        ] {
            let err = SqlError::overflow_for_data_type_from(from, to, 400_000_000);
            assert_eq!(err.number, 220);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, state, "state of 220, {from} -> {to}");
            assert_eq!(
                err.message,
                format!("Value out of range for data type {to}: 400000000.")
            );
        }
        // `bigint`, `numeric` and `smallmoney` towards `tinyint` raise 8115 state 2
        // instead of 220 (`DECLARE @b bigint = 300; SELECT CAST(@b AS tinyint);`), so the
        // fallback of `overflow_state` is defensive.
        assert_eq!(
            SqlError::arithmetic_overflow_from("expression", "tinyint").state,
            super::DEFAULT_OVERFLOW_STATE
        );
    }

    /// The three stateless overflow constructors keep the signature and the state their
    /// callers of `vauban-types` were written against: the twins add states, they do not
    /// change them.
    #[test]
    fn the_stateless_overflow_constructors_are_unchanged() {
        assert_eq!(SqlError::arithmetic_overflow("numeric", "money").state, 2);
        assert_eq!(SqlError::overflow_for_type("tinyint", 300.0).state, 2);
        assert_eq!(SqlError::overflow_for_data_type("smallint", 40000).state, 2);
    }

    /// Error 8115 towards `numeric` keeps its number and severity but changes state, and
    /// the two-argument constructor is untouched: `SELECT CAST('1234.5' AS decimal(5,2));`
    /// (state 8), `SELECT CAST(CAST(1234.5 AS float) AS decimal(5,2));` (state 6) and
    /// `SELECT CAST('99999999999' AS bigint) * 1000000000;` (state 2).
    #[test]
    fn arithmetic_overflow_to_numeric_states_follow_the_source() {
        let from_varchar = SqlError::arithmetic_overflow_to_numeric("varchar");
        assert_eq!(from_varchar.number, 8115);
        assert_eq!(from_varchar.severity, 16);
        assert_eq!(from_varchar.state, 8);
        assert_eq!(
            from_varchar.message,
            "Converting varchar to data type numeric overflowed."
        );

        for from in ["nvarchar", "int", "bigint", "numeric", "money"] {
            assert_eq!(SqlError::arithmetic_overflow_to_numeric(from).state, 8);
        }
        for from in ["float", "real"] {
            let err = SqlError::arithmetic_overflow_to_numeric(from);
            assert_eq!(err.state, 6, "state from {from}");
            assert_eq!(
                err.message,
                format!("Converting {from} to data type numeric overflowed.")
            );
        }

        let expression = SqlError::arithmetic_overflow("expression", "bigint");
        assert_eq!(expression.state, 2);
        assert_eq!(
            expression.message,
            "Converting expression to data type bigint overflowed."
        );
    }

    #[test]
    fn invalid_object_name_208_message_is_exact() {
        let err = SqlError::invalid_object_name("dbo.t");
        assert_eq!(err.number, 208);
        assert_eq!(err.message, "Unknown object name 'dbo.t'.");
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 0);
    }

    #[test]
    fn invalid_column_name_207_message_is_exact() {
        let err = SqlError::invalid_column_name("c");
        assert_eq!(err.number, 207);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(err.message, "Unknown column name 'c'.");
    }

    #[test]
    fn incorrect_syntax_near_sets_line() {
        let err = SqlError::incorrect_syntax_near("SELEC", 1);
        assert_eq!(err.number, 102);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 1);
        assert_eq!(err.message, "Syntax error near 'SELEC'.");
    }

    #[test]
    fn divide_by_zero_8134_has_no_arguments() {
        let err = SqlError::divide_by_zero();
        assert_eq!(err.number, 8134);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(err.message, "Division by zero.");
    }

    #[test]
    fn cannot_insert_null_515_message_and_state() {
        let err = SqlError::cannot_insert_null("c", "db.dbo.t", "INSERT");
        assert_eq!(err.number, 515);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 2);
        assert_eq!(
            err.message,
            "Column 'c' of table 'db.dbo.t' does not accept NULL; INSERT fails."
        );
    }

    #[test]
    fn cannot_insert_null_515_update_statement() {
        let err = SqlError::cannot_insert_null("c", "db.dbo.t", "UPDATE");
        assert!(err.message.ends_with(" UPDATE fails."));
    }

    #[test]
    fn string_or_binary_data_truncated_2628_message_and_state() {
        let err = SqlError::string_or_binary_data_truncated("db.dbo.t", "code", "ABCDEFGHIJ");
        assert_eq!(err.number, 2628);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "Data too long for table 'db.dbo.t', column 'code': the value 'ABCDEFGHIJ' would be cut."
        );
    }

    #[test]
    fn procedure_not_found_2812_message_is_exact() {
        let err = SqlError::procedure_not_found("dbo.p");
        assert_eq!(err.number, 2812);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 62);
        assert_eq!(err.message, "Unknown stored procedure 'dbo.p'.");
    }

    #[test]
    fn cannot_open_database_4060_message_and_severity() {
        let err = SqlError::cannot_open_database("nope");
        assert_eq!(err.number, 4060);
        assert_eq!(err.severity, 11);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The database \"nope\" requested at login could not be opened; login refused."
        );
    }

    #[test]
    fn database_not_found_911_message_is_exact() {
        let err = SqlError::database_not_found("nope");
        assert_eq!(err.number, 911);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "No database named 'nope' exists; check the spelling of the name."
        );
    }

    #[test]
    fn unique_violation_2627_message_is_exact() {
        let err = SqlError::unique_violation("PRIMARY KEY", "PK_t", "dbo.t", "(1)");
        assert_eq!(err.number, 2627);
        assert_eq!(err.severity, 14);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The PRIMARY KEY constraint 'PK_t' rejects a duplicate key in object 'dbo.t': the value (1) exists already."
        );
    }

    #[test]
    fn duplicate_key_index_2601_message_is_exact() {
        let err = SqlError::duplicate_key_index("dbo.t", "IX_t", "(1)");
        assert_eq!(err.number, 2601);
        assert_eq!(err.severity, 14);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "Duplicate key in object 'dbo.t' for unique index 'IX_t': the value (1) exists already."
        );
    }

    #[test]
    fn fk_violation_547_with_column() {
        let err = SqlError::fk_violation(
            "INSERT",
            "FOREIGN KEY",
            "FK_child_parent",
            "db",
            "dbo.parent",
            Some("id"),
        );
        assert_eq!(err.number, 547);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 0);
        assert_eq!(
            err.message,
            "INSERT violates the FOREIGN KEY constraint \"FK_child_parent\" in database \"db\", table \"dbo.parent\", column 'id'."
        );
    }

    #[test]
    fn fk_violation_547_without_column() {
        let err = SqlError::fk_violation("INSERT", "CHECK", "CK_t_positive", "db", "dbo.t", None);
        assert_eq!(
            err.message,
            "INSERT violates the CHECK constraint \"CK_t_positive\" in database \"db\", table \"dbo.t\"."
        );
    }

    #[test]
    fn login_failed_18456_message_is_exact() {
        let err = SqlError::login_failed("sa");
        assert_eq!(err.number, 18456);
        assert_eq!(err.severity, 14);
        assert_eq!(err.state, 1);
        assert_eq!(err.message, "Login refused for user 'sa'.");
    }

    #[test]
    fn conversion_failed_245_message_is_exact() {
        let err = SqlError::conversion_failed("varchar", "abc", "int");
        assert_eq!(err.number, 245);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The varchar value 'abc' could not be converted to data type int."
        );
    }

    #[test]
    fn error_converting_data_type_8114_message_and_state() {
        let err = SqlError::error_converting_data_type("varchar", "numeric");
        assert_eq!(err.number, 8114);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 5);
        assert_eq!(
            err.message,
            "Data type varchar could not be converted to numeric."
        );
    }

    #[test]
    fn conversion_failed_datetime_241_message_is_exact() {
        let err = SqlError::conversion_failed_datetime();
        assert_eq!(err.number, 241);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The character string could not be converted to a date or time."
        );
    }

    #[test]
    fn arithmetic_overflow_8115_message_and_state() {
        let err = SqlError::arithmetic_overflow("expression", "int");
        assert_eq!(err.number, 8115);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 2);
        assert_eq!(
            err.message,
            "Converting expression to data type int overflowed."
        );
    }

    #[test]
    fn unclosed_quotation_mark_is_105() {
        let err = SqlError::unclosed_quotation_mark("abc;", 4);
        assert_eq!(err.number, 105);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 4);
        assert_eq!(
            err.message,
            "Quotation mark left open after the string 'abc;'."
        );
    }

    #[test]
    fn missing_end_comment_mark_is_113() {
        let err = SqlError::missing_end_comment_mark(2);
        assert_eq!(err.number, 113);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 2);
        assert_eq!(err.message, "Comment is not closed: '*/' expected.");
    }

    #[test]
    fn incorrect_syntax_near_keyword_is_156() {
        let err = SqlError::incorrect_syntax_near_keyword("FROM", 3);
        assert_eq!(err.number, 156);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(err.line, 3);
        assert_eq!(err.message, "Syntax error near the keyword 'FROM'.");
    }

    #[test]
    fn variable_already_declared_is_134() {
        let err = SqlError::variable_already_declared("@X");
        assert_eq!(err.number, 134);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The variable '@X' is already declared; a batch or a procedure declares each name once."
        );
    }

    #[test]
    fn must_declare_scalar_variable_is_137() {
        let err = SqlError::must_declare_scalar_variable("@x");
        assert_eq!(err.number, 137);
        assert_eq!(err.severity, 15);
        assert_eq!(err.message, "The scalar variable \"@x\" is not declared.");
    }

    /// The state of 137 follows the form: the variable a statement assigns sends 1, the one
    /// it reads sends 2, with the queries in the comments, which answer the same number,
    /// the same severity and the same text.
    #[test]
    fn must_declare_scalar_variable_states_follow_the_form() {
        // SELECT @x = 1; — and SET @x = 1;, and SELECT @x = 1, @y = 2;
        let assigned = SqlError::must_declare_scalar_variable_assigned("@x");
        // SELECT @x; — and SELECT @x + 1;, PRINT @x;, SELECT 1 WHERE @x = 1;
        let read = SqlError::must_declare_scalar_variable("@x");

        assert_eq!(assigned.state, 1);
        assert_eq!(read.state, 2);
        for err in [&assigned, &read] {
            assert_eq!(err.number, 137);
            assert_eq!(err.severity, 15);
            assert_eq!(err.message, "The scalar variable \"@x\" is not declared.");
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }

        // SELECT @x = @y; — the read of `@y` wins over the assignment of `@x`.
        assert_eq!(SqlError::must_declare_scalar_variable("@y").state, 2);
    }

    /// Error 1062 for `SELECT TOP (1) WITH TIES 1 AS c;`: severity 15 on the wire where
    /// the catalogue holds 16, state 1, and a text with no argument.
    #[test]
    fn top_with_ties_without_order_by_is_1062() {
        let err = SqlError::top_with_ties_without_order_by();
        assert_eq!(err.number, 1062);
        assert_eq!(err.severity, 15);
        assert_eq!(
            message_template(1062).expect("1062 is catalogued").severity,
            16
        );
        assert_eq!(err.state, 1);
        assert_eq!(err.message, "TOP WITH TIES requires an ORDER BY clause.");
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
    }

    /// Errors 107 and 117 for each form of a qualified wildcard. Both severity 15 and
    /// state 1; 107 prints the prefix, 117 the whole name, its `%S_MSG` filled with
    /// `column` and its `%d` with 3.
    #[test]
    fn qualified_wildcard_errors_are_107_and_117() {
        let expected: &[(SqlError, u32, &str)] = &[
            (
                // SELECT t.*;
                SqlError::column_prefix_does_not_match("t"),
                107,
                "The column prefix 't' matches no table or alias of the query.",
            ),
            (
                // SELECT [dbo].[t].*; — the server prints the prefix brackets stripped.
                SqlError::column_prefix_does_not_match("dbo.t"),
                107,
                "The column prefix 'dbo.t' matches no table or alias of the query.",
            ),
            (
                // SELECT db1.dbo.t.*;
                SqlError::column_prefix_does_not_match("db1.dbo.t"),
                107,
                "The column prefix 'db1.dbo.t' matches no table or alias of the query.",
            ),
            (
                // SELECT a.b.c.d.*;
                SqlError::too_many_column_prefixes("a.b.c.d"),
                117,
                "The column name 'a.b.c.d' has too many prefixes; at most 3 are allowed.",
            ),
            (
                // SELECT a.b.c.d.e.*;
                SqlError::too_many_column_prefixes("a.b.c.d.e"),
                117,
                "The column name 'a.b.c.d.e' has too many prefixes; at most 3 are allowed.",
            ),
        ];
        for (err, number, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, 15, "severity of {number}");
            assert_eq!(err.state, 1, "state of {number}");
            assert_eq!(&err.message, message);
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }
    }

    /// Error 281 names the source in its `%ls`: six sources under one target print six
    /// different words; four targets under one source print the same one, which is what
    /// separates "the `%ls` is the source" from "the `%ls` is the target".
    #[test]
    fn invalid_style_number_names_the_source_type() {
        let sources: &[(&str, &str)] = &[
            // SELECT CONVERT(varchar(30), CAST('2020-01-01' AS date), 999);
            (
                "date",
                "Style 999 is not defined for converting date to a character string.",
            ),
            // SELECT CONVERT(varchar(30), CAST('10:00:00' AS time), 999);
            (
                "time",
                "Style 999 is not defined for converting time to a character string.",
            ),
            // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00' AS datetime), 999);
            (
                "datetime",
                "Style 999 is not defined for converting datetime to a character string.",
            ),
            // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00' AS smalldatetime), 999);
            (
                "smalldatetime",
                "Style 999 is not defined for converting smalldatetime to a character string.",
            ),
            // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00' AS datetime2), 999);
            (
                "datetime2",
                "Style 999 is not defined for converting datetime2 to a character string.",
            ),
            // SELECT CONVERT(varchar(30), CAST('2020-01-01 10:00:00 +02:00' AS datetimeoffset), 999);
            (
                "datetimeoffset",
                "Style 999 is not defined for converting datetimeoffset to a character string.",
            ),
        ];
        for (from, message) in sources {
            let err = SqlError::invalid_style_number(999, from);
            assert_eq!(err.number, 281);
            assert_eq!(err.severity, 16);
            assert_eq!(err.state, 1);
            assert_eq!(&err.message, message);
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }

        // CONVERT(nvarchar(30), …), CONVERT(char(30), …) and CONVERT(nchar(30), …) print
        // the same text as CONVERT(varchar(30), …): the target never reaches the message,
        // so the constructor has no parameter for it.
        assert_eq!(
            SqlError::invalid_style_number(999, "date").message,
            sources[0].1
        );

        // SELECT CONVERT(varchar(30), CAST('2020-01-01' AS date), -1); and …, 1000);
        assert_eq!(
            SqlError::invalid_style_number(-1, "date").message,
            "Style -1 is not defined for converting date to a character string."
        );
        assert_eq!(
            SqlError::invalid_style_number(1000, "date").message,
            "Style 1000 is not defined for converting date to a character string."
        );
    }

    /// Error 9807 and its state 0 for `SELECT CONVERT(date, '2020-01-01', 100);`; 8134
    /// (state 1) and 235 (state 0) are asserted beside it as the two states a reader has
    /// to tell apart.
    #[test]
    fn input_does_not_follow_style_is_9807_state_zero() {
        let err = SqlError::input_does_not_follow_style(100);
        assert_eq!(err.number, 9807);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 0);
        assert_eq!(
            err.message,
            "The input string does not match style 100; change the string or the style."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // The two states a reader has to tell apart: 1 next to 0.
        assert_eq!(SqlError::divide_by_zero().state, 1);
        assert_eq!(SqlError::char_to_money_syntax().state, 0);

        // The seventeen strict styles (`SELECT CONVERT(date, '2020-01-01', <style>);` and
        // its siblings towards `time(7)`, `datetime2(7)` and `datetimeoffset(7)`): each
        // carries its own number into the `%d`, at state 0.
        for style in [
            6, 7, 8, 9, 12, 13, 14, 24, 100, 106, 107, 108, 109, 112, 113, 114, 130,
        ] {
            let err = SqlError::input_does_not_follow_style(style);
            assert_eq!(err.state, 0);
            assert!(
                err.message
                    .starts_with(&format!("The input string does not match style {style};")),
                "style {style}"
            );
        }
    }

    /// Errors 1031 and 1014 for `SELECT TOP (101) PERCENT 1;` and
    /// `SELECT TOP (NULL) PERCENT 1;`, both severity 15, beside 8134 and 8115 at 16 and
    /// 127, 1060 and 102 at 15.
    #[test]
    fn top_percent_errors_are_1031_and_1014_severity_15() {
        let percent = SqlError::percent_out_of_range();
        assert_eq!(percent.number, 1031);
        assert_eq!(percent.severity, 15);
        assert_eq!(percent.state, 1);
        assert_eq!(
            percent.message,
            "A percentage has to lie between 0 and 100."
        );

        let invalid = SqlError::top_invalid_value();
        assert_eq!(invalid.number, 1014);
        assert_eq!(invalid.severity, 15);
        assert_eq!(invalid.state, 1);
        assert_eq!(
            invalid.message,
            "The value of the TOP or FETCH clause is not valid."
        );

        for err in [&percent, &invalid] {
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }

        // Neighbouring numbers keep the two severities apart: 15 against 16.
        assert_eq!(SqlError::top_negative().severity, 15);
        assert_eq!(SqlError::top_null().severity, 15);
        assert_eq!(SqlError::divide_by_zero().severity, 16);
    }

    /// The state of 506 follows the predicate, not the operand the message quotes: the
    /// three operands narrow send 1, one Unicode operand among the three sends 2, with
    /// the queries in the comments, which answer the same number, the same severity and
    /// the same text.
    #[test]
    fn invalid_escape_states_follow_the_predicate() {
        // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE 'ab';
        let narrow = SqlError::invalid_escape("ab", "LIKE");
        // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE N'ab'; — the escape operand alone flips it,
        // and so do N'a' LIKE 'a' ESCAPE 'ab' and 'a' LIKE N'a' ESCAPE 'ab'.
        let unicode = SqlError::invalid_escape_unicode("ab", "LIKE");

        assert_eq!(narrow.state, 1);
        assert_eq!(unicode.state, 2);
        for err in [&narrow, &unicode] {
            assert_eq!(err.number, 506);
            assert_eq!(err.severity, 16);
            assert_eq!(
                err.message,
                "The escape character \"ab\" of the LIKE predicate must be a single character."
            );
            assert_eq!(err.line, 0);
            assert_eq!(err.procedure, None);
        }

        // SELECT 1 WHERE 'a' LIKE 'a' ESCAPE ''; and its Unicode twin: same rule.
        assert_eq!(SqlError::invalid_escape("", "LIKE").state, 1);
        assert_eq!(SqlError::invalid_escape_unicode("", "LIKE").state, 2);
    }

    /// The state of 8114 for a conversion written in an expression: `datetimeoffset`
    /// sends 31 from each of the four narrow source names, the other targets send 5, with
    /// the queries in the comments.
    ///
    /// The four names are all in the table because the caller passes a bare type name and
    /// the server normalizes the fixed-length ones in the message only: the state of
    /// `char` is that of `varchar`, not a fallback.
    #[test]
    fn error_converting_data_type_states_follow_the_target_in_an_expression() {
        // SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));           -> varchar
        // SELECT CAST(N'0001-01-01T01:59:59+02:00' AS datetimeoffset(7));          -> nvarchar
        // SELECT CAST(CAST('…' AS char(30)) AS datetimeoffset(7));                 -> varchar
        // SELECT CAST(CAST(N'…' AS nchar(30)) AS datetimeoffset(7));               -> nvarchar
        // and the `varchar(max)` / `nvarchar(max)` forms, which print the same two names.
        for from in ["varchar", "nvarchar", "char", "nchar"] {
            let err = SqlError::error_converting_data_type(from, "datetimeoffset");
            assert_eq!(err.state, 31, "state of {from} to datetimeoffset");
            assert_eq!(err.number, 8114);
            assert_eq!(err.severity, 16);
            assert_eq!(
                err.message,
                format!("Data type {from} could not be converted to datetimeoffset.")
            );
        }

        // The other targets, from each source: state 5.
        // SELECT CAST('abc' AS bigint); / CAST('' AS numeric(10,2)); / CAST('abc' AS float);
        // SELECT CONVERT(varbinary(8), '0x1', 1);
        // EXEC sp_executesql N'SELECT @a', N'@a int', @a = 'abc';
        for from in ["varchar", "nvarchar", "char", "nchar"] {
            for to in ["bigint", "numeric", "float", "real", "varbinary", "int"] {
                let err = SqlError::error_converting_data_type(from, to);
                assert_eq!(err.state, 5, "state of {from} to {to}");
                assert_eq!(err.number, 8114);
                assert_eq!(err.severity, 16);
            }
        }
    }

    /// The counter-example to "the target settles the state of 8114": the same pair, the
    /// same message, two states, because the path differs. Seven parameter-bound
    /// variants, each state 5:
    ///
    /// ```text
    /// EXEC sp_executesql N'SELECT @a', N'@a datetimeoffset(7)', @a = '0001-01-01T01:59:59+02:00';
    /// … the same with N'…', with datetimeoffset(0), with datetimeoffset(3),
    /// … with a second parameter before it, with an OUTPUT parameter,
    /// EXEC #p @a = '0001-01-01T01:59:59+02:00';   -- procedure taking a datetimeoffset(7)
    /// ```
    ///
    /// against 31 for `SELECT CAST('0001-01-01T01:59:59+02:00' AS datetimeoffset(7));`,
    /// `CONVERT(datetimeoffset(7), …, 126)`, a `DECLARE` assignment and an `INSERT`. The
    /// control pair `varchar` to `numeric` sends 5 down both paths, so the pair alone does
    /// not explain the split.
    ///
    /// The crate has no constructor for the parameter path and needs none: `vauban-types`
    /// converts inside expressions alone. The test keeps the rule so that the day a caller
    /// binds a parameter, it knows this constructor does not answer for it.
    #[test]
    fn error_converting_data_type_ignores_the_parameter_path() {
        let in_an_expression = SqlError::error_converting_data_type("varchar", "datetimeoffset");
        assert_eq!(in_an_expression.state, 31);

        // The control pair, which sends 5 on both paths.
        let control = SqlError::error_converting_data_type("varchar", "numeric");
        assert_eq!(control.state, 5);

        // The parameter path sends 5 for the very same message; the constructor renders
        // the expression path, so the two differ here — that is the point of the vector.
        let parameter_path_state = 5;
        assert_ne!(in_an_expression.state, parameter_path_state);
        assert_eq!(control.state, parameter_path_state);
    }

    #[test]
    fn conversion_not_allowed_pair() {
        let explicit = SqlError::explicit_conversion_not_allowed("date", "int");
        assert_eq!(explicit.number, 529);
        assert_eq!(
            explicit.message,
            "No explicit conversion exists from date to int."
        );

        let implicit = SqlError::implicit_conversion_not_allowed("date", "int");
        assert_eq!(implicit.number, 257);
        assert_eq!(
            implicit.message,
            "No implicit conversion from date to int; use CONVERT explicitly."
        );

        let clash = SqlError::operand_type_clash("date", "int");
        assert_eq!(clash.number, 206);
        assert_eq!(
            clash.message,
            "Type mismatch: date cannot be combined with int."
        );
    }

    #[test]
    fn datepart_not_supported_is_9810() {
        let err = SqlError::datepart_not_supported("hour", "dateadd", "date");
        assert_eq!(err.number, 9810);
        assert_eq!(
            err.message,
            "Datepart hour cannot be used with the date function dateadd on data type date."
        );
    }

    /// The state of 536 follows the calling function, not the number: `left` and `right`
    /// send 6 where `substring` sends 8 (`SELECT LEFT('abc', -1);`,
    /// `SELECT RIGHT('abc', -1);`, `SELECT SUBSTRING('abc', 1, -1);`). The message is
    /// the same in the three, the function name apart.
    #[test]
    fn invalid_length_parameter_states_follow_the_function() {
        let expected: &[(&str, u8)] = &[("left", 6), ("right", 6), ("substring", 8)];
        for &(function, state) in expected {
            let err = SqlError::invalid_length_parameter(function);
            assert_eq!(err.number, 536, "number for {function}");
            assert_eq!(err.severity, 16, "severity for {function}");
            assert_eq!(err.state, state, "state for {function}");
            assert_eq!(
                err.message,
                format!("The length given to the {function} function is not valid.")
            );
        }

        // The case of the argument is the caller's, not a different code path.
        assert_eq!(SqlError::invalid_length_parameter("LEFT").state, 6);
        assert_eq!(
            SqlError::invalid_length_parameter("LEFT").message,
            "The length given to the LEFT function is not valid."
        );

        // A function without a row keeps the default 1.
        assert_eq!(SqlError::invalid_length_parameter("stuff").state, 1);
    }

    #[test]
    fn function_arity_errors() {
        let exact = SqlError::function_arg_count("LEN", 1);
        assert_eq!(exact.number, 174);
        assert_eq!(exact.severity, 15);
        assert_eq!(
            exact.message,
            "The function LEN takes exactly 1 argument(s)."
        );

        let range = SqlError::function_arg_count_range("ROUND", 2, 3);
        assert_eq!(range.number, 189);
        assert_eq!(range.severity, 15);
        assert_eq!(
            range.message,
            "The function ROUND takes between 2 and 3 arguments."
        );
    }

    #[test]
    fn invalid_argument_type_is_8116() {
        let err = SqlError::invalid_argument_type("uniqueidentifier", 1, "len");
        assert_eq!(err.number, 8116);
        assert_eq!(err.severity, 16);
        assert_eq!(
            err.message,
            "Data type uniqueidentifier is not accepted for argument 1 of the len function."
        );
    }

    #[test]
    fn binder_errors() {
        let unknown = SqlError::not_a_recognized_name("NO_SUCH_FN", "built-in function");
        assert_eq!(unknown.number, 195);
        assert_eq!(unknown.severity, 15);
        assert_eq!(
            unknown.message,
            "'NO_SUCH_FN' is not a known built-in function name."
        );

        let operand = SqlError::invalid_operand_type("uniqueidentifier", "add");
        assert_eq!(operand.number, 8117);
        assert_eq!(
            operand.message,
            "Data type uniqueidentifier is not accepted by the add operator."
        );

        let boolean = SqlError::non_boolean_expression("1");
        assert_eq!(boolean.number, 4145);
        assert_eq!(boolean.severity, 15);
        assert_eq!(
            boolean.message,
            "A condition is expected near '1', but the expression is not boolean."
        );

        let ty = SqlError::cannot_find_data_type(1, "foo");
        assert_eq!(ty.number, 2715);
        assert_eq!(
            ty.message,
            "Column, parameter or variable #1: unknown data type foo."
        );
    }

    #[test]
    fn types_errors() {
        let range = SqlError::out_of_range_conversion("date", "datetime");
        assert_eq!(range.number, 242);
        assert_eq!(range.state, 3);
        assert_eq!(
            range.message,
            "Converting date to datetime produced a value outside the target range."
        );

        let integral = SqlError::overflow_for_data_type("tinyint", 300);
        assert_eq!(integral.number, 220);
        assert_eq!(integral.state, 2);
        assert_eq!(
            integral.message,
            "Value out of range for data type tinyint: 300."
        );

        // `SELECT CAST(1e40 AS real);` prints the value with a C `%f`, not in scientific
        // notation: six decimals and 17 significant digits.
        let floating = SqlError::overflow_for_type("real", 1e40);
        assert_eq!(floating.number, 232);
        assert_eq!(floating.state, 2);
        assert_eq!(
            floating.message,
            "Value out of range for type real: 10000000000000000000000000000000000000000.000000."
        );
        assert!(
            floating
                .message
                .ends_with(": 10000000000000000000000000000000000000000.000000.")
        );

        let collation = SqlError::invalid_collation("Klingon_CI_AS");
        assert_eq!(collation.number, 448);
        assert_eq!(collation.message, "Unknown collation 'Klingon_CI_AS'.");

        let style = SqlError::unsupported_convert_style(112, "date", "datetimeoffset");
        assert_eq!(style.number, 9809);
        assert_eq!(
            style.message,
            "Style 112 is not defined for converting date to datetimeoffset."
        );

        let guid = SqlError::conversion_failed_guid();
        assert_eq!(guid.number, 8169);
        assert_eq!(
            guid.message,
            "The character string could not be converted to uniqueidentifier."
        );
    }

    #[test]
    fn sysfn_errors() {
        let option = SqlError::not_a_recognized_option("bogus", "datepart");
        assert_eq!(option.number, 155);
        assert_eq!(option.severity, 15);
        assert_eq!(option.message, "'bogus' is not a known datepart option.");

        let overflow = SqlError::datetime_overflow("datetime");
        assert_eq!(overflow.number, 517);
        assert_eq!(
            overflow.message,
            "The addition overflowed the 'datetime' column."
        );

        let datediff = SqlError::datediff_overflow();
        assert_eq!(datediff.number, 535);
        assert_eq!(
            datediff.message,
            "datediff overflowed: too many dateparts separate the two instants. Call datediff with a coarser datepart."
        );

        let float_op = SqlError::invalid_floating_point_operation();
        assert_eq!(float_op.number, 3623);
        assert_eq!(
            float_op.message,
            "The floating point operation is undefined."
        );

        let coalesce = SqlError::coalesce_all_null();
        assert_eq!(coalesce.number, 4127);
        assert_eq!(
            coalesce.message,
            "COALESCE needs at least one argument other than the NULL constant."
        );

        let construct = SqlError::cannot_construct_type("datetime");
        assert_eq!(construct.number, 289);
        assert_eq!(
            construct.message,
            "Data type datetime cannot be built from these arguments: at least one value is out of range."
        );

        let truncated = SqlError::string_or_binary_truncated();
        assert_eq!(truncated.number, 8152);
        assert_eq!(truncated.severity, 16);
        assert_eq!(truncated.state, 17);
        assert_eq!(
            truncated.message,
            "Data too long: the string or binary value would be cut."
        );
    }

    /// Error 191, the same for seven nesting shapes and six paths into the parser:
    /// severity 15, the catalogue's, so no override; state 1; a text with no argument.
    ///
    /// The severity is asserted against the two other numbers of the same family: 125 is
    /// severity 15 like 191, 8631 is severity 17. 125 is not catalogued; the assertion
    /// below holds its value for the day it arrives, rather than pinning its absence.
    #[test]
    fn nested_too_deeply_is_191_severity_15_state_1() {
        let err = SqlError::nested_too_deeply(4);
        assert_eq!(err.number, 191);
        assert_eq!(err.severity, 15);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "The statement is nested too deeply; split it into smaller queries."
        );
        assert_eq!(err.line, 4);
        assert_eq!(err.procedure, None);

        // The catalogue carries the published severity unchanged: no `SEVERITY_OVERRIDES`
        // row for 191.
        assert_eq!(message_template(191).map(|def| def.severity), Some(15));
        assert!(!SEVERITY_OVERRIDES.iter().any(|(number, ..)| *number == 191));

        // The other two answers to deep nesting differ in severity as well as in number:
        // 125 severity 15 (an eleventh nested `CASE`) and 8631 severity 17 (a flat chain
        // of 20 000 `+`).
        for (number, severity) in [(125u32, 15u8), (8631, 17)] {
            if let Some(def) = message_template(number) {
                assert_eq!(def.severity, severity, "severity of error {number}");
            }
        }
    }

    /// Error 8631, the same four fields for ten operators, nine statement contexts and six
    /// paths into the compiler. Severity 17 is the catalogue's, so no override; state 1;
    /// a text with no argument.
    #[test]
    fn stack_limit_reached_is_8631_severity_17_state_1() {
        let err = SqlError::stack_limit_reached();
        assert_eq!(err.number, 8631);
        assert_eq!(err.severity, 17);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "Internal error: the server ran out of stack; the query is probably nested too deeply and needs simplifying."
        );
        // The binder puts the line of the *statement* on it further up, so it leaves this
        // constructor without one.
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // The catalogue carries the published severity unchanged: no `SEVERITY_OVERRIDES`
        // row for 8631.
        assert_eq!(message_template(8631).map(|def| def.severity), Some(17));
        assert!(
            !SEVERITY_OVERRIDES
                .iter()
                .any(|(number, ..)| *number == 8631)
        );
    }

    /// The three numbers of the deep-expression family are distinct, and a caller that
    /// confused two of them would send a different severity as well as a different number.
    ///
    /// A flat chain of 20 000 `+` answers 8631 severity 17, 20 000 prefix `-` answers 191
    /// severity 15, and an eleventh nested `CASE` answers 125 severity 15 state 4. 125 is
    /// not catalogued here; the loop holds its severity for the day it arrives.
    #[test]
    fn the_three_numbers_of_the_family_are_not_interchangeable() {
        let flat = SqlError::stack_limit_reached();
        let nested = SqlError::nested_too_deeply(1);
        assert_ne!(flat.number, nested.number);
        assert_ne!(flat.severity, nested.severity, "17 against 15");
        assert_ne!(flat.message, nested.message);
        // Both are at state 1 all the same: the state does not tell them apart.
        assert_eq!(flat.state, nested.state);

        for (number, severity) in [(125u32, 15u8), (191, 15), (8631, 17)] {
            if let Some(def) = message_template(number) {
                assert_eq!(def.severity, severity, "severity of error {number}");
            }
        }
    }

    // ---------------------------------------------------------------------------------
    // The DDL errors; the query that raises each line is in the comment next to it.
    // ---------------------------------------------------------------------------------

    /// Error 1801, as `CREATE DATABASE master;` printed it: severity 16, state 3.
    #[test]
    fn database_already_exists_1801() {
        let err = SqlError::database_already_exists("d");
        assert_eq!(err.number, 1801);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 3);
        assert_eq!(
            err.message,
            "A database named 'd' exists already; pick another name."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
        // The catalogue publishes the severity the wire carried: no override row.
        assert_eq!(message_template(1801).map(|def| def.severity), Some(16));
        assert!(
            !SEVERITY_OVERRIDES
                .iter()
                .any(|(number, ..)| *number == 1801)
        );
    }

    /// Error 2714, as a second `CREATE TABLE t (a int);` printed it: severity 16, state 6.
    #[test]
    fn object_already_exists_2714() {
        let err = SqlError::object_already_exists("t");
        assert_eq!(err.number, 2714);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 6);
        assert_eq!(
            err.message,
            "An object named 't' exists already in the database."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
        // CREATE TABLE dbo.t2 (a int); twice prints the name without its schema.
        assert_eq!(
            SqlError::object_already_exists("t2").message,
            "An object named 't2' exists already in the database."
        );
        assert_eq!(message_template(2714).map(|def| def.severity), Some(16));
        assert!(
            !SEVERITY_OVERRIDES
                .iter()
                .any(|(number, ..)| *number == 2714)
        );
    }

    /// Error 3701: the two `%S_MSG` carry the action and the kind, the template stays the
    /// catalogue's, and the state follows the kind.
    #[test]
    fn cannot_drop_3701() {
        let expected: &[(&str, u8, &str, &str)] = &[
            // DROP TABLE nope;
            (
                "table",
                5,
                "nope",
                "Unable to drop the table 'nope': it does not exist or is not accessible.",
            ),
            // DROP DATABASE nope_db;
            (
                "database",
                1,
                "nope_db",
                "Unable to drop the database 'nope_db': it does not exist or is not accessible.",
            ),
            // DROP INDEX ix_nope ON t3; — the name joins the table and the index.
            (
                "index",
                7,
                "t3.ix_nope",
                "Unable to drop the index 't3.ix_nope': it does not exist or is not accessible.",
            ),
            // DROP VIEW nope_v;
            (
                "view",
                5,
                "nope_v",
                "Unable to drop the view 'nope_v': it does not exist or is not accessible.",
            ),
            // DROP PROCEDURE nope_p;
            (
                "procedure",
                5,
                "nope_p",
                "Unable to drop the procedure 'nope_p': it does not exist or is not accessible.",
            ),
            // DROP FUNCTION nope_f;
            (
                "function",
                5,
                "nope_f",
                "Unable to drop the function 'nope_f': it does not exist or is not accessible.",
            ),
            // DROP TRIGGER nope_tr;
            (
                "trigger",
                5,
                "nope_tr",
                "Unable to drop the trigger 'nope_tr': it does not exist or is not accessible.",
            ),
            // DROP SEQUENCE nope_sequence;
            (
                "sequence",
                5,
                "nope_sequence",
                "Unable to drop the sequence 'nope_sequence': it does not exist or is not accessible.",
            ),
            // DROP SEQUENCE dbo.nope_sequence; — the qualifier stays in the name.
            (
                "sequence",
                5,
                "dbo.nope_sequence",
                "Unable to drop the sequence 'dbo.nope_sequence': it does not exist or is not accessible.",
            ),
            // DROP SYNONYM nope_synonym;
            (
                "synonym",
                5,
                "nope_synonym",
                "Unable to drop the synonym 'nope_synonym': it does not exist or is not accessible.",
            ),
            // DROP SYNONYM dbo.nope_synonym;
            (
                "synonym",
                5,
                "dbo.nope_synonym",
                "Unable to drop the synonym 'dbo.nope_synonym': it does not exist or is not accessible.",
            ),
        ];
        for (kind, state, name, message) in expected {
            let err = SqlError::cannot_drop("drop", kind, name);
            assert_eq!(err.number, 3701);
            assert_eq!(err.severity, 11, "severity of the {kind} filling");
            assert_eq!(err.state, *state, "state of the {kind} filling");
            assert_eq!(&err.message, message, "message of the {kind} filling");
        }

        // The template holds two `%S_MSG` then the `%.*ls`, in that order.
        assert_eq!(
            message_template(3701).map(|def| def.template),
            Some("Unable to %S_MSG the %S_MSG '%.*ls': it does not exist or is not accessible.")
        );
        // A kind without a row falls back to the catalogue default; the fallback is read
        // on `assembly`, which has no row, against `sequence`, which has one.
        assert_eq!(
            SqlError::cannot_drop("drop", "assembly", "a").state,
            super::UNKNOWN_DDL_STATE
        );
        assert_ne!(
            SqlError::cannot_drop("drop", "sequence", "nope_sequence").state,
            super::UNKNOWN_DDL_STATE
        );
    }

    /// Error 226, as `BEGIN TRANSACTION; CREATE DATABASE d;` printed it: severity 16,
    /// state 5, and the statement name in the `%ls`.
    #[test]
    fn statement_not_allowed_in_transaction_226() {
        let err = SqlError::statement_not_allowed_in_transaction("CREATE DATABASE");
        assert_eq!(err.number, 226);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 5);
        assert_eq!(
            err.message,
            "CREATE DATABASE is not allowed inside a multi-statement transaction."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // BEGIN TRANSACTION; ALTER DATABASE scratch_db SET RECOVERY SIMPLE;
        let alter = SqlError::statement_not_allowed_in_transaction("ALTER DATABASE");
        assert_eq!(alter.state, 6);
        assert_eq!(
            alter.message,
            "ALTER DATABASE is not allowed inside a multi-statement transaction."
        );
        // BEGIN TRANSACTION; ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP = 1;
        let scoped =
            SqlError::statement_not_allowed_in_transaction("ALTER DATABASE SCOPED CONFIGURATION");
        assert_eq!(scoped.state, 7);
        assert_eq!(
            scoped.message,
            "ALTER DATABASE SCOPED CONFIGURATION is not allowed inside a multi-statement transaction."
        );

        // BEGIN TRANSACTION; DROP DATABASE d; answers 574, not 226, and its sentence is
        // not the one of 226.
        let inside = message_template(574).expect("574 is in the catalog");
        assert_ne!(
            inside.template,
            message_template(226).map_or("", |d| d.template)
        );
    }

    /// Error 209, as `SELECT c FROM (SELECT 1 AS c) AS x CROSS JOIN (SELECT 2 AS c) AS y;`
    /// printed it: severity 16, state 1.
    #[test]
    fn ambiguous_column_name_209() {
        let err = SqlError::ambiguous_column_name("a");
        assert_eq!(err.number, 209);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(err.message, "Column name 'a' is ambiguous.");
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);
        // The same query over `[c d]` prints the name without its delimiters.
        assert_eq!(
            SqlError::ambiguous_column_name("c d").message,
            "Column name 'c d' is ambiguous."
        );
    }

    /// Error 3708 for `DROP DATABASE master;`: severity 16, state 4, and the three
    /// `%S_MSG` filled `drop`, `database` and `database`.
    #[test]
    fn cannot_drop_system_database_3708() {
        let err = SqlError::cannot_drop_system_database("master");
        assert_eq!(err.number, 3708);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 4);
        assert_eq!(
            err.message,
            "Unable to drop the database 'master': it is a system database."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // The three other system databases and the two case spellings. The delimited
        // forms print `master` and `msdb` without their brackets, and a list prints
        // `master`.
        for name in ["tempdb", "model", "msdb", "MASTER", "MaStEr"] {
            let other = SqlError::cannot_drop_system_database(name);
            assert_eq!(other.number, 3708);
            assert_eq!(other.severity, 16);
            assert_eq!(other.state, 4, "state of the {name} batch");
            assert_eq!(
                other.message,
                format!("Unable to drop the database '{name}': it is a system database.")
            );
        }

        // The sentence is not the one 3701 prints for a missing database, and neither is
        // the number.
        let missing = SqlError::cannot_drop("drop", "database", "nope_db");
        assert_eq!(missing.number, 3701);
        assert!(err.message.ends_with(": it is a system database."));
        assert!(
            missing
                .message
                .ends_with(": it does not exist or is not accessible.")
        );
    }

    /// Error 574 for `BEGIN TRANSACTION; DROP DATABASE d;`: severity 16, state 0,
    /// `DROP DATABASE` in the `%ls`.
    #[test]
    fn drop_database_in_transaction_574() {
        let err = SqlError::drop_database_in_transaction();
        assert_eq!(err.number, 574);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 0);
        assert_eq!(
            err.message,
            "DROP DATABASE is not allowed inside a user transaction."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // The same four fields over `master`: on that path the server answers 574 and not
        // the 3708 of `DROP DATABASE master;` alone.
        assert_ne!(
            err.number,
            SqlError::cannot_drop_system_database("master").number
        );

        // 226 is another sentence for another statement.
        let create = SqlError::statement_not_allowed_in_transaction("CREATE DATABASE");
        assert_eq!(create.number, 226);
        assert_ne!(create.state, err.state);
        assert!(err.message.contains("inside a user transaction."));
        assert!(
            create
                .message
                .contains("inside a multi-statement transaction.")
        );
    }

    /// Error 2705 for `CREATE TABLE t_dup (a int, a int);`: severity 16, state 3, the
    /// column name then the table name.
    #[test]
    fn duplicate_column_name_2705() {
        let err = SqlError::duplicate_column_name("a", "t_dup");
        assert_eq!(err.number, 2705);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 3);
        assert_eq!(
            err.message,
            "Column 'a' of table 't_dup' is defined twice; column names have to be unique."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        // Four other shapes at state 3: the later spelling of the column
        // (`a int, A int`), the delimited table `[t dup]`, the temporary table `#t_dup`
        // and the table variable `DECLARE @t TABLE (a int, a int);`.
        for (column, table) in [("A", "t_dup"), ("a", "t dup"), ("a", "#t_dup"), ("a", "@t")] {
            let other = SqlError::duplicate_column_name(column, table);
            assert_eq!(other.number, 2705);
            assert_eq!(other.severity, 16);
            assert_eq!(other.state, 3, "state of the {table} batch");
            assert_eq!(
                other.message,
                format!(
                    "Column '{column}' of table '{table}' is defined twice; column names have to be unique."
                )
            );
        }

        // The arguments are not interchangeable: swapping them swaps the two names.
        assert!(
            SqlError::duplicate_column_name("t_dup", "a")
                .message
                .contains("Column 't_dup' of table 'a'")
        );
    }

    /// Error 8148 for `CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2);`:
    /// severity 16, state 0, the column then the table as written.
    #[test]
    fn multiple_column_defaults_8148() {
        let err = SqlError::multiple_column_defaults("a", "dbo.t");
        assert_eq!(err.number, 8148);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 0);
        assert_eq!(
            err.message,
            "A second DEFAULT constraint is refused for column 'a' of table 'dbo.t'."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        let quoted = SqlError::multiple_column_defaults("A col", "dbo.T Q");
        assert_eq!(
            quoted.message,
            "A second DEFAULT constraint is refused for column 'A col' of table 'dbo.T Q'."
        );
        let unqualified = SqlError::multiple_column_defaults("a", "t");
        assert_eq!(
            unqualified.message,
            "A second DEFAULT constraint is refused for column 'a' of table 't'."
        );
    }

    /// Error 1754 for `CREATE TABLE dbo.t (a int IDENTITY DEFAULT 1);`: severity 16,
    /// state 0, the table without its schema then the column.
    #[test]
    fn default_on_identity_column_1754() {
        let err = SqlError::default_on_identity_column("t", "a");
        assert_eq!(err.number, 1754);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 0);
        assert_eq!(
            err.message,
            "A DEFAULT cannot sit on an IDENTITY column. Table 't', column 'a'."
        );
        assert_eq!(err.line, 0);
        assert_eq!(err.procedure, None);

        let quoted = SqlError::default_on_identity_column("T I", "A col");
        assert_eq!(
            quoted.message,
            "A DEFAULT cannot sit on an IDENTITY column. Table 'T I', column 'A col'."
        );
    }

    /// Error 2715 has two states for one sentence: 3 for a scalar declaration and a
    /// routine parameter, 6 for the column list of a table.
    #[test]
    fn cannot_find_data_type_in_table_2715_state_6() {
        // CREATE TABLE t_unknown (a foo);
        let column = SqlError::cannot_find_data_type_in_table(1, "foo");
        assert_eq!(column.number, 2715);
        assert_eq!(column.severity, 16);
        assert_eq!(column.state, 6);
        assert_eq!(
            column.message,
            "Column, parameter or variable #1: unknown data type foo."
        );
        assert_eq!(column.line, 0);
        assert_eq!(column.procedure, None);

        // CREATE TABLE t_unknown (a int, b foo); prints the rank of the column.
        assert_eq!(
            SqlError::cannot_find_data_type_in_table(2, "foo").message,
            "Column, parameter or variable #2: unknown data type foo."
        );
        // CREATE TABLE t_unknown (a dbo.foo); keeps the qualifier.
        assert_eq!(
            SqlError::cannot_find_data_type_in_table(1, "dbo.foo").message,
            "Column, parameter or variable #1: unknown data type dbo.foo."
        );

        // DECLARE @x foo; and the CREATE PROCEDURE parameter stay at 3: same number, same
        // sentence, another state, so the two constructors are not interchangeable.
        let declaration = SqlError::cannot_find_data_type(1, "foo");
        assert_eq!(declaration.state, 3);
        assert_eq!(declaration.message, column.message);
        assert_ne!(declaration.state, column.state);
    }

    /// The DDL constructors carry their own states, not the catalogue default.
    ///
    /// Counter-check: the fourteen calls below span seven distinct states and the ordered
    /// vector holds each call, so resetting one of them to [`UNKNOWN_DDL_STATE`], or
    /// giving 2715 one state for its two contexts, changes a vector and reddens this test.
    #[test]
    fn the_ddl_states_are_not_the_default() {
        let mut states = vec![
            // CREATE DATABASE master; tempdb; model; [master]; a user database twice.
            SqlError::database_already_exists("d").state,
            // CREATE TABLE t (a int); twice, and the same over an existing view.
            SqlError::object_already_exists("t").state,
            // DROP TABLE nope;
            SqlError::cannot_drop("drop", "table", "nope").state,
            // DROP DATABASE nope_db;
            SqlError::cannot_drop("drop", "database", "nope").state,
            // DROP INDEX ix_nope ON t3;
            SqlError::cannot_drop("drop", "index", "t3.ix_nope").state,
            // BEGIN TRANSACTION; CREATE DATABASE scratch_db;
            SqlError::statement_not_allowed_in_transaction("CREATE DATABASE").state,
            // SELECT c FROM (SELECT 1 AS c) AS x CROSS JOIN (SELECT 2 AS c) AS y; and
            // the five other shapes tried, which sent state 1 too.
            SqlError::ambiguous_column_name("c").state,
            // DROP SEQUENCE nope_sequence; and DROP SYNONYM nope_synonym;
            SqlError::cannot_drop("drop", "sequence", "nope_sequence").state,
            SqlError::cannot_drop("drop", "synonym", "nope_synonym").state,
            // DROP DATABASE master; tempdb; model; msdb; and the five other spellings.
            SqlError::cannot_drop_system_database("master").state,
            // BEGIN TRANSACTION; DROP DATABASE scratch_db; and the five other shapes.
            SqlError::drop_database_in_transaction().state,
            // CREATE TABLE t_dup (a int, a int); and the eight other shapes.
            SqlError::duplicate_column_name("a", "t_dup").state,
            // CREATE TABLE t_unknown (a foo); against DECLARE @x foo;, which sends 3.
            SqlError::cannot_find_data_type_in_table(1, "foo").state,
            SqlError::cannot_find_data_type(1, "foo").state,
        ];
        assert_eq!(states, vec![3, 6, 5, 1, 7, 5, 1, 5, 5, 4, 0, 3, 6, 3]);
        states.sort_unstable();
        states.dedup();
        assert_eq!(states, vec![0, 1, 3, 4, 5, 6, 7]);
    }

    /// Error 271 quotes the column between double quotes, where 8102 uses single ones
    /// (`UPDATE dbo.tc SET c = 1;` over a computed column `c`).
    #[test]
    fn computed_column_271_is_rendered() {
        let error = SqlError::cannot_update_computed_column("c");
        assert_eq!((error.number, error.severity, error.state), (271, 16, 1));
        assert_eq!(
            error.message,
            "The column \"c\" cannot be modified: it is computed, or it comes out of a UNION."
        );
        assert_eq!(error.line, 0);
        let identity = SqlError::cannot_update_identity_column("id");
        assert_eq!(
            (identity.number, identity.severity, identity.state),
            (8102, 16, 1)
        );
        assert_eq!(identity.message, "Identity column 'id' cannot be updated.");
    }

    /// The DML, flow control, transaction and concurrency constructors, on four fields:
    /// number, severity on the wire, state, and the substituted message compared as a
    /// whole string. Each row is built with the arguments of the query quoted next to it,
    /// so the comparison leaves no specifier behind. Thirty rows for twenty-nine
    /// numbers: 1222 has one constructor per lock granularity.
    #[test]
    fn dml_and_transaction_messages_are_rendered() {
        let expected: &[(SqlError, u32, u8, u8, &str)] = &[
            (
                // SELECT 1 WHERE 1 = (SELECT a, b FROM dbo.t2);
                SqlError::only_one_expression_in_subquery(),
                116,
                16,
                1,
                "A subquery not introduced by EXISTS must select a single expression.",
            ),
            (
                // DECLARE @x int; SELECT @x = a, b FROM dbo.t2;
                SqlError::assignment_mixed_with_data_retrieval(),
                141,
                15,
                1,
                "A SELECT that assigns variables cannot also return rows.",
            ),
            (
                // SELECT DISTINCT a FROM dbo.t2 ORDER BY b;
                SqlError::order_by_item_not_in_distinct_select_list(),
                145,
                15,
                1,
                "With SELECT DISTINCT, each ORDER BY item has to be in the select list.",
            ),
            (
                // SELECT a FROM dbo.t2 UNION SELECT a, b FROM dbo.t2;
                SqlError::set_operator_column_count_mismatch(),
                205,
                16,
                1,
                "Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions.",
            ),
            (
                // EXEC('CREATE PROCEDURE dbo.p266 AS BEGIN TRANSACTION;'); EXEC dbo.p266;
                SqlError::transaction_count_after_execute(0, 1),
                266,
                16,
                2,
                "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before 0, after 1).",
            ),
            (
                // SELECT (SELECT a FROM dbo.tv); over two rows
                SqlError::subquery_returned_more_than_one_value(),
                512,
                16,
                1,
                "The subquery returned several values, which is not allowed after a comparison operator or as an expression.",
            ),
            (
                // SELECT * FROM dbo.lone WITH (NOLOCK, TABLOCKX);
                SqlError::conflicting_locking_hints(),
                1047,
                15,
                1,
                "The locking hints contradict each other.",
            ),
            (
                // UPDATE dbo.lone WITH (NOLOCK) SET a = 1;
                SqlError::nolock_not_allowed_on_target(),
                1065,
                15,
                1,
                "NOLOCK and READUNCOMMITTED cannot be applied to the target table of INSERT, UPDATE, DELETE or MERGE.",
            ),
            (
                // ALTER TABLE dbo.lone ADD CONSTRAINT fk_bad FOREIGN KEY (nosuch) REFERENCES dbo.parent(id);
                SqlError::foreign_key_references_invalid_column("fk_bad", "nosuch", "lone"),
                1769,
                16,
                1,
                "Foreign key 'fk_bad' names the column 'nosuch', which the referencing table 'lone' does not have.",
            ),
            (
                // ALTER TABLE dbo.lone ADD CONSTRAINT fk_nopk FOREIGN KEY (a) REFERENCES dbo.nopk(id);
                SqlError::no_matching_key_in_referenced_table("dbo.nopk", "fk_nopk"),
                1776,
                16,
                0,
                "The referenced table 'dbo.nopk' has no primary or unique key matching the columns of foreign key 'fk_nopk'.",
            ),
            (
                // DROP TABLE dbo.parent; while another table references it
                SqlError::cannot_drop_referenced_object("dbo.parent"),
                3726,
                16,
                1,
                "Object 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be dropped.",
            ),
            (
                // SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRANSACTION; SELECT v FROM dbo.s WHERE k = 1; in snapdb
                SqlError::snapshot_isolation_not_allowed("snapdb"),
                3952,
                16,
                1,
                "Database 'snapdb' does not allow snapshot isolation; enable it with ALTER DATABASE.",
            ),
            (
                // TRUNCATE TABLE dbo.parent; while another table references it
                SqlError::cannot_truncate_referenced_table("dbo.parent"),
                4712,
                16,
                1,
                "Table 'dbo.parent' is referenced by a FOREIGN KEY constraint and cannot be truncated.",
            ),
            (
                // ALTER TABLE dbo.notempty ADD b int NOT NULL; on a table holding one row
                SqlError::cannot_add_column_to_non_empty_table("b", "notempty"),
                4901,
                16,
                1,
                "A column added to a non-empty table has to be nullable, have a DEFAULT, or be an identity or timestamp column. Column 'b' cannot be added to table 'notempty'.",
            ),
            (
                // INSERT INTO dbo.ident VALUES (5, 1); on a table whose first column is an IDENTITY
                SqlError::identity_insert_requires_column_list("dbo.ident"),
                8101,
                16,
                1,
                "An explicit value for the identity column of table 'dbo.ident' requires a column list and IDENTITY_INSERT ON.",
            ),
            (
                // UPDATE dbo.ident SET id = 2;
                SqlError::cannot_update_identity_column("id"),
                8102,
                16,
                1,
                "Identity column 'id' cannot be updated.",
            ),
            (
                // SELECT a FROM dbo.t2 GROUP BY a HAVING b > 1;
                SqlError::column_invalid_in_having("dbo.t2", "b"),
                8121,
                16,
                1,
                "Column 'dbo.t2.b' of the HAVING clause is neither aggregated nor part of GROUP BY.",
            ),
            (
                // SELECT SUM(COUNT(*)) FROM dbo.t2;
                SqlError::nested_aggregate(),
                130,
                15,
                1,
                "An aggregate function cannot be applied to an expression that holds an aggregate or a subquery.",
            ),
            (
                // SELECT a FROM dbo.t2 GROUP BY SUM(b);
                SqlError::aggregate_in_group_by(),
                144,
                15,
                1,
                "A GROUP BY expression cannot hold an aggregate or a subquery.",
            ),
            (
                // SELECT a FROM dbo.t2 WHERE COUNT(*) > 1;
                SqlError::aggregate_in_where(),
                147,
                15,
                1,
                "An aggregate cannot appear in a WHERE clause, except inside a subquery of a HAVING clause or a select list, aggregating an outer reference.",
            ),
            (
                // INSERT INTO dbo.t2 VALUES (1); on a two-column table
                SqlError::column_count_does_not_match_table(),
                213,
                16,
                1,
                "The supplied values do not match the columns of the table.",
            ),
            (
                // INSERT INTO dbo.ident (id, v) VALUES (1, 1); with IDENTITY_INSERT off
                SqlError::identity_insert_is_off("ident"),
                544,
                16,
                1,
                "An explicit value for the identity column of table 'ident' requires IDENTITY_INSERT ON.",
            ),
            (
                // SET IDENTITY_INSERT dbo.t1 ON; SET IDENTITY_INSERT dbo.t2 ON;
                SqlError::identity_insert_already_on("master", "dbo", "t1", "dbo.t2"),
                8107,
                16,
                1,
                "A session holds IDENTITY_INSERT for one table at a time; 'master.dbo.t1' already has it, and 'dbo.t2' is refused.",
            ),
            (
                // SET IDENTITY_INSERT dbo.plain ON;
                SqlError::identity_insert_table_has_no_identity("dbo.plain"),
                8106,
                16,
                1,
                "Table 'dbo.plain' lacks an IDENTITY column; SET IDENTITY_INSERT cannot run on it.",
            ),
            (
                // SET IDENTITY_INSERT dbo.nosuch ON;
                SqlError::cannot_find_object_for_identity_insert("dbo.nosuch"),
                1088,
                16,
                11,
                "Object \"dbo.nosuch\" was not found: it does not exist or is not accessible.",
            ),
            (
                // two sessions updating the same two rows in opposite order
                SqlError::deadlock_victim(60, "lock"),
                1205,
                13,
                51,
                "Process 60 was chosen as the victim of a deadlock on lock resources; run the transaction again.",
            ),
            (
                // SET LOCK_TIMEOUT 2000; SELECT v FROM dbo.s WHERE k = 1; behind an uncommitted update
                SqlError::lock_request_timeout_on_row(),
                1222,
                16,
                51,
                "The lock could not be acquired before the timeout.",
            ),
            (
                // SET LOCK_TIMEOUT 2000; DROP TABLE dbo.s; behind an uncommitted update
                SqlError::lock_request_timeout_on_object(),
                1222,
                16,
                56,
                "The lock could not be acquired before the timeout.",
            ),
            (
                // COMMIT TRANSACTION; outside a transaction
                SqlError::commit_without_begin(),
                3902,
                16,
                1,
                "COMMIT TRANSACTION without a matching BEGIN TRANSACTION.",
            ),
            (
                // ROLLBACK TRANSACTION; outside a transaction
                SqlError::rollback_without_begin(),
                3903,
                16,
                1,
                "ROLLBACK TRANSACTION without a matching BEGIN TRANSACTION.",
            ),
            (
                // UPDATE dbo.s SET v = v + 100 WHERE k = 1; under snapshot isolation, after another session committed the row
                SqlError::snapshot_update_conflict("dbo.s", "cc"),
                3960,
                16,
                2,
                "Update conflict under snapshot isolation: a row of table 'dbo.s' in database 'cc' was changed by another transaction. Retry or change the isolation level.",
            ),
            (
                // SELECT a, b FROM dbo.t2 GROUP BY a;
                SqlError::column_invalid_in_select_list("dbo.t2", "b"),
                8120,
                16,
                1,
                "Column 'dbo.t2.b' of the select list is neither aggregated nor part of GROUP BY.",
            ),
        ];
        for (err, number, severity, state, message) in expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of error {number}");
            assert_eq!(err.state, *state, "state of error {number}");
            assert_eq!(&err.message, message, "message of error {number}");
            assert!(
                !err.message.contains('%'),
                "error {number} still carries a specifier"
            );
            assert_eq!(err.line, 0, "line of error {number}");
        }
        assert_eq!(expected.len(), 32);
        let mut numbers: Vec<u32> = expected.iter().map(|row| row.1).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), 31);
    }

    /// Fifteen DML numbers and their sixteen constructors, 1222 counting twice, each
    /// sending the catalogue's severity.
    #[test]
    fn dml_constructors_exist_for_the_catalogued_numbers() {
        let calls: &[(SqlError, u32, u8)] = &[
            (SqlError::column_count_does_not_match_table(), 213, 16),
            (SqlError::identity_insert_is_off("ident"), 544, 16),
            (SqlError::deadlock_victim(60, "lock"), 1205, 13),
            (SqlError::lock_request_timeout_on_row(), 1222, 16),
            (SqlError::lock_request_timeout_on_object(), 1222, 16),
            (SqlError::commit_without_begin(), 3902, 16),
            (SqlError::rollback_without_begin(), 3903, 16),
            (SqlError::snapshot_update_conflict("dbo.s", "cc"), 3960, 16),
            (
                SqlError::column_invalid_in_select_list("dbo.t2", "b"),
                8120,
                16,
            ),
            (SqlError::more_columns_than_values(), 109, 15),
            (SqlError::more_values_than_columns(), 110, 15),
            (SqlError::select_list_shorter_than_insert_list(), 120, 15),
            (SqlError::select_list_longer_than_insert_list(), 121, 15),
            (SqlError::column_specified_more_than_once("a"), 264, 16),
            (SqlError::default_or_null_as_identity_value(), 339, 16),
            (SqlError::table_value_constructor_rows_differ(), 10709, 16),
        ];
        for (err, number, severity) in calls {
            assert_eq!(err.number, *number);
            assert_eq!(err.severity, *severity, "severity of error {number}");
            assert!(
                !err.message.contains('%'),
                "error {number} still carries a specifier"
            );
            let def = message_template(*number).expect("catalogued");
            assert_eq!(def.severity, *severity, "published severity of {number}");
        }
        assert_eq!(calls.len(), 16);
        let mut numbers: Vec<u32> = calls.iter().map(|row| row.1).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), 15);
    }

    /// Error 1222 sends two states, so it carries two constructors rather than a state
    /// the caller patches: 51 while waiting for a row lock
    /// (`SELECT v FROM dbo.s WHERE k = 1;` and `UPDATE dbo.s SET v = v + 5 WHERE k = 1;`
    /// behind another session's uncommitted update) and 56 while waiting for an object
    /// lock (`ALTER TABLE dbo.s ADD zz int NULL;`, `DROP TABLE dbo.s;` and a `TABLOCKX`
    /// read). Number, severity and sentence are shared; the state is the axis that moved.
    #[test]
    fn lock_request_timeout_1222_has_one_state_per_granularity() {
        let row = SqlError::lock_request_timeout_on_row();
        let object = SqlError::lock_request_timeout_on_object();
        assert_eq!((row.number, object.number), (1222, 1222));
        assert_eq!((row.severity, object.severity), (16, 16));
        assert_eq!(row.message, object.message);
        assert_eq!(row.state, 51);
        assert_eq!(object.state, 56);
    }

    /// The two `%ld` of 266 keep the order of the template, previous count then current:
    /// a procedure opening two transactions prints `after 2`.
    #[test]
    fn transaction_count_after_execute_prints_previous_then_current() {
        let err = SqlError::transaction_count_after_execute(0, 2);
        assert_eq!(
            err.message,
            "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before 0, after 2)."
        );
        assert_eq!((err.number, err.severity, err.state), (266, 16, 2));
    }

    /// Error 116 travels with severity 16 where the catalogue holds 15: the catalogue
    /// keeps its value and the constructor overrides it, as `SEVERITY_OVERRIDES` records.
    #[test]
    fn only_one_expression_in_subquery_overrides_the_published_severity() {
        let err = SqlError::only_one_expression_in_subquery();
        assert_eq!(err.severity, 16);
        assert_eq!(message_template(116).map(|def| def.severity), Some(15));
        assert!(SEVERITY_OVERRIDES.contains(&(116, 16, 15)));
        for number in [141u32, 145, 205, 266, 512, 1047, 1065, 1769, 1776, 3726] {
            assert!(
                !SEVERITY_OVERRIDES.iter().any(|(n, ..)| *n == number),
                "error {number} takes no severity override"
            );
        }
    }
    /// The DDL and name resolution constructors, on four fields: number, severity on the
    /// wire, state, and the substituted message compared as a whole string. Each row is
    /// built with the arguments of the query quoted next to it. Thirty-three rows for
    /// twenty-eight numbers: 1909 has one constructor per column list, 3723 one per
    /// constraint kind and 103 one per shape of token; 208 and 447 each take a second
    /// constructor for another context.
    #[test]
    fn ddl_and_name_resolution_messages_are_rendered() {
        let identifier = "b".repeat(128);
        let digits = "9".repeat(128);
        let money = format!("${}", "1".repeat(127));
        let expected: Vec<(SqlError, u32, u8, u8, String)> = vec![
            (
                // CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY, b int NOT NULL PRIMARY KEY);
                SqlError::multiple_primary_keys("dbo.t"),
                8110,
                16,
                0,
                "Table 'dbo.t' can have a single PRIMARY KEY constraint.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int NULL, CONSTRAINT pk_t PRIMARY KEY (a));
                SqlError::primary_key_on_nullable_column("t"),
                8111,
                16,
                1,
                "A PRIMARY KEY of table 't' cannot include a nullable column."
                    .to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int NOT NULL PRIMARY KEY CLUSTERED, b int NOT NULL UNIQUE CLUSTERED);
                SqlError::multiple_clustered_index_constraints("dbo.t"),
                8112,
                16,
                0,
                "Constraints of table 'dbo.t' can create a single clustered index."
                    .to_owned(),
            ),
            (
                // CREATE CLUSTERED INDEX ix_b ON dbo.t (b); after CREATE CLUSTERED INDEX ix_a ON dbo.t (a);
                SqlError::more_than_one_clustered_index("dbo.t", "ix_a"),
                1902,
                16,
                3,
                "The table 'dbo.t' can have a single clustered index; drop 'ix_a' first.".to_owned(),
            ),
            (
                // CREATE INDEX ix_t ON dbo.t (nosuchcolumn);
                SqlError::column_does_not_exist_in_target("nosuchcolumn"),
                1911,
                16,
                1,
                "The target table or view has no column named 'nosuchcolumn'.".to_owned(),
            ),
            (
                // CREATE INDEX ix_t ON dbo.t (b); after the same name was created on (a)
                SqlError::index_name_already_exists("ix_t", "dbo.t"),
                1913,
                16,
                1,
                "An index or statistics named 'ix_t' exists already on table 'dbo.t'.".to_owned(),
            ),
            (
                // CREATE INDEX ix_t ON dbo.t (a, a);
                SqlError::duplicate_column_in_index("a"),
                1909,
                16,
                1,
                "The index cannot repeat a column: 'a' is listed twice."
                    .to_owned(),
            ),
            (
                // CREATE INDEX ix_t ON dbo.t (a) INCLUDE (a);
                SqlError::duplicate_column_in_included_columns("a"),
                1909,
                16,
                2,
                "The index cannot repeat a column: 'a' is listed twice."
                    .to_owned(),
            ),
            (
                // after 8111 and after 1909 (`COULD_NOT_CREATE_CONSTRAINT_1750_SEVERITY`)
                SqlError::could_not_create_constraint_or_index(),
                1750,
                16,
                0,
                "The constraint or index was not created; the previous errors say why.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int IDENTITY DEFAULT 1);
                SqlError::default_on_identity_column("t", "a"),
                1754,
                16,
                0,
                "A DEFAULT cannot sit on an IDENTITY column. Table 't', column 'a'.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int NOT NULL CONSTRAINT c1 PRIMARY KEY, b int NOT NULL CONSTRAINT c1 UNIQUE);
                SqlError::duplicate_name_in_this_context("c1"),
                8168,
                16,
                0,
                "The name 'c1' is used more than once for a constraint, column, index or trigger here; names have to be unique.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t2 (a int NOT NULL INDEX ix_t (a), b int NOT NULL, INDEX ix_t (b));
                SqlError::duplicate_index_name_in_this_context("ix_t"),
                8168,
                16,
                1,
                "The name 'ix_t' is used more than once for a constraint, column, index or trigger here; names have to be unique.".to_owned(),
            ),
            (
                // DROP INDEX pk_t ON dbo.t; over the index of a PRIMARY KEY
                SqlError::cannot_drop_constraint_index("dbo.t.pk_t", "PRIMARY KEY"),
                3723,
                16,
                4,
                "Index 'dbo.t.pk_t' enforces a PRIMARY KEY constraint and cannot be dropped directly.".to_owned(),
            ),
            (
                // DROP INDEX uq_t ON dbo.t; over the index of a UNIQUE constraint
                SqlError::cannot_drop_constraint_index("dbo.t.uq_t", "UNIQUE KEY"),
                3723,
                16,
                5,
                "Index 'dbo.t.uq_t' enforces a UNIQUE KEY constraint and cannot be dropped directly.".to_owned(),
            ),
            (
                // ALTER TABLE dbo.t DROP CONSTRAINT nosuch;
                SqlError::constraint_not_on_table("nosuch"),
                3728,
                16,
                1,
                "'nosuch' is not the name of a constraint here.".to_owned(),
            ),
            (
                // ALTER TABLE dbo.nosuch ADD c int NULL;
                SqlError::cannot_find_object_to_alter_table("dbo.nosuch"),
                4902,
                16,
                1,
                "Object \"dbo.nosuch\" was not found: it does not exist or this login lacks permissions.".to_owned(),
            ),
            (
                // ALTER TABLE dbo.t DROP COLUMN nosuch;
                SqlError::alter_table_drop_column_missing("nosuch", "t"),
                4924,
                16,
                1,
                "ALTER TABLE DROP COLUMN could not run: column 'nosuch' is missing from table 't'.".to_owned(),
            ),
            (
                // CREATE INDEX ix ON dbo.ix_t(b); ALTER TABLE dbo.ix_t DROP COLUMN b;
                SqlError::object_depends_on_column("index", "ix", "b"),
                5074,
                16,
                1,
                "index 'ix' still depends on column 'b'.".to_owned(),
            ),
            (
                // CREATE INDEX ix_v ON dbo.v (a); over a view created without WITH SCHEMABINDING
                SqlError::cannot_create_index_on_view("v"),
                1939,
                16,
                1,
                "The index requires the view 'v' to be schema bound."
                    .to_owned(),
            ),
            (
                // CREATE INDEX ix_t ON dbo.nosuch (a);
                SqlError::cannot_find_object_to_index("dbo.nosuch"),
                1088,
                16,
                12,
                "Object \"dbo.nosuch\" was not found: it does not exist or is not accessible.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a decimal(9,2) IDENTITY(1,1) NOT NULL);
                SqlError::invalid_identity_column_type("a"),
                2749,
                16,
                2,
                "Identity column 'a' has to be a non-nullable int, bigint, smallint, tinyint, or decimal or numeric of scale 0.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int IDENTITY(1,1) NULL);
                SqlError::identity_on_nullable_column("a", "dbo.t"),
                8147,
                16,
                1,
                "Column 'a' of table 'dbo.t' is nullable and cannot be an IDENTITY column."
                    .to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int DEFAULT 1 CONSTRAINT c DEFAULT 2);
                SqlError::multiple_column_defaults("a", "dbo.t"),
                8148,
                16,
                0,
                "A second DEFAULT constraint is refused for column 'a' of table 'dbo.t'."
                    .to_owned(),
            ),
            (
                // SELECT 1 FROM sys.sp_executesql;
                SqlError::invalid_object_name_of_another_type("sys.sp_executesql"),
                208,
                16,
                3,
                "Unknown object name 'sys.sp_executesql'.".to_owned(),
            ),
            (
                // CREATE TABLE dbo.t (a int COLLATE Latin1_General_CI_AS NOT NULL);
                SqlError::collate_on_non_string_column("int"),
                447,
                16,
                1,
                "COLLATE cannot apply to an expression of type int.".to_owned(),
            ),
            (
                // SELECT 1 FROM @tv;
                SqlError::must_declare_table_variable("@tv"),
                1087,
                15,
                2,
                "The table variable \"@tv\" is not declared.".to_owned(),
            ),
            (
                // SELECT 1 FROM nosuchserver.somedb.dbo.t;
                SqlError::linked_server_not_found("nosuchserver"),
                7202,
                11,
                2,
                "Server 'nosuchserver' is not registered in sys.servers; check the name or add it with sp_addlinkedserver.".to_owned(),
            ),
            (
                // SELECT 1 FROM dbo.t AS x, dbo.t AS x;
                SqlError::duplicate_correlation_name("x"),
                1011,
                16,
                1,
                "Correlation name 'x' is used more than once in the FROM clause.".to_owned(),
            ),
            (
                // SELECT 1 FROM dbo.t AS u JOIN dbo.u ON 1 = 1;
                SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
                1012,
                16,
                1,
                "Correlation name 'u' is also the exposed name of table 'dbo.u'; give one of them another alias.".to_owned(),
            ),
            (
                // SELECT 1 FROM dbo.t, dbo.t;
                SqlError::same_exposed_names("dbo.t", "dbo.t"),
                1013,
                16,
                1,
                "\"dbo.t\" and \"dbo.t\" expose the same name in the FROM clause; give them distinct aliases.".to_owned(),
            ),
            (
                // UPDATE t SET a = 1 FROM dbo.t AS x, dbo.t AS y;
                SqlError::table_is_ambiguous("t"),
                8154,
                16,
                1,
                "The reference to table 't' is ambiguous.".to_owned(),
            ),
            (
                // SELECT * FROM (SELECT a, a + 1 FROM dbo.t) AS d;
                SqlError::no_column_name_in_derived_table(2, "d"),
                8155,
                16,
                2,
                "Column 2 of 'd' has no name.".to_owned(),
            ),
            (
                // SELECT 1 AS [];
                SqlError::object_or_column_name_missing(),
                1038,
                15,
                4,
                "An object or column name is empty: each SELECT INTO column needs a name, and an alias written \"\" or [] is not allowed.".to_owned(),
            ),
            (
                // SELECT 1 AS "<129 b> left unclosed
                SqlError::identifier_too_long(&identifier, 128),
                103,
                15,
                4,
                format!("The identifier beginning with '{identifier}' exceeds the maximum length of 128."),
            ),
            (
                // SELECT 1 x <200 digits>;
                SqlError::number_too_long(&digits, 128),
                103,
                15,
                5,
                format!("The number beginning with '{digits}' exceeds the maximum length of 128."),
            ),
            (
                // SELECT 1 x $<130 digits>;
                SqlError::money_literal_too_long(&money, 128),
                103,
                15,
                6,
                format!("The number beginning with '{money}' exceeds the maximum length of 128."),
            ),
            (
                // CREATE TABLE nosuchdb.dbo.t (a int NOT NULL);
                SqlError::database_does_not_exist("nosuchdb"),
                2702,
                16,
                2,
                "No database named 'nosuchdb' exists.".to_owned(),
            ),
            (
                // TRUNCATE TABLE dbo.nosuch;
                SqlError::cannot_find_object_to_truncate("nosuch"),
                4701,
                16,
                1,
                "Object \"nosuch\" was not found: it does not exist or is not accessible.".to_owned(),
            ),
            (
                // a login asking for the database nosuchdb
                SqlError::cannot_open_login_database("nosuchdb", "master"),
                4063,
                11,
                1,
                "The database \"nosuchdb\" requested at login could not be opened; the default database \"master\" is used instead.".to_owned(),
            ),
        ];
        for (err, number, severity, state, message) in &expected {
            assert_eq!(err.number, *number, "number of {message}");
            assert_eq!(err.severity, *severity, "severity of error {number}");
            assert_eq!(err.state, *state, "state of error {number}");
            assert_eq!(&err.message, message, "message of error {number}");
            assert!(
                !err.message.contains('%'),
                "error {number} still carries a specifier"
            );
            assert_eq!(err.line, 0, "line of error {number}");
        }
        assert_eq!(expected.len(), 39);
        let mut numbers: Vec<u32> = expected.iter().map(|row| row.1).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), 34);
    }

    /// Error 1909 sends two states, so it carries two constructors rather than a state
    /// the caller patches: 1 when the repeated column is in the key
    /// list (`CREATE INDEX ix_t ON dbo.t (a, a);`, a `PRIMARY KEY (a, a)`) and
    /// 2 when it is in the `INCLUDE` list (`… (a) INCLUDE (a);` and `… (a) INCLUDE
    /// (b, b);`). Number, severity and sentence are shared; the state is the axis that
    /// moved.
    #[test]
    fn duplicate_column_1909_has_one_state_per_column_list() {
        let key = SqlError::duplicate_column_in_index("a");
        let included = SqlError::duplicate_column_in_included_columns("a");
        assert_eq!((key.number, included.number), (1909, 1909));
        assert_eq!((key.severity, included.severity), (16, 16));
        assert_eq!(key.message, included.message);
        assert_eq!(key.state, 1);
        assert_eq!(included.state, 2);
    }

    /// Error 8168 sends two states, so it carries two constructors rather than a state
    /// the caller patches: 0 when the repeated name is a constraint
    /// name (a `CREATE TABLE` with `CONSTRAINT c1 PRIMARY KEY` and
    /// `CONSTRAINT c1 UNIQUE`, or two `CONSTRAINT c1 CHECK`) and 1 when it is the name
    /// of two indexes of a `CREATE TABLE`
    /// (`CREATE TABLE dbo.t2 (a int NOT NULL INDEX ix_t (a), b int NOT NULL, INDEX ix_t (b));`).
    /// Number, severity and sentence are shared; the state is the
    /// axis that moved. Two columns of one name are a third answer, 2705 state 3, which
    /// `duplicate_column_name` builds.
    #[test]
    fn duplicate_name_8168_has_one_state_per_context() {
        let constraint = SqlError::duplicate_name_in_this_context("c1");
        let index = SqlError::duplicate_index_name_in_this_context("c1");
        assert_eq!((constraint.number, index.number), (8168, 8168));
        assert_eq!((constraint.severity, index.severity), (16, 16));
        assert_eq!(constraint.message, index.message);
        assert_eq!(constraint.state, 0);
        assert_eq!(index.state, 1);
        let column = SqlError::duplicate_column_name("a", "t");
        assert_eq!((column.number, column.state), (2705, 3));
    }

    /// Error 3723 reads its state from the constraint kind it prints: 4 for a
    /// `PRIMARY KEY`, 5 for a `UNIQUE KEY`, and the catalogue default for another kind.
    #[test]
    fn drop_index_3723_state_follows_the_constraint_kind() {
        assert_eq!(
            SqlError::cannot_drop_constraint_index("dbo.t.pk", "PRIMARY KEY").state,
            4
        );
        assert_eq!(
            SqlError::cannot_drop_constraint_index("dbo.t.uq", "UNIQUE KEY").state,
            5
        );
        assert_eq!(
            SqlError::cannot_drop_constraint_index("dbo.t.xx", "CHECK").state,
            UNKNOWN_DDL_STATE
        );
    }

    /// Error 103 sends three states, one per shape of token, and the
    /// `%S_MSG` follows two of the three: `identifier` for a name longer than 128
    /// characters, `number` for a long literal, whether or not it opens with `$`. The
    /// money literal and the plain one differ by their state alone, which is why they have
    /// two constructors.
    #[test]
    fn identifier_too_long_103_has_one_state_per_shape() {
        let name = SqlError::identifier_too_long("abc", 128);
        let number = SqlError::number_too_long("123", 128);
        let money = SqlError::money_literal_too_long("$123", 128);
        assert_eq!((name.state, number.state, money.state), (4, 5, 6));
        assert!(
            name.message
                .starts_with("The identifier beginning with 'abc'")
        );
        assert!(
            number
                .message
                .starts_with("The number beginning with '123'")
        );
        assert!(
            money
                .message
                .starts_with("The number beginning with '$123'")
        );
        assert_eq!(
            (name.severity, number.severity, money.severity),
            (15, 15, 15)
        );
    }

    /// Error 208 keeps one entry and takes a second constructor: the state is what
    /// separates a name that resolves to nothing (1) from a name that resolves to an
    /// object of another type (3). The sentence is the same in both.
    #[test]
    fn invalid_object_name_208_has_one_constructor_per_resolution() {
        let missing = SqlError::invalid_object_name("sys.sp_executesql");
        let other_type = SqlError::invalid_object_name_of_another_type("sys.sp_executesql");
        assert_eq!(missing.message, other_type.message);
        assert_eq!((missing.number, other_type.number), (208, 208));
        assert_eq!((missing.severity, other_type.severity), (16, 16));
        assert_eq!((missing.state, other_type.state), (1, 3));
    }

    /// Error 447 keeps one entry and takes a second constructor: the first carries the
    /// state 0 of a `COLLATE` in an expression, the second the state 1 of a `COLLATE` in
    /// a column definition ([`COLLATE_ON_NON_STRING_447_COLUMN_STATE`]). Number, severity
    /// and sentence are shared; the state is the axis that moves.
    #[test]
    fn collate_on_non_string_has_one_constructor_per_context() {
        for ty in ["int", "date"] {
            let expression = SqlError::collate_on_non_string(ty);
            let column = SqlError::collate_on_non_string_column(ty);
            assert_eq!(expression.message, column.message);
            assert_eq!(
                expression.message,
                format!("COLLATE cannot apply to an expression of type {ty}.")
            );
            assert_eq!((expression.number, column.number), (447, 447));
            assert_eq!((expression.severity, column.severity), (16, 16));
            assert_eq!((expression.state, column.state), (0, 1));
        }
    }

    /// Seven numbers whose catalogued severity is not the one the server sends; the
    /// catalogue keeps its value and the constructors override it, as
    /// `SEVERITY_OVERRIDES` records. 1750 is the widest gap: catalogued 10, the class of
    /// an informational message, and sent 16.
    #[test]
    fn ddl_and_name_resolution_severity_overrides() {
        let overridden: &[(SqlError, u32, u8)] = &[
            (SqlError::duplicate_correlation_name("x"), 1011, 15),
            (
                SqlError::correlation_name_is_a_table_name("u", "dbo.u"),
                1012,
                15,
            ),
            (SqlError::same_exposed_names("dbo.t", "dbo.t"), 1013, 15),
            (SqlError::cannot_find_object_to_index("dbo.t"), 1088, 15),
            (SqlError::could_not_create_constraint_or_index(), 1750, 10),
            (SqlError::table_is_ambiguous("t"), 8154, 15),
            (SqlError::no_column_name_in_derived_table(2, "d"), 8155, 15),
        ];
        for (err, number, published) in overridden {
            assert_eq!(err.severity, 16, "wire severity of {number}");
            assert_eq!(
                message_template(*number).map(|def| def.severity),
                Some(*published),
                "published severity of {number}"
            );
            assert!(SEVERITY_OVERRIDES.contains(&(*number, 16, *published)));
        }
        for number in [
            103u32, 1038, 1087, 1902, 1909, 1911, 1913, 2702, 4063, 7202, 8110,
        ] {
            assert!(
                !SEVERITY_OVERRIDES.iter().any(|(n, ..)| *n == number),
                "error {number} takes no severity override"
            );
        }
    }
}

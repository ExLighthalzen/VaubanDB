//! Objects, environment, identities and the two predicates: `OBJECT_ID`, `OBJECT_NAME`,
//! `SCHEMA_NAME`, `SCHEMA_ID`, `DB_ID`, `USER_NAME`, `SUSER_SNAME`, `HOST_NAME`, `APP_NAME`,
//! `@@IDENTITY`, `SCOPE_IDENTITY`, `IDENT_CURRENT`, `ISNUMERIC` and `ISDATE`.
//!
//! None of these functions knows the catalogue: everything that names an object, a
//! database or a schema goes through the [`EvalContext`], whose defaults answer `None`
//! when `catalog` and `session` do not fill them in. `None` becomes `NULL`, which is also
//! what SQL Server answers for an object it does not know.
//!
//! `ISNUMERIC` and `ISDATE` are **conversion attempts** and nothing more: they ask
//! [`vauban_types::convert`] whether the argument reads as a `money`, a `float` or a
//! `datetime`, and answer `1` when it does. SQL Server's own predicates have a grammar of
//! their own that differs from its `CAST` on a handful of shapes; those are listed on the
//! two evaluation functions and are deliberately **not** reproduced here: this crate
//! carries no number or date grammar of its own.
//!
//! The fourteen functions are scalar and non-deterministic, `ISNUMERIC` excepted, which the
//! test `all_fourteen_are_registered_as_scalar` asserts entry by entry over `DEFS`.
//! `ISNUMERIC`'s answer depends on nothing but its argument (`ISDATE` depends on
//! `SET DATEFORMAT` and `SET LANGUAGE`: `SELECT ISDATE('30/1/2020'), ISDATE('1/30/2020');`
//! answers `1, 0` after `SET DATEFORMAT dmy` or `SET LANGUAGE French` where `us_english`
//! answers `0, 1`).

use vauban_errors::SqlResult;
use vauban_types::{Decimal, Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, convert};

use crate::builtins::args::invalid_argument_type;
use crate::context::EvalContext;
use crate::registry::{Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// The schema a session resolves unqualified names in, until `catalog` and `session`
/// know the user's default schema: `SCHEMA_NAME()` answers it
/// (`schema_name_without_argument_is_dbo`) and so does `USER_NAME()` when the context has
/// no user (see [`user_name_eval`]).
const DEFAULT_SCHEMA: &str = "dbo";

/// Result type of `OBJECT_ID`: `int`, nullable (Microsoft Learn, "OBJECT_ID
/// (Transact-SQL)": *Return types: int*).
fn int_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::Int, true))
}

/// Result type of `DB_ID`: **`smallint`**, nullable.
///
/// The documented type is `int`, the type on the wire is not:
/// `SELECT DB_ID('no_such_db'), DB_ID();` reports two `smallint` columns through TDS
/// (`return_types_are_exact`).
fn smallint_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::SmallInt, true))
}

/// Result type of `OBJECT_NAME`, `SCHEMA_NAME`, `USER_NAME`, `SUSER_SNAME`, `HOST_NAME`
/// and `APP_NAME`: `nvarchar(128)`, nullable.
///
/// The six are `sysname`, which is `nvarchar(128)` **NOT NULL** as a column type. As the type of an *expression* it is nullable, and several of these
/// functions really answer `NULL` (`OBJECT_NAME(-1)`, `SCHEMA_NAME(99)`, `USER_NAME(NULL)`,
/// `SUSER_SNAME(NULL)`). `HOST_NAME()` is **not** one of them for a client that announces
/// an empty workstation name: SQL Server answers a non-`NULL` string there. The six are
/// `nvarchar(128)`, nullable, the type of a `CAST('x' AS nvarchar(128))` column.
fn name_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(128)), true))
}

/// Result type of `@@IDENTITY`, `SCOPE_IDENTITY()` and `IDENT_CURRENT`: `numeric(38, 0)`,
/// nullable (Microsoft Learn: *Returns numeric(38,0)*).
fn identity_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    Ok(TypeInfo::new(
        SqlType::Numeric {
            precision: 38,
            scale: 0,
        },
        true,
    ))
}

/// Error 8116 for the four date types `ISNUMERIC` and `ISDATE` both refuse, `Ok(())` for
/// any other argument type.
///
/// Microsoft Learn ("ISDATE (Transact-SQL)") names `datetime2`, `time` and
/// `datetimeoffset`; SQL Server 2022 refuses **`date`** as well, for both functions:
/// `SELECT ISDATE(CAST('2020-01-01' AS date));` is 8116 on argument 1 of `isdate` and
/// `SELECT ISNUMERIC(CAST('2020-01-01' AS date));` the same with `isnumeric`
/// (`predicates_refuse_the_date_only_types`). `datetime` and `smalldatetime` are accepted by
/// both (`ISDATE(GETDATE())` is `1`, `ISNUMERIC(GETDATE())` is `0`). A `NULL` of a
/// refused type is refused too: `ISDATE(CAST(NULL AS date))` and `ISNUMERIC(CAST(NULL
/// AS time))` are 8116, so the check is on the **type** and runs at bind time; the two
/// evaluations repeat it so that a caller who skipped `check_call` gets the same answer.
fn refuse_date_only_types(args: &[TypeInfo], function: &str) -> SqlResult<()> {
    match args.first().map(|info| &info.ty) {
        Some(
            ty @ (SqlType::Date
            | SqlType::Time(_)
            | SqlType::DateTime2(_)
            | SqlType::DateTimeOffset(_)),
        ) => Err(invalid_argument_type(ty, 1, function)),
        _ => Ok(()),
    }
}

/// Result type of `ISNUMERIC`: `int`, not nullable, after the 8116 check of
/// [`refuse_date_only_types`]. A `SELECT ... INTO` types the column as nullable, but that
/// is how it types a scalar function call in general; the function itself does not answer
/// `NULL` (`ISNUMERIC(NULL)` is `0`; `isnumeric_edge_cases`).
fn isnumeric_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    refuse_date_only_types(args, "ISNUMERIC")?;
    Ok(TypeInfo::new(SqlType::Int, false))
}

/// Result type of `ISDATE`: `int`, never `NULL`, after the 8116 check of
/// [`refuse_date_only_types`]. Same remark on `SELECT … INTO` as [`isnumeric_type`].
fn isdate_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    refuse_date_only_types(args, "ISDATE")?;
    Ok(TypeInfo::new(SqlType::Int, false))
}

/// Builds a `Value::String` from `text`.
fn string(text: &str) -> Value {
    Value::String(SqlString {
        text: text.to_owned(),
    })
}

/// The `index`-th argument read as a name: `Ok(None)` for `NULL`, the text otherwise.
///
/// A non-character argument is converted by [`vauban_types::convert`] to
/// `nvarchar(max)`, the way SQL Server converts it implicitly: `OBJECT_ID(1)` and
/// `OBJECT_ID(GETDATE())` both answer `NULL` because no object bears those names, not
/// because the argument was refused. `(max)` rather than `sysname`, so that a four-part
/// name is never truncated before the context sees it.
fn name_argument(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<String>> {
    let value = &args.values[index];
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(SqlType::NVarChar(Len::Max), true);
    match convert(value, &args.types[index], &target, None)? {
        Value::String(s) => Ok(Some(s.text)),
        _ => Ok(None),
    }
}

/// The `index`-th argument read as an `int`: `Ok(None)` for `NULL`, the number otherwise.
///
/// The conversion is [`vauban_types::convert`] and so are its errors, which SQL Server
/// raises identically for these functions: `OBJECT_NAME('abc')` is 245 `Conversion failed
/// when converting the varchar value 'abc' to data type int.` and
/// `OBJECT_NAME(CAST(99999999999 AS bigint))` is 8115 `Arithmetic overflow error
/// converting expression to data type int.` A `decimal` is truncated: `SCHEMA_NAME(1.5)`
/// answers what `SCHEMA_NAME(1)` answers.
fn int_argument(args: &EvalArgs<'_>, index: usize) -> SqlResult<Option<i32>> {
    let value = &args.values[index];
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let target = TypeInfo::new(SqlType::Int, true);
    match convert(value, &args.types[index], &target, None)? {
        Value::I32(n) => Ok(Some(n)),
        _ => Ok(None),
    }
}

/// `true` when the optional argument at `index` was written and is `NULL`.
fn optional_argument_is_null(args: &EvalArgs<'_>, index: usize) -> bool {
    matches!(args.values.get(index), Some(Value::Null))
}

/// Evaluates `OBJECT_ID(name [, type])`.
///
/// `NULL` when `name` is `NULL`, when the context does not know the object, and when the
/// second argument is `NULL`: `SELECT OBJECT_ID('sys.objects', NULL);` is `NULL` on SQL
/// Server where `OBJECT_ID('sys.objects', 'V')` is not. Any other second argument is
/// **accepted and ignored** in V1: SQL Server filters by object type
/// (`OBJECT_ID('sys.objects', 'U')` is `NULL` because `sys.objects` is a view, `'ZZ'` and
/// `1` are `NULL` too), which needs a catalogue this crate does not see. The name is
/// passed to the context as written: resolving its one to four parts is the `binder`'s
/// job.
fn object_id_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    if optional_argument_is_null(args, 1) {
        return Ok(Value::Null);
    }
    Ok(match name_argument(args, 0)? {
        Some(name) => ctx
            .object_id_when_stable(&name)?
            .map_or(Value::Null, Value::I32),
        None => Value::Null,
    })
}

/// Evaluates `OBJECT_NAME(id [, database_id])`.
///
/// `NULL` when `id` is `NULL` or unknown (`OBJECT_NAME(-1)`),
/// and when the second argument is `NULL`: `OBJECT_NAME(OBJECT_ID('sys.objects'), NULL)`
/// is `NULL` where the same call with `DB_ID()` answers `objects`. Any other database
/// identifier is **accepted and ignored** in V1; SQL Server answers `NULL` for a database
/// it does not have (`99999`), which the context will decide once `catalog` exists.
fn object_name_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    if optional_argument_is_null(args, 1) {
        return Ok(Value::Null);
    }
    Ok(match int_argument(args, 0)? {
        Some(id) => ctx
            .object_name(id)
            .map_or(Value::Null, |name| string(&name)),
        None => Value::Null,
    })
}

/// Evaluates `SCHEMA_NAME([id])`.
///
/// Without an argument, the default schema of the session, `dbo` in V1. With one, the
/// schema the context knows under this identifier,
/// `NULL` otherwise (`SCHEMA_NAME(99)`) and `NULL` for a `NULL` argument: `SELECT
/// SCHEMA_NAME(NULL);` is `NULL` on SQL Server, not `dbo`.
fn schema_name_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    if args.values.is_empty() {
        return Ok(string(DEFAULT_SCHEMA));
    }
    Ok(match int_argument(args, 0)? {
        Some(id) => ctx
            .schema_name(id)
            .map_or(Value::Null, |name| string(&name)),
        None => Value::Null,
    })
}

/// Evaluates `SCHEMA_ID([name])`.
///
/// Without an argument, the identifier of the session's default schema
/// (`ctx.schema_id(None)`); with one, the identifier the context resolves for that name,
/// `NULL` when it resolves nothing and `NULL` for a `NULL` argument: `SCHEMA_ID()` and
/// `SCHEMA_ID('dbo')` answer `1`, `SCHEMA_ID('no_such_schema')` and `SCHEMA_ID(NULL)`
/// answer `NULL`, and `SCHEMA_ID(1)` answers `NULL`, the number being converted to the
/// name `1`, which no schema bears, the way [`name_argument`] converts it
/// (`schema_id_dbo_is_some`, `schema_id_unknown_is_null`).
///
/// The result is an `int` ([`int_type`]) and not the `smallint` of `DB_ID`: `SELECT
/// SCHEMA_ID() AS n WHERE 1 = 0;` describes an `int` column, where the same query on
/// `DB_ID()` describes a `smallint` one.
fn schema_id_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let id = if args.values.is_empty() {
        ctx.schema_id(None)
    } else {
        match name_argument(args, 0)? {
            Some(name) => ctx.schema_id(Some(&name)),
            None => None,
        }
    };
    Ok(id.map_or(Value::Null, Value::I32))
}

/// Evaluates `DB_ID([name])`.
///
/// Without an argument, the identifier of the current database
/// (`ctx.database_id(None)`); with one, the identifier of the named database or `NULL`
/// when there is no such database or when the argument is `NULL` (`DB_ID(NULL)`
/// is `NULL` where `DB_ID()` is `1`). The answer is a `smallint` ([`smallint_type`]): an
/// identifier the context could not fit in one would be a bug of the context, and answers
/// `NULL` rather than a wrong number.
fn db_id_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let id = if args.values.is_empty() {
        ctx.database_id(None)
    } else {
        match name_argument(args, 0)? {
            Some(name) => ctx.database_id(Some(&name)),
            None => None,
        }
    };
    Ok(id
        .and_then(|id| i16::try_from(id).ok())
        .map_or(Value::Null, Value::I16))
}

/// Evaluates `USER_NAME([id])`: the database user of the session.
///
/// `dbo` when the context has no user, and not `NULL`: SQL Server answers `dbo` for a
/// sysadmin login, the default schema and the default user coincide in V1, and a `NULL`
/// here would make `LEN(USER_NAME()) > 0` false. An argument is **accepted and ignored**
/// in V1, with one exception: `USER_NAME(NULL)` is `NULL` where `USER_NAME()` and
/// `USER_NAME(1)` are `dbo`.
/// SQL Server otherwise resolves the identifier in `sys.database_principals`
/// (`USER_NAME(0)` is `public`, `USER_NAME(-1)` is `NULL`), which waits for `catalog`.
fn user_name_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    if optional_argument_is_null(args, 0) {
        return Ok(Value::Null);
    }
    Ok(string(ctx.user_name().unwrap_or(DEFAULT_SCHEMA)))
}

/// Evaluates `SUSER_SNAME([sid])`: the login of the session, `NULL` when the context has
/// none.
///
/// An argument is **accepted and ignored** in V1, with the same exception as
/// [`user_name_eval`]: `SUSER_SNAME(NULL)` is `NULL` where `SUSER_SNAME()` and
/// `SUSER_SNAME(0x01)` answer the login. SQL Server otherwise looks the `varbinary` SID up
/// in `sys.server_principals` (`SUSER_SNAME(0x0105)` is `NULL`), and refuses a character
/// argument with 257 `Implicit conversion from data type varchar to varbinary is not
/// allowed.`; neither is reproduced before `catalog` exists.
fn suser_sname_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    if optional_argument_is_null(args, 0) {
        return Ok(Value::Null);
    }
    Ok(ctx.login_name().map_or(Value::Null, string))
}

/// Evaluates `HOST_NAME()`: the workstation the client announced, `NULL` when the context
/// hands out none.
///
/// This is where VaubanDB differs from SQL Server for a client that announces an
/// **empty** workstation name, and the difference is not this function's: SQL Server
/// answers a non-`NULL` string, while `session::eval_context::non_empty` maps the empty
/// string to `None`, so VaubanDB answers `NULL`. The gap lives in `session`.
fn host_name_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(ctx.host_name().map_or(Value::Null, string))
}

/// Evaluates `APP_NAME()`: the application the client announced, `NULL` when it
/// announced none.
fn app_name_eval(_args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(ctx.app_name().map_or(Value::Null, string))
}

/// Brings an identity value the context handed out to the `numeric(38, 0)` of the call.
///
/// The context answers a [`Decimal`] of its own precision and scale; the result type of
/// the three identity functions is fixed ([`identity_type`]), so the value is converted to
/// `args.result` by [`vauban_types::convert`] rather than relabelled.
fn identity_value(args: &EvalArgs<'_>, identity: Option<Decimal>) -> SqlResult<Value> {
    let Some(identity) = identity else {
        return Ok(Value::Null);
    };
    let from = TypeInfo::new(
        SqlType::Numeric {
            precision: identity.precision,
            scale: identity.scale,
        },
        true,
    );
    convert(&Value::Decimal(identity), &from, args.result, None)
}

/// Evaluates `@@IDENTITY`: the last identity value the session generated, `NULL` before
/// an insert.
fn identity_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    identity_value(args, ctx.last_identity())
}

/// Evaluates `SCOPE_IDENTITY()`: the last identity value generated in the current scope,
/// `NULL` when the scope generated none.
fn scope_identity_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    identity_value(args, ctx.scope_identity())
}

/// Evaluates `IDENT_CURRENT(table)`: the last identity value generated for a table,
/// whatever the session, `NULL` when the table is unknown or has no identity column
/// and when the argument is `NULL`.
///
/// On an unknown table SQL Server also sends an informational message; it is not
/// reproduced.
fn ident_current_eval(args: &EvalArgs<'_>, ctx: &dyn EvalContext) -> SqlResult<Value> {
    let Some(table) = name_argument(args, 0)? else {
        return Ok(Value::Null);
    };
    identity_value(args, ctx.ident_current(&table))
}

/// `true` when `text` is empty or made of spaces only.
///
/// Both predicates answer `0` on such a string although the conversion behind them
/// succeeds: `SELECT CAST('' AS money), CAST(' ' AS float), ISNUMERIC(''), ISNUMERIC(' ');`
/// is `0.0000, 0, 0, 0` and `SELECT CAST('' AS datetime), CAST(' ' AS datetime),
/// ISDATE(''), ISDATE(' ');` is `1900-01-01, 1900-01-01, 0, 0`. Only the space counts:
/// `ISNUMERIC(CHAR(9))` and `ISNUMERIC(CHAR(10))` are `1` on SQL Server, because a control
/// character alone converts to `money` `0.0000` there (`CAST(CHAR(12) AS money)`), and
/// the conversion is what decides past this check.
fn is_blank(text: &str) -> bool {
    text.chars().all(|c| c == ' ')
}

/// Whether `value`, read as `from`, converts to `to`. The error itself is not wanted: a
/// failed attempt is the `0` of the predicate.
fn converts_to(value: &Value, from: &TypeInfo, to: SqlType) -> bool {
    convert(value, from, &TypeInfo::new(to, true), None).is_ok()
}

/// Evaluates `ISNUMERIC(expr)`: `1` when the argument is, or reads as, a number.
///
/// - `NULL` is `0`, whatever its accepted type (`ISNUMERIC(CAST(NULL AS int))` is `0`;
///   a `NULL` of a refused type is 8116, see [`refuse_date_only_types`]);
/// - a value of a numeric family — `bit`, the integers, `decimal`/`numeric`, `float`/`real`,
///   `money`/`smallmoney` — is `1`;
/// - `datetime`, `smalldatetime`, `uniqueidentifier` and the binary types are `0`;
///   `date`, `time`, `datetime2` and `datetimeoffset` are 8116
///   ([`refuse_date_only_types`]);
/// - a character value is `1` when it is not blank ([`is_blank`]) and reads as a `money`
///   **or** as a `float` by [`vauban_types::convert`]. The `money` reading brings the
///   currency symbol, the thousands separators and the point (`'$5'`, `'1,000'`,
///   `'$1,000.50'`, `'-$5'`, `'$-5'`, `'.5'`, `'5.'`, `'1,'`, `',1'` are all `1`); the
///   `float` reading brings the exponent (`'1e5'`, `'1e-5'`, `'1.5e5'`), and its overflow
///   makes `'1e309'` and `'1e400'` `0`. Neither reads `'12a'`, `'1e'`, `'0x10'`,
///   `'1.5.5'`, `'5$'`, `'1 000'` or `'++1'`, which are `0` on SQL Server too. A `decimal`
///   reading would add nothing: whatever it accepts, `float` accepts.
///
/// **What `types` reads.** SQL Server's `CAST` reads a lone `$`, `+`, `-`, `.`, `,`, `-,`
/// or `$-.` as `money` `0.0000`, and `'1d5'` as the `float` `100000`;
/// [`vauban_types::convert`] reads them the same way, so `ISNUMERIC` answers `1` on them
/// as SQL Server does, and `0` on `'--'`, `'$$'`, `'.-$'`, `'1f5'` and `'1d5e2'`, which
/// both `CAST`s refuse (`isnumeric_follows_the_cast_on_the_money_and_float_shapes`).
///
/// **Deliberate differences from SQL Server**, whose `ISNUMERIC` refuses some shapes its
/// `CAST` reads, or the reverse; this crate carries no number grammar of its own:
/// `CAST('1' + CHAR(9) AS money)` is `1.0000` but `ISNUMERIC('1' + CHAR(9))` is `0` (a
/// trailing tabulation or line feed fails the predicate, a leading one does not);
/// `CAST('- 5' AS money)` is `-5.0000` but `ISNUMERIC('- 5')` is `0`;
/// `ISNUMERIC(N'１２３')` (full-width digits), `ISNUMERIC(N'−5')` (U+2212),
/// `ISNUMERIC('1 .5')`, `ISNUMERIC('-5-')` and `ISNUMERIC(CHAR(160))` are `1` where each
/// `CAST` of the same text to `money` or `float` fails; and
/// `'99999999999999999999999999999999999999999.5'` (41 digits and a fraction) is `0`
/// although `CAST` to `float` reads it as `1e+41` and the same 41 digits without the
/// fraction are `1`. This function follows `convert` on each of them.
fn isnumeric_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    refuse_date_only_types(args.types, "ISNUMERIC")?;
    let value = &args.values[0];
    let from = &args.types[0];
    let numeric = match value {
        Value::Null => false,
        Value::String(s) => {
            !is_blank(&s.text)
                && (converts_to(value, from, SqlType::Money)
                    || converts_to(value, from, SqlType::Float))
        }
        _ => matches!(
            from.ty.family(),
            TypeFamily::Bit
                | TypeFamily::Integer
                | TypeFamily::ExactNumeric
                | TypeFamily::ApproxNumeric
                | TypeFamily::Money
        ),
    };
    Ok(Value::I32(i32::from(numeric)))
}

/// Evaluates `ISDATE(expr)`: `1` when the argument is, or reads as, a `datetime`.
///
/// - `NULL` is `0`, whatever its accepted type (`ISDATE(CAST(NULL AS datetime))` is `0`;
///   a `NULL` of a refused type is 8116, see [`refuse_date_only_types`]);
/// - a `datetime` or `smalldatetime` is `1` (`ISDATE(GETDATE())`); the four other date
///   types are 8116 ([`refuse_date_only_types`]);
/// - every non-character, non-date value is `0` **without** a conversion attempt:
///   `ISDATE(1)`, `ISDATE(43000)`, `ISDATE(CAST(1 AS float))`, `ISDATE(CAST(1 AS money))`,
///   `ISDATE(CAST(1 AS bit))`, `ISDATE(0x00)` and `ISDATE(NEWID())` are all `0` on SQL
///   Server although `CAST(43000 AS datetime)` is a date;
/// - a character value is `1` when it is not blank ([`is_blank`]) and reads as a
///   `datetime` by [`vauban_types::convert`]: `'2020-01-01'`, `'12:00'`, `'20200101'`,
///   `'1/2/2020'`, `'jan 1 2020'`, `'2020-01-01T12:00:00'` and `'2020-01-01T12:00:00Z'`
///   are `1`; `'2020-13-01'`, `'abc'`, `'30/1/2020'` (under `us_english`), `'1752-12-31'`,
///   `'10000-01-01'` and `'9999-12-31 23:59:59.999'` (rounds past the top of `datetime`)
///   are `0`.
///
/// The reading is the one `types` does under the default `us_english`/`mdy` settings:
/// [`vauban_types::convert`] takes no `SET DATEFORMAT`, so the answer does not follow the
/// session's setting yet, which is why the function is registered non-deterministic.
///
/// Unlike `ISNUMERIC`, no shape is known where SQL Server's `ISDATE` and its `CAST(... AS
/// datetime)` disagree. The shapes SQL Server's `CAST` raises 241 on (an offset or a `Z`
/// after a space, `'2020-01-01 12:00:00+02:00'` and `'2020-01-01 12:00:00Z'`; more than
/// three fractional digits, `'2020-01-01 12:00:00.1234'`; a tabulation at either end,
/// `CHAR(9) + '2020-01-01'` and `CHAR(9)` alone) are refused by `convert`, and the
/// predicate follows (`isdate_follows_the_cast_on_the_datetime_shapes`).
fn isdate_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    refuse_date_only_types(args.types, "ISDATE")?;
    let value = &args.values[0];
    let from = &args.types[0];
    let date = match value {
        Value::Null => false,
        Value::String(s) => !is_blank(&s.text) && converts_to(value, from, SqlType::DateTime),
        Value::DateTime(_) => true,
        _ => false,
    };
    Ok(Value::I32(i32::from(date)))
}

/// `OBJECT_ID`: Microsoft Learn, "OBJECT_ID (Transact-SQL)". One or two arguments
/// (`SELECT OBJECT_ID();` and `SELECT OBJECT_ID('a', 'b', 'c');` are both 189).
const OBJECT_ID_DEF: FunctionDef = FunctionDef {
    name: "OBJECT_ID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(1, 2),
    return_type: int_type,
    eval: object_id_eval,
    aggregate: None,
};

/// `OBJECT_NAME`: Microsoft Learn, "OBJECT_NAME (Transact-SQL)". One or two arguments.
const OBJECT_NAME_DEF: FunctionDef = FunctionDef {
    name: "OBJECT_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(1, 2),
    return_type: name_type,
    eval: object_name_eval,
    aggregate: None,
};

/// `SCHEMA_NAME`: Microsoft Learn, "SCHEMA_NAME (Transact-SQL)". Zero or one argument.
const SCHEMA_NAME_DEF: FunctionDef = FunctionDef {
    name: "SCHEMA_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: name_type,
    eval: schema_name_eval,
    aggregate: None,
};

/// `SCHEMA_ID`: Microsoft Learn, "SCHEMA_ID (Transact-SQL)". Zero or one argument (`SELECT
/// SCHEMA_ID('a', 'b');` is 189; `arities_follow_sql_server`).
const SCHEMA_ID_DEF: FunctionDef = FunctionDef {
    name: "SCHEMA_ID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: int_type,
    eval: schema_id_eval,
    aggregate: None,
};

/// `DB_ID`: Microsoft Learn, "DB_ID (Transact-SQL)". Zero or one argument; the return
/// type is `smallint` ([`smallint_type`]).
const DB_ID_DEF: FunctionDef = FunctionDef {
    name: "DB_ID",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: smallint_type,
    eval: db_id_eval,
    aggregate: None,
};

/// `USER_NAME`: Microsoft Learn, "USER_NAME (Transact-SQL)". Zero or one argument.
const USER_NAME_DEF: FunctionDef = FunctionDef {
    name: "USER_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: name_type,
    eval: user_name_eval,
    aggregate: None,
};

/// `SUSER_SNAME`: Microsoft Learn, "SUSER_SNAME (Transact-SQL)". Zero or one argument.
const SUSER_SNAME_DEF: FunctionDef = FunctionDef {
    name: "SUSER_SNAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Range(0, 1),
    return_type: name_type,
    eval: suser_sname_eval,
    aggregate: None,
};

/// `HOST_NAME`: Microsoft Learn, "HOST_NAME (Transact-SQL)". No argument (`SELECT
/// HOST_NAME(1);` is 174).
const HOST_NAME_DEF: FunctionDef = FunctionDef {
    name: "HOST_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: host_name_eval,
    aggregate: None,
};

/// `APP_NAME`: Microsoft Learn, "APP_NAME (Transact-SQL)". No argument.
const APP_NAME_DEF: FunctionDef = FunctionDef {
    name: "APP_NAME",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: name_type,
    eval: app_name_eval,
    aggregate: None,
};

/// `@@IDENTITY`: Microsoft Learn, "@@IDENTITY (Transact-SQL)".
const IDENTITY_DEF: FunctionDef = FunctionDef {
    name: "@@IDENTITY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: identity_type,
    eval: identity_eval,
    aggregate: None,
};

/// `SCOPE_IDENTITY`: Microsoft Learn, "SCOPE_IDENTITY (Transact-SQL)". No argument
/// (`SELECT SCOPE_IDENTITY(1);` is 174).
const SCOPE_IDENTITY_DEF: FunctionDef = FunctionDef {
    name: "SCOPE_IDENTITY",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(0),
    return_type: identity_type,
    eval: scope_identity_eval,
    aggregate: None,
};

/// `IDENT_CURRENT`: Microsoft Learn, "IDENT_CURRENT (Transact-SQL)". Exactly one
/// argument (`SELECT IDENT_CURRENT();` is 174).
const IDENT_CURRENT_DEF: FunctionDef = FunctionDef {
    name: "IDENT_CURRENT",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: identity_type,
    eval: ident_current_eval,
    aggregate: None,
};

/// `ISNUMERIC`: Microsoft Learn, "ISNUMERIC (Transact-SQL)". The only deterministic
/// function of this file.
const ISNUMERIC_DEF: FunctionDef = FunctionDef {
    name: "ISNUMERIC",
    kind: FunctionKind::Scalar,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: isnumeric_type,
    eval: isnumeric_eval,
    aggregate: None,
};

/// `ISDATE`: Microsoft Learn, "ISDATE (Transact-SQL)". Non-deterministic: its answer
/// follows `SET DATEFORMAT` and `SET LANGUAGE`.
const ISDATE_DEF: FunctionDef = FunctionDef {
    name: "ISDATE",
    kind: FunctionKind::Scalar,
    deterministic: false,
    arity: Arity::Exact(1),
    return_type: isdate_type,
    eval: isdate_eval,
    aggregate: None,
};

/// Registers the fourteen functions of this module.
pub(crate) fn register_all() {
    register(OBJECT_ID_DEF);
    register(OBJECT_NAME_DEF);
    register(SCHEMA_NAME_DEF);
    register(SCHEMA_ID_DEF);
    register(DB_ID_DEF);
    register(USER_NAME_DEF);
    register(SUSER_SNAME_DEF);
    register(HOST_NAME_DEF);
    register(APP_NAME_DEF);
    register(IDENTITY_DEF);
    register(SCOPE_IDENTITY_DEF);
    register(IDENT_CURRENT_DEF);
    register(ISNUMERIC_DEF);
    register(ISDATE_DEF);
}

#[cfg(test)]
mod tests;

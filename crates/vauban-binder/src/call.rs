//! `CAST`, `CONVERT` and function calls: registry lookup and arity.
//!
//! Four kinds of node land here, and none of them types itself:
//!
//! - `CAST(e AS t)` and `CONVERT(t, e [, style])` become a single
//!   [`BoundExprKind::Convert`], whose `ty` is the target type resolved by
//!   [`resolve_data_type`](crate::datatype::resolve_data_type). Whether the pair of types
//!   converts is decided by [`is_castable`]; *evaluating* the conversion is
//!   `types::convert`, at execution time, and needs a value.
//! - `f(a, b)` becomes a [`BoundExprKind::Function`]: [`vauban_sysfn::lookup`] finds
//!   the definition — an unknown name is error **195**, raised here and nowhere else — and
//!   [`vauban_sysfn::check_call`] does the whole typing, arity included. **No typing
//!   rule of a built-in is written in this file**: a wrong result type is a bug of
//!   `sysfn`, not of the binder, which adds the line of the node to the error and nothing
//!   else. The one argument this file reads itself is the `datepart` **keyword** of
//!   `DATEPART` and its four relatives, which is a word of the language and not an
//!   expression, and which SQL Server refuses while compiling: error **155** is raised in
//!   [`bind_arguments`], whose documentation says why it is raised there rather than in
//!   the evaluation.
//! - `@@ROWCOUNT` and its family are functions of the same registry, called with no
//!   argument; an unknown one is error **137**, not 195 (see [`bind_variable_function`]).
//! - `CURRENT_TIMESTAMP` and the four other niladic functions arrive as column references
//!   and are recognised by [`bind_niladic`] before `expr.rs` raises error 207.
//!
//! # What SQL Server answers
//!
//! | Query | Answer |
//! |---|---|
//! | `SELECT NO_SUCH_FN(1);` | 195/15/10, the name not being a built-in function |
//! | `SELECT no_such_fn(1);` | 195/15/10 — the name is printed **as written** |
//! | `SELECT foo.bar(1);` | 4121/16/1, neither a column `foo` nor a function `foo.bar` |
//! | `SELECT dbo.LEN('abc');` | 4121/16/1, same wording with `dbo.LEN` |
//! | `SELECT sys.LEN('abc');` | 4121/16/1, same wording with `sys.LEN` |
//! | `SELECT @@NO_SUCH;` | 137/15/2, the scalar variable is not declared |
//! | `SELECT CAST(NEWID() AS int);` | 529/16/1, no explicit conversion from `uniqueidentifier` to `int` |
//! | `SELECT PATINDEX('%bc%', NULL);` | 8116/16/1, `NULL` invalid for argument 2 of `patindex` |
//! | `SELECT NULLIF(NULL, 1);` | 4151/16/1, the first argument of `NULLIF` cannot be the `NULL` constant |
//! | `SELECT CHARINDEX('b', NULL);`, `SELECT LEN(NULL);`, `SELECT STUFF(NULL, 1, 1, 'x');` | accepted, `NULL` |
//! | `SELECT ISNULL(NULL, 'x');` | accepted, a **`varchar`** holding `x` — the bare `NULL` takes the type of its sibling ([`retype_untyped_nulls`]) |
//! | `SELECT SUM(1);`, `SELECT COUNT(*);` | accepted, `1` |
//!
//! Each number above has its constructor in `vauban-errors`: **no number of the catalogue
//! is spelled by hand in this file, and none of them leaves it as an internal 50000**. The
//! last row of the table above, `SELECT SUM(1);` and `SELECT COUNT(*);`, still raises an
//! internal error: a select list holding an aggregate is routed to `aggregate.rs` by
//! `query.rs`, which does not bind it yet, and [`bind_function`] refuses an aggregate
//! written anywhere else.

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{ColumnRef, DataType, Expr, Ident, Literal, ObjectName, Span};
use vauban_sysfn::{FunctionDef, FunctionKind, check_call, lookup, parse_datepart};
use vauban_types::{Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, implicit_result_type};

use crate::bound::{BoundExpr, BoundExprKind};
use crate::context::BindContext;
use crate::datatype::resolve_data_type;
use crate::errors::line_of;
use crate::expr::{Scope, implicit_conversion_may_be_null};

/// Length a character or binary type gets in a `CAST`/`CONVERT` target written without
/// one, where a declaration would give it 1.
///
/// `SELECT CAST(SQL_VARIANT_PROPERTY(CAST('abcdefghijklmnopqrstuvwxyz0123456789'
/// AS varchar), 'MaxLength') AS int);` answers `30`, and the same query over
/// `CAST(0x…20 bytes… AS varbinary)` answers `30` too.
const CAST_DEFAULT_LEN: u16 = 30;

/// The 1-based rank `resolve_data_type` prints in message 2715.
///
/// A conversion has exactly one type and it is not a column of a statement, so the rank is
/// 1 here; `DECLARE` and `CREATE TABLE` are the callers that really count.
const CONVERSION_TYPE_POSITION: u32 = 1;

/// The niladic functions T-SQL accepts **without parentheses**, upper-cased.
///
/// `CURRENT_TIMESTAMP`, `SESSION_USER`, `SYSTEM_USER`, `CURRENT_USER` and `USER`. The
/// parser reads a bare name as a column reference, so [`bind_niladic`] is what tells them
/// from a column before error 207 is raised.
const NILADIC_FUNCTIONS: [&str; 5] = [
    "CURRENT_TIMESTAMP",
    "SESSION_USER",
    "SYSTEM_USER",
    "CURRENT_USER",
    "USER",
];

/// Binds `CAST(e AS t)` and `TRY_CAST(e AS t)`.
///
/// # Errors
///
/// - 529 when the pair of types has no explicit conversion ([`is_castable`]);
/// - the error of the target type, translated to 243 when the name is not a type at all
///   (see [`target_type`]);
/// - whatever binding the source expression raises.
pub(crate) fn bind_cast(e: &Expr, ctx: &BindContext<'_>, scope: &Scope) -> SqlResult<BoundExpr> {
    let Expr::Cast {
        expr,
        ty,
        try_,
        span,
    } = e
    else {
        return Err(bug("bind_cast: the node is not a CAST"));
    };
    bind_conversion(ty, expr, None, *try_, span, ctx, scope)
}

/// Binds `CONVERT(t, e [, style])` and `TRY_CONVERT(t, e [, style])`.
///
/// The difference with [`bind_cast`] is the written order of the arguments and the third
/// one: the bound node is the same [`BoundExprKind::Convert`].
///
/// # Errors
///
/// Those of [`bind_cast`], plus the internal error of a non-constant `style`
/// ([`bind_style`]).
pub(crate) fn bind_convert(e: &Expr, ctx: &BindContext<'_>, scope: &Scope) -> SqlResult<BoundExpr> {
    let Expr::Convert {
        ty,
        expr,
        style,
        try_,
        span,
    } = e
    else {
        return Err(bug("bind_convert: the node is not a CONVERT"));
    };
    let style = match style {
        Some(style) => Some(bind_style(style, ctx, scope)?),
        None => None,
    };
    bind_conversion(ty, expr, style, *try_, span, ctx, scope)
}

/// The body shared by `CAST` and `CONVERT`, in the order the checks must happen.
///
/// # Nullability
///
/// An explicit conversion is nullable, the `TRY_` forms like the others, on the
/// `fNullable` bit of the COLMETADATA token (`every_explicit_conversion_is_nullable`):
///
/// | Query | Type token | `fNullable` |
/// |---|---|---|
/// | `SELECT 1;` | `INT4TYPE` | 0 |
/// | `SELECT CAST(1 AS int);` | `INTNTYPE` | 1 |
/// | `SELECT CONVERT(int, 1);` | `INTNTYPE` | 1 |
/// | `SELECT CAST('a' AS varchar(10));` | `BIGVARCHRTYPE` | 1 |
/// | `SELECT ISNULL(CAST(1 AS int), 0);` | `INT4TYPE` | 0 |
///
/// The first two rows are two expressions of the same type and the same value, and the
/// last one shows that an expression is not nullable *because* it is an expression: the
/// conversion is. The **implicit** conversions the binder inserts follow another rule
/// (`expr::implicit_conversion_may_be_null`).
///
/// # Collation
///
/// A conversion between two character types keeps the collation of the **source**; the
/// other results take the default collation.
fn bind_conversion(
    target: &DataType,
    source: &Expr,
    style: Option<i32>,
    try_: bool,
    span: &Span,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let to = target_type(target, line)?;
    let value = bind_operand(source, ctx, scope)?;
    if !is_conversion_null(source) && !is_castable(&value.ty.ty, &to) {
        return Err(
            SqlError::explicit_conversion_not_allowed(value.ty.ty.name(), to.name())
                .with_line(line),
        );
    }
    let mut ty = TypeInfo::new(to, true);
    if to.is_string() && value.ty.ty.is_string() {
        ty.collation = value.ty.collation;
    }
    Ok(BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(value),
            style,
            try_,
        },
        ty,
        line,
    })
}

/// Whether the source of a conversion is the `NULL` constant **as written**, a shape `CAST`
/// and `CONVERT` accept on each of the targets below.
///
/// [`is_castable`] answers by the source *type*, and `literal.rs` types the bare `NULL` an
/// `int` (`literal::binds_null_as_nullable_int`), which would make
/// `SELECT CAST(NULL AS date);` answer 529 where SQL Server answers a `date` NULL. This
/// rule therefore reads the AST, not the bound type, and `bind_conversion` consults it
/// before [`is_castable`].
///
/// On the five types [`is_castable`] refuses from `int` (`date`, `time(7)`,
/// `datetime2(7)`, `datetimeoffset(7)`, `uniqueidentifier`), plus `int` and `varchar(20)`
/// as witnesses, each crossed with `CAST`, `CONVERT`, `TRY_CAST`, `TRY_CONVERT` and these
/// five written sources, SQL Server answers:
///
/// | Source written | `CAST(<source> AS date)` | `CAST(<source> AS int)` |
/// |---|---|---|
/// | `NULL` | `date` NULL | `int` NULL |
/// | `((NULL))` | `date` NULL | `int` NULL |
/// | `+NULL` | `date` NULL | `int` NULL |
/// | `CAST(NULL AS int)` | 529 | `int` NULL |
/// | `CASE WHEN 1=1 THEN NULL ELSE 1 END` | 529 | `int` NULL |
///
/// The last two rows are what separates the two readings: both evaluate to a NULL whose
/// type is `int`, and both are refused on `date`, so the rule holds on the constant being
/// *written* there and not on the value being NULL. The guard does not ask what the target
/// is, and on the two witness targets that changes nothing.
///
/// `-NULL` and `~NULL` do not reach this guard: they stay on the [`is_castable`] path,
/// which reads the type their operand carries.
///
/// It does not reuse [`is_untyped_null`], which serves the argument retyping of a call:
/// the unary plus of the table above is stated for a conversion, not for a call argument.
fn is_conversion_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(Literal::Null, _) => true,
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: vauban_parser::UnaryOp::Plus,
            expr: inner,
            ..
        } => is_conversion_null(inner),
        _ => false,
    }
}

/// The target type of a conversion: [`resolve_data_type`] plus the two rules a conversion
/// does not share with a declaration.
///
/// 1. A character or binary type written **without a length** is 30 long, not 1
///    ([`CAST_DEFAULT_LEN`]).
/// 2. A name that is not a type is error **243** (not a defined system type), severity 16,
///    state 1, not the 2715 of a declaration: `SELECT CAST(1 AS foo);` and
///    `SELECT CAST(1 AS [foo]);` answer 243/16/1 while `DECLARE @v foo;` answers 2715/16/3.
///
/// `resolve_data_type` answers 2715 for two different faults, an unknown name and a
/// parameter out of bounds, because the numbers of the second family are not in the
/// catalogue yet. The two are told apart by asking the same function about the **name
/// alone**: each known type resolves without arguments, so a failure there means the
/// name itself is unknown, and a success means the arguments are what was refused. That
/// keeps the list of type names in one place, `datatype.rs`.
///
/// # Errors
///
/// **243** (`SqlError::not_a_defined_system_type`) for an unknown name; the error of
/// `resolve_data_type`, unchanged, for a parameter out of bounds. Both carry `line`, which
/// the caller already computed: `SELECT CAST(1 AS foo);` reports the line of the `CAST`,
/// not of the type name.
fn target_type(ty: &DataType, line: u32) -> SqlResult<SqlType> {
    match resolve_data_type(ty, CONVERSION_TYPE_POSITION) {
        Ok(resolved) => Ok(cast_default_length(resolved, ty.args.is_empty())),
        Err(_) if !is_known_type_name(ty) => {
            Err(SqlError::not_a_defined_system_type(&ty.name).with_line(line))
        }
        Err(err) => Err(err),
    }
}

/// Whether `ty.name` names a known type, the arguments written after it left aside.
///
/// Asks [`resolve_data_type`] about the name with **no** argument, a form each type
/// accepts (each parameter has a declaration default).
fn is_known_type_name(ty: &DataType) -> bool {
    let bare = DataType {
        name: ty.name.clone(),
        args: Vec::new(),
        span: ty.span,
    };
    resolve_data_type(&bare, CONVERSION_TYPE_POSITION).is_ok()
}

/// Applies [`CAST_DEFAULT_LEN`] to a character or binary target written without a length.
///
/// `without_argument` is false as soon as the user wrote parentheses, so `varchar(1)` stays
/// `varchar(1)` and the bare `varchar` becomes `varchar(30)`.
fn cast_default_length(ty: SqlType, without_argument: bool) -> SqlType {
    if !without_argument {
        return ty;
    }
    let len = Len::Fixed(CAST_DEFAULT_LEN);
    match ty {
        SqlType::Char(_) => SqlType::Char(len),
        SqlType::VarChar(_) => SqlType::VarChar(len),
        SqlType::NChar(_) => SqlType::NChar(len),
        SqlType::NVarChar(_) => SqlType::NVarChar(len),
        SqlType::Binary(_) => SqlType::Binary(len),
        SqlType::VarBinary(_) => SqlType::VarBinary(len),
        other => other,
    }
}

/// Binds the third argument of `CONVERT` into the `i32` style the bound node carries.
///
/// # Errors
///
/// An **internal** error when the expression is not an integer constant. SQL Server
/// accepts a variable there (`DECLARE @s int = 112; SELECT CONVERT(varchar(30), GETDATE(),
/// @s);` answers the date in style 112), so this is a deliberate difference from SQL
/// Server: the bound plan carries the style as a number because `types::convert`
/// dispatches on it, and the binder does not evaluate anything.
fn bind_style(style: &Expr, ctx: &BindContext<'_>, scope: &Scope) -> SqlResult<i32> {
    let bound = bind_operand(style, ctx, scope)?;
    let BoundExprKind::Literal(value) = &bound.kind else {
        return Err(bug(
            "CONVERT: the style must be an integer constant (SQL Server also accepts an \
             expression, which is not implemented yet)",
        ));
    };
    if bound.ty.ty.family() != TypeFamily::Integer {
        return Err(bug(format!(
            "CONVERT: the style must be an integer constant, not a {}",
            bound.ty.ty.error_name()
        )));
    }
    match value {
        Value::I8(n) => Ok(i32::from(*n)),
        Value::I16(n) => Ok(i32::from(*n)),
        Value::I32(n) => Ok(*n),
        Value::I64(n) => i32::try_from(*n)
            .map_err(|_| bug("CONVERT: the style does not fit in an int".to_owned())),
        other => Err(bug(format!(
            "CONVERT: the style must be an integer constant, not {other:?}"
        ))),
    }
}

/// Binds a function call `f(a, b)` against the `sysfn` registry.
///
/// The name is the **last** component of the `ObjectName`, and the whole typing is
/// [`vauban_sysfn::check_call`]'s: the binder adds nothing but the line of the node.
///
/// # Qualified names
///
/// A qualifier, whichever it is, answers 4121/16/1 (neither a column nor a user-defined
/// function of that name): `SELECT foo.bar(1);`, `SELECT dbo.LEN('abc');` and
/// `SELECT sys.LEN('abc');` alike, through `SqlError::cannot_find_column_or_function`.
///
/// # Errors
///
/// - **195**, severity 15, state 10, with the name spelled as the user wrote it — the
///   message echoes the source text and does **not** upper-case it,
///   `SELECT no_such_fn(1);` printing `'no_such_fn'` and `SELECT NO_SUCH_FN(1);` printing
///   `'NO_SUCH_FN'` — and the line of the node;
/// - **4121** for a qualified name, see above;
/// - 174, 189 and 8116 from `check_call`, passed on untouched but for the line;
/// - **155**, severity 15, state 1, naming the keyword and the function, for the five
///   functions whose first argument is a `datepart` keyword ([`bind_arguments`]);
/// - 8116 and 4151 for an untyped `NULL` argument the function refuses
///   ([`untyped_null_is_refused`]); an untyped `NULL` the function accepts is retyped from
///   its siblings first ([`retype_untyped_nulls`]);
/// - an internal error for `COUNT(*)`, for `DISTINCT` in a call and for a call whose
///   definition is an aggregate — the select list that holds one is routed to
///   `aggregate.rs` by `query.rs` before reaching here — and one for `OVER`.
pub(crate) fn bind_function(
    e: &Expr,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<BoundExpr> {
    let Expr::Function {
        name,
        args,
        star,
        distinct,
        over,
        span,
    } = e
    else {
        return Err(bug("bind_function: the node is not a function call"));
    };
    let line = line_of(span);
    let def = lookup_function(name, line)?;
    if *star {
        return Err(bug(
            "COUNT(*): the aggregate calls of a select list are not implemented yet",
        ));
    }
    if *distinct {
        return Err(bug(
            "DISTINCT in a function call: the aggregate calls of a select list are not \
             implemented yet",
        ));
    }
    if over.is_some() {
        return Err(bug("OVER: the window functions are not implemented yet"));
    }
    if def.kind == FunctionKind::Aggregate {
        return Err(bug(format!(
            "{}: the aggregate calls of a select list are not implemented yet",
            def.name
        )));
    }
    let mut bound: Vec<BoundExpr> = bind_arguments(def, args, line, ctx, scope)?;
    refuse_untyped_nulls(def, args, line)?;
    retype_untyped_nulls(&mut bound, args);
    let types: Vec<TypeInfo> = bound.iter().map(|arg| arg.ty.clone()).collect();
    let ty = check_call(def, &types).map_err(|err| err.with_line(line))?;
    let ty = with_inferred_nullability(def, &bound, ty);
    let ty = with_narrowed_nullif(def, &bound, ty);
    Ok(BoundExpr {
        kind: BoundExprKind::Function { def, args: bound },
        ty,
        line,
    })
}

/// Narrows the result type of `NULLIF` over an integer literal, as SQL Server does.
///
/// `sysfn` gives `NULLIF(a, b)` the type of `a` and cannot do better: a `return_type`
/// receives types, not nodes. SQL Server types the call on the **literal itself** when `a`
/// is one, taking the narrowest of `tinyint`, `smallint` and `int` that holds its value
/// (on the COLMETADATA of `SELECT NULLIF(…, 9) AS c WHERE 1 = 0;`):
///
/// | `a` | 0 | 255 | 256 | 32767 | −1 | −32768 | −32769 | 2147483647 | 2147483648 |
/// |---|---|---|---|---|---|---|---|---|---|
/// | type of `NULLIF(a, 9)` | `tinyint` | `tinyint` | `smallint` | `smallint` | `smallint` | `smallint` | `int` | `int` | `numeric` |
///
/// The last column needs nothing: `2147483648` is already a `numeric(10, 0)` literal
/// ([`vauban_types::parse_literal`]), so the result is either one of the three integer
/// types above or the type the argument already had — the narrowing does not widen.
///
/// **What is not narrowed** tells the rule from its neighbours. The construct matters:
/// `SELECT -1;`, `SELECT ISNULL(-1, 0);`, `SELECT COALESCE(-1, 0);`, `SELECT CASE WHEN 1 =
/// 1 THEN -1 ELSE NULL END;` and `SELECT IIF(1 = 1, 1, NULL);` are all `int` on the same
/// literals, so this is `NULLIF`'s own rule and not a rule about literals. The shape of
/// the argument matters too: `NULLIF(1 + 1, 9)`, `NULLIF(ABS(-1), 9)`, `NULLIF(CAST(1 AS
/// int), 9)`, `NULLIF(CONVERT(int, 1), 9)` and `NULLIF(@v, 9)` with `@v int` are `int`,
/// while `NULLIF((1), 9)` is a `tinyint` and `NULLIF(-(1), 9)` a `smallint` — parentheses
/// and the unary minus are part of the literal, an operator or a conversion is not. Hence
/// [`narrowed_literal`] walks [`BoundExprKind::Negate`] and stops at anything else, where
/// [`folded_integer`] also walks conversions.
///
/// The first argument alone counts: `NULLIF(CAST(1 AS int), CAST(0 AS bigint))` is an
/// `int`, so the narrowing has no second operand to consider.
///
/// The visible consequence is not the column type but the **result set**: the compile-time
/// length check of the executor fires on an `int` argument and not on a `smallint` one, so
/// `SELECT SUBSTRING('abc', 1, NULLIF(-1, 0));` opens a result set and fails at run time
/// while `SELECT SUBSTRING('abc', 1, NULLIF(-100000, 0));` opens no result set on SQL
/// Server.
fn with_narrowed_nullif(def: &FunctionDef, args: &[BoundExpr], ty: TypeInfo) -> TypeInfo {
    if def.name != "NULLIF" {
        return ty;
    }
    match args.first().and_then(narrowed_literal) {
        Some(narrowed) => TypeInfo { ty: narrowed, ..ty },
        None => ty,
    }
}

/// The narrowest of `tinyint`, `smallint` and `int` that holds `e`, when `e` is an integer
/// literal under unary minuses — and `None` for a node of another shape, a conversion
/// included, or for a value outside the three ranges ([`with_narrowed_nullif`], test
/// `nullif_narrows_neither_another_function_nor_another_shape`).
fn narrowed_literal(e: &BoundExpr) -> Option<SqlType> {
    let value = narrowed_value(e)?;
    [SqlType::TinyInt, SqlType::SmallInt, SqlType::Int]
        .into_iter()
        .find(|target| integer_fits(value, target))
}

/// The integer value of a literal under unary minuses, for [`narrowed_literal`].
///
/// A `decimal` literal of scale 0 counts: `2147483648` is a `numeric(10, 0)` and not a
/// `bigint` in T-SQL, yet `SELECT NULLIF(-2147483648, 9);` is an `int` on SQL Server. A
/// scale of one digit or more is not an integer literal: `SELECT NULLIF(1.50, 9);` stays
/// `numeric`.
fn narrowed_value(e: &BoundExpr) -> Option<i128> {
    match &e.kind {
        BoundExprKind::Literal(Value::I8(v)) => Some(i128::from(*v)),
        BoundExprKind::Literal(Value::I16(v)) => Some(i128::from(*v)),
        BoundExprKind::Literal(Value::I32(v)) => Some(i128::from(*v)),
        BoundExprKind::Literal(Value::I64(v)) => Some(i128::from(*v)),
        BoundExprKind::Literal(Value::Decimal(d)) if d.scale == 0 => Some(d.mantissa),
        BoundExprKind::Negate(inner) => narrowed_value(inner).map(|v| -v),
        _ => None,
    }
}

/// Finds the definition a call name denotes, or raises error 195.
///
/// Lookup is case-insensitive ([`vauban_sysfn::lookup`]) but the message prints the
/// name **as written**.
fn lookup_function(name: &ObjectName, line: u32) -> SqlResult<&'static FunctionDef> {
    if name.server.is_some() || name.database.is_some() || name.schema.is_some() {
        return Err(
            SqlError::cannot_find_column_or_function(&qualified_name(name)).with_line(line),
        );
    }
    let written = name.name.value.as_str();
    lookup(written).ok_or_else(|| unknown_function(written, line))
}

/// The parts of a function name joined by dots, as message 4121 prints them: the
/// identifiers themselves, delimiters already stripped by the parser
/// (`SELECT [dbo].[LEN](1);` prints `dbo.LEN`).
fn qualified_name(name: &ObjectName) -> String {
    [
        name.server.as_ref(),
        name.database.as_ref(),
        name.schema.as_ref(),
        Some(&name.name),
    ]
    .into_iter()
    .flatten()
    .map(|ident| ident.value.as_str())
    .collect::<Vec<_>>()
    .join(".")
}

/// Error 195 for a name no built-in function answers to.
fn unknown_function(written: &str, line: u32) -> SqlError {
    SqlError::not_a_recognized_name(written, "built-in function").with_line(line)
}

/// The built-in functions whose **first** argument is a `datepart` keyword and not an
/// expression, upper-cased as the registry spells them.
///
/// Five names, one rule:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT DATEPART(foo, GETDATE());` | 155/15/1, `'foo'` not a recognized `datepart` option |
/// | `SELECT DATENAME(foo, GETDATE());` | 155/15/1 … `datename` option |
/// | `SELECT DATEADD(foo, 1, GETDATE());` | 155/15/1 … `dateadd` option |
/// | `SELECT DATEDIFF(foo, GETDATE(), GETDATE());` | 155/15/1 … `datediff` option |
/// | `SELECT DATETRUNC(foo, GETDATE());` | 155/15/1 … `datetrunc` option |
///
/// The word before `option` is the **function that was called**, lower-cased, which is why
/// nothing here reads `datepart` as a constant. A name of the list the registry does not
/// know yet answers 195 from [`bind_function`] until it is registered.
///
/// `YEAR`, `MONTH` and `DAY` are **not** here: their keyword is written into the name, so
/// their only argument is an ordinary expression.
const DATEPART_KEYWORD_FUNCTIONS: [&str; 5] =
    ["DATEPART", "DATENAME", "DATEADD", "DATEDIFF", "DATETRUNC"];

/// The position of the `datepart` keyword in a call of `def`, `None` when `def` takes none.
///
/// The first argument for the five functions of [`DATEPART_KEYWORD_FUNCTIONS`], however
/// many arguments follow it (`DATEADD` has two, `DATEPART` one).
fn datepart_keyword_position(def: &FunctionDef) -> Option<usize> {
    DATEPART_KEYWORD_FUNCTIONS.contains(&def.name).then_some(0)
}

/// Binds the arguments of a call, reading the `datepart` keyword of the five functions that
/// take one as a keyword instead of as an expression.
///
/// # Why the keyword is refused here and not at evaluation
///
/// SQL Server refuses an unknown keyword while **compiling** the batch. What separates the
/// two hypotheses is a statement that does not evaluate its select list, because a query
/// that does evaluate it answers the same either way and separates nothing;
/// `SELECT 1 / 0 WHERE 1 = 0;` answering zero rows and no error is the control that fixes
/// what "not evaluated" means:
///
/// | Query | Answer | Says |
/// |---|---|---|
/// | `SELECT DATEPART(foo, GETDATE()) WHERE 1 = 0;` | 155, **no result set** | compiled, never ran |
/// | `SELECT TOP 0 DATEPART(foo, GETDATE());` | 155, no result set | idem |
/// | `SELECT 1; SELECT DATEPART(foo, GETDATE());` | 155, no result set at all, not even the `1` | the whole batch compiles first |
/// | `SELECT 1; SELECT no_such_column;` | 207, no result set at all | 155 behaves like a binding error |
///
/// And the same three shapes say the opposite of error **9810**, which is therefore *not*
/// moved here and stays in `sysfn`'s evaluation:
///
/// | Query | Answer | Says |
/// |---|---|---|
/// | `SELECT DATEPART(hour, CAST('2020-03-01' AS date)) WHERE 1 = 0;` | one `int` column, 0 rows, **no error** | never evaluated, never raised |
/// | `SELECT TOP 0 DATEPART(hour, CAST('2020-03-01' AS date));` | idem | idem |
/// | `SELECT 1; SELECT DATEPART(hour, CAST('2020-03-01' AS date));` | the row `1`, then 9810 | the first statement ran |
/// | `SELECT 1; SELECT 1 / 0;` | the row `1`, then 8134 | 9810 behaves like an execution error |
///
/// # Order of the checks
///
/// The reason the keyword is read before the other arguments are bound:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT NOSUCHFN(foo, 1);` | 195 — an unknown function is found before its arguments are looked at |
/// | `SELECT DATEPART(foo);` | 174 (`datepart` requires 2 arguments) — the arity beats the keyword |
/// | `SELECT DATEPART(foo, no_such_column);` | 155 — the keyword beats error 207 on the next argument |
/// | `SELECT DATEPART(year, no_such_column);` | 207 — when the keyword is good |
/// | `SELECT DATEPART(foo, CAST('13:45:30' AS time(7)));` | 155 — the keyword beats 9810 too, which is just as well since 9810 comes later |
///
/// So, **on SQL Server**: 195 (the caller), then the arity, then the keyword, then the rest
/// of the arguments. The arity is not raised here — `check_call` owns the wording — a
/// wrong one turns the keyword check off, which lets the call reach `check_call` with its
/// 174.
///
/// Past the keyword the order above is SQL Server's and not ours, and the difference is
/// wider than this function: the binder binds the arguments before counting them.
/// `SELECT DATEPART(foo, no_such_column, 1);` answers 174 on SQL Server and 207 here, and
/// `SELECT LEN('a', no_such_column);` diverges the same way on a function that takes no
/// keyword. A general difference of the binder, left as is.
fn bind_arguments(
    def: &FunctionDef,
    args: &[Expr],
    line: u32,
    ctx: &BindContext<'_>,
    scope: &Scope,
) -> SqlResult<Vec<BoundExpr>> {
    let keyword = datepart_keyword_position(def);
    let validate = def.arity.accepts(args.len());
    let mut bound = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        let node = match (keyword == Some(index), arg) {
            (true, Expr::Column(column)) => bind_datepart_keyword(def, column, validate, line)?,
            (true, _) if validate => {
                // Keep the variable, unknown-function and arity diagnostics. A column or
                // cast-type error in the keyword expression instead yields 1023
                // (`foo + 1`, `LEN(foo)`, `CAST(1 AS nosuch)`).
                if let Err(error) = bind_operand(arg, ctx, scope)
                    && matches!(error.number, 137 | 174 | 189 | 195 | 8631)
                {
                    return Err(error);
                }
                return Err(SqlError::invalid_function_parameter(
                    1,
                    &def.name.to_ascii_lowercase(),
                )
                .with_line(line));
            }
            _ => bind_operand(arg, ctx, scope)?,
        };
        bound.push(node);
    }
    Ok(bound)
}

/// Turns the name written in the keyword position into the literal the evaluation reads
/// back, or raises error **155**.
///
/// # What counts as the keyword
///
/// The **written** name, delimiters stripped and case kept, which is what the message
/// echoes. `SELECT DATEPART([year], '2020-03-01');` answers `2020`, so a delimited
/// identifier is a keyword like any other; `SELECT DATEPART([FoO], GETDATE());` and
/// `SELECT DATEPART(FoO, GETDATE());` both answer 155 naming `'FoO'`, so the case is
/// neither folded nor canonicalized; and `SELECT DATEPART(t.year, GETDATE());` answers 155
/// naming `'t.year'`, so a qualified name is printed whole and is not a keyword.
///
/// Trailing spaces are the one part of the spelling the recognition drops; see
/// [`recognized_spelling`], which also shows they are kept in the message.
///
/// # What is not a name
///
/// `DATEPART('year', GETDATE())`, `DATEPART(year + 1, GETDATE())`,
/// `DATEPART(GETDATE(), GETDATE())` and a declared varchar variable answer 1023/15/1. An
/// undeclared `@v` remains 137. The five bare niladic names also raise 1023. The same
/// string keyword with `WHERE 1 = 0` still raises 1023 during binding.
///
/// # The literal
///
/// A `varchar` holding the keyword, not `NULL`. It exists because `sysfn`'s evaluation
/// reads its first argument as a value; passing the user's spelling rather than the
/// canonical one keeps the safety net's own 155 able to echo the user's text, and costs
/// nothing since [`parse_datepart`] ignores case.
///
/// It holds the spelling [`recognized_spelling`] returns and not the raw one, so that a
/// keyword the binding accepted is a keyword the evaluation accepts too. Nothing is lost:
/// the trailing spaces do not reach the client, since error **9810** — the one later
/// message that names the component — prints the *canonical* keyword, `hour`, on
/// `SELECT DATEPART(hour, CAST('2020-03-01' AS date));`,
/// `SELECT DATEPART(hh, CAST('2020-03-01' AS date));` and
/// `SELECT DATEPART([hh ], CAST('2020-03-01' AS date));` alike.
///
/// The declared length stays that of the **written** name, an upper bound of the text it
/// holds: a `varchar(n)` takes a shorter value, and this type is not sent to a client —
/// it is read by `check_call` and by the `return_type` of `sysfn`, which look at the other
/// arguments. Keeping it also keeps the length away from `0`, which `[   ]` would give.
fn bind_datepart_keyword(
    def: &FunctionDef,
    column: &ColumnRef,
    validate: bool,
    line: u32,
) -> SqlResult<BoundExpr> {
    let written = written_name(column);
    let keyword = recognized_spelling(&written);
    if validate
        && column.qualifier.is_none()
        && !column.name.quoted
        && NILADIC_FUNCTIONS.contains(&written.to_ascii_uppercase().as_str())
    {
        return Err(
            SqlError::invalid_function_parameter(1, &def.name.to_ascii_lowercase()).with_line(line),
        );
    }
    if validate && parse_datepart(keyword).is_none() {
        return Err(
            SqlError::not_a_recognized_option(&written, &def.name.to_ascii_lowercase())
                .with_line(line),
        );
    }
    let len = u16::try_from(written.len()).unwrap_or(u16::MAX);
    let text = keyword.to_owned();
    Ok(BoundExpr {
        ty: TypeInfo::new(SqlType::VarChar(Len::Fixed(len)), false),
        kind: BoundExprKind::Literal(Value::String(SqlString { text })),
        line: line_of(&column.span),
    })
}

/// The part of a written name the `datepart` recognition looks at: everything but its
/// **trailing spaces**.
///
/// # The rule, and its exact edge
///
/// SQL Server ignores the spaces that end the name when it looks the keyword up, and keeps
/// them when it quotes the name back:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT DATEPART([year ], GETDATE());` | `2026` — recognized |
/// | `SELECT DATEPART([year  ], GETDATE());` | `2026` — any number of them |
/// | `SELECT DATEPART([yy ], GETDATE());` | `2026` — abbreviations too |
/// | `SELECT DATEPART([YeAr ], GETDATE());` | `2026` — and the case is still folded |
/// | `SELECT DATENAME([month ], GETDATE());` | `September` — not a quirk of `DATEPART` |
/// | `SELECT DATEADD([year ], 1, GETDATE());` | a `datetime` a year later |
/// | `SELECT DATEDIFF([year ], GETDATE(), GETDATE());` | `0` |
/// | `SELECT DATETRUNC([year ], GETDATE());` | `2026-01-01 00:00:00.000` |
/// | `SELECT DATEPART([foo ], GETDATE());` | 155 naming `'foo '` — the space **is** in the message |
/// | `SELECT DATENAME([foo ], GETDATE());` | 155 naming `'foo '` |
///
/// So the same five functions of [`DATEPART_KEYWORD_FUNCTIONS`] share it, the trimming is
/// for the lookup alone, and the message keeps the name letter for letter — which is why
/// this returns a slice and the caller keeps `written` for the error.
///
/// # What it is not
///
/// It is **not** a rule about identifiers, and it is **not** about whitespace. Each line
/// below answers 155 quoting the name exactly as it was written, escapes included:
///
/// | Query | Answer |
/// |---|---|
/// | `SELECT 1 AS [a ];` | a column really named `a `, space kept — identifiers are untouched |
/// | `SELECT DATEPART([ year], GETDATE());` | 155 `' year'` — leading spaces count |
/// | `SELECT DATEPART([ year ], GETDATE());` | 155 `' year '` — one leading space is enough |
/// | `SELECT DATEPART([ye ar], GETDATE());` | 155 `'ye ar'` — inner spaces count |
/// | `SELECT DATEPART([year\t], GETDATE());` | 155 `'year\t'` — a tab is not a space |
/// | `SELECT DATEPART([year\n], GETDATE());` | 155 `'year\n'` — nor a newline |
/// | `SELECT DATEPART([year\r], GETDATE());` | 155 `'year\r'` — nor a carriage return |
/// | `SELECT DATEPART([year\u{a0}], GETDATE());` | 155 `'year\u{a0}'` — nor a no-break space |
/// | `SELECT DATEPART([   ], GETDATE());` | 155 `'   '` — trimming to nothing matches nothing |
///
/// Hence `trim_end_matches(' ')` and not `trim_end`, whose definition of whitespace covers
/// the four characters the three middle lines refute.
fn recognized_spelling(written: &str) -> &str {
    written.trim_end_matches(' ')
}

/// A column reference as the user wrote it, qualifier included, delimiters already stripped
/// by the parser: `year` stays `year`, `[FoO]` becomes `FoO`, `t.year` becomes `t.year`.
fn written_name(column: &ColumnRef) -> String {
    match &column.qualifier {
        Some(qualifier) => format!("{}.{}", qualified_name(qualifier), column.name.value),
        None => column.name.value.clone(),
    }
}

/// Refuses the arguments written as the bare constant `NULL` that their function rejects.
///
/// There is **no general rule**: `SELECT CHARINDEX('b', NULL);`, `SELECT LEN(NULL);`,
/// `SELECT REPLACE(NULL, 'a', 'b');`, `SELECT REVERSE(NULL);`, `SELECT REPLICATE(NULL,
/// 2);`, `SELECT STUFF(NULL, 1, 1, 'x');`, `SELECT ROUND(NULL, 1);`, `SELECT ISNULL(NULL,
/// 'x');`, `SELECT NULLIF(1, NULL);` and `SELECT PATINDEX(NULL, 'abc');` are accepted by
/// SQL Server, and the refused calls are few. So the check is a list of refusals
/// ([`untyped_null_is_refused`]) and not an inference.
///
/// The test is on the **AST**, not on the bound type: `bind_literal` types a bare `NULL` as
/// a nullable `int`, which `CAST(NULL AS int)` also produces, and SQL Server tells the two
/// apart — `SELECT PATINDEX('%bc%', CAST(NULL AS varchar(10)));` is accepted.
///
/// Skipped when the arity is already wrong, so that 174 and 189 keep the precedence they
/// have inside `check_call`.
fn refuse_untyped_nulls(def: &FunctionDef, args: &[Expr], line: u32) -> SqlResult<()> {
    if !def.arity.accepts(args.len()) {
        return Ok(());
    }
    for (index, arg) in args.iter().enumerate() {
        if !is_untyped_null(arg) {
            continue;
        }
        let position = u8::try_from(index + 1).unwrap_or(u8::MAX);
        if let Some(error) = untyped_null_is_refused(def.name, position, line) {
            return Err(error);
        }
    }
    Ok(())
}

/// The error a bare `NULL` at `position` of `function` deserves, `None` when it is
/// accepted.
///
/// Each entry is quoted with the query SQL Server refuses. A function that is not listed
/// accepts a bare `NULL` like any other nullable argument.
fn untyped_null_is_refused(function: &str, position: u8, line: u32) -> Option<SqlError> {
    match (function, position) {
        // `SELECT PATINDEX('%bc%', NULL);` → 8116, while argument 1 and the whole of
        // CHARINDEX accept it.
        //
        // `SELECT DATEADD(day, NULL, CAST('2020-01-01' AS date));` → 8116 on argument 2,
        // where `retype_untyped_nulls` would make `check_call` name the sibling `date`;
        // `SELECT DATEADD(day, 1, NULL);` is a `datetime` and is accepted.
        //
        // `SELECT SUBSTRING(NULL, 1, 1);` → 8116 on argument 1, while `SELECT LEFT(NULL,
        // 1);` and `SELECT UPPER(NULL);` are accepted `varchar`.
        ("PATINDEX", 2) | ("DATEADD", 2) | ("SUBSTRING", 1) => Some(
            SqlError::invalid_argument_type("NULL", position, &function.to_ascii_lowercase())
                .with_line(line),
        ),
        // `SELECT NULLIF(NULL, 1);` → 4151/16/1, the symmetric of the 4127 of COALESCE;
        // `SELECT NULLIF(1, NULL);` is accepted.
        ("NULLIF", 1) => Some(SqlError::nullif_first_argument_null().with_line(line)),
        _ => None,
    }
}

/// Gives the arguments written as the bare constant `NULL` the type their siblings imply.
///
/// A bare `NULL` has no type of its own. `bind_literal` types it as a nullable `int`,
/// which is what `SELECT NULL` announces, and adds: "`ISNULL`, `COALESCE` and `CASE`
/// replace it by the type of the other branch". In a call, "the other branch" is
/// the rest of the argument list: `SELECT ISNULL(NULL, 'x');` answers a `varchar` holding
/// `x`, not an `int`, and `SELECT CHARINDEX('b', NULL);` types its second argument as a
/// string. The substituted type is the one
/// [`implicit_result_type`](vauban_types::implicit_result_type) folds over the
/// arguments that **are** typed — the same precedence rule `COALESCE` uses — and the result
/// stays nullable.
///
/// The bound argument itself is retyped, not only the list handed to `check_call`: the
/// contract of [`BoundExprKind::Function`] is that the executor builds its `EvalArgs` from
/// `args[i].ty`, so the two must agree.
///
/// Nothing happens when the fold has no answer: no typed argument at all
/// (`COALESCE(NULL, NULL)`, which SQL Server answers with 4127, not raised here) or two
/// arguments with no common type. The bare `NULL` then keeps its `int`, which is a
/// difference from SQL Server and not a rule (`SELECT REVERSE(NULL);` answers a `varchar`
/// there).
fn retype_untyped_nulls(bound: &mut [BoundExpr], args: &[Expr]) {
    if !args.iter().any(is_untyped_null) {
        return;
    }
    let mut context: Option<TypeInfo> = None;
    for (argument, written) in bound.iter().zip(args) {
        if is_untyped_null(written) {
            continue;
        }
        context = match context {
            None => Some(argument.ty.clone()),
            Some(current) => match implicit_result_type(&current, &argument.ty) {
                Ok(common) => Some(common),
                // No common type: the call is refused by `check_call` or by the operator
                // that reads it, with its own message. Nothing to substitute.
                Err(_) => return,
            },
        };
    }
    let Some(context) = context else {
        return;
    };
    for (argument, written) in bound.iter_mut().zip(args) {
        if is_untyped_null(written) {
            argument.ty = TypeInfo {
                nullable: true,
                ..context.clone()
            };
        }
    }
}

/// Whether an expression is the bare constant `NULL`, parentheses aside.
///
/// Shared with `expr.rs`, which asks the same question of the operands of an operator
/// ([`crate::expr`]): the test is on the **AST**, because `bind_literal` gives the bare
/// `NULL` the same nullable `int` that `CAST(NULL AS int)` produces.
pub(crate) fn is_untyped_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(Literal::Null, _) => true,
        Expr::Nested(inner, _) => is_untyped_null(inner),
        _ => false,
    }
}

/// Binds a global variable `@@ROWCOUNT`, `@@SPID`, `@@VERSION`… — a function of the
/// registry called with no argument, not a variable.
///
/// `name` carries its `@@` prefix, which is part of the registry key: `@@spid` and
/// `@@SPID` find the same definition.
///
/// # Errors
///
/// **137**, severity 15, state 2. SQL Server does not treat `@@x` as a function name in
/// its message: `SELECT @@NO_SUCH;` and `SELECT @no_such;` answer the same 137, with the
/// name quoted as written. Plus the error `check_call` raises on a definition whose
/// arity is not zero, which would be a bug of the registration.
pub(crate) fn bind_variable_function(name: &str, span: &Span) -> SqlResult<BoundExpr> {
    let line = line_of(span);
    let def =
        lookup(name).ok_or_else(|| SqlError::must_declare_scalar_variable(name).with_line(line))?;
    call_without_argument(def, line)
}

/// Binds a niladic function written **without parentheses**, or answers `None` when the
/// name is an ordinary one.
///
/// `CURRENT_TIMESTAMP`, `SESSION_USER`, `SYSTEM_USER`, `CURRENT_USER` and `USER` are the
/// five. The parser reads them as column references, so `expr.rs` calls this **before**
/// raising error 207 on an unknown column: `Some(..)` means the name was a function after
/// all.
///
/// Answers `None` — and not an error — when the name is delimited (`[USER]` is a column,
/// not a function), when it is not one of the five, and when the registry does not know it
/// yet: a name the engine cannot honour must stay a column, so that the user gets the 207
/// of `expr.rs` and not a message about a function.
///
/// The caller checks that the reference is **unqualified**: `t.USER` is a column of `t`,
/// and the caller alone holds the qualifier.
///
/// # Errors
///
/// Whatever `check_call` raises on a definition whose arity is not zero, which would be a
/// bug of the registration.
pub(crate) fn bind_niladic(name: &Ident, span: &Span) -> SqlResult<Option<BoundExpr>> {
    if name.quoted
        || !NILADIC_FUNCTIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&name.value))
    {
        return Ok(None);
    }
    match lookup(&name.value) {
        Some(def) => call_without_argument(def, line_of(span)).map(Some),
        None => Ok(None),
    }
}

/// Types a call of `def` with no argument at all, the shape both `@@x` and the niladic
/// functions have.
fn call_without_argument(def: &'static FunctionDef, line: u32) -> SqlResult<BoundExpr> {
    let ty = check_call(def, &[]).map_err(|err| err.with_line(line))?;
    let ty = with_inferred_nullability(def, &[], ty);
    Ok(BoundExpr {
        kind: BoundExprKind::Function {
            def,
            args: Vec::new(),
        },
        ty,
        line,
    })
}

/// Puts on the type `check_call` computed the nullability SQL Server infers for the call.
///
/// `sysfn` types a call from its arguments and, for most functions, makes the result
/// nullable when an argument is. SQL Server does not: a built-in function is nullable by
/// **what it is**, not by what it receives, which the `fNullable` bit of the COLMETADATA
/// token says function by function. Two shapes separate the three classes — a constant
/// argument at the top level of a `SELECT`, and a `DECLARE`d variable `@v` of the
/// argument's type, which is nullable and not constant:
///
/// | Class | `f(constant)` | `f(@v)` | Functions |
/// |---|---|---|---|
/// | [`FunctionNullability::Always`] | 1 | 1 | `LEN`, `UPPER`, `LEFT`, `ABS`, `SQRT`, `YEAR`, `DATEPART`, `NEWID`, `DB_NAME`, `@@SERVERNAME`… |
/// | [`FunctionNullability::Operands`] | 0 | 1 | `CEILING`, `FLOOR`, `ROUND`, `SIGN`, `DATEFROMPARTS` |
/// | [`FunctionNullability::Never`] | 0 | 0 | `CONCAT`, `PI`, `GETDATE`, `SYSDATETIME`, `@@SPID`, `@@ROWCOUNT`… |
///
/// `SELECT LEN('abc');` is nullable and `SELECT CEILING(1.5);` is not: the first column
/// tells `Always` from the two others. `SELECT CEILING(@f);` is nullable (so are `SIGN`,
/// `ROUND` and `FLOOR` of a variable) and `SELECT CONCAT(@s, 'x');` is not: the second
/// column tells `Operands` from `Never`. An argument that is neither constant nor nullable
/// leaves an `Operands` function non-nullable — `SELECT CEILING(ISNULL(@n, 1.5));` is
/// **not** nullable, nor are `FLOOR`, `ROUND` and `SIGN` of the same argument — which is
/// what "follows its arguments" means, and what a third shape would not tell from
/// `Never`. `CONCAT(@s, NULL)` is not nullable either. The
/// aggregates `MAX(1)`, `SUM(1)`, `COUNT(*)`, `COUNT_BIG(*)` are nullable and are
/// classified for the day they are bound; so are `@@VERSION` and `SERVERPROPERTY`, which
/// `vauban-compat` registers and this crate's tests do not see. The catalogue and session
/// functions (`OBJECT_ID`, `DB_ID`, `SUSER_SNAME`, `ISNUMERIC`, `@@IDENTITY`…) are
/// `Always` at each arity, `SCHEMA_NAME()` as much as `SCHEMA_NAME(1)`.
///
/// `ISNULL` and `COALESCE` have a rule of their own each ([`isnull_is_nullable`],
/// [`coalesce_is_nullable`]). A function the table does not know keeps the answer of
/// `sysfn`, which is that of `Operands`; the test `every_registered_function_is_classified`
/// makes sure the registry and the table do not drift apart.
///
/// The table lives here and not in each `return_type` of `sysfn`; moving each row next to
/// its function is a legitimate follow-up.
fn with_inferred_nullability(def: &FunctionDef, args: &[BoundExpr], ty: TypeInfo) -> TypeInfo {
    let nullable = match nullability_class(def.name) {
        FunctionNullability::Always => true,
        FunctionNullability::Never => false,
        FunctionNullability::Operands => ty.nullable,
        FunctionNullability::IsNull => isnull_is_nullable(args, &ty),
        FunctionNullability::Coalesce => coalesce_is_nullable(args, &ty),
    };
    TypeInfo { nullable, ..ty }
}

/// How SQL Server infers the nullability of a built-in function, see
/// [`with_inferred_nullability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionNullability {
    /// Nullable whatever the arguments, a constant included.
    Always,
    /// Nullable when an argument is: the answer `sysfn` computes.
    Operands,
    /// Never nullable, even with a nullable argument.
    Never,
    /// `ISNULL`, [`isnull_is_nullable`].
    IsNull,
    /// `COALESCE`, [`coalesce_is_nullable`].
    Coalesce,
}

/// The class of a function by its registry name, `Operands` for a name not listed.
fn nullability_class(name: &str) -> FunctionNullability {
    if ALWAYS_NULLABLE.contains(&name) {
        FunctionNullability::Always
    } else if NEVER_NULLABLE.contains(&name) {
        FunctionNullability::Never
    } else if OPERANDS_NULLABLE.contains(&name) {
        FunctionNullability::Operands
    } else {
        match name {
            "ISNULL" => FunctionNullability::IsNull,
            "COALESCE" => FunctionNullability::Coalesce,
            // Not listed: the answer of `sysfn`, until the test over the registry asks
            // for the function to be classified.
            _ => FunctionNullability::Operands,
        }
    }
}

/// The functions that are nullable whatever they receive ([`with_inferred_nullability`]).
const ALWAYS_NULLABLE: [&str; 65] = [
    "LEN",
    "UPPER",
    "LOWER",
    "LTRIM",
    "RTRIM",
    "LEFT",
    "RIGHT",
    "SUBSTRING",
    "REPLACE",
    "REVERSE",
    "REPLICATE",
    "STUFF",
    "CHARINDEX",
    "PATINDEX",
    "ASCII",
    "CHAR",
    "UNICODE",
    "NCHAR",
    "QUOTENAME",
    "DATALENGTH",
    "ABS",
    "SQRT",
    "POWER",
    "RAND",
    "YEAR",
    "MONTH",
    "DAY",
    "DATEPART",
    "DATENAME",
    "NEWID",
    "@@SERVERNAME",
    "@@VERSION",
    "SERVERPROPERTY",
    "DB_NAME",
    "USER",
    "CURRENT_USER",
    "SESSION_USER",
    "SYSTEM_USER",
    "COLLATIONPROPERTY",
    "SQL_VARIANT_PROPERTY",
    // The catalogue and session functions, at each arity: `@@IDENTITY`,
    // `SCOPE_IDENTITY()`, `IDENT_CURRENT`, `APP_NAME()`, `HOST_NAME()`, `DB_ID`, `ISDATE`,
    // `ISNUMERIC`, `OBJECT_ID`, `OBJECT_NAME`, `SCHEMA_NAME`, `SCHEMA_ID`, `SUSER_SNAME`,
    // `USER_NAME`; the date arithmetic `DATEADD`, `DATEDIFF`, `EOMONTH`; and `XACT_STATE`.
    "@@IDENTITY",
    "SCOPE_IDENTITY",
    "IDENT_CURRENT",
    "APP_NAME",
    "HOST_NAME",
    "DB_ID",
    "ISDATE",
    "ISNUMERIC",
    "OBJECT_ID",
    "OBJECT_NAME",
    "SCHEMA_NAME",
    "SCHEMA_ID",
    "SUSER_SNAME",
    "USER_NAME",
    "DATEADD",
    "DATEDIFF",
    "EOMONTH",
    "XACT_STATE",
    "NULLIF",
    "AVG",
    "COUNT",
    "COUNT_BIG",
    "MAX",
    "MIN",
    "SUM",
];

/// The functions that are never nullable ([`with_inferred_nullability`]).
const NEVER_NULLABLE: [&str; 12] = [
    "CONCAT",
    "PI",
    "GETDATE",
    "GETUTCDATE",
    "SYSDATETIME",
    "SYSUTCDATETIME",
    "CURRENT_TIMESTAMP",
    "@@SPID",
    "@@ROWCOUNT",
    "@@ERROR",
    "@@TRANCOUNT",
    "@@LOCK_TIMEOUT",
];

/// The functions whose result follows their arguments ([`with_inferred_nullability`]);
/// listed so that the test over the registry can tell "classified" from "forgotten".
///
/// `DATEFROMPARTS(2020, 2, 29)` → 0; `DATEFROMPARTS(@y, 2, 29)` with `@y int` → 1 and
/// `DATEFROMPARTS(ISNULL(@y, 1), 2, 29)` → 0, the 0/1 pair of this class, and the third
/// shape that tells it from `Always`.
const OPERANDS_NULLABLE: [&str; 5] = ["CEILING", "FLOOR", "ROUND", "SIGN", "DATEFROMPARTS"];

/// The nullability of `ISNULL(check, replacement)`: that of `check`, **and** that of the
/// replacement read as a value.
///
/// On the `fNullable` bit of COLMETADATA (`@i` a declared `int`, `@t` a declared
/// `tinyint`, `@s` a declared `varchar(10)`, `@d` a declared `datetime`):
///
/// | Query | `fNullable` | Says |
/// |---|---|---|
/// | `ISNULL(1, @i)`, `ISNULL(@@SPID, @i)` | 0 | a non-nullable `check` decides alone |
/// | `ISNULL(CAST(1 AS int), @i)`, `ISNULL(1 + 1, @i)` | 1 | `check` is read as it is announced, a folded `CAST` stays nullable |
/// | `ISNULL(@i, 1)`, `ISNULL(@i, ISNULL(@i, 1))` | 0 | |
/// | `ISNULL(@i, @i)`, `ISNULL(@i, NULL)`, `ISNULL(@i, CAST(NULL AS int))` | 1 | |
/// | `ISNULL(@i, CAST(1 AS int))`, `ISNULL(@i, 1 + 1)`, `ISNULL(@i, LEN('abc'))`, `ISNULL(@i, -CAST(1 AS int))` | 0 | a **constant** replacement is folded and read by its value |
/// | `ISNULL(@s, UPPER('a'))`, `ISNULL(@s, LOWER('a'))`, `ISNULL(@n, QUOTENAME('a'))` | 1 | three functions the server does not fold |
/// | `ISNULL(@i, TRY_CAST('x' AS int))`, `ISNULL(@i, NULLIF(1, 1))`, `ISNULL(@i, ASCII(''))` | 1 | a constant that folds to `NULL` |
/// | `ISNULL(@t, 300)`, `ISNULL(@t, CAST(300 AS int))`, `ISNULL(@n21, 10)` | 1 | a constant the type of `check` cannot hold |
/// | `ISNULL(@s, 'abcdefghijklmnop')` | 0 | a string literal is truncated, not refused |
/// | `ISNULL(@i, @@SPID + 1)`, `ISNULL(@i, ABS(@@SPID))`, `ISNULL(@s, UPPER(@s))` | 1 | a non-constant replacement is read as it is announced… |
/// | `ISNULL(@d, SYSDATETIME())`, `ISNULL(@s, CONCAT(ISNULL(@s, 'a'), 'x'))` | 1 | … **converted** to the type of `check` (`expr::implicit_conversion_may_be_null`) |
/// | `ISNULL(@i, @@SPID)`, `ISNULL(@d, GETDATE())` | 0 | a lossless conversion adds nothing |
///
/// The binder folds nothing, so the constant replacement is **read** rather
/// than computed: [`is_foldable`] says whether SQL Server would fold it, and
/// [`folded_may_be_null`] whether the folded value could be `NULL`. The second reading is
/// structural and errs on the nullable side where the value alone decides: `CHAR(300)` and
/// `STUFF('abc', 0, 1, 'x')` fold to `NULL`, `CHAR(65)` and `STUFF('abc', 1, 1, 'x')` do
/// not, and the binder cannot tell them apart — it calls the six functions that answer
/// `NULL` to a valid argument nullable, which keeps `tds` from meeting a `NULL` in a column
/// announced `NOT NULL`. `CASE WHEN 1 = 1 THEN 1 END` folds to `1` on the server and stays
/// nullable here for the same reason.
fn isnull_is_nullable(args: &[BoundExpr], result: &TypeInfo) -> bool {
    let [check, replacement] = args else {
        return true;
    };
    check.ty.nullable && replacement_may_be_null(replacement, &result.ty)
}

/// Whether the replacement of an `ISNULL`, converted to `target`, may be `NULL`.
fn replacement_may_be_null(e: &BoundExpr, target: &SqlType) -> bool {
    if !is_foldable(e) {
        return e.ty.nullable || implicit_conversion_may_be_null(&e.ty.ty, target);
    }
    match folded_integer(e) {
        Some(value) => !integer_fits(value, target),
        None => folded_may_be_null(e, target),
    }
}

/// The nullability of `COALESCE(a, b, …)`: that of the `CASE WHEN a IS NOT NULL THEN a
/// WHEN b IS NOT NULL THEN b … ELSE z END` it stands for, each
/// branch converted to the common type — nullable as soon as one branch is
/// ([`crate::expr`], `bind_case`).
///
/// On the `fNullable` bit of COLMETADATA (`@i` a declared `int`, `@n` a declared
/// `decimal(10,2)`):
///
/// | Query | `fNullable` |
/// |---|---|
/// | `COALESCE(ISNULL(@i, 1), 2)`, `COALESCE(ISNULL(@i, 1), @@SPID)` | 0 |
/// | `COALESCE(ISNULL(@i, 1), ISNULL(@n, 1.5))`, `COALESCE(ISNULL(@i, 1), 1.5)` | 0 — `int` fits the common `decimal` |
/// | `COALESCE(@i, 2)`, `COALESCE(@@SPID, @i)`, `COALESCE(NULL, @i, 1)` | 1 — a branch is nullable |
/// | `COALESCE(ISNULL(@i, 1), CAST(2 AS int))`, `COALESCE(ISNULL(@i, 1), LEN('abc'))` | 1 — a branch is announced nullable |
/// | `COALESCE(CAST(1 AS int), 2)`, `COALESCE(1 + 1, @i)` | 1 |
/// | `COALESCE(NULL, 1)`, `COALESCE(1, CAST(2 AS int))`, `COALESCE(1, 2)` | 0 |
/// | `COALESCE(1, 1.5)` | 1 — the `int` does not fit the common `numeric(2,1)` |
///
/// The last two rows are the server folding a `CASE` whose first condition is a constant:
/// a leading bare `NULL` drops out (`WHEN NULL IS NOT NULL` is false) and a leading
/// non-null **literal** is the whole answer (`WHEN 1 IS NOT NULL` is true), converted to
/// the common type — and that conversion counts. A literal alone is read that way —
/// `COALESCE(CAST(1 AS int), 2)` keeps the nullability the `CAST` announces — and in the
/// leading position alone, `COALESCE(NULL, @i, 1)` being nullable through `@i`.
fn coalesce_is_nullable(args: &[BoundExpr], result: &TypeInfo) -> bool {
    let mut rest = args
        .iter()
        .skip_while(|arg| is_null_literal(arg))
        .peekable();
    match rest.peek() {
        None => true,
        Some(first) if matches!(first.kind, BoundExprKind::Literal(_)) => {
            implicit_conversion_may_be_null(&first.ty.ty, &result.ty)
        }
        Some(_) => rest
            .any(|arg| arg.ty.nullable || implicit_conversion_may_be_null(&arg.ty.ty, &result.ty)),
    }
}

/// Whether `e` is the bound literal `NULL`, typed or not.
fn is_null_literal(e: &BoundExpr) -> bool {
    matches!(e.kind, BoundExprKind::Literal(Value::Null))
}

/// Whether SQL Server folds `e` at compile time: a tree of literals under operators,
/// conversions, `CASE` and deterministic built-in functions — save three.
///
/// What is **not** foldable: an expression that depends on a variable, a nondeterministic
/// function, and a few more this binder does not see. Three deterministic functions of the
/// registry are not folded — `ISNULL(@s, UPPER('a'))`, `ISNULL(@s, LOWER('a'))` and
/// `ISNULL(@n, QUOTENAME('a'))` are nullable where `ISNULL(@s, LTRIM('a'))` and twenty
/// other calls are not ([`isnull_is_nullable`]); [`NOT_FOLDED`] names them and nothing
/// explains them.
fn is_foldable(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Literal(_) => true,
        // A column has no value at bind time, no more than a variable.
        BoundExprKind::ColumnRef(_) | BoundExprKind::Variable { .. } => false,
        BoundExprKind::Arith { left, right, .. }
        | BoundExprKind::Compare { left, right, .. }
        | BoundExprKind::Logical { left, right, .. } => is_foldable(left) && is_foldable(right),
        BoundExprKind::Negate(inner)
        | BoundExprKind::BitNot(inner)
        | BoundExprKind::Not(inner)
        | BoundExprKind::IsNull { expr: inner, .. }
        | BoundExprKind::Convert { expr: inner, .. }
        | BoundExprKind::Collate { expr: inner } => is_foldable(inner),
        BoundExprKind::In { expr, list, .. } => is_foldable(expr) && list.iter().all(is_foldable),
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => is_foldable(expr) && is_foldable(pattern) && escape.as_deref().is_none_or(is_foldable),
        BoundExprKind::Case {
            operand,
            arms,
            else_,
        } => {
            operand.as_deref().is_none_or(is_foldable)
                && arms
                    .iter()
                    .all(|arm| is_foldable(&arm.when) && is_foldable(&arm.then))
                && else_.as_deref().is_none_or(is_foldable)
        }
        BoundExprKind::Function { def, args } => {
            def.kind == FunctionKind::Scalar
                && def.deterministic
                && !NOT_FOLDED.contains(&def.name)
                && args.iter().all(is_foldable)
        }
        // A subquery runs a plan, which is the executor's business and not a value the
        // binder may read.
        BoundExprKind::Exists(_)
        | BoundExprKind::ScalarSubquery(_)
        | BoundExprKind::InSubquery { .. } => false,
    }
}

/// The deterministic functions SQL Server does not fold ([`is_foldable`]).
const NOT_FOLDED: [&str; 3] = ["UPPER", "LOWER", "QUOTENAME"];

/// The functions that answer `NULL` to a valid, non-null argument, whose folded value the
/// binder therefore cannot read ([`folded_may_be_null`]): `CHAR(300)`, `NCHAR(-1)`,
/// `ASCII('')`, `UNICODE('')`, `STUFF('abc', 0, 1, 'x')`, `REPLICATE('a', -1)`,
/// `SQL_VARIANT_PROPERTY(1, 'foo')`, `COLLATIONPROPERTY('x', 'foo')`, `NULLIF(1, 1)`.
const NULL_ON_A_VALID_ARGUMENT: [&str; 9] = [
    "CHAR",
    "NCHAR",
    "ASCII",
    "UNICODE",
    "STUFF",
    "REPLICATE",
    "SQL_VARIANT_PROPERTY",
    "COLLATIONPROPERTY",
    "NULLIF",
];

/// Whether a foldable `e` ([`is_foldable`]) could fold to `NULL`, read on its shape: a
/// `NULL` literal, a `TRY_` conversion, a `CASE` without `ELSE` or with a branch that may,
/// a function of [`NULL_ON_A_VALID_ARGUMENT`] or of an argument that may. `ISNULL` and
/// `COALESCE` are `NULL` when their arguments are, `CONCAT` is not.
///
/// A predicate is not a value and does not reach here; it answers `false` for
/// completeness.
fn folded_may_be_null(e: &BoundExpr, target: &SqlType) -> bool {
    match &e.kind {
        BoundExprKind::Literal(Value::Null) => true,
        BoundExprKind::Literal(Value::String(_)) => false,
        BoundExprKind::Literal(_) => implicit_conversion_may_be_null(&e.ty.ty, target),
        // Neither is folded ([`is_foldable`]); the conservative answer is the one a
        // variable gets.
        BoundExprKind::ColumnRef(_) | BoundExprKind::Variable { .. } => true,
        BoundExprKind::Arith { left, right, .. } => {
            folded_may_be_null(left, target) || folded_may_be_null(right, target)
        }
        BoundExprKind::Negate(inner) | BoundExprKind::BitNot(inner) => {
            folded_may_be_null(inner, target)
        }
        BoundExprKind::Collate { expr } => folded_may_be_null(expr, target),
        BoundExprKind::Convert { expr, try_, .. } => *try_ || folded_may_be_null(expr, target),
        BoundExprKind::Case { arms, else_, .. } => {
            arms.iter().any(|arm| folded_may_be_null(&arm.then, target))
                || else_
                    .as_deref()
                    .is_none_or(|e| folded_may_be_null(e, target))
        }
        BoundExprKind::Function { def, args } => match def.name {
            "ISNULL" | "COALESCE" => args.iter().all(|arg| folded_may_be_null(arg, target)),
            "CONCAT" => false,
            name if NULL_ON_A_VALID_ARGUMENT.contains(&name) => true,
            _ => args.iter().any(|arg| folded_may_be_null(arg, target)),
        },
        BoundExprKind::Compare { .. }
        | BoundExprKind::Logical { .. }
        | BoundExprKind::Not(_)
        | BoundExprKind::IsNull { .. }
        | BoundExprKind::In { .. }
        | BoundExprKind::Like { .. }
        | BoundExprKind::Exists(_)
        | BoundExprKind::InSubquery { .. } => false,
        // Not folded ([`is_foldable`]), so the conservative answer is the one a column
        // reference gets.
        BoundExprKind::ScalarSubquery(_) => true,
    }
}

/// The value of an integer literal under conversions to integer types and unary minuses
/// — `300`, `-1`, `CAST(300 AS int)` — or `None` when `e` is anything else or when a
/// conversion on the way does not hold the value.
fn folded_integer(e: &BoundExpr) -> Option<i128> {
    match &e.kind {
        BoundExprKind::Literal(value) => match value {
            Value::Bit(bit) => Some(i128::from(*bit)),
            Value::I8(v) => Some(i128::from(*v)),
            Value::I16(v) => Some(i128::from(*v)),
            Value::I32(v) => Some(i128::from(*v)),
            Value::I64(v) => Some(i128::from(*v)),
            _ => None,
        },
        BoundExprKind::Negate(inner) => folded_integer(inner).map(|v| -v),
        BoundExprKind::Convert {
            expr, try_: false, ..
        } => folded_integer(expr).filter(|value| integer_fits(*value, &e.ty.ty)),
        _ => None,
    }
}

/// Whether an integer `value` is held by `target` without loss — an integer type by its
/// range, an exact numeric by its integral digits, `money` by its range; another target
/// holds it, or refuses it with an error that is not a `NULL`.
fn integer_fits(value: i128, target: &SqlType) -> bool {
    match target {
        SqlType::Bit => (0..=1).contains(&value),
        SqlType::TinyInt => i128::from(u8::MIN) <= value && value <= i128::from(u8::MAX),
        SqlType::SmallInt => i128::from(i16::MIN) <= value && value <= i128::from(i16::MAX),
        SqlType::Int => i128::from(i32::MIN) <= value && value <= i128::from(i32::MAX),
        SqlType::BigInt => i128::from(i64::MIN) <= value && value <= i128::from(i64::MAX),
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            decimal_digits(value) <= precision - scale
        }
        SqlType::Money => value.unsigned_abs() <= 922_337_203_685_477,
        SqlType::SmallMoney => value.unsigned_abs() <= 214_748,
        _ => true,
    }
}

/// The number of decimal digits of `value`, at least one.
fn decimal_digits(value: i128) -> u8 {
    let mut rest = value.unsigned_abs();
    let mut digits: u8 = 1;
    while rest >= 10 {
        rest /= 10;
        digits = digits.saturating_add(1);
    }
    digits
}

/// Whether SQL Server converts `from` to `to` at all, `CAST` and `CONVERT` included.
///
/// The conversion chart of SQL Server has four states (implicit, explicit, explicit with
/// information loss, forbidden) over 24 types; the binder needs the last one alone, and a
/// table of 24 × 24 is not what it takes to answer it. The rule is stated **by family**
/// ([`families_convert`]) with the exceptions named **by type** ([`exceptions_allow`]) —
/// twenty lines instead of five hundred and seventy-six.
///
/// A pair that converts but may still fail on a value (`varchar` → `int`) passes here and
/// raises 245 at execution time: `SELECT CAST('1' AS int);` succeeds and `SELECT CAST('x'
/// AS int);` does not, and neither is a binding error.
///
/// The simplification is deliberate: the pairs inside the `DateTime` family other than
/// `date` ↔ `time`, and the pairs whose two types share a family, are taken as legal.
fn is_castable(from: &SqlType, to: &SqlType) -> bool {
    families_convert(from.family(), to.family()) && exceptions_allow(from, to)
}

/// The family half of [`is_castable`]: `uniqueidentifier` alone is walled off.
///
/// `SELECT CAST(1 AS uniqueidentifier);`, `SELECT CAST(CAST(1 AS bit) AS
/// uniqueidentifier);`, `SELECT CAST(NEWID() AS int);` and `SELECT CAST(NEWID() AS
/// datetime);` answer 529, while `SELECT CAST(0x00000000000000000000000000000000 AS
/// uniqueidentifier);` succeeds. A GUID therefore converts to and from character and binary
/// types, and to nothing else.
fn families_convert(from: TypeFamily, to: TypeFamily) -> bool {
    match (from, to) {
        (TypeFamily::Guid, TypeFamily::Guid) => true,
        (TypeFamily::Guid, TypeFamily::Character | TypeFamily::Binary)
        | (TypeFamily::Character | TypeFamily::Binary, TypeFamily::Guid) => true,
        (TypeFamily::Guid, _) | (_, TypeFamily::Guid) => false,
        _ => true,
    }
}

/// The by-type half of [`is_castable`]: what the families are too coarse to say.
///
/// 1. The four date and time types that are **not** `datetime`/`smalldatetime` do not
///    convert to or from a number: `SELECT CAST(CAST('2020-01-01' AS date) AS
///    int);`, `SELECT CAST(CAST('2020-01-01' AS datetimeoffset) AS int);`, `SELECT CAST(1
///    AS date);`, `SELECT CAST(CAST(1 AS money) AS date);` and `SELECT CAST(CAST(1 AS
///    float) AS date);` answer 529, while `SELECT CAST(CAST('2020-01-01' AS datetime)
///    AS int);`, `SELECT CAST(1 AS datetime);`, `SELECT CAST(CAST(1 AS bit) AS datetime);`,
///    `SELECT CAST(CAST(1 AS money) AS datetime);` and `SELECT CAST(CAST(1 AS numeric(5,2))
///    AS datetime);` succeed. Binary is **not** concerned: `SELECT
///    CAST(CAST('2020-01-01' AS datetime2) AS varbinary(20));` succeeds, and `SELECT
///    CAST(0x00 AS date);` reaches execution and fails there with 241.
/// 2. `date` and `time` do not convert to each other: `SELECT CAST(CAST('2020-01-01' AS
///    date) AS time(7));` and `SELECT CAST(CAST('12:00' AS time) AS date);` both answer 529,
///    although both types are in the same family.
fn exceptions_allow(from: &SqlType, to: &SqlType) -> bool {
    if is_partial_datetime(from) && is_number(to.family())
        || is_partial_datetime(to) && is_number(from.family())
    {
        return false;
    }
    !matches!(
        (from, to),
        (SqlType::Date, SqlType::Time(_)) | (SqlType::Time(_), SqlType::Date)
    )
}

/// The date and time types that carry only a part of an instant, as opposed to `datetime`
/// and `smalldatetime`, which SQL Server still lets a number stand for.
fn is_partial_datetime(ty: &SqlType) -> bool {
    matches!(
        ty,
        SqlType::Date | SqlType::Time(_) | SqlType::DateTime2(_) | SqlType::DateTimeOffset(_)
    )
}

/// The families a number belongs to. `bit` counts: `CAST(CAST(1 AS bit) AS datetime)`
/// succeeds and `CAST(CAST(1 AS bit) AS date)` does not, exactly like `int`.
fn is_number(family: TypeFamily) -> bool {
    matches!(
        family,
        TypeFamily::Bit
            | TypeFamily::Integer
            | TypeFamily::ExactNumeric
            | TypeFamily::ApproxNumeric
            | TypeFamily::Money
    )
}

/// Binds a sub-expression: an argument of a call, the source of a conversion, a style.
///
/// Expression binding belongs to `expr.rs`: this is one line of wiring. It exists so that
/// the four entry points above read the same, and so that a scope can be threaded through
/// in one place.
fn bind_operand(e: &Expr, ctx: &BindContext<'_>, scope: &Scope) -> SqlResult<BoundExpr> {
    crate::expr::bind_expr(e, ctx, scope)
}

/// An engine bug, reported to the client as the generic error 50000.
fn bug(message: impl Into<String>) -> SqlError {
    SqlError::from(InternalError::Bug(message.into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        ALWAYS_NULLABLE, FunctionNullability, NEVER_NULLABLE, OPERANDS_NULLABLE, bind_cast,
        bind_convert, bind_function, bind_niladic, bind_variable_function,
        datepart_keyword_position, is_castable, nullability_class, recognized_spelling,
    };
    use crate::expr::Scope;
    use std::sync::Once;
    use vauban_errors::{SqlError, SqlResult};
    use vauban_parser::{ColumnRef, DataType, Expr, Ident, Literal, ObjectName, Span, TypeArg};
    use vauban_sysfn::{
        Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind, StaticContext, check_call, lookup,
        register, register_builtins,
    };
    use vauban_types::{Len, SqlType, TypeInfo, Value};

    use crate::bound::{BoundExpr, BoundExprKind};
    use crate::context::{BindContext, SessionOptions};

    /// The registry, filled once for the whole test binary.
    ///
    /// `register_builtins` registers the built-ins of `sysfn`; `vauban-compat` is **not** a
    /// development dependency of this crate, so `@@VERSION` and `SERVERPROPERTY` are absent
    /// and no test names them. The place-holders below fill the other gaps: see
    /// [`placeholder`].
    ///
    /// Shared with the tests of `expr.rs`, which bind the same nodes through
    /// [`bind_expr`](crate::expr::bind_expr): one registry for the whole test binary, so
    /// that no test depends on the order the others ran in.
    pub(crate) fn registry() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            register_builtins();
            placeholder("LEN", FunctionKind::Scalar, Arity::Exact(1), int_type);
            placeholder("ROUND", FunctionKind::Scalar, Arity::Range(2, 3), int_type);
            placeholder("SUM", FunctionKind::Aggregate, Arity::Exact(1), int_type);
            placeholder("COUNT", FunctionKind::Aggregate, Arity::Exact(1), int_type);
            placeholder(
                "CURRENT_TIMESTAMP",
                FunctionKind::Scalar,
                Arity::Exact(0),
                datetime_type,
            );
        });
    }

    /// Registers `name` when the registry does not know it yet, and does nothing otherwise.
    ///
    /// The tests are written over `LEN`, `ROUND`, `SUM`, `COUNT` and `CURRENT_TIMESTAMP`,
    /// some of which may not be registered yet. Rather than drop those shapes, the tests
    /// register a definition with the arity and the kind of the real function — the two
    /// properties the binder reads. The condition matters: once the real definitions
    /// land, `register_builtins` provides them, this helper does nothing (a second
    /// registration of the same name would panic), and the assertions below keep their
    /// meaning against the real registry.
    fn placeholder(
        name: &'static str,
        kind: FunctionKind,
        arity: Arity,
        return_type: fn(&[TypeInfo]) -> SqlResult<TypeInfo>,
    ) {
        if lookup(name).is_none() {
            register(FunctionDef {
                name,
                kind,
                deterministic: true,
                arity,
                return_type,
                eval: eval_null,
                aggregate: None,
            });
        }
    }

    fn int_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
        Ok(TypeInfo::new(SqlType::Int, false))
    }

    fn datetime_type(_args: &[TypeInfo]) -> SqlResult<TypeInfo> {
        Ok(TypeInfo::new(SqlType::DateTime, false))
    }

    fn eval_null(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
        // Not called: the binder does not evaluate. `StaticContext` is named so that the
        // signature stays the registry's.
        let _ = StaticContext::default();
        Ok(Value::Null)
    }

    /// A span on `line`; the line alone survives into a `BoundExpr`.
    fn span(line: u32) -> Span {
        Span {
            line,
            column: 8,
            offset: 7,
            len: 1,
        }
    }

    /// The context the tests bind under: no catalogue, no variable, default options.
    fn ctx() -> BindContext<'static> {
        BindContext::scalar("", SessionOptions::default())
    }

    /// Binds a node built by hand, on line 1.
    ///
    /// The tests build the same nodes the parser produces for the text named in each
    /// helper below.
    fn b(e: &Expr) -> BoundExpr {
        bind(e).unwrap_or_else(|err| panic!("{e:?} must bind, got {err}"))
    }

    /// The error of a node that must not bind.
    fn err(e: &Expr) -> SqlError {
        match bind(e) {
            Ok(bound) => panic!("{e:?} must not bind, got {bound:?}"),
            Err(err) => err,
        }
    }

    /// Dispatches to the entry point the node belongs to, as `expr.rs` does.
    fn bind(e: &Expr) -> SqlResult<BoundExpr> {
        registry();
        match e {
            Expr::Cast { .. } => bind_cast(e, &ctx(), &Scope::empty()),
            Expr::Convert { .. } => bind_convert(e, &ctx(), &Scope::empty()),
            Expr::Function { .. } => bind_function(e, &ctx(), &Scope::empty()),
            Expr::Variable { name, span } => bind_variable_function(name, span),
            other => panic!("no entry point for {other:?}"),
        }
    }

    /// `<literal>`, on line 1.
    fn lit(literal: Literal) -> Expr {
        Expr::Literal(literal, span(1))
    }

    /// An integer literal.
    fn int(text: &str) -> Expr {
        lit(Literal::Integer(text.to_owned()))
    }

    /// A `varchar` literal.
    fn text(value: &str) -> Expr {
        lit(Literal::Str {
            value: value.to_owned(),
            unicode: false,
        })
    }

    /// `<name>` or `<name>(<args>)`, as a data type.
    fn ty(name: &str, args: &[TypeArg]) -> DataType {
        DataType {
            name: name.to_owned(),
            args: args.to_vec(),
            span: span(1),
        }
    }

    /// `CAST(<expr> AS <ty>)` on line 1.
    fn cast(expr: Expr, target: DataType) -> Expr {
        Expr::Cast {
            expr: Box::new(expr),
            ty: target,
            try_: false,
            span: span(1),
        }
    }

    /// `TRY_CAST(<expr> AS <ty>)` on line 1.
    fn try_cast(expr: Expr, target: DataType) -> Expr {
        Expr::Cast {
            expr: Box::new(expr),
            ty: target,
            try_: true,
            span: span(1),
        }
    }

    /// `CONVERT(<ty>, <expr>[, <style>])` on line 1.
    fn convert(target: DataType, expr: Expr, style: Option<Expr>) -> Expr {
        Expr::Convert {
            ty: target,
            expr: Box::new(expr),
            style: style.map(Box::new),
            try_: false,
            span: span(1),
        }
    }

    /// `<name>(<args>)` on `line`.
    fn call_on(line: u32, name: &str, args: Vec<Expr>) -> Expr {
        Expr::Function {
            name: ObjectName {
                server: None,
                database: None,
                schema: None,
                name: ident(name),
                span: span(line),
            },
            args,
            star: false,
            distinct: false,
            over: None,
            span: span(line),
        }
    }

    /// `<name>(<args>)` on line 1.
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        call_on(1, name, args)
    }

    /// An undelimited identifier.
    fn ident(value: &str) -> Ident {
        Ident {
            value: value.to_owned(),
            quoted: false,
        }
    }

    /// `<name>` as a bare column reference, which is what the parser makes of a `datepart`
    /// keyword: it has no idea the word is one.
    fn column(value: &str) -> Expr {
        Expr::Column(ColumnRef {
            qualifier: None,
            name: ident(value),
            span: span(1),
        })
    }

    /// `[<name>]`, a delimited column reference.
    fn delimited_column(value: &str) -> Expr {
        Expr::Column(ColumnRef {
            qualifier: None,
            name: Ident {
                value: value.to_owned(),
                quoted: true,
            },
            span: span(1),
        })
    }

    /// `<qualifier>.<name>`, a column reference with one part in front of it.
    fn qualified_column(qualifier: &str, value: &str) -> Expr {
        Expr::Column(ColumnRef {
            qualifier: Some(ObjectName {
                server: None,
                database: None,
                schema: None,
                name: ident(qualifier),
                span: span(1),
            }),
            name: ident(value),
            span: span(1),
        })
    }

    /// The text of a bound string literal, or a panic when the node is not one.
    fn literal_text(bound: &BoundExpr) -> String {
        match &bound.kind {
            BoundExprKind::Literal(Value::String(s)) => s.text.clone(),
            other => panic!("expected a string literal, got {other:?}"),
        }
    }

    /// The pieces of a bound `Convert`, or a panic when the node is not one.
    fn conversion(bound: &BoundExpr) -> (&BoundExpr, Option<i32>, bool) {
        match &bound.kind {
            BoundExprKind::Convert { expr, style, try_ } => (expr, *style, *try_),
            other => panic!("expected a Convert, got {other:?}"),
        }
    }

    /// The pieces of a bound `Function`, or a panic when the node is not one.
    fn function(bound: &BoundExpr) -> (&'static FunctionDef, &[BoundExpr]) {
        match &bound.kind {
            BoundExprKind::Function { def, args } => (def, args),
            other => panic!("expected a Function, got {other:?}"),
        }
    }

    #[test]
    fn cast_target_type() {
        // CAST(1.5 AS int)
        let bound = b(&cast(
            lit(Literal::Decimal("1.5".to_owned())),
            ty("int", &[]),
        ));
        assert_eq!(bound.ty.ty, SqlType::Int);
        let (source, style, try_) = conversion(&bound);
        assert!(style.is_none());
        assert!(!try_);
        assert!(matches!(source.kind, BoundExprKind::Literal(_)));

        // CAST(1 AS varchar(10))
        let bound = b(&cast(int("1"), ty("varchar", &[TypeArg::Number(10)])));
        assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(10)));
    }

    #[test]
    fn cast_default_length_is_30() {
        // CAST(1 AS varchar): 30, where `DECLARE @v varchar` is 1.
        let bound = b(&cast(int("1"), ty("varchar", &[])));
        assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(30)));

        // CAST(0x00 AS varbinary)
        let bound = b(&cast(
            lit(Literal::Binary("00".to_owned())),
            ty("varbinary", &[]),
        ));
        assert_eq!(bound.ty.ty, SqlType::VarBinary(Len::Fixed(30)));

        // The four other sized types, and the proof that a written length wins.
        for (name, expected) in [
            ("char", SqlType::Char(Len::Fixed(30))),
            ("nchar", SqlType::NChar(Len::Fixed(30))),
            ("nvarchar", SqlType::NVarChar(Len::Fixed(30))),
            ("binary", SqlType::Binary(Len::Fixed(30))),
        ] {
            assert_eq!(b(&cast(int("1"), ty(name, &[]))).ty.ty, expected, "{name}");
        }
        assert_eq!(
            b(&cast(int("1"), ty("varchar", &[TypeArg::Number(1)])))
                .ty
                .ty,
            SqlType::VarChar(Len::Fixed(1))
        );
        assert_eq!(
            b(&cast(int("1"), ty("varchar", &[TypeArg::Max]))).ty.ty,
            SqlType::VarChar(Len::Max)
        );
        // A type without a length is untouched.
        assert_eq!(b(&cast(int("1"), ty("bigint", &[]))).ty.ty, SqlType::BigInt);
    }

    #[test]
    fn convert_argument_order_and_style() {
        // CONVERT(varchar(10), 1) is CAST(1 AS varchar(10)).
        let converted = b(&convert(
            ty("varchar", &[TypeArg::Number(10)]),
            int("1"),
            None,
        ));
        let casted = b(&cast(int("1"), ty("varchar", &[TypeArg::Number(10)])));
        assert_eq!(converted.ty, casted.ty);
        let (converted_source, converted_style, converted_try) = conversion(&converted);
        let (cast_source, cast_style, cast_try) = conversion(&casted);
        assert_eq!(converted_style, cast_style);
        assert_eq!(converted_try, cast_try);
        assert_eq!(converted_source.ty, cast_source.ty);

        // CONVERT(varchar(30), CAST('2020-01-01' AS date), 112)
        let bound = b(&convert(
            ty("varchar", &[TypeArg::Number(30)]),
            cast(text("2020-01-01"), ty("date", &[])),
            Some(int("112")),
        ));
        assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(30)));
        assert_eq!(conversion(&bound).1, Some(112));
    }

    #[test]
    fn convert_style_must_be_an_integer_constant() {
        // CONVERT(varchar(30), 1, '112'): SQL Server accepts a non-constant style, the
        // binder does not. The difference is an internal error.
        let error = err(&convert(
            ty("varchar", &[TypeArg::Number(30)]),
            int("1"),
            Some(text("112")),
        ));
        assert_eq!(error.number, 50000);
        assert!(error.message.contains("style"), "{}", error.message);
    }

    #[test]
    fn every_explicit_conversion_is_nullable() {
        // TRY_CAST('x' AS int)
        let bound = b(&try_cast(text("x"), ty("int", &[])));
        assert!(bound.ty.nullable);
        assert!(conversion(&bound).2);

        // CAST(1 AS int): `INTNTYPE`, `fNullable = 1` on the wire, where `SELECT 1` is an
        // `INT4TYPE` with `fNullable = 0`.
        let bound = b(&cast(int("1"), ty("int", &[])));
        assert!(bound.ty.nullable);
        assert!(!conversion(&bound).2);

        // CONVERT(int, 1) and CAST('a' AS varchar(10)): same rule, whatever the target.
        assert!(b(&convert(ty("int", &[]), int("1"), None)).ty.nullable);
        assert!(
            b(&cast(text("a"), ty("varchar", &[TypeArg::Number(10)])))
                .ty
                .nullable
        );

        // CAST(CAST(NULL AS int) AS bigint): a nullable source stays nullable.
        let bound = b(&cast(
            cast(lit(Literal::Null), ty("int", &[])),
            ty("bigint", &[]),
        ));
        assert!(bound.ty.nullable);
    }

    #[test]
    fn illegal_cast_is_529() {
        // CAST(NEWID() AS int): 529/16/1 on SQL Server (module documentation).
        let error = err(&cast(call("NEWID", Vec::new()), ty("int", &[])));
        assert_eq!(error.number, 529);
        assert_eq!(error.severity, 16);
        assert_eq!(error.state, 1);
        assert_eq!(
            error.message,
            "No explicit conversion exists from uniqueidentifier to int."
        );
        assert_eq!(error.line, 1);

        // CAST('1' AS int) binds: the conversion is legal and the value alone can fail,
        // at execution time, with 245.
        assert_eq!(b(&cast(text("1"), ty("int", &[]))).ty.ty, SqlType::Int);
    }

    #[test]
    fn is_castable_follows_sql_server() {
        let date = SqlType::Date;
        let time = SqlType::Time(7);
        let datetime = SqlType::DateTime;
        let datetime2 = SqlType::DateTime2(7);
        let offset = SqlType::DateTimeOffset(7);
        let guid = SqlType::UniqueIdentifier;
        let int = SqlType::Int;
        let bit = SqlType::Bit;
        let money = SqlType::Money;
        let float = SqlType::Float;
        let numeric = SqlType::Numeric {
            precision: 5,
            scale: 2,
        };
        let varchar = SqlType::VarChar(Len::Fixed(30));
        let varbinary = SqlType::VarBinary(Len::Fixed(20));

        // Refused, each one 529 on SQL Server.
        for (from, to) in [
            (&guid, &int),
            (&guid, &datetime),
            (&int, &guid),
            (&bit, &guid),
            (&date, &int),
            (&offset, &int),
            (&int, &date),
            (&money, &date),
            (&float, &date),
            (&date, &time),
            (&time, &date),
        ] {
            assert!(!is_castable(from, to), "{from:?} -> {to:?}");
        }

        // Accepted, each one a successful query on SQL Server (or a *runtime* failure,
        // which is not a binding error: varbinary -> date answers 241).
        for (from, to) in [
            (&datetime, &int),
            (&int, &datetime),
            (&bit, &datetime),
            (&money, &datetime),
            (&numeric, &datetime),
            (&datetime2, &varbinary),
            (&datetime, &varbinary),
            (&varbinary, &guid),
            (&varbinary, &date),
            (&guid, &varchar),
            (&varchar, &guid),
            (&varchar, &int),
            (&date, &datetime2),
        ] {
            assert!(is_castable(from, to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn unknown_type_in_a_cast_is_243() {
        // CAST(1 AS foo): 243 and not the 2715 of a declaration.
        let error = err(&cast(int("1"), ty("foo", &[])));
        assert_eq!((error.number, error.severity, error.state), (243, 16, 1));
        assert_eq!(error.message, "foo is not a known system type.");

        // A known name with a parameter out of bounds is a different fault: the error of
        // `resolve_data_type` passes through, placeholder number included.
        let error = err(&cast(int("1"), ty("varchar", &[TypeArg::Number(9000)])));
        assert_eq!(error.number, 2715);
    }

    /// `NULLIF` over an integer literal takes the narrowest integer type that holds the
    /// literal, as SQL Server does.
    ///
    /// Eleven values and their boundaries, then the constructs and the shapes that do
    /// **not** narrow. Without [`with_narrowed_nullif`] each line of the first table
    /// answers `int`, which is what the second table still answers: the two together are
    /// what tells the rule from "an integer literal is narrow" and from "a `CASE` is
    /// narrow".
    #[test]
    fn nullif_narrows_an_integer_literal() {
        let narrowed = |text_of_literal: &str| -> SqlType {
            b(&call("NULLIF", vec![int(text_of_literal), int("9")]))
                .ty
                .ty
        };
        for (literal, expected) in [
            ("0", SqlType::TinyInt),
            ("1", SqlType::TinyInt),
            ("255", SqlType::TinyInt),
            ("256", SqlType::SmallInt),
            ("32767", SqlType::SmallInt),
            ("32768", SqlType::Int),
            ("2147483647", SqlType::Int),
        ] {
            assert_eq!(narrowed(literal), expected, "NULLIF({literal}, 9)");
        }

        // The negative side, written the way the parser sees it: a unary minus over a
        // literal. `-1` is a `smallint` because `tinyint` starts at 0.
        let negative = |text_of_literal: &str| -> SqlType {
            b(&call("NULLIF", vec![minus(int(text_of_literal)), int("9")]))
                .ty
                .ty
        };
        assert_eq!(negative("1"), SqlType::SmallInt);
        assert_eq!(negative("32768"), SqlType::SmallInt);
        assert_eq!(negative("32769"), SqlType::Int);
        // `2147483648` is a `numeric(10, 0)` literal and not a `bigint`; negated it fits
        // an `int`, and SQL Server says `int`.
        assert_eq!(negative("2147483648"), SqlType::Int);
        // Past `int` in either direction nothing narrows: the literal keeps its `numeric`.
        let numeric_10 = SqlType::Numeric {
            precision: 10,
            scale: 0,
        };
        assert_eq!(narrowed("2147483648"), numeric_10);
        assert_eq!(
            negative("5000000000"),
            SqlType::Numeric {
                precision: 10,
                scale: 0
            }
        );
        // A literal with a scale is not an integer literal: `NULLIF(1.50, 9)` is `numeric`.
        assert_eq!(
            b(&call(
                "NULLIF",
                vec![lit(Literal::Decimal("1.50".to_owned())), int("9")]
            ))
            .ty
            .ty,
            SqlType::Numeric {
                precision: 3,
                scale: 2
            }
        );

        // Nullable, narrowed or not: the rule of `sysfn` is untouched.
        assert!(b(&call("NULLIF", vec![int("1"), int("9")])).ty.nullable);
    }

    /// The counter-proofs of [`nullif_narrows_an_integer_literal`]: what keeps `int`.
    #[test]
    fn nullif_narrows_neither_another_function_nor_another_shape() {
        // The construct: the same literals under ISNULL and COALESCE stay `int`.
        for name in ["ISNULL", "COALESCE"] {
            let bound = b(&call(name, vec![int("1"), int("9")]));
            assert_eq!(bound.ty.ty, SqlType::Int, "{name}(1, 9)");
        }

        // The shape: a conversion, an arithmetic operator and a call are not literals.
        let int_type = ty("int", &[]);
        assert_eq!(
            b(&call("NULLIF", vec![cast(int("1"), int_type), int("9")]))
                .ty
                .ty,
            SqlType::Int
        );
        assert_eq!(
            b(&call("NULLIF", vec![arith(int("1"), int("1")), int("9")]))
                .ty
                .ty,
            SqlType::Int
        );
        assert_eq!(
            b(&call("NULLIF", vec![call("ABS", vec![int("1")]), int("9")]))
                .ty
                .ty,
            SqlType::Int
        );

        // The second argument plays no part: its type does not widen the result, and it
        // does not narrow it either.
        let bigint_type = ty("bigint", &[]);
        assert_eq!(
            b(&call(
                "NULLIF",
                vec![cast(int("1"), ty("int", &[])), cast(int("0"), bigint_type)]
            ))
            .ty
            .ty,
            SqlType::Int
        );
        assert_eq!(
            b(&call("NULLIF", vec![text("abc"), text("x")])).ty.ty,
            SqlType::VarChar(Len::Fixed(3))
        );
    }

    #[test]
    fn function_call_types() {
        // LEN('abc')
        let bound = b(&call("LEN", vec![text("abc")]));
        let (def, args) = function(&bound);
        assert_eq!(def.name, "LEN");
        assert_eq!(args.len(), 1);
        let expected = check_call(def, &[args[0].ty.clone()]).expect("LEN types its call");
        assert_eq!(bound.ty.ty, expected.ty);
        assert_eq!(bound.ty.ty, SqlType::Int);
        // `SELECT LEN('abc')` is an `INTNTYPE` with `fNullable = 1`: the registry answers
        // by the argument, the binder by the function.
        assert!(bound.ty.nullable);

        // ISNULL(NULL, 'x')
        let bound = b(&call("ISNULL", vec![lit(Literal::Null), text("x")]));
        let (def, args) = function(&bound);
        assert_eq!(def.name, "ISNULL");
        assert_eq!(args.len(), 2);
        assert_eq!(bound.ty.ty, SqlType::VarChar(Len::Fixed(1)));

        // The lookup is case-insensitive but the definition keeps its spelling.
        assert_eq!(
            function(&b(&call("isnull", vec![int("1"), int("2")])))
                .0
                .name,
            "ISNULL"
        );
    }

    #[test]
    fn unknown_function_is_195() {
        let error = err(&call("NO_SUCH_FN", vec![int("1")]));
        assert_eq!(error.number, 195);
        assert_eq!(error.severity, 15);
        assert_eq!(error.state, 10);
        assert_eq!(
            error.message,
            "'NO_SUCH_FN' is not a known built-in function name."
        );
        assert_eq!(error.line, 1);

        // The message prints the name as the user wrote it, not a canonical spelling
        // (`SELECT no_such_fn(1);` names `'no_such_fn'`).
        assert_eq!(
            err(&call("no_such_fn", vec![int("1")])).message,
            "'no_such_fn' is not a known built-in function name."
        );

        // The line is the one of the node.
        assert_eq!(err(&call_on(3, "NO_SUCH_FN", Vec::new())).line, 3);
    }

    #[test]
    fn a_qualified_name_is_4121() {
        // `foo.bar(1)`: 4121, with `dbo` and `sys` as qualifiers too (module documentation).
        let mut qualified = call("bar", vec![int("1")]);
        if let Expr::Function { name, .. } = &mut qualified {
            name.schema = Some(ident("foo"));
        }
        let error = err(&qualified);
        assert_eq!((error.number, error.severity, error.state), (4121, 16, 1));
        assert_eq!(
            error.message,
            "Neither a column \"foo\" nor a user-defined function or aggregate \"foo.bar\" was found, or the name is ambiguous."
        );

        // `dbo.ISNULL(...)` and `sys.ISNULL(...)` answer 4121 too: `SELECT dbo.LEN('abc');`
        // and `SELECT sys.LEN('abc');` answer it on SQL Server.
        for schema in ["dbo", "SYS"] {
            let mut refused = call("ISNULL", vec![int("1"), int("2")]);
            if let Expr::Function { name, .. } = &mut refused {
                name.schema = Some(ident(schema));
            }
            assert_eq!(err(&refused).number, 4121, "{schema}");
        }
    }

    #[test]
    fn arity_errors_come_from_check_call() {
        // LEN() and LEN('a', 'b'): exact arity, 174.
        for args in [Vec::new(), vec![text("a"), text("b")]] {
            let error = err(&call("LEN", args));
            assert_eq!(error.number, 174);
            assert_eq!(error.severity, 15);
        }
        // ROUND(1): a range, 189.
        assert_eq!(err(&call("ROUND", vec![int("1")])).number, 189);

        // The binder rewrites no message: it is `check_call`'s, with the line added.
        let def = lookup("LEN").expect("LEN is registered");
        let expected = check_call(def, &[]).expect_err("LEN takes one argument");
        let actual = err(&call("LEN", Vec::new()));
        assert_eq!(actual.message, expected.message);
        assert_eq!(actual.number, expected.number);
    }

    #[test]
    fn errors_from_check_call_carry_the_line() {
        // "SELECT 1;\n\nSELECT LEN()": the call is on line 3.
        let error = err(&call_on(3, "LEN", Vec::new()));
        assert_eq!(error.number, 174);
        assert_eq!(error.line, 3);
    }

    #[test]
    fn untyped_null_arguments_follow_sql_server() {
        // PATINDEX('%bc%', NULL) → 8116, argument 2 named.
        let error = err(&call("PATINDEX", vec![text("%bc%"), lit(Literal::Null)]));
        assert_eq!(error.number, 8116);
        assert_eq!(error.severity, 16);
        assert_eq!(
            error.message,
            "Data type NULL is not accepted for argument 2 of the patindex function."
        );
        assert_eq!(error.line, 1);

        // The same call with a typed NULL is accepted.
        assert!(
            b(&call(
                "PATINDEX",
                vec![
                    text("%bc%"),
                    cast(lit(Literal::Null), ty("varchar", &[TypeArg::Number(10)])),
                ],
            ))
            .ty
            .nullable
        );

        // NULLIF(NULL, 1) → 4151; NULLIF(1, NULL) is legal.
        let error = err(&call("NULLIF", vec![lit(Literal::Null), int("1")]));
        assert_eq!((error.number, error.severity, error.state), (4151, 16, 1));
        assert!(
            error
                .message
                .contains("first argument of NULLIF cannot be the NULL constant"),
            "{}",
            error.message
        );
        assert!(
            b(&call("NULLIF", vec![int("1"), lit(Literal::Null)]))
                .ty
                .nullable
        );

        // Accepted elsewhere: PATINDEX argument 1, and the whole of CHARINDEX.
        assert!(
            b(&call("PATINDEX", vec![lit(Literal::Null), text("abc")]))
                .ty
                .nullable
        );
        assert!(
            b(&call("CHARINDEX", vec![text("b"), lit(Literal::Null)]))
                .ty
                .nullable
        );

        // A wrong arity keeps precedence over the NULL check.
        assert_eq!(err(&call("PATINDEX", vec![lit(Literal::Null)])).number, 174);
    }

    #[test]
    fn global_variables() {
        assert_eq!(
            bind_variable_function("@@ROWCOUNT", &span(1))
                .expect("@@ROWCOUNT binds")
                .ty
                .ty,
            SqlType::Int
        );
        assert_eq!(
            bind_variable_function("@@SPID", &span(1))
                .expect("@@SPID binds")
                .ty
                .ty,
            SqlType::SmallInt
        );
        // Case-insensitive, and bound as a call with no argument.
        let bound = bind_variable_function("@@spid", &span(1)).expect("@@spid binds");
        let (def, args) = function(&bound);
        assert_eq!(def.name, "@@SPID");
        assert!(args.is_empty());

        // An unknown `@@x` is 137, not 195: `SELECT @@NO_SUCH;` answers 137 naming
        // `"@@NO_SUCH"`.
        let error =
            bind_variable_function("@@NO_SUCH", &span(4)).expect_err("@@NO_SUCH is unknown");
        assert_eq!(error.number, 137);
        assert_eq!(error.severity, 15);
        assert_eq!(error.state, 2);
        assert_eq!(
            error.message,
            "The scalar variable \"@@NO_SUCH\" is not declared."
        );
        assert_eq!(error.line, 4);
    }

    #[test]
    fn niladic_without_parentheses() {
        registry();
        let bound = bind_niladic(&ident("CURRENT_TIMESTAMP"), &span(1))
            .expect("no error")
            .expect("CURRENT_TIMESTAMP is a function");
        assert_eq!(bound.ty.ty, SqlType::DateTime);
        assert_eq!(function(&bound).0.name, "CURRENT_TIMESTAMP");

        // Case-insensitive, like every other name.
        assert!(
            bind_niladic(&ident("current_timestamp"), &span(1))
                .expect("no error")
                .is_some()
        );

        // An ordinary name stays a column: `expr.rs` raises 207 on it.
        assert!(
            bind_niladic(&ident("c"), &span(1))
                .expect("no error")
                .is_none()
        );

        // A delimited name is a column even when it is spelled like a function.
        let quoted = Ident {
            value: "CURRENT_TIMESTAMP".to_owned(),
            quoted: true,
        };
        assert!(bind_niladic(&quoted, &span(1)).expect("no error").is_none());

        // A name that *looks* niladic and is not stays a column too. The five names of
        // [`NILADIC_FUNCTIONS`] are registered, so no name reaches the `None` arm of
        // `lookup` in [`bind_niladic`]; what is pinned is the arm above it, the name that
        // is not in the list.
        //
        // `CURRENT_DATE` and `CURRENT_TIME` would not do: they sit in the *parser's*
        // `NILADIC_FUNCTIONS` (seven names), not in the binder's (five), so they take this
        // very arm and say nothing more than `c` does. `CURRENT_CATALOG` is in neither
        // list -- the SQL:2003 niladic function SQL Server does not implement:
        // `SELECT CURRENT_CATALOG;` answers 207, not 156.
        assert!(lookup("CURRENT_CATALOG").is_none());
        assert!(
            bind_niladic(&ident("CURRENT_CATALOG"), &span(1))
                .expect("no error")
                .is_none()
        );
    }

    #[test]
    fn aggregates_are_refused() {
        // COUNT(*): the star is refused before anything else.
        let mut star = call("COUNT", Vec::new());
        if let Expr::Function { star: s, .. } = &mut star {
            *s = true;
        }
        let error = err(&star);
        assert_eq!(error.number, 50000);
        assert!(
            error.message.contains("not implemented yet"),
            "{}",
            error.message
        );

        // SUM(1): an aggregate definition, refused by its kind.
        let error = err(&call("SUM", vec![int("1")]));
        assert_eq!(error.number, 50000);
        assert!(
            error.message.contains("not implemented yet"),
            "{}",
            error.message
        );

        // DISTINCT and OVER, same answer.
        let mut distinct = call("ISNULL", vec![int("1"), int("2")]);
        if let Expr::Function { distinct: d, .. } = &mut distinct {
            *d = true;
        }
        assert_eq!(err(&distinct).number, 50000);
    }

    #[test]
    fn a_node_of_another_kind_is_an_internal_error() {
        let empty = Scope::empty();
        let error = bind_cast(&int("1"), &ctx(), &empty).expect_err("a literal is not a CAST");
        assert_eq!(error.number, 50000);
        let error =
            bind_convert(&int("1"), &ctx(), &empty).expect_err("a literal is not a CONVERT");
        assert_eq!(error.number, 50000);
        let error = bind_function(&int("1"), &ctx(), &empty).expect_err("a literal is not a call");
        assert_eq!(error.number, 50000);

        // An operand `expr.rs` owns: `bind_operand` is wired on `bind_expr`, so it binds
        // instead of raising an internal error.
        registry();
        let unary = Expr::Unary {
            op: vauban_parser::UnaryOp::Minus,
            expr: Box::new(int("1")),
            span: span(1),
        };
        assert_eq!(
            b(&cast(unary, ty("int", &[]))).ty,
            vauban_types::TypeInfo::new(vauban_types::SqlType::Int, true)
        );
    }

    // ---- the `datepart` keyword is read while binding ----

    #[test]
    fn the_keyword_rule_covers_five_functions() {
        registry();
        // The two the registry knows.
        for name in ["DATEPART", "DATENAME"] {
            let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(datepart_keyword_position(def), Some(0), "{name}");
        }
        // The three others: `SELECT DATEADD(foo, 1, GETDATE());` answers 155 naming
        // `'foo'` and `dateadd`, and DATEDIFF and DATETRUNC answer the same with their own
        // name.
        for name in ["DATEADD", "DATEDIFF", "DATETRUNC"] {
            assert_eq!(datepart_keyword_position(&fake(name)), Some(0), "{name}");
        }
        // The shorthands write the keyword into their name, so their argument is ordinary.
        for name in ["YEAR", "MONTH", "DAY", "LEN"] {
            let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(datepart_keyword_position(def), None, "{name}");
        }
    }

    /// A definition that is not in the registry, to name a function that is not written yet.
    fn fake(name: &'static str) -> FunctionDef {
        FunctionDef {
            name,
            kind: FunctionKind::Scalar,
            deterministic: true,
            arity: Arity::Exact(2),
            return_type: int_type,
            eval: eval_null,
            aggregate: None,
        }
    }

    #[test]
    fn a_known_keyword_becomes_a_string_literal() {
        // DATEPART(year, GETDATE()): the word is not a column, it is a value the evaluation
        // reads back.
        let bound = b(&call(
            "DATEPART",
            vec![column("year"), call("GETDATE", vec![])],
        ));
        let (def, args) = function(&bound);
        assert_eq!(def.name, "DATEPART");
        assert_eq!(literal_text(&args[0]), "year");
        assert_eq!(args[0].ty.ty, SqlType::VarChar(Len::Fixed(4)));
        assert!(!args[0].ty.nullable);
        assert_eq!(bound.ty.ty, SqlType::Int);
    }

    #[test]
    fn a_delimited_keyword_is_a_keyword() {
        // `SELECT DATEPART([year], '2020-03-01');` answers 2020 on SQL Server 2022.
        let bound = b(&call(
            "DATEPART",
            vec![delimited_column("year"), text("2020-03-01")],
        ));
        let (_, args) = function(&bound);
        assert_eq!(literal_text(&args[0]), "year");
    }

    #[test]
    fn an_unknown_keyword_is_155_with_the_name_of_the_function() {
        // `SELECT DATEPART(foo, GETDATE());` and `SELECT DATENAME(foo, GETDATE());`: the
        // word before `option` is the function that was called, lower-cased.
        let error = err(&call_on(
            3,
            "DATEPART",
            vec![column("foo"), call("GETDATE", vec![])],
        ));
        assert_eq!(error.number, 155);
        assert_eq!(error.severity, 15);
        assert_eq!(error.state, 1);
        assert_eq!(error.message, "'foo' is not a known datepart option.");
        assert_eq!(error.line, 3);

        let error = err(&call(
            "DATENAME",
            vec![column("foo"), call("GETDATE", vec![])],
        ));
        assert_eq!(error.message, "'foo' is not a known datename option.");
    }

    #[test]
    fn the_message_keeps_the_spelling_that_was_written() {
        // Neither folded nor canonicalized, delimiters aside: `FoO` and `[FoO]` both print
        // `'FoO'`, and a qualified name prints whole.
        assert_eq!(
            err(&call(
                "DATEPART",
                vec![column("FoO"), call("GETDATE", vec![])]
            ))
            .message,
            "'FoO' is not a known datepart option."
        );
        assert_eq!(
            err(&call(
                "DATEPART",
                vec![delimited_column("FoO"), call("GETDATE", vec![])]
            ))
            .message,
            "'FoO' is not a known datepart option."
        );
        assert_eq!(
            err(&call(
                "DATEPART",
                vec![qualified_column("t", "year"), call("GETDATE", vec![])]
            ))
            .message,
            "'t.year' is not a known datepart option."
        );
    }

    #[test]
    fn trailing_spaces_are_dropped_from_the_keyword() {
        // `SELECT DATEPART([year ], '2020-03-01');` answers 2020, and so do `[year  ]`,
        // `[yy ]` and `[YeAr ]`; `SELECT DATENAME([mm ], '2020-03-01');` answers `March`.
        // The literal handed on is the trimmed spelling, so that a keyword the binding
        // accepted the evaluation accepts too.
        for (written, keyword) in [
            ("year ", "year"),
            ("year  ", "year"),
            ("yy ", "yy"),
            ("YeAr ", "YeAr"),
        ] {
            let bound = b(&call(
                "DATEPART",
                vec![delimited_column(written), text("2020-03-01")],
            ));
            let (_, args) = function(&bound);
            assert_eq!(literal_text(&args[0]), keyword, "{written:?}");
        }

        let bound = b(&call(
            "DATENAME",
            vec![delimited_column("mm "), text("2020-03-01")],
        ));
        let (_, args) = function(&bound);
        assert_eq!(literal_text(&args[0]), "mm");
    }

    #[test]
    fn the_declared_length_of_the_keyword_stays_that_of_the_written_name() {
        // An upper bound of the text it holds, which a `varchar(n)` takes, and which keeps
        // the length away from 0 for a name made only of spaces.
        let bound = b(&call(
            "DATEPART",
            vec![delimited_column("year  "), text("2020-03-01")],
        ));
        let (_, args) = function(&bound);
        assert_eq!(args[0].ty.ty, SqlType::VarChar(Len::Fixed(6)));
    }

    #[test]
    fn a_trailing_space_is_kept_in_the_message_of_an_unknown_keyword() {
        // `SELECT DATEPART([foo ], GETDATE());` answers 155 naming `'foo '`: dropped for
        // the lookup, kept in the quotation.
        assert_eq!(
            err(&call(
                "DATEPART",
                vec![delimited_column("foo "), call("GETDATE", vec![])]
            ))
            .message,
            "'foo ' is not a known datepart option."
        );
        assert_eq!(
            err(&call(
                "DATENAME",
                vec![delimited_column("foo "), call("GETDATE", vec![])]
            ))
            .message,
            "'foo ' is not a known datename option."
        );
    }

    #[test]
    fn only_trailing_spaces_are_dropped() {
        // The bounds of the rule: a leading space, an inner space, and the four blanks
        // that are not the space character keep the name unknown and print it as it was
        // written. `[   ]` shows that trimming to nothing matches nothing rather than
        // everything.
        for written in [
            " year",
            " year ",
            "ye ar",
            "year\t",
            "year\n",
            "year\r",
            "year\u{a0}",
            "   ",
        ] {
            let error = err(&call(
                "DATEPART",
                vec![delimited_column(written), call("GETDATE", vec![])],
            ));
            assert_eq!(error.number, 155, "{written:?}");
            assert_eq!(
                error.message,
                format!("'{written}' is not a known datepart option."),
                "{written:?}"
            );
        }
    }

    #[test]
    fn the_recognition_helper_drops_nothing_but_trailing_spaces() {
        // The helper is the one place a trailing space is dropped, which is what keeps the
        // rule out of the rest of the binder: `SELECT 1 AS [a ];` gives a column really
        // named `a `, space kept.
        assert_eq!(recognized_spelling("a "), "a");
        assert_eq!(recognized_spelling(" a"), " a");
        assert_eq!(recognized_spelling("a\t"), "a\t");
        assert_eq!(recognized_spelling("   "), "");
    }

    #[test]
    fn the_keyword_is_read_before_the_arguments_that_follow() {
        // `SELECT DATEPART(foo, no_such_column);` answers 155, not the 207 of the column,
        // and `SELECT DATEPART(year, no_such_column);` answers 207: the order, not a
        // DATEPART that swallows 207.
        assert_eq!(
            err(&call(
                "DATEPART",
                vec![column("foo"), column("no_such_column")]
            ))
            .number,
            155
        );
        let error = err(&call(
            "DATEPART",
            vec![column("year"), column("no_such_column")],
        ));
        assert_eq!(error.number, 207);
        assert_eq!(error.message, "Unknown column name 'no_such_column'.");
    }

    #[test]
    fn the_arity_is_counted_before_the_keyword_is_read() {
        // `SELECT DATEPART(foo);` answers 174 (two arguments required), not 155: a call
        // with the wrong number of arguments has no keyword position to speak of.
        let error = err(&call("DATEPART", vec![column("foo")]));
        assert_eq!(error.number, 174);
        assert_eq!(
            error.message,
            "The function datepart takes exactly 2 argument(s)."
        );
    }

    #[test]
    fn what_is_not_a_name_keeps_the_binding_of_an_expression() {
        // `SELECT DATEPART(@v, GETDATE());` answers 137, the error of the variable, not
        // 155.
        let variable = Expr::Variable {
            name: "@v".to_owned(),
            span: span(1),
        };
        let error = err(&call("DATEPART", vec![variable, call("GETDATE", vec![])]));
        assert_eq!(error.number, 137);
    }

    #[test]
    fn niladic_datepart_names_keep_the_delimiter_distinction() {
        for name in [
            "user",
            "current_user",
            "session_user",
            "system_user",
            "current_timestamp",
        ] {
            let error = err(&call(
                "DATEPART",
                vec![column(name), call("GETDATE", vec![])],
            ));
            assert_eq!((error.number, error.severity, error.state), (1023, 15, 1));
            assert_eq!(
                err(&call(
                    "DATEPART",
                    vec![delimited_column(name), call("GETDATE", vec![])]
                ))
                .number,
                155
            );
        }
    }

    #[test]
    fn a_keyword_expression_keeps_the_binding_diagnostics() {
        for (arg, number) in [
            (arith(variable("@v"), int("1")), 137),
            (call("NOSUCHFN", vec![]), 195),
            (call("LEN", vec![]), 174),
        ] {
            assert_eq!(
                err(&call("DATEPART", vec![arg, call("GETDATE", vec![])])).number,
                number
            );
        }
    }

    #[test]
    fn expression_dateparts_are_invalid_parameters() {
        for arg in [
            text("year"),
            int("12"),
            lit(Literal::Null),
            arith(column("year"), int("1")),
            call("GETDATE", vec![]),
        ] {
            let error = err(&call("DATEPART", vec![arg, call("GETDATE", vec![])]));
            assert_eq!((error.number, error.severity, error.state), (1023, 15, 1));
            assert_eq!(error.message, "Parameter 1 of datepart is not valid.");
        }
        assert_eq!(err(&call("DATEPART", vec![text("year")])).number, 174);
    }

    #[test]
    fn year_month_and_day_take_an_expression_and_not_a_keyword() {
        // Their keyword is written into the name: the only argument is a value, so an
        // unknown column there is the 207 it would be anywhere else.
        let error = err(&call("YEAR", vec![column("no_such_column")]));
        assert_eq!(error.number, 207);
        assert_eq!(error.message, "Unknown column name 'no_such_column'.");
    }

    /// Each function of the registry has a nullability class, and no name is in two
    /// classes: a function registered without a row in the tables of
    /// `with_inferred_nullability` would silently get the answer of `sysfn`.
    ///
    /// The failure names the offenders at once and says how to classify them, so that
    /// whoever registers a function can classify it without reading this file.
    #[test]
    fn every_registered_function_is_classified() {
        registry();
        let mut unclassified = Vec::new();
        let mut duplicated = Vec::new();
        for def in vauban_sysfn::all() {
            let listed = [
                ALWAYS_NULLABLE.contains(&def.name),
                NEVER_NULLABLE.contains(&def.name),
                OPERANDS_NULLABLE.contains(&def.name),
                matches!(def.name, "ISNULL" | "COALESCE"),
            ];
            match listed.iter().filter(|listed| **listed).count() {
                0 => unclassified.push(def.name),
                1 => {}
                _ => duplicated.push(def.name),
            }
        }
        assert!(
            duplicated.is_empty(),
            "listed in more than one of ALWAYS_NULLABLE, NEVER_NULLABLE and \
             OPERANDS_NULLABLE (crates/vauban-binder/src/call.rs): {duplicated:?}"
        );
        assert!(
            unclassified.is_empty(),
            "registered in `vauban-sysfn` but in none of the nullability tables of \
             crates/vauban-binder/src/call.rs (ALWAYS_NULLABLE, NEVER_NULLABLE, \
             OPERANDS_NULLABLE, next to `with_inferred_nullability`): {unclassified:?}. \
             ALWAYS_NULLABLE: the result is nullable whatever the arguments; \
             NEVER_NULLABLE: the result is not nullable, even with a nullable argument; \
             OPERANDS_NULLABLE: the result is nullable when an argument is."
        );
        assert_eq!(nullability_class("LEN"), FunctionNullability::Always);
        assert_eq!(nullability_class("CONCAT"), FunctionNullability::Never);
        assert_eq!(nullability_class("CEILING"), FunctionNullability::Operands);
        assert_eq!(nullability_class("ISNULL"), FunctionNullability::IsNull);
        assert_eq!(nullability_class("COALESCE"), FunctionNullability::Coalesce);
        assert_eq!(nullability_class("NO_SUCH"), FunctionNullability::Operands);
    }

    /// The three classes on their two shapes (`fNullable` of COLMETADATA): a constant
    /// argument, and a non-constant argument that is not nullable, which
    /// `ISNULL(NULL, <literal>)` is not — it is a constant — so `@@SPID` and `GETDATE()`
    /// stand for it here.
    #[test]
    fn function_nullability_follows_its_class() {
        // `SELECT LEN('abc');`, `SELECT ABS(1);`, `SELECT YEAR(GETDATE());` → 1.
        assert!(b(&call("LEN", vec![text("abc")])).ty.nullable);
        assert!(b(&call("ABS", vec![int("1")])).ty.nullable);
        assert!(b(&call("YEAR", vec![call("GETDATE", vec![])])).ty.nullable);
        // `SELECT NEWID();`, `SELECT DB_NAME();`, `SELECT @@SERVERNAME;` → 1.
        assert!(b(&call("NEWID", vec![])).ty.nullable);
        assert!(b(&call("DB_NAME", vec![])).ty.nullable);
        assert!(b(&variable("@@SERVERNAME")).ty.nullable);
        // `SELECT CEILING(1.5);`, `SELECT SIGN(1);`, `SELECT SIGN(@@SPID);` → 0;
        // `SELECT SIGN(@i);` → 1: the argument decides.
        assert!(!b(&call("CEILING", vec![int("1")])).ty.nullable);
        assert!(!b(&call("SIGN", vec![int("1")])).ty.nullable);
        assert!(!b(&call("SIGN", vec![variable("@@SPID")])).ty.nullable);
        assert!(
            b(&call("SIGN", vec![call("ABS", vec![variable("@@SPID")])]))
                .ty
                .nullable
        );
        assert!(!b(&variable("@@SPID")).ty.nullable);
        // `SELECT CONCAT(NULL, NULL);`, `SELECT CONCAT('a', CAST('x' AS varchar(1)));`,
        // `SELECT GETDATE();`, `SELECT PI();`, `SELECT @@ROWCOUNT;` → 0.
        assert!(
            !b(&call(
                "CONCAT",
                vec![lit(Literal::Null), lit(Literal::Null)]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call(
                "CONCAT",
                vec![
                    text("a"),
                    cast(text("x"), ty("varchar", &[TypeArg::Number(1)]))
                ]
            ))
            .ty
            .nullable
        );
        assert!(!b(&call("GETDATE", vec![])).ty.nullable);
        assert!(!b(&call("PI", vec![])).ty.nullable);
        assert!(!b(&variable("@@ROWCOUNT")).ty.nullable);
    }

    /// `ISNULL` on the shapes of `isnull_is_nullable`, with `@@SPID` as the value that
    /// is neither constant nor nullable and `NEWID()` as the one that is nullable.
    #[test]
    fn isnull_reads_its_replacement_as_a_value() {
        let spid = || variable("@@SPID");
        let nullable_int = || call("ABS", vec![spid()]);
        // `ISNULL(1, <nullable>)`, `ISNULL(@@SPID, <nullable>)` → 0: the check decides.
        assert!(
            !b(&call("ISNULL", vec![int("1"), nullable_int()]))
                .ty
                .nullable
        );
        assert!(!b(&call("ISNULL", vec![spid(), nullable_int()])).ty.nullable);
        // `ISNULL(CAST(1 AS int), <nullable>)`, `ISNULL(1 + 1, <nullable>)` → 1: the
        // check is read as announced.
        assert!(
            b(&call(
                "ISNULL",
                vec![cast(int("1"), ty("int", &[])), nullable_int()]
            ))
            .ty
            .nullable
        );
        // `ISNULL(<nullable>, 1)`, `ISNULL(<nullable>, ISNULL(<nullable>, 1))` → 0.
        assert!(
            !b(&call("ISNULL", vec![nullable_int(), int("1")]))
                .ty
                .nullable
        );
        assert!(
            !b(&call(
                "ISNULL",
                vec![
                    nullable_int(),
                    call("ISNULL", vec![nullable_int(), int("1")])
                ]
            ))
            .ty
            .nullable
        );
        // `ISNULL(<nullable>, NULL)`, `ISNULL(<nullable>, CAST(NULL AS int))`,
        // `ISNULL(<nullable>, <nullable>)` → 1.
        assert!(
            b(&call("ISNULL", vec![nullable_int(), lit(Literal::Null)]))
                .ty
                .nullable
        );
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_int(), cast(lit(Literal::Null), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call("ISNULL", vec![nullable_int(), nullable_int()]))
                .ty
                .nullable
        );
        // A constant replacement is folded and read by its value:
        // `ISNULL(<nullable>, CAST(1 AS int))`, `ISNULL(<nullable>, LEN('abc'))`,
        // `ISNULL(<nullable>, LEN('abc') + 1)`, `ISNULL(<nullable>, ISNULL(NULL, 1))` → 0.
        assert!(
            !b(&call(
                "ISNULL",
                vec![nullable_int(), cast(int("1"), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call(
                "ISNULL",
                vec![nullable_int(), call("LEN", vec![text("abc")])]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call(
                "ISNULL",
                vec![
                    nullable_int(),
                    call("ISNULL", vec![lit(Literal::Null), int("1")])
                ]
            ))
            .ty
            .nullable
        );
        // … unless the server does not fold it — `ISNULL(<nullable>, UPPER('a'))` → 1 —
        // or it folds to `NULL`: `TRY_CAST('x' AS int)`, `NULLIF(1, 1)`, `ASCII('')`.
        let nullable_str = || call("UPPER", vec![variable("@@SERVERNAME")]);
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_str(), call("UPPER", vec![text("a")])]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call(
                "ISNULL",
                vec![nullable_str(), call("LTRIM", vec![text("a")])]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_int(), try_cast(text("x"), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_int(), call("NULLIF", vec![int("1"), int("1")])]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_int(), call("ASCII", vec![text("")])]
            ))
            .ty
            .nullable
        );
        // A constant the type of the check cannot hold: `ISNULL(<nullable tinyint>, 300)`
        // and `ISNULL(<nullable tinyint>, CAST(300 AS int))` → 1, `ISNULL(…, 1)` → 0.
        let nullable_tiny = || cast(spid(), ty("tinyint", &[]));
        assert!(
            b(&call("ISNULL", vec![nullable_tiny(), int("300")]))
                .ty
                .nullable
        );
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_tiny(), cast(int("300"), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call("ISNULL", vec![nullable_tiny(), int("1")]))
                .ty
                .nullable
        );
        // A string literal is truncated, not refused: `ISNULL(<varchar(1)>, 'abc…')` → 0.
        let nullable_char = || cast(spid(), ty("varchar", &[TypeArg::Number(1)]));
        assert!(
            !b(&call(
                "ISNULL",
                vec![nullable_char(), text("abcdefghijklmnop")]
            ))
            .ty
            .nullable
        );
        // A non-constant replacement is read as announced, once converted:
        // `ISNULL(<nullable int>, @@SPID + 1)` → 1, `ISNULL(<nullable int>, @@SPID)` → 0,
        // `ISNULL(<nullable datetime>, SYSDATETIME())` → 1, `ISNULL(…, GETDATE())` → 0.
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_int(), arith(spid(), int("1"))]
            ))
            .ty
            .nullable
        );
        assert!(!b(&call("ISNULL", vec![nullable_int(), spid()])).ty.nullable);
        let nullable_datetime = || cast(call("GETDATE", vec![]), ty("datetime", &[]));
        assert!(
            b(&call(
                "ISNULL",
                vec![nullable_datetime(), call("SYSDATETIME", vec![])]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call(
                "ISNULL",
                vec![nullable_datetime(), call("GETDATE", vec![])]
            ))
            .ty
            .nullable
        );
    }

    /// `COALESCE` on the shapes of `coalesce_is_nullable`.
    #[test]
    fn coalesce_is_a_case_over_its_branches() {
        let spid = || variable("@@SPID");
        let nullable_int = || call("ABS", vec![spid()]);
        // `COALESCE(@@SPID, 2)` → 0, `COALESCE(<nullable>, 2)` → 1.
        assert!(!b(&call("COALESCE", vec![spid(), int("2")])).ty.nullable);
        assert!(
            b(&call("COALESCE", vec![nullable_int(), int("2")]))
                .ty
                .nullable
        );
        // `COALESCE(@@SPID, CAST(2 AS int))`, `COALESCE(@@SPID, LEN('abc'))` → 1: a
        // branch is announced nullable.
        assert!(
            b(&call(
                "COALESCE",
                vec![spid(), cast(int("2"), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call(
                "COALESCE",
                vec![spid(), call("LEN", vec![text("abc")])]
            ))
            .ty
            .nullable
        );
        // `COALESCE(@@SPID, 1.5)` → 0: a `smallint` fits the common `numeric`.
        assert!(
            !b(&call(
                "COALESCE",
                vec![spid(), lit(Literal::Decimal("1.5".to_owned()))]
            ))
            .ty
            .nullable
        );
        // `COALESCE(NULL, 1)`, `COALESCE(1, CAST(2 AS int))`, `COALESCE(1, <nullable>)`
        // → 0: a leading `NULL` drops, a leading literal is the answer.
        assert!(
            !b(&call("COALESCE", vec![lit(Literal::Null), int("1")]))
                .ty
                .nullable
        );
        assert!(
            !b(&call(
                "COALESCE",
                vec![int("1"), cast(int("2"), ty("int", &[]))]
            ))
            .ty
            .nullable
        );
        assert!(
            !b(&call("COALESCE", vec![int("1"), nullable_int()]))
                .ty
                .nullable
        );
        // `COALESCE(1, 1.5)` → 1: the leading literal is the answer, converted to the
        // `numeric(2,1)` the server computes. Asserted on the rule and not on the call:
        // `sysfn` types that call `numeric(11,1)` — it does not give the literal its own
        // digits as `expr::numeric_view` does for a `CASE` — and an `int` fits that.
        let one = BoundExpr {
            kind: BoundExprKind::Literal(Value::I32(1)),
            ty: TypeInfo::new(SqlType::Int, false),
            line: 1,
        };
        let numeric =
            |precision: u8, scale: u8| TypeInfo::new(SqlType::Numeric { precision, scale }, false);
        assert!(super::coalesce_is_nullable(
            std::slice::from_ref(&one),
            &numeric(2, 1)
        ));
        assert!(!super::coalesce_is_nullable(
            std::slice::from_ref(&one),
            &numeric(11, 1)
        ));
        // `COALESCE(NULL, <nullable>, 1)`, `COALESCE(CAST(1 AS int), 2)` → 1.
        assert!(
            b(&call(
                "COALESCE",
                vec![lit(Literal::Null), nullable_int(), int("1")]
            ))
            .ty
            .nullable
        );
        assert!(
            b(&call(
                "COALESCE",
                vec![cast(int("1"), ty("int", &[])), int("2")]
            ))
            .ty
            .nullable
        );
    }

    /// `@x` or `@@x` on line 1.
    fn variable(name: &str) -> Expr {
        Expr::Variable {
            name: name.to_owned(),
            span: span(1),
        }
    }

    /// `-<expr>` on line 1, the shape the parser gives a negative literal.
    fn minus(expr: Expr) -> Expr {
        Expr::Unary {
            op: vauban_parser::UnaryOp::Minus,
            expr: Box::new(expr),
            span: span(1),
        }
    }

    /// `<left> + <right>` on line 1, bound through `expr.rs` as an argument is.
    fn arith(left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op: vauban_parser::BinaryOp::Add,
            op_span: span(1),
            left: Box::new(left),
            right: Box::new(right),
            span: span(1),
        }
    }
}

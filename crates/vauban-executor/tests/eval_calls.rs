//! `CASE`, `CAST`/`CONVERT`, the calls of the `sysfn` registry and `IN`.
//!
//! # Why the trees below are built by hand
//!
//! The trees are built the way `tests/eval_expr.rs` builds them, with the **real rules**
//! of the crates the binder would have used for each piece: `parse_literal` for a
//! literal, `binary_op_type` for an operator and `check_call` for the result type of a
//! call, so that `EvalArgs::result` is genuinely what the binder would have put on the
//! node. Each test states the SQL it stands for.
//!
//! Only the target type of a conversion is written by hand, mirroring
//! `call::bind_conversion`: `TypeInfo::new(target, try_ || source.nullable)`, with the
//! collation of the source kept for a string-to-string conversion.

use std::cell::Cell;
use std::sync::Once;

use vauban_binder::{BoundCaseArm, BoundExpr, BoundExprKind, CompareOp, SessionOptions};
use vauban_errors::{SqlError, SqlResult};
use vauban_executor::{ExecContext, eval_expr};
use vauban_sysfn::{
    Arity, EvalArgs, EvalContext, FunctionDef, FunctionKind, StaticContext, check_call, lookup,
    register, register_builtins,
};
use vauban_types::{
    BinaryOp, Decimal, Len, LiteralKind, SqlString, SqlType, TypeInfo, Value, binary_op_type,
    parse_literal,
};

// ---------------------------------------------------------------------------------------
// Evaluating
// ---------------------------------------------------------------------------------------

/// Evaluates `expr` with the session context `context`, the built-ins registered.
fn run_with(expr: &BoundExpr, context: &dyn EvalContext) -> SqlResult<Value> {
    register_builtins();
    let mut ctx = ExecContext::scalar(context, SessionOptions::default());
    eval_expr(expr, None, &mut ctx)
}

/// The value of `expr` under the default options.
fn v(expr: &BoundExpr) -> Value {
    run_with(expr, &StaticContext::default()).expect("the expression evaluates")
}

/// The error `expr` raises under the default options.
fn err(expr: &BoundExpr) -> SqlError {
    run_with(expr, &StaticContext::default()).expect_err("the expression raises")
}

/// The text of a [`Value::String`], for readable assertions.
fn text_of(value: &Value) -> &str {
    match value {
        Value::String(s) => &s.text,
        other => panic!("expected a character value, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Building the bound tree a statement would have produced
// ---------------------------------------------------------------------------------------

/// A literal, with the value and the type `types` gives it (`parse_literal`).
fn lit(kind: LiteralKind, text: &str) -> BoundExpr {
    let (value, ty) = parse_literal(kind, text).expect("the literal is well formed");
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty,
        line: 1,
    }
}

/// An integer literal: `1`, `0`, `300`.
fn int(text: &str) -> BoundExpr {
    lit(LiteralKind::Integer, text)
}

/// A fixed-point literal: `1.5`.
fn dec(text: &str) -> BoundExpr {
    lit(LiteralKind::Decimal, text)
}

/// A character literal written `'…'`, typed `varchar(n)`.
fn str_lit(text: &str) -> BoundExpr {
    lit(LiteralKind::Str, text)
}

/// A character literal written `N'…'`, typed `nvarchar(n)`.
fn nstr_lit(text: &str) -> BoundExpr {
    lit(LiteralKind::NStr, text)
}

/// The untyped `NULL`, which the binder binds to `Value::Null` typed `int`, nullable.
fn null() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo::new(SqlType::Int, true),
        line: 1,
    }
}

/// The bare `NULL` as `call::retype_untyped_nulls` leaves it **inside a call**: a bare
/// `NULL` has no type of its own, so the binder gives it the type its typed siblings imply,
/// nullable. `ISNULL(NULL, 'x')` is a `varchar` holding `x`, not an `int`, precisely because
/// of that pass, and the contract of `BoundExprKind::Function` is that `args[i].ty` is what
/// the executor hands to `sysfn`.
fn null_like(sibling: &BoundExpr) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo {
            nullable: true,
            ..sibling.ty.clone()
        },
        line: 1,
    }
}

/// A literal of an arbitrary value and type, for the operands a literal cannot spell.
fn value_of(value: Value, ty: SqlType) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(value),
        ty: TypeInfo::new(ty, false),
        line: 1,
    }
}

/// `left op right`, typed by `binary_op_type` the way `bind_arith` types it.
fn arith(op: BinaryOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    let ty = binary_op_type(op, &left.ty, &right.ty).expect("the operands accept the operator");
    BoundExpr {
        kind: BoundExprKind::Arith {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty,
        line: 1,
    }
}

/// `1 / 0`: the expression whose evaluation raises 8134, used to prove a short-circuit.
fn divide_by_zero() -> BoundExpr {
    arith(BinaryOp::Div, int("1"), int("0"))
}

/// `left = right`, or any other comparison, typed `bit` as `bind_comparison` types it.
fn cmp(op: CompareOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    let nullable = left.ty.nullable || right.ty.nullable;
    BoundExpr {
        kind: BoundExprKind::Compare {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line: 1,
    }
}

/// A searched `CASE`, the only shape the binder produces: `bind_case` desugars
/// `CASE a WHEN b THEN …` into `CASE WHEN a = b THEN …` and always leaves `operand` at
/// `None`.
///
/// The type of the node is the type of the first `THEN`, which is enough here: every vector
/// below has branches of one type, so the `Convert` nodes `bind_case` would insert around
/// the branches would all be identities.
fn case(arms: Vec<(BoundExpr, BoundExpr)>, else_: Option<BoundExpr>) -> BoundExpr {
    let ty = TypeInfo {
        nullable: else_.is_none(),
        ..arms
            .first()
            .expect("a CASE has at least one arm")
            .1
            .ty
            .clone()
    };
    BoundExpr {
        kind: BoundExprKind::Case {
            operand: None,
            arms: arms
                .into_iter()
                .map(|(when, then)| BoundCaseArm { when, then })
                .collect(),
            else_: else_.map(Box::new),
        },
        ty,
        line: 1,
    }
}

/// `CASE a WHEN b THEN … WHEN c THEN … [ELSE …] END`, in the desugared shape the binder
/// produces: the operand is **cloned into each arm**, which is what makes it read once per
/// tested arm.
fn simple_case(
    operand: &BoundExpr,
    arms: Vec<(BoundExpr, BoundExpr)>,
    else_: Option<BoundExpr>,
) -> BoundExpr {
    let arms = arms
        .into_iter()
        .map(|(when, then)| (cmp(CompareOp::Eq, operand.clone(), when), then))
        .collect();
    case(arms, else_)
}

/// `CAST(expr AS to)` or `TRY_CAST(expr AS to)`, typed as `call::bind_conversion` types it.
fn cast(expr: BoundExpr, to: SqlType, try_: bool) -> BoundExpr {
    conversion(expr, to, None, try_)
}

/// `CONVERT(to, expr, style)` and its `TRY_` twin: the same bound node as [`cast`], with a
/// style.
fn conversion(expr: BoundExpr, to: SqlType, style: Option<i32>, try_: bool) -> BoundExpr {
    let mut ty = TypeInfo::new(to, try_ || expr.ty.nullable);
    if to.is_string() && expr.ty.ty.is_string() {
        ty.collation = expr.ty.collation;
    }
    BoundExpr {
        kind: BoundExprKind::Convert {
            expr: Box::new(expr),
            style,
            try_,
        },
        ty,
        line: 1,
    }
}

/// A call of a registered built-in, typed by `sysfn::check_call` itself: `EvalArgs::result`
/// is then exactly the type the binder would have computed.
fn call(name: &str, args: Vec<BoundExpr>) -> BoundExpr {
    register_builtins();
    let def = lookup(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    let types: Vec<TypeInfo> = args.iter().map(|arg| arg.ty.clone()).collect();
    let ty = check_call(def, &types).expect("the call is well formed");
    BoundExpr {
        kind: BoundExprKind::Function { def, args },
        ty,
        line: 1,
    }
}

/// `expr [NOT] IN (list)`, typed `bit` as `bind_in` types it. Every vector below already
/// has one common type, so no `Convert` is inserted.
fn in_list(expr: BoundExpr, list: Vec<BoundExpr>, negated: bool) -> BoundExpr {
    let nullable = expr.ty.nullable || list.iter().any(|item| item.ty.nullable);
    BoundExpr {
        kind: BoundExprKind::In {
            expr: Box::new(expr),
            list,
            negated,
        },
        ty: TypeInfo::new(SqlType::Bit, nullable),
        line: 1,
    }
}

// ---------------------------------------------------------------------------------------
// `CASE`
// ---------------------------------------------------------------------------------------

#[test]
fn case_searched() {
    // `CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END`
    let matched = case(
        vec![(cmp(CompareOp::Eq, int("1"), int("1")), str_lit("a"))],
        Some(str_lit("b")),
    );
    assert_eq!(text_of(&v(&matched)), "a");

    // `CASE WHEN 1 = 0 THEN 'a' ELSE 'b' END`
    let unmatched = case(
        vec![(cmp(CompareOp::Eq, int("1"), int("0")), str_lit("a"))],
        Some(str_lit("b")),
    );
    assert_eq!(text_of(&v(&unmatched)), "b");

    // `CASE WHEN 1 = 0 THEN 'a' END`: no `ELSE`, so the answer is `NULL`.
    let no_else = case(
        vec![(cmp(CompareOp::Eq, int("1"), int("0")), str_lit("a"))],
        None,
    );
    assert_eq!(v(&no_else), Value::Null);
}

#[test]
fn case_unknown_is_not_true() {
    // `CASE WHEN NULL = NULL THEN 'a' ELSE 'b' END`: the `WHEN` is unknown, which is not
    // true, so the arm does not fire.
    let expr = case(
        vec![(cmp(CompareOp::Eq, null(), null()), str_lit("a"))],
        Some(str_lit("b")),
    );
    assert_eq!(text_of(&v(&expr)), "b");
}

#[test]
fn case_short_circuits() {
    // `CASE WHEN 1 = 0 THEN 1 / 0 ELSE 1 END`: the `THEN` of an arm that did not fire is
    // never evaluated, so 8134 is not raised.
    let then_is_spared = case(
        vec![(cmp(CompareOp::Eq, int("1"), int("0")), divide_by_zero())],
        Some(int("1")),
    );
    assert_eq!(v(&then_is_spared), Value::I32(1));

    // `CASE WHEN 1 = 1 THEN 1 ELSE 1 / 0 END`: a matched arm spares the `ELSE`.
    let else_is_spared = case(
        vec![(cmp(CompareOp::Eq, int("1"), int("1")), int("1"))],
        Some(divide_by_zero()),
    );
    assert_eq!(v(&else_is_spared), Value::I32(1));

    // And the `WHEN` of a later arm is spared too.
    let later_when_is_spared = case(
        vec![
            (cmp(CompareOp::Eq, int("1"), int("1")), int("1")),
            (
                cmp(CompareOp::Eq, divide_by_zero(), int("1")),
                divide_by_zero(),
            ),
        ],
        None,
    );
    assert_eq!(v(&later_when_is_spared), Value::I32(1));
}

#[test]
fn case_simple() {
    // `CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END`, in the shape `bind_case` produces.
    let expr = simple_case(
        &int("2"),
        vec![(int("1"), str_lit("a")), (int("2"), str_lit("b"))],
        None,
    );
    assert_eq!(text_of(&v(&expr)), "b");
}

/// Counts how many times the evaluation asked the session for `@@SPID`.
///
/// `@@SPID` rather than `GETDATE()` because it is an integer: the desugared `CASE`
/// compares the operand with each `WHEN` and an integer
/// comparison needs no conversion node, which keeps the tree the same shape as the one the
/// binder builds. Everything but `spid` is delegated to a [`StaticContext`].
struct SpidCounter {
    /// The value `@@SPID` answers.
    spid: i16,
    /// How many times it has been asked for.
    reads: Cell<u32>,
    /// The answers to everything else.
    inner: StaticContext,
}

impl SpidCounter {
    fn new(spid: i16) -> Self {
        Self {
            spid,
            reads: Cell::new(0),
            inner: StaticContext::default(),
        }
    }
}

impl EvalContext for SpidCounter {
    fn spid(&self) -> i16 {
        self.reads.set(self.reads.get() + 1);
        self.spid
    }

    fn now_local(&self) -> vauban_types::DateTime2 {
        self.inner.now_local()
    }

    fn now_utc(&self) -> vauban_types::DateTime2 {
        self.inner.now_utc()
    }

    fn rowcount(&self) -> i64 {
        self.inner.rowcount()
    }

    fn last_identity(&self) -> Option<Decimal> {
        self.inner.last_identity()
    }

    fn current_database(&self) -> &str {
        self.inner.current_database()
    }

    fn server_name(&self) -> &str {
        self.inner.server_name()
    }

    fn object_id(&self, name: &str) -> Option<i32> {
        self.inner.object_id(name)
    }

    fn object_name(&self, id: i32) -> Option<String> {
        self.inner.object_name(id)
    }

    fn variable(&self, name: &str) -> Option<Value> {
        self.inner.variable(name)
    }
}

/// `CASE @@SPID WHEN 1 THEN 1 WHEN 2 THEN 2 ELSE 0 END`, and how often `@@SPID` is read.
fn spid_case_reads(spid: i16) -> (Value, u32) {
    let smallint = |n: i16| value_of(Value::I16(n), SqlType::SmallInt);
    let expr = simple_case(
        &call("@@SPID", Vec::new()),
        vec![(smallint(1), int("1")), (smallint(2), int("2"))],
        Some(int("0")),
    );
    let context = SpidCounter::new(spid);
    let value = run_with(&expr, &context).expect("the expression evaluates");
    (value, context.reads.get())
}

/// How often the operand of a **simple** `CASE` is read, and why it is not always once.
///
/// The operand is evaluated once when the first arm matches, which is what the first
/// assertion pins down. It is not, when a later arm matches: `bind_case` desugars the
/// simple form by **cloning** the operand into every arm. That is not a gap: SQL Server
/// does the same, as the query quoted in the documentation of `expr.rs` shows (a quarter of
/// the rows fall through both arms of `CASE ABS(CHECKSUM(NEWID())) % 2 WHEN 0 … WHEN 1 …`,
/// which is only possible if the operand is read again for the second arm).
#[test]
fn case_simple_reads_its_operand_once_per_tested_arm() {
    let (value, reads) = spid_case_reads(1);
    assert_eq!(value, Value::I32(1));
    assert_eq!(reads, 1, "the first arm matches: one read, and no more");

    let (value, reads) = spid_case_reads(2);
    assert_eq!(value, Value::I32(2));
    assert_eq!(reads, 2, "the second arm matches: one read per tested arm");

    let (value, reads) = spid_case_reads(9);
    assert_eq!(value, Value::I32(0));
    assert_eq!(reads, 2, "no arm matches: every arm was tested, none more");
}

// ---------------------------------------------------------------------------------------
// `CAST`, `CONVERT` and their `TRY_` twins
// ---------------------------------------------------------------------------------------

#[test]
fn cast_numeric() {
    // `CAST(1.5 AS int)`: truncation towards zero, not rounding.
    let truncates = cast(dec("1.5"), SqlType::Int, false);
    assert_eq!(v(&truncates), Value::I32(1));

    // `CAST(1 AS varchar(10))`
    let to_text = cast(int("1"), SqlType::VarChar(Len::Fixed(10)), false);
    assert_eq!(text_of(&v(&to_text)), "1");
}

#[test]
fn cast_null_is_null() {
    // `CAST(NULL AS int)`
    assert_eq!(v(&cast(null(), SqlType::Int, false)), Value::Null);
    // And a `NULL` of a character type, which never reaches `types::convert` either.
    let typed = BoundExpr {
        kind: BoundExprKind::Literal(Value::Null),
        ty: TypeInfo::new(SqlType::VarChar(Len::Fixed(3)), true),
        line: 1,
    };
    assert_eq!(v(&cast(typed, SqlType::Int, false)), Value::Null);
}

#[test]
fn cast_failure_is_245() {
    // `CAST('abc' AS int)`: the source type named in the message is the one the binder gave
    // the operand, never a guess made from the value.
    let error = err(&cast(str_lit("abc"), SqlType::Int, false));
    assert_eq!(error.number, 245);
    assert_eq!(error.severity, 16);
    assert_eq!(error.state, 1);
    assert!(error.message.contains("varchar"), "{}", error.message);
}

/// What a `TRY_CAST` swallows, and what it does not.
///
/// `TRY_CAST('abc' AS int)` and `TRY_CONVERT(int, 'abc')` are `NULL`, which is the easy
/// half. The interesting one is `TRY_CAST(300 AS tinyint)`, which answers `NULL` on SQL
/// Server, so an **overflow is swallowed too**, and so is an impossible `CONVERT` style.
/// `convert::RAISED_THROUGH_TRY` states the rule the other way round: everything becomes
/// `NULL` but 529 and the internal 50000.
#[test]
fn try_cast_swallows_the_error() {
    let bad_value = cast(str_lit("abc"), SqlType::Int, true);
    assert_eq!(v(&bad_value), Value::Null);

    let converted = conversion(str_lit("abc"), SqlType::Int, None, true);
    assert_eq!(v(&converted), Value::Null);

    // `CAST(300 AS tinyint)` is error 220; `TRY_CAST(300 AS tinyint)` is `NULL`.
    let raising = cast(int("300"), SqlType::TinyInt, false);
    assert_eq!(err(&raising).number, 220);
    let swallowed = cast(int("300"), SqlType::TinyInt, true);
    assert_eq!(v(&swallowed), Value::Null);

    // `TRY_CAST(123456.789 AS decimal(5, 2))`: an overflow of an exact target, 8115.
    let target = SqlType::Decimal {
        precision: 5,
        scale: 2,
    };
    assert_eq!(err(&cast(dec("123456.789"), target, false)).number, 8115);
    assert_eq!(v(&cast(dec("123456.789"), target, true)), Value::Null);
}

#[test]
fn convert_with_style() {
    // `CONVERT(varchar(30), CAST('2020-01-02' AS date), 112)`: style 112 is `yyyymmdd`,
    // so the answer is `20200102`.
    let date = cast(str_lit("2020-01-02"), SqlType::Date, false);
    let formatted = conversion(date, SqlType::VarChar(Len::Fixed(30)), Some(112), false);
    assert_eq!(text_of(&v(&formatted)), "20200102");
}

// ---------------------------------------------------------------------------------------
// Calls of the registry
// ---------------------------------------------------------------------------------------

#[test]
fn function_calls() {
    // `LEN('abc')`
    assert_eq!(v(&call("LEN", vec![str_lit("abc")])), Value::I32(3));
    // `ISNULL(NULL, 'x')` and `ISNULL('a', 'x')`
    let replaced = call("ISNULL", vec![null_like(&str_lit("x")), str_lit("x")]);
    assert_eq!(text_of(&v(&replaced)), "x");
    let kept = call("ISNULL", vec![str_lit("a"), str_lit("x")]);
    assert_eq!(text_of(&v(&kept)), "a");
    // `COALESCE(NULL, NULL, 3)`
    let three = int("3");
    let coalesced = call(
        "COALESCE",
        vec![null_like(&three), null_like(&three), three.clone()],
    );
    assert_eq!(v(&coalesced), Value::I32(3));
}

/// Name of the test function that inspects its own [`EvalArgs`].
const PROBE: &str = "TEST_EVAL_ARGS";

/// Result type of [`PROBE`]: `nvarchar(200)`, wide enough for the description it writes.
fn probe_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    // Reads the argument so that a wrong arity would be visible here as well.
    let _ = args;
    Ok(TypeInfo::new(SqlType::NVarChar(Len::Fixed(200)), false))
}

/// Writes a declared type the way T-SQL spells it, for the three types this test uses.
fn spell(info: &TypeInfo) -> String {
    match info.ty {
        SqlType::VarChar(Len::Fixed(n)) => format!("varchar({n})"),
        SqlType::NVarChar(Len::Fixed(n)) => format!("nvarchar({n})"),
        other => format!("{other:?}"),
    }
}

/// Answers `<len> values, <len> types, types[0]=…, result=…`: everything the executor
/// promised to hand over.
fn probe_eval(args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Ok(Value::String(SqlString {
        text: format!(
            "{} values, {} types, types[0]={}, result={}",
            args.values.len(),
            args.types.len(),
            spell(&args.types[0]),
            spell(args.result),
        ),
    }))
}

/// Registers [`PROBE`] once for the whole test binary: a second registration panics.
fn register_probe() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register(FunctionDef {
            name: PROBE,
            kind: FunctionKind::Scalar,
            deterministic: true,
            arity: Arity::Exact(1),
            return_type: probe_return_type,
            eval: probe_eval,
            aggregate: None,
        });
    });
}

/// The reason `FunctionDef::eval` takes an [`EvalArgs`] and not a slice of values.
///
/// `'a'` and `N'a'` evaluate to the very same [`Value::String`]; only the bound plan knows
/// that one is a `varchar(1)` and the other an `nvarchar(1)`. Without the types travelling
/// with the values, `DATALENGTH('abc')` and `DATALENGTH(N'abc')` could not answer 3 and 6.
#[test]
fn eval_args_carry_the_bound_types() {
    register_probe();

    let ascii = str_lit("a");
    let national = nstr_lit("a");
    // The same value, two types: this is the whole point.
    assert_eq!(v(&ascii), v(&national));

    let described = v(&call(PROBE, vec![str_lit("a")]));
    assert_eq!(
        text_of(&described),
        "1 values, 1 types, types[0]=varchar(1), result=nvarchar(200)"
    );

    let described = v(&call(PROBE, vec![nstr_lit("é")]));
    assert_eq!(
        text_of(&described),
        "1 values, 1 types, types[0]=nvarchar(1), result=nvarchar(200)"
    );

    // And the registry itself agrees on the result type: `args.result` is `check_call`'s
    // answer, not something the executor made up.
    let def = lookup(PROBE).expect("the probe is registered");
    let from_check_call = check_call(def, &[str_lit("a").ty]).expect("the call is well formed");
    assert_eq!(spell(&from_check_call), "nvarchar(200)");

    // The same on a real built-in: `DATALENGTH` answers 3 for a `varchar(3)` and 6 for an
    // `nvarchar(3)` holding the same three characters.
    assert_eq!(v(&call("DATALENGTH", vec![str_lit("abc")])), Value::I32(3));
    assert_eq!(v(&call("DATALENGTH", vec![nstr_lit("abc")])), Value::I32(6));
}

/// The two lazy built-ins, and the closed list they form.
///
/// `ISNULL(1, 1 / 0)` and `COALESCE(1, 1 / 0)` answer 1 without raising 8134: the arguments
/// after the first non-`NULL` one are not evaluated at all. Every other function of the
/// registry evaluates all of its arguments, which the third assertion pins down.
#[test]
fn isnull_does_not_evaluate_the_replacement() {
    let isnull = call("ISNULL", vec![int("1"), divide_by_zero()]);
    assert_eq!(v(&isnull), Value::I32(1));

    let coalesce = call("COALESCE", vec![int("1"), divide_by_zero()]);
    assert_eq!(v(&coalesce), Value::I32(1));

    // The laziness stops at the first non-`NULL`: a `NULL` first argument does evaluate the
    // second, and `COALESCE(NULL, 1 / 0)` therefore raises, as SQL Server's own rewriting
    // into a `CASE` would.
    let eager = call("COALESCE", vec![null(), divide_by_zero()]);
    assert_eq!(err(&eager).number, 8134);

    // And a function that is not on the list evaluates everything: `NULLIF` is next to
    // `ISNULL` in `sysfn` and is **not** lazy.
    let nullif = call("NULLIF", vec![int("1"), divide_by_zero()]);
    assert_eq!(err(&nullif).number, 8134);
}

// ---------------------------------------------------------------------------------------
// `IN`
// ---------------------------------------------------------------------------------------

#[test]
fn in_three_valued() {
    // `1 IN (1, 2)`
    let found = in_list(int("1"), vec![int("1"), int("2")], false);
    assert_eq!(v(&found), Value::Bit(true));

    // `3 IN (1, 2)`
    let absent = in_list(int("3"), vec![int("1"), int("2")], false);
    assert_eq!(v(&absent), Value::Bit(false));

    // `3 IN (1, NULL)`: no equality holds and one is unknown, so the whole thing is unknown.
    let unknown = in_list(int("3"), vec![int("1"), null()], false);
    assert_eq!(v(&unknown), Value::Null);

    // `1 IN (1, NULL)`: true wins over unknown.
    let true_wins = in_list(int("1"), vec![int("1"), null()], false);
    assert_eq!(v(&true_wins), Value::Bit(true));

    // `3 NOT IN (1, NULL)`: `NOT unknown` is still unknown — the classic trap.
    let negated_unknown = in_list(int("3"), vec![int("1"), null()], true);
    assert_eq!(v(&negated_unknown), Value::Null);

    // `3 NOT IN (1, 2)`
    let negated_false = in_list(int("3"), vec![int("1"), int("2")], true);
    assert_eq!(v(&negated_false), Value::Bit(true));

    // `NULL IN (1)`: the tested value is unknown, so the answer is too.
    let null_operand = in_list(null(), vec![int("1")], false);
    assert_eq!(v(&null_operand), Value::Null);
}

/// Every element of a list is evaluated — which is a **gap**, not a rule of the engine.
///
/// `IN` is `x = a OR x = b`, and this crate evaluates both operands of an `OR`, so it
/// evaluates every element and raises 8134 even though the first one already matches. SQL
/// Server answers 1 instead.
///
/// On one and the same variable declaration, `@a IN (3, 1 / 0)` raises a division error and
/// `@a IN (1 / 0, 3)` answers a value on SQL Server. What tells the two apart is described
/// in the module documentation of `expr.rs`, next to the same gap on `AND`. The assertion
/// below pins what this crate does today, not what SQL Server does.
#[test]
fn in_evaluates_every_element() {
    let expr = in_list(int("1"), vec![int("1"), divide_by_zero()], false);
    assert_eq!(err(&expr).number, 8134);
}

// ---------------------------------------------------------------------------------------
// Four expressions of one select list
// ---------------------------------------------------------------------------------------

/// `SELECT 1 + 1, LEN('abc'), CAST(1.5 AS int), ISNULL(NULL, 'x')`, expression by
/// expression.
#[test]
fn the_four_expressions_of_one_select() {
    assert_eq!(v(&arith(BinaryOp::Add, int("1"), int("1"))), Value::I32(2));
    assert_eq!(v(&call("LEN", vec![str_lit("abc")])), Value::I32(3));
    assert_eq!(v(&cast(dec("1.5"), SqlType::Int, false)), Value::I32(1));
    assert_eq!(
        text_of(&v(&call(
            "ISNULL",
            vec![null_like(&str_lit("x")), str_lit("x")]
        ))),
        "x"
    );
}

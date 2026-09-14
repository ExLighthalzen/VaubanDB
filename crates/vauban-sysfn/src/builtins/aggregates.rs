//! `COUNT`, `COUNT_BIG`, `SUM`, `AVG`, `MIN` and `MAX`: the six aggregates of the V1.
//!
//! An aggregate is not evaluated like a scalar function: the `executor` asks
//! [`FunctionDef::aggregate`] for one [`AggregateState`] per group, feeds it one value per
//! row with `step`, then calls `finish`. The `eval` field of the six definitions below is
//! therefore not called by the `executor` and reports an internal bug.
//!
//! # Return types
//!
//! Microsoft Learn, "SUM (Transact-SQL)" and "AVG (Transact-SQL)", table *Return types*:
//!
//! | argument | `SUM` ([`additive_result_type`]) | `AVG` ([`average_result_type`]) |
//! |---|---|---|
//! | `tinyint`, `smallint`, `int` | `int` | `int` |
//! | `bigint` | `bigint` | `bigint` |
//! | `decimal(p, s)`, `numeric(p, s)` | `decimal(38, s)` | `decimal(38, s)` divided by `int` |
//! | `money`, `smallmoney` | `money` | `money` |
//! | `float`, `real` | `float` | `float` |
//! | anything else | error 8117 | error 8117 |
//!
//! The visible consequence of the first row: `AVG` over `int` values is an **integral**
//! division, so the average of 1, 2 and 2 is 1. The third row is the subtle one, which
//! [`average_result_type`] documents: the average of a `decimal` is not a
//! `decimal(38, s)` but that type *divided by an `int`*, which raises the scale to 6.
//! `COUNT` returns an `int` and `COUNT_BIG` a `bigint`, both non-nullable; `MIN` and
//! `MAX` return the type of their argument, nullable (`min_max_use_the_collation`).
//!
//! # `NULL`
//!
//! The six aggregates ignore `NULL` values (Learn, "Aggregate functions (Transact-SQL)").
//! An empty group, or a group of nothing but `NULL`s, aggregates to `NULL` — to `0` for
//! `COUNT` and `COUNT_BIG`. As soon as one `NULL` has been ignored, SQL Server emits the
//! informational message 8153; the accumulator only records the fact, in
//! [`AggregateState::null_eliminated`], and the `executor` builds the message.
//!
//! # What is not here
//!
//! `COUNT(*)` counts rows, not values: it has no argument and is not an entry of this
//! registry. It is a syntactic form the `binder` rewrites.
//! `DISTINCT` is removed by the `binder`/`executor` before the values reach an
//! accumulator, and the grouping itself belongs to the `planner`/`executor`.
//!
//! No arithmetic rule is written here: the additions and the division go through
//! [`vauban_types::eval_binary`], the widening through
//! [`vauban_types::convert`], and the comparison of `MIN`/`MAX` through
//! [`vauban_types::compare`].

use std::cmp::Ordering;

use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_types::{
    BinaryOp, Collation, SqlType, TypeInfo, Value, binary_op_type, compare, convert, eval_binary,
};

use crate::context::EvalContext;
use crate::registry::{AggregateState, Arity, EvalArgs, FunctionDef, FunctionKind, register};

/// The precision `SUM` and `AVG` give to an exact numeric result, whatever the precision
/// of their argument (Learn: `decimal(p, s)` aggregates to `decimal(38, s)`).
const AGGREGATE_PRECISION: u8 = 38;

// ---------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------

/// The type of the only argument of an aggregate.
///
/// `check_call` has already checked the arity, so the argument is always there; a missing
/// one is a broken precondition of the caller, reported as an internal bug rather than as
/// a SQL error, and never as a panic (which the conventions forbid on the query path).
fn sole_argument_type<'a>(args: &'a [TypeInfo], function: &str) -> SqlResult<&'a TypeInfo> {
    args.first()
        .ok_or_else(|| InternalError::Bug(format!("{function}: the argument is missing")).into())
}

/// The result type of `SUM` and `AVG` over an argument of type `arg`.
///
/// The table of the module documentation. The result is always nullable: a group with no
/// non-`NULL` value aggregates to `NULL` even when the column cannot hold one.
///
/// # Errors
///
/// 8117 for an argument type that is not a number (`sum_of_a_string_is_8117`):
/// `operator` is the lower-case name of the aggregate, and the name of the type comes from
/// [`SqlType::error_name`] (`numeric` for `decimal`), not from a table local to this
/// crate. `bit` is no more summable than a string, as `bit + bit` is no addition.
///
/// The untyped `NULL` constant is refused the same way, `SELECT SUM(NULL);` raising 8117
/// with the word `NULL` in place of a type name; that word is not the name of any type,
/// so the `binder`, which alone sees an untyped constant, produces it.
fn additive_result_type(arg: &TypeInfo, operator: &str) -> SqlResult<TypeInfo> {
    let ty = match arg.ty {
        SqlType::TinyInt | SqlType::SmallInt | SqlType::Int => SqlType::Int,
        SqlType::BigInt => SqlType::BigInt,
        SqlType::Decimal { scale, .. } => SqlType::Decimal {
            precision: AGGREGATE_PRECISION,
            scale,
        },
        SqlType::Numeric { scale, .. } => SqlType::Numeric {
            precision: AGGREGATE_PRECISION,
            scale,
        },
        SqlType::Float | SqlType::Real => SqlType::Float,
        SqlType::Money | SqlType::SmallMoney => SqlType::Money,
        other => return Err(SqlError::invalid_operand_type(other.error_name(), operator)),
    };
    Ok(TypeInfo::new(ty, true))
}

/// The `eval` every aggregate carries: the field is mandatory in a [`FunctionDef`], but an
/// aggregate is computed by its [`AggregateFactory`](crate::AggregateFactory), never here.
///
/// Reaching this is a bug of the caller (the `executor` dispatches on
/// [`FunctionDef::kind`]), so it is an internal error, not a SQL error the user could have
/// caused. The double `into()` is needed: `SqlResult<T>` is a `Result<T, SqlError>` and the
/// conversion from [`InternalError`] does not happen on its own inside `Err(...)`.
fn not_a_scalar(name: &str) -> SqlError {
    InternalError::Bug(format!(
        "{name} is an aggregate: use FunctionDef::aggregate"
    ))
    .into()
}

// ---------------------------------------------------------------------------------------
// COUNT and COUNT_BIG
// ---------------------------------------------------------------------------------------

/// Accumulator of `COUNT` and `COUNT_BIG`: how many values of the group are not `NULL`.
///
/// One structure for the two, because only the type of the result differs. The count is
/// kept in an `i64` and checked against `i32::MAX` at every `step` for `COUNT`, so the
/// overflow is reported on the row that causes it.
#[derive(Debug)]
struct CountState {
    /// `true` for `COUNT_BIG`, whose result is a `bigint`.
    wide: bool,
    /// Number of non-`NULL` values seen so far.
    count: i64,
    /// `true` as soon as one `NULL` has been ignored (message 8153).
    null_eliminated: bool,
}

impl CountState {
    /// A fresh accumulator, `wide` telling `COUNT_BIG` from `COUNT`.
    fn new(wide: bool) -> Self {
        CountState {
            wide,
            count: 0,
            null_eliminated: false,
        }
    }

    /// The name of the result type, for the overflow message.
    fn result_name(&self) -> &'static str {
        if self.wide { "bigint" } else { "int" }
    }
}

impl AggregateState for CountState {
    fn step(&mut self, v: &Value) -> SqlResult<()> {
        if matches!(v, Value::Null) {
            self.null_eliminated = true;
            return Ok(());
        }
        let next = self
            .count
            .checked_add(1)
            .ok_or_else(|| SqlError::arithmetic_overflow("expression", self.result_name()))?;
        if !self.wide && next > i64::from(i32::MAX) {
            return Err(SqlError::arithmetic_overflow("expression", "int"));
        }
        self.count = next;
        Ok(())
    }

    fn finish(self: Box<Self>) -> SqlResult<Value> {
        if self.wide {
            return Ok(Value::I64(self.count));
        }
        // `step` already refused anything wider than an `int`; the conversion is written
        // without `unwrap` all the same, as the conventions require.
        i32::try_from(self.count)
            .map(Value::I32)
            .map_err(|_| SqlError::arithmetic_overflow("expression", "int"))
    }

    fn null_eliminated(&self) -> bool {
        self.null_eliminated
    }
}

/// Builds the accumulator of `COUNT`. Every argument type is countable.
fn count_factory(_arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    Ok(Box::new(CountState::new(false)))
}

/// Builds the accumulator of `COUNT_BIG`.
fn count_big_factory(_arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    Ok(Box::new(CountState::new(true)))
}

/// Result type of `COUNT`: `int`, non-nullable — an empty group counts 0, never `NULL`.
fn count_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    sole_argument_type(args, "count")?;
    Ok(TypeInfo::new(SqlType::Int, false))
}

/// Result type of `COUNT_BIG`: `bigint`, non-nullable.
fn count_big_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    sole_argument_type(args, "count_big")?;
    Ok(TypeInfo::new(SqlType::BigInt, false))
}

/// `COUNT` is an aggregate: see [`not_a_scalar`].
fn count_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("COUNT"))
}

/// `COUNT_BIG` is an aggregate: see [`not_a_scalar`].
fn count_big_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("COUNT_BIG"))
}

// ---------------------------------------------------------------------------------------
// SUM and AVG
// ---------------------------------------------------------------------------------------

/// Accumulator of `SUM` and of `AVG`, which only differ by their `finish`.
///
/// Every value is widened to the result type before being added, so that the sum of a
/// column of `smallint` overflows at the bounds of an `int` and not at those of a
/// `smallint`. The addition itself is [`vauban_types::eval_binary`]: the propagation
/// of precision and scale belongs to `types`.
#[derive(Debug)]
struct AdditiveState {
    /// Declared type of the aggregated argument, the source of each conversion.
    argument: TypeInfo,
    /// Result type of the aggregate, the target of each conversion and of each addition.
    result: TypeInfo,
    /// Sum of the values seen so far, `None` while none has been seen.
    total: Option<Value>,
    /// Number of non-`NULL` values, the divisor of `AVG`.
    count: i64,
    /// `true` as soon as one `NULL` has been ignored (message 8153).
    null_eliminated: bool,
}

impl AdditiveState {
    /// A fresh accumulator over an argument of type `argument`, whose result is `result`.
    fn new(argument: &TypeInfo, result: TypeInfo) -> Self {
        AdditiveState {
            argument: argument.clone(),
            result,
            total: None,
            count: 0,
            null_eliminated: false,
        }
    }

    /// Adds one non-`NULL` value, already widened to the result type.
    fn accumulate(&mut self, widened: Value) -> SqlResult<()> {
        self.total = Some(match self.total.take() {
            None => widened,
            Some(total) => eval_binary(BinaryOp::Add, &total, &widened, &self.result)?,
        });
        Ok(())
    }
}

impl AggregateState for AdditiveState {
    fn step(&mut self, v: &Value) -> SqlResult<()> {
        if matches!(v, Value::Null) {
            self.null_eliminated = true;
            return Ok(());
        }
        let widened = convert(v, &self.argument, &self.result, None)?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| SqlError::arithmetic_overflow("expression", "bigint"))?;
        self.accumulate(widened)
    }

    fn finish(self: Box<Self>) -> SqlResult<Value> {
        Ok(self.total.unwrap_or(Value::Null))
    }

    fn null_eliminated(&self) -> bool {
        self.null_eliminated
    }
}

/// Accumulator of `AVG`: the sum of the group divided by the number of values it held.
///
/// The division is [`vauban_types::eval_binary`] at the type
/// [`average_result_type`] computed, so `AVG` over `int` values divides two `int`s and
/// truncates towards zero, as `SELECT 5 / 3;` does (`avg_of_int_truncates`).
#[derive(Debug)]
struct AverageState {
    /// The sum and the count, shared with `SUM`.
    sum: AdditiveState,
    /// Type of the quotient, which is not that of the sum for a decimal argument.
    quotient: TypeInfo,
}

impl AggregateState for AverageState {
    fn step(&mut self, v: &Value) -> SqlResult<()> {
        self.sum.step(v)
    }

    fn finish(self: Box<Self>) -> SqlResult<Value> {
        let Some(total) = &self.sum.total else {
            return Ok(Value::Null);
        };
        // `eval_binary` wants both operands in their common type, which is the type of
        // the sum for the five families: `int` against `int`, `decimal(38, s)` against an
        // `int` count, `money` against an `int` count. So the count is read in the type
        // of the sum, and only the *result* type is the quotient's.
        let counter = TypeInfo::new(SqlType::BigInt, false);
        let divisor = convert(
            &Value::I64(self.sum.count),
            &counter,
            &self.sum.result,
            None,
        )?;
        eval_binary(BinaryOp::Div, total, &divisor, &self.quotient)
    }

    fn null_eliminated(&self) -> bool {
        self.sum.null_eliminated()
    }
}

/// The result type of `AVG`: the type of the sum **divided by an `int`**.
///
/// For a `decimal(p, s)` argument the
/// result is `decimal(38, s)` *divided by* `int`, not `decimal(38, s)`: the average goes
/// through the division rule of "Precision, scale, and length", which raises the scale to
/// at least 6. `SELECT AVG(c) FROM (VALUES (CAST(1.00 AS decimal(9,2))), (CAST(2.00 AS
/// decimal(9,2))), (CAST(2.00 AS decimal(9,2)))) v(c);` answers **1.666666** and `SELECT
/// AVG(1.50);` answers **1.500000** (`avg_of_decimal_keeps_the_scale`), where a
/// `decimal(38, 2)` result would have printed `1.66` and `1.50`. The other four families
/// are unaffected: `int / int` is an `int`, `bigint` stays `bigint`, `float` stays
/// `float` and `money` stays `money`.
///
/// The rule itself is not written here: [`vauban_types::binary_op_type`] holds it.
fn average_result_type(sum: &TypeInfo) -> SqlResult<TypeInfo> {
    binary_op_type(BinaryOp::Div, sum, &TypeInfo::new(SqlType::Int, false))
}

/// Result type of `SUM`; 8117 for a non-numeric argument.
fn sum_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    additive_result_type(sole_argument_type(args, "sum")?, "sum")
}

/// Result type of `AVG`: [`average_result_type`]; 8117 for a non-numeric argument.
fn avg_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    let sum = additive_result_type(sole_argument_type(args, "avg")?, "avg")?;
    average_result_type(&sum)
}

/// Builds the accumulator of `SUM`, or reports 8117 for an argument it cannot add.
fn sum_factory(arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    let result = additive_result_type(arg, "sum")?;
    Ok(Box::new(AdditiveState::new(arg, result)))
}

/// Builds the accumulator of `AVG`, or reports 8117 for an argument it cannot average.
fn avg_factory(arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    let sum = additive_result_type(arg, "avg")?;
    let quotient = average_result_type(&sum)?;
    Ok(Box::new(AverageState {
        sum: AdditiveState::new(arg, sum),
        quotient,
    }))
}

/// `SUM` is an aggregate: see [`not_a_scalar`].
fn sum_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("SUM"))
}

/// `AVG` is an aggregate: see [`not_a_scalar`].
fn avg_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("AVG"))
}

// ---------------------------------------------------------------------------------------
// MIN and MAX
// ---------------------------------------------------------------------------------------

/// Accumulator of `MIN` and `MAX`: the value that beats every other one of the group.
///
/// The two differ by one field, `keep`: `MIN` keeps a candidate that compares
/// [`Ordering::Less`] than the champion, `MAX` one that compares [`Ordering::Greater`].
/// The comparison is [`vauban_types::compare`], so strings obey the collation rather
/// than the ordinal order of their bytes — and the collation cannot be read off a
/// [`Value::String`], which is why the factory keeps the one of the argument type.
#[derive(Debug)]
struct ExtremumState {
    /// Collation of the argument, [`Collation::DEFAULT`] when it carries none.
    collation: Collation,
    /// The ordering a candidate must have against the champion to replace it.
    keep: Ordering,
    /// Best value seen so far, `None` while none has been seen.
    best: Option<Value>,
    /// `true` as soon as one `NULL` has been ignored (message 8153).
    null_eliminated: bool,
}

impl ExtremumState {
    /// A fresh accumulator over an argument of type `arg`, keeping the `keep` candidates.
    fn new(arg: &TypeInfo, keep: Ordering) -> Self {
        ExtremumState {
            collation: arg.collation.unwrap_or(Collation::DEFAULT),
            keep,
            best: None,
            null_eliminated: false,
        }
    }
}

impl AggregateState for ExtremumState {
    fn step(&mut self, v: &Value) -> SqlResult<()> {
        if matches!(v, Value::Null) {
            self.null_eliminated = true;
            return Ok(());
        }
        let replaces = match &self.best {
            None => true,
            Some(best) => compare(v, best, &self.collation)? == Some(self.keep),
        };
        if replaces {
            self.best = Some(v.clone());
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> SqlResult<Value> {
        Ok(self.best.unwrap_or(Value::Null))
    }

    fn null_eliminated(&self) -> bool {
        self.null_eliminated
    }
}

/// Result type of `MIN` and `MAX`: the type of the argument, collation included, made
/// nullable — an empty group has no smallest value.
fn extremum_return_type(args: &[TypeInfo], function: &str) -> SqlResult<TypeInfo> {
    let arg = sole_argument_type(args, function)?;
    Ok(TypeInfo {
        nullable: true,
        ..arg.clone()
    })
}

/// Result type of `MIN`.
fn min_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    extremum_return_type(args, "min")
}

/// Result type of `MAX`.
fn max_return_type(args: &[TypeInfo]) -> SqlResult<TypeInfo> {
    extremum_return_type(args, "max")
}

/// Builds the accumulator of `MIN`, which keeps the smallest value under the collation.
fn min_factory(arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    Ok(Box::new(ExtremumState::new(arg, Ordering::Less)))
}

/// Builds the accumulator of `MAX`, which keeps the largest value under the collation.
fn max_factory(arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
    Ok(Box::new(ExtremumState::new(arg, Ordering::Greater)))
}

/// `MIN` is an aggregate: see [`not_a_scalar`].
fn min_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("MIN"))
}

/// `MAX` is an aggregate: see [`not_a_scalar`].
fn max_eval(_args: &EvalArgs<'_>, _ctx: &dyn EvalContext) -> SqlResult<Value> {
    Err(not_a_scalar("MAX"))
}

// ---------------------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------------------

/// `COUNT`: Microsoft Learn, "COUNT (Transact-SQL)". `COUNT(*)` is not this entry.
const COUNT_DEF: FunctionDef = FunctionDef {
    name: "COUNT",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: count_return_type,
    eval: count_eval,
    aggregate: Some(count_factory),
};

/// `COUNT_BIG`: Microsoft Learn, "COUNT_BIG (Transact-SQL)".
const COUNT_BIG_DEF: FunctionDef = FunctionDef {
    name: "COUNT_BIG",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: count_big_return_type,
    eval: count_big_eval,
    aggregate: Some(count_big_factory),
};

/// `SUM`: Microsoft Learn, "SUM (Transact-SQL)".
const SUM_DEF: FunctionDef = FunctionDef {
    name: "SUM",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: sum_return_type,
    eval: sum_eval,
    aggregate: Some(sum_factory),
};

/// `AVG`: Microsoft Learn, "AVG (Transact-SQL)".
const AVG_DEF: FunctionDef = FunctionDef {
    name: "AVG",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: avg_return_type,
    eval: avg_eval,
    aggregate: Some(avg_factory),
};

/// `MIN`: Microsoft Learn, "MIN (Transact-SQL)".
const MIN_DEF: FunctionDef = FunctionDef {
    name: "MIN",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: min_return_type,
    eval: min_eval,
    aggregate: Some(min_factory),
};

/// `MAX`: Microsoft Learn, "MAX (Transact-SQL)".
const MAX_DEF: FunctionDef = FunctionDef {
    name: "MAX",
    kind: FunctionKind::Aggregate,
    deterministic: true,
    arity: Arity::Exact(1),
    return_type: max_return_type,
    eval: max_eval,
    aggregate: Some(max_factory),
};

/// Registers the six aggregates of the V1 in the global registry.
pub(crate) fn register_all() {
    register(COUNT_DEF);
    register(COUNT_BIG_DEF);
    register(SUM_DEF);
    register(AVG_DEF);
    register(MIN_DEF);
    register(MAX_DEF);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::all;
    use crate::builtins::register_builtins;
    use vauban_types::{Decimal, Len, SqlString};

    fn int(nullable: bool) -> TypeInfo {
        TypeInfo::new(SqlType::Int, nullable)
    }

    fn decimal(precision: u8, scale: u8) -> TypeInfo {
        TypeInfo::new(SqlType::Decimal { precision, scale }, true)
    }

    fn numeric(precision: u8, scale: u8) -> TypeInfo {
        TypeInfo::new(SqlType::Numeric { precision, scale }, true)
    }

    fn varchar(length: u16) -> TypeInfo {
        TypeInfo::new(SqlType::VarChar(Len::Fixed(length)), true)
    }

    fn text(value: &str) -> Value {
        Value::String(SqlString {
            text: value.to_owned(),
        })
    }

    fn dec(mantissa: i128, precision: u8, scale: u8) -> Value {
        Value::Decimal(Decimal {
            mantissa,
            precision,
            scale,
        })
    }

    /// Builds the accumulator of `def` over an argument of type `arg`.
    fn state(def: &FunctionDef, arg: &TypeInfo) -> SqlResult<Box<dyn AggregateState>> {
        let factory = def
            .aggregate
            .expect("an aggregate definition must expose its factory");
        factory(arg)
    }

    /// Feeds `values` to a fresh accumulator of `def` and returns its result.
    fn aggregate(def: &FunctionDef, arg: &TypeInfo, values: &[Value]) -> SqlResult<Value> {
        let mut state = state(def, arg)?;
        for value in values {
            state.step(value)?;
        }
        state.finish()
    }

    /// Feeds `values` to a fresh accumulator of `def` and returns its `null_eliminated`
    /// flag together with its result.
    fn aggregate_with_flag(
        def: &FunctionDef,
        arg: &TypeInfo,
        values: &[Value],
    ) -> SqlResult<(Value, bool)> {
        let mut state = state(def, arg)?;
        for value in values {
            state.step(value)?;
        }
        let flag = state.null_eliminated();
        Ok((state.finish()?, flag))
    }

    #[test]
    fn aggregate_field_matches_kind() {
        register_builtins();
        for def in all() {
            assert_eq!(
                def.aggregate.is_some(),
                def.kind == FunctionKind::Aggregate,
                "{} breaks the invariant `aggregate.is_some() == (kind == Aggregate)`",
                def.name
            );
        }
        // The six of this module are really in there, under any case.
        for name in ["count", "COUNT_BIG", "Sum", "avg", "MIN", "max"] {
            let def = crate::lookup(name).expect("the aggregate must be registered");
            assert_eq!(def.kind, FunctionKind::Aggregate);
            assert_eq!(def.arity, Arity::Exact(1));
            assert!(def.deterministic);
        }
    }

    #[test]
    fn count_ignores_nulls() {
        let values = [Value::I32(1), Value::Null, Value::I32(3)];
        assert_eq!(
            aggregate(&COUNT_DEF, &int(true), &values),
            Ok(Value::I32(2))
        );
        assert_eq!(
            aggregate(&COUNT_BIG_DEF, &int(true), &values),
            Ok(Value::I64(2))
        );

        assert_eq!(aggregate(&COUNT_DEF, &int(true), &[]), Ok(Value::I32(0)));
        assert_eq!(
            aggregate(&COUNT_BIG_DEF, &int(true), &[]),
            Ok(Value::I64(0))
        );

        let nulls = [Value::Null, Value::Null];
        assert_eq!(
            aggregate_with_flag(&COUNT_DEF, &int(true), &nulls),
            Ok((Value::I32(0), true))
        );
        // A group without any NULL leaves the flag down.
        assert_eq!(
            aggregate_with_flag(&COUNT_DEF, &int(true), &[Value::I32(1)]),
            Ok((Value::I32(1), false))
        );

        // COUNT is an int and never NULL, COUNT_BIG a bigint.
        let counted = (COUNT_DEF.return_type)(&[int(true)]).expect("COUNT must type");
        assert_eq!(counted.ty, SqlType::Int);
        assert!(!counted.nullable);
        let counted_big = (COUNT_BIG_DEF.return_type)(&[int(true)]).expect("COUNT_BIG must type");
        assert_eq!(counted_big.ty, SqlType::BigInt);
        assert!(!counted_big.nullable);
    }

    #[test]
    fn sum_of_ints_is_int() {
        assert_eq!(
            aggregate(&SUM_DEF, &int(true), &[Value::I32(1), Value::I32(2)]),
            Ok(Value::I32(3))
        );

        let from_smallint =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::SmallInt, true)]).expect("must type");
        assert_eq!(from_smallint.ty, SqlType::Int);
        assert!(from_smallint.nullable);
        let from_tinyint =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::TinyInt, true)]).expect("must type");
        assert_eq!(from_tinyint.ty, SqlType::Int);
        let from_bigint =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::BigInt, true)]).expect("must type");
        assert_eq!(from_bigint.ty, SqlType::BigInt);
        let from_real =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Real, true)]).expect("must type");
        assert_eq!(from_real.ty, SqlType::Float);
        let from_smallmoney =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::SmallMoney, true)]).expect("must type");
        assert_eq!(from_smallmoney.ty, SqlType::Money);

        // A smallint sums in an int: the widening happens before the addition, so the
        // total is not bounded by the argument type.
        let smallint = TypeInfo::new(SqlType::SmallInt, true);
        assert_eq!(
            aggregate(
                &SUM_DEF,
                &smallint,
                &[Value::I16(30_000), Value::I16(30_000)]
            ),
            Ok(Value::I32(60_000))
        );
    }

    #[test]
    fn sum_of_decimal_is_38_s() {
        let from_decimal = (SUM_DEF.return_type)(&[decimal(9, 2)]).expect("must type");
        assert_eq!(
            from_decimal.ty,
            SqlType::Decimal {
                precision: 38,
                scale: 2
            }
        );
        let from_numeric = (SUM_DEF.return_type)(&[numeric(5, 4)]).expect("must type");
        assert_eq!(
            from_numeric.ty,
            SqlType::Numeric {
                precision: 38,
                scale: 4
            }
        );
        // The sum itself keeps the scale of the argument.
        assert_eq!(
            aggregate(&SUM_DEF, &decimal(9, 2), &[dec(100, 9, 2), dec(250, 9, 2)]),
            Ok(dec(350, 38, 2))
        );
    }

    #[test]
    fn sum_overflow_is_an_error() {
        let err = aggregate(&SUM_DEF, &int(true), &[Value::I32(i32::MAX), Value::I32(1)])
            .expect_err("the sum must overflow the int result type");
        assert_eq!(err.number, 8115);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 2);
        assert_eq!(
            err.message,
            "Converting expression to data type int overflowed."
        );
    }

    #[test]
    fn sum_of_a_string_is_8117() {
        let err = match state(&SUM_DEF, &varchar(10)) {
            Err(err) => err,
            // `Box<dyn AggregateState>` is not `Debug`, so `expect_err` is out of reach.
            Ok(_) => panic!("a varchar cannot be summed"),
        };
        assert_eq!(err.number, 8117);
        assert_eq!(err.severity, 16);
        assert_eq!(err.state, 1);
        assert_eq!(
            err.message,
            "Data type varchar is not accepted by the sum operator."
        );
        // The same refusal at binding time, and the same for AVG with its own word.
        let bound = (SUM_DEF.return_type)(&[varchar(10)]).expect_err("must be refused");
        assert_eq!(bound.number, 8117);
        assert_eq!(bound.message, err.message);
        let avg = (AVG_DEF.return_type)(&[varchar(10)]).expect_err("must be refused");
        assert_eq!(
            avg.message,
            "Data type varchar is not accepted by the avg operator."
        );
        // `bit` is no more summable than a string.
        let bit = (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Bit, true)])
            .expect_err("must be refused");
        assert_eq!(bit.number, 8117);
        assert_eq!(
            bit.message,
            "Data type bit is not accepted by the sum operator."
        );
        // The name of the type comes from `SqlType::error_name`: `numeric` for a
        // `decimal`, which is why a decimal argument is accepted and a date is not.
        let date =
            (SUM_DEF.return_type)(&[TypeInfo::new(SqlType::Date, true)]).expect_err("refused");
        assert_eq!(
            date.message,
            "Data type date is not accepted by the sum operator."
        );
    }

    #[test]
    fn avg_of_int_truncates() {
        assert_eq!(
            aggregate(
                &AVG_DEF,
                &int(true),
                &[Value::I32(1), Value::I32(2), Value::I32(2)]
            ),
            Ok(Value::I32(1))
        );
        // Truncation towards zero for a negative average: -5 / 2 is -2, not -3.
        assert_eq!(
            aggregate(&AVG_DEF, &int(true), &[Value::I32(-3), Value::I32(-2)]),
            Ok(Value::I32(-2))
        );
        let typed = (AVG_DEF.return_type)(&[int(true)]).expect("must type");
        assert_eq!(typed.ty, SqlType::Int);
        assert!(typed.nullable);
    }

    #[test]
    fn avg_of_decimal_keeps_the_scale() {
        // The scale of the argument is never lost, but the division raises it to 6: the
        // result is `decimal(38, 2)` divided by an `int`, not `decimal(38, 2)`:
        // SELECT AVG(1.50); answers 1.500000.
        let typed = (AVG_DEF.return_type)(&[decimal(9, 2)]).expect("must type");
        assert_eq!(
            typed.ty,
            SqlType::Decimal {
                precision: 38,
                scale: 6
            }
        );
        assert!(typed.nullable);
        // The `numeric` spelling survives the division (`numeric`, not `decimal`).
        let from_numeric = (AVG_DEF.return_type)(&[numeric(5, 4)]).expect("must type");
        assert_eq!(
            from_numeric.ty,
            SqlType::Numeric {
                precision: 38,
                scale: 6
            }
        );
        // 1.00 and 2.00 average to 1.50, written at the scale of the quotient.
        assert_eq!(
            aggregate(&AVG_DEF, &decimal(9, 2), &[dec(100, 9, 2), dec(200, 9, 2)]),
            Ok(dec(1_500_000, 38, 6))
        );
        // 1.00, 2.00 and 2.00 average to 1.666666: the quotient is truncated, not
        // rounded (a rounding would have written 1.666667).
        assert_eq!(
            aggregate(
                &AVG_DEF,
                &decimal(9, 2),
                &[dec(100, 9, 2), dec(200, 9, 2), dec(200, 9, 2)]
            ),
            Ok(dec(1_666_666, 38, 6))
        );
    }

    #[test]
    fn sum_and_avg_of_the_other_families() {
        // float and real both aggregate as float, money and smallmoney as money: the
        // widening happens at `step`, so a `real` value is added as an `f64`.
        let real = TypeInfo::new(SqlType::Real, true);
        assert_eq!(
            aggregate(&SUM_DEF, &real, &[Value::F32(1.5), Value::F32(2.25)]),
            Ok(Value::F64(3.75))
        );
        assert_eq!(
            aggregate(&AVG_DEF, &real, &[Value::F32(1.5), Value::F32(2.5)]),
            Ok(Value::F64(2.0))
        );
        assert_eq!(
            (AVG_DEF.return_type)(std::slice::from_ref(&real))
                .expect("must type")
                .ty,
            SqlType::Float
        );

        // money is an exact integer of ten-thousandths: 1.5000 + 2.2500 = 3.7500.
        let money = TypeInfo::new(SqlType::Money, true);
        assert_eq!(
            aggregate(
                &SUM_DEF,
                &money,
                &[Value::Money(15_000), Value::Money(22_500)]
            ),
            Ok(Value::Money(37_500))
        );
        assert_eq!(
            aggregate(
                &AVG_DEF,
                &money,
                &[Value::Money(15_000), Value::Money(22_500)]
            ),
            Ok(Value::Money(18_750))
        );
        assert_eq!(
            (AVG_DEF.return_type)(std::slice::from_ref(&money))
                .expect("must type")
                .ty,
            SqlType::Money
        );

        // bigint stays bigint on both sides, and the average is still integral.
        let bigint = TypeInfo::new(SqlType::BigInt, true);
        assert_eq!(
            aggregate(&AVG_DEF, &bigint, &[Value::I64(5), Value::I64(2)]),
            Ok(Value::I64(3))
        );
        assert_eq!(
            (AVG_DEF.return_type)(std::slice::from_ref(&bigint))
                .expect("must type")
                .ty,
            SqlType::BigInt
        );
    }

    #[test]
    fn min_max_use_the_collation() {
        let ty = varchar(10);
        assert_eq!(ty.collation, Some(Collation::DEFAULT));
        let values = [text("abc"), text("ABD")];
        // Under the default collation 'abc' < 'ABD'; an ordinal comparison of the bytes
        // would answer 'ABD' for the minimum.
        assert_eq!(aggregate(&MIN_DEF, &ty, &values), Ok(text("abc")));
        assert_eq!(aggregate(&MAX_DEF, &ty, &values), Ok(text("ABD")));
        assert_eq!(aggregate(&MIN_DEF, &ty, &[]), Ok(Value::Null));

        let typed = (MIN_DEF.return_type)(std::slice::from_ref(&ty)).expect("must type");
        assert_eq!(typed.ty, SqlType::VarChar(Len::Fixed(10)));
        assert_eq!(typed.collation, Some(Collation::DEFAULT));
        assert!(typed.nullable);
        assert_eq!((MAX_DEF.return_type)(std::slice::from_ref(&ty)), Ok(typed));

        // The factory remembers the collation of the argument...
        assert_eq!(
            ExtremumState::new(&ty, Ordering::Less).collation,
            Collation::DEFAULT
        );
        // ...and falls back on the default one when the type carries none.
        let uncollated = TypeInfo {
            collation: None,
            ..ty
        };
        assert_eq!(
            ExtremumState::new(&uncollated, Ordering::Less).collation,
            Collation::DEFAULT
        );
        assert_eq!(aggregate(&MIN_DEF, &uncollated, &values), Ok(text("abc")));
    }

    #[test]
    fn empty_group_is_null_except_count() {
        for def in [&SUM_DEF, &AVG_DEF, &MIN_DEF, &MAX_DEF] {
            assert_eq!(
                aggregate(def, &int(true), &[]),
                Ok(Value::Null),
                "{} over an empty group",
                def.name
            );
            // A group of nothing but NULLs is an empty group that raises the flag.
            assert_eq!(
                aggregate_with_flag(def, &int(true), &[Value::Null]),
                Ok((Value::Null, true)),
                "{} over a group of NULLs",
                def.name
            );
        }
        assert_eq!(aggregate(&COUNT_DEF, &int(true), &[]), Ok(Value::I32(0)));
    }

    #[test]
    fn null_eliminated_flag() {
        assert_eq!(
            aggregate_with_flag(&SUM_DEF, &int(true), &[Value::I32(1), Value::Null]),
            Ok((Value::I32(1), true))
        );
        assert_eq!(
            aggregate_with_flag(&SUM_DEF, &int(true), &[Value::I32(1)]),
            Ok((Value::I32(1), false))
        );
        // The same for the other four that skip NULLs.
        assert_eq!(
            aggregate_with_flag(&AVG_DEF, &int(true), &[Value::I32(4), Value::Null]),
            Ok((Value::I32(4), true))
        );
        assert_eq!(
            aggregate_with_flag(&MAX_DEF, &int(true), &[Value::I32(4), Value::Null]),
            Ok((Value::I32(4), true))
        );
        assert_eq!(
            aggregate_with_flag(&COUNT_BIG_DEF, &int(true), &[Value::Null]),
            Ok((Value::I64(0), true))
        );
    }

    #[test]
    fn eval_of_an_aggregate_is_a_bug() {
        let ctx = crate::context::StaticContext::default();
        let result = TypeInfo::new(SqlType::Int, true);
        for def in [
            &COUNT_DEF,
            &COUNT_BIG_DEF,
            &SUM_DEF,
            &AVG_DEF,
            &MIN_DEF,
            &MAX_DEF,
        ] {
            let args = EvalArgs {
                values: &[Value::I32(1)],
                types: std::slice::from_ref(&result),
                result: &result,
            };
            let err = (def.eval)(&args, &ctx).expect_err("an aggregate has no scalar eval");
            assert!(
                err.message.contains("use FunctionDef::aggregate"),
                "{}: {}",
                def.name,
                err.message
            );
        }
    }
}

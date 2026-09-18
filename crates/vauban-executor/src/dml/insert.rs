use std::collections::HashSet;

use vauban_catalog::{Catalog, ColumnMeta, ObjectId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Literal, UnaryOp};
use vauban_planner::PhysicalInsert;
use vauban_storage::{RowId, Storage, TableId, TxnId};
use vauban_txn::TxnHandle;
use vauban_types::{
    BinaryOp, Decimal, LiteralKind, SqlType, TypeInfo, Value, eval_binary, parse_literal,
};

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::errors::at;
use crate::locking;
use crate::operator::build_operator;
use crate::row::{ExecOutcome, Row};

/// Runs one `INSERT`.
pub(crate) fn execute(stmt: &PhysicalInsert, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let storage: &dyn Storage = ctx.storage()?;
    let txn_id = ctx.handle()?.id;

    let (col_metas, table_name, table_obj_id, table_id) = {
        let catalog: &Catalog = ctx.catalog()?;
        let handle: &TxnHandle = ctx.handle()?;
        let snap = catalog.snapshot(handle);
        let meta = snap
            .table_by_storage(stmt.table)
            .ok_or_else(|| bug("INSERT: table not found in the catalogue"))?;
        let db_name: String = snap
            .database_by_id(meta.database)
            .map_or_else(|| "?".to_owned(), |db| db.name.clone());
        let col_metas: Vec<ColumnMeta> = meta.columns.clone();
        // The three-part name 515 prints, built once for the whole statement rather than
        // per row: both loops of `write_one` need it.
        let table_name = format!("{}.{}.{}", db_name, meta.schema, meta.name);
        (col_metas, table_name, meta.id, stmt.table)
    };

    let mut root = build_operator(&stmt.source)?;

    let rows: Vec<Row> = if stmt.spool {
        let mut materialised: Vec<Row> = Vec::new();
        root.open(ctx)?;
        while let Some(row) = root.next(ctx)? {
            if ctx.cancelled() {
                root.close();
                return Ok(ExecOutcome::Cancelled);
            }
            materialised.push(row);
        }
        root.close();
        materialised
    } else {
        Vec::new()
    };

    let catalog: &Catalog = ctx.catalog()?;
    let handle: &TxnHandle = ctx.handle()?;

    let mut count: u64 = 0;

    if stmt.spool {
        for row in &rows {
            let id = write_one(
                row,
                stmt,
                &col_metas,
                &table_name,
                txn_id,
                table_id,
                table_obj_id,
                storage,
                catalog,
                handle,
            )?;
            locking::write_lock(ctx, table_id, id)?;
            count += 1;
        }
    } else {
        root.open(ctx)?;
        while let Some(row) = root.next(ctx)? {
            if ctx.cancelled() {
                root.close();
                return Ok(ExecOutcome::Cancelled);
            }
            let id = write_one(
                &row,
                stmt,
                &col_metas,
                &table_name,
                txn_id,
                table_id,
                table_obj_id,
                storage,
                catalog,
                handle,
            )?;
            locking::write_lock(ctx, table_id, id)?;
            count += 1;
        }
        root.close();
    }

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = count as i64;
    }
    Ok(ExecOutcome::NoRows)
}

/// Builds the storage row of one source row and writes it, answering the identifier the
/// storage gave it.
///
/// The exclusive lock of that row is taken by the caller, on the identifier this function
/// answers: a lock names a row, and the row has no identifier before it is written. Nothing
/// reads the row in between — another transaction sees neither the version nor its
/// identifier until this one commits.
#[allow(clippy::too_many_arguments)]
fn write_one(
    row: &[Value],
    stmt: &PhysicalInsert,
    col_metas: &[ColumnMeta],
    table_name: &str,
    txn_id: TxnId,
    table_id: TableId,
    table_obj_id: ObjectId,
    storage: &dyn Storage,
    catalog: &Catalog,
    handle: &TxnHandle,
) -> SqlResult<RowId> {
    let mut output = vec![Value::Null; col_metas.len()];
    let mut covered: HashSet<usize> = HashSet::new();
    for (i, binding) in stmt.columns.iter().enumerate() {
        let ordinal = binding.index;
        let src = &row[i];
        let col_meta = &col_metas[ordinal];
        let converted = assign_value(src.clone(), &binding.ty, col_meta, table_name, "INSERT")
            .map_err(|e| at(e, 0))?;
        output[ordinal] = converted;
        covered.insert(ordinal);
    }
    for (ordinal, col_meta) in col_metas.iter().enumerate() {
        if covered.contains(&ordinal) {
            continue;
        }
        if col_meta.identity.is_some() {
            let dec = catalog.next_identity(handle, table_obj_id)?;
            output[ordinal] = identity_value(&dec, col_meta)?;
            continue;
        }
        if let Some(ref default) = col_meta.default {
            // The literal of the constraint follows the conversion an explicit value goes
            // through: `eval_default` gives its own type, `assign_value` writes the column's.
            let (value, from) = eval_default(default)?;
            let converted =
                assign_value(value, &from, col_meta, table_name, "INSERT").map_err(|e| at(e, 0))?;
            output[ordinal] = converted;
            continue;
        }
        if col_meta.ty.nullable {
            output[ordinal] = Value::Null;
            continue;
        }
        return Err(at(
            SqlError::cannot_insert_null(&col_meta.name, table_name, "INSERT"),
            0,
        ));
    }
    let stored = vauban_storage::Row(output);
    storage.insert(txn_id, table_id, &stored).map_err(|err| {
        let snap = catalog.snapshot(handle);
        let meta = snap
            .table_by_storage(table_id)
            .expect("INSERT: table not found in the catalogue");
        crate::dml::constraints::translate_unique(err, meta, &snap, &stored.0, "INSERT")
    })
}

/// Evaluates the literal of a `DEFAULT` constraint, with the type it is written with.
///
/// The type is what [`assign_value`] converts from, and it is the type the same literal
/// would carry in a `VALUES` row: `types::parse_literal` is the one place that decides it,
/// so `DEFAULT 5` on an `int` column arrives as an `I32` typed `int` and `DEFAULT 1.25` as
/// a `numeric(3,2)`, exactly as `VALUES (5)` and `VALUES (1.25)` do.
///
/// # What counts as a literal here
///
/// A leading `+` or `-` and a pair of parentheses belong to the literal: `DEFAULT -5`,
/// `DEFAULT (-5)` and `DEFAULT 0x01` are constraints a column carries, and each writes and
/// reads back at the type of the column (`tests/insert.rs::signed_and_binary_defaults`).
/// The sign is computed as `0 - literal` through `types::eval_binary`, which keeps the
/// range rules and the 8115 they raise in `types` rather than restating them here.
///
/// Anything else is refused with the internal error: `DEFAULT (1 + 1)` and
/// `DEFAULT GETDATE()` are expressions, not literals, and this function does not evaluate
/// expressions (`tests/insert.rs::an_arithmetic_default_is_not_a_literal`).
fn eval_default(expr: &Expr) -> SqlResult<(Value, TypeInfo)> {
    match expr {
        Expr::Literal(Literal::Null, _) => Ok((Value::Null, TypeInfo::new(SqlType::Int, true))),
        Expr::Literal(Literal::Default, _) => Err(bug(
            "INSERT: DEFAULT keyword in a DEFAULT constraint is a self-reference",
        )),
        // `DEFAULT (5)`: a pair of parentheses around the literal does not make it an
        // expression (`tests/insert.rs::signed_and_binary_defaults`).
        Expr::Nested(inner, _) => eval_default(inner),
        Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => eval_default(inner),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr: inner,
            ..
        } => {
            let (value, ty) = eval_default(inner)?;
            let zero = zero_of(&value)?;
            let negated = eval_binary(BinaryOp::Sub, &zero, &value, &ty)?;
            Ok((negated, ty))
        }
        Expr::Literal(literal, _) => {
            let (kind, text) = literal_payload(literal)?;
            parse_literal(kind, text)
        }
        _ => Err(bug("INSERT: default expression is not a literal")),
    }
}

/// The [`LiteralKind`] and the payload `types::parse_literal` reads for `literal`.
///
/// `NULL` and `DEFAULT` carry no payload and are answered by [`eval_default`] before this
/// function is reached.
fn literal_payload(literal: &Literal) -> SqlResult<(LiteralKind, &str)> {
    Ok(match literal {
        Literal::Integer(text) => (LiteralKind::Integer, text.as_str()),
        Literal::Decimal(text) => (LiteralKind::Decimal, text.as_str()),
        Literal::Float(text) => (LiteralKind::Float, text.as_str()),
        Literal::Money(text) => (LiteralKind::Money, text.as_str()),
        Literal::Binary(text) => (LiteralKind::Hex, text.as_str()),
        Literal::Str {
            value,
            unicode: false,
        } => (LiteralKind::Str, value.as_str()),
        Literal::Str {
            value,
            unicode: true,
        } => (LiteralKind::NStr, value.as_str()),
        Literal::Null | Literal::Default => {
            return Err(bug("INSERT: NULL and DEFAULT carry no literal payload"));
        }
    })
}

/// The zero a signed default subtracts its literal from, in the family of that literal.
///
/// `eval_binary` widens an integer to `i64` before it computes and rebuilds the result in
/// the type it is given, so the integral variants share one zero. A character or binary
/// literal takes no sign: `DEFAULT -0x01` and `DEFAULT -'a'` are refused here rather than
/// given a meaning the write path would have to invent.
fn zero_of(value: &Value) -> SqlResult<Value> {
    match value {
        Value::I8(_) | Value::I16(_) | Value::I32(_) | Value::I64(_) => Ok(Value::I64(0)),
        Value::Decimal(d) => Ok(Value::Decimal(Decimal {
            mantissa: 0,
            precision: d.precision,
            scale: d.scale,
        })),
        Value::Money(_) => Ok(Value::Money(0)),
        Value::F32(_) => Ok(Value::F32(0.0)),
        Value::F64(_) => Ok(Value::F64(0.0)),
        _ => Err(bug("INSERT: a signed default needs a numeric literal")),
    }
}

fn identity_value(dec: &Decimal, col_meta: &ColumnMeta) -> SqlResult<Value> {
    let int_val = dec.mantissa;
    match col_meta.ty.ty {
        SqlType::TinyInt => Ok(Value::I8(int_val as u8)),
        SqlType::SmallInt => Ok(Value::I16(int_val as i16)),
        SqlType::Int => Ok(Value::I32(int_val as i32)),
        SqlType::BigInt => Ok(Value::I64(int_val as i64)),
        _ => Err(bug("INSERT: IDENTITY on an unsupported type")),
    }
}

fn bug(what: &str) -> SqlError {
    SqlError::from(vauban_errors::InternalError::Bug(what.to_owned()))
}

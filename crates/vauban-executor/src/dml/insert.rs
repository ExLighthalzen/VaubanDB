use std::collections::HashSet;

use vauban_catalog::{Catalog, ColumnMeta, ObjectId};
use vauban_errors::{SqlError, SqlResult};
use vauban_parser::{Expr, Literal};
use vauban_planner::PhysicalInsert;
use vauban_storage::{Storage, TableId, TxnId};
use vauban_txn::TxnHandle;
use vauban_types::{Decimal, Len, SqlType, TypeInfo, Value};

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::errors::at;
use crate::operator::build_operator;
use crate::row::{ExecOutcome, Row};

/// Runs one `INSERT`.
pub(crate) fn execute(stmt: &PhysicalInsert, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let storage: &dyn Storage = ctx.storage()?;
    let txn_id = ctx.handle()?.id;

    let (col_metas, db_name, table_obj_id, table_id, tbl_schema, tbl_name) = {
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
        (
            col_metas,
            db_name,
            meta.id,
            stmt.table,
            meta.schema.clone(),
            meta.name.clone(),
        )
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
            write_one(
                row,
                stmt,
                &col_metas,
                &db_name,
                &tbl_schema,
                &tbl_name,
                txn_id,
                table_id,
                table_obj_id,
                storage,
                catalog,
                handle,
            )?;
            count += 1;
        }
    } else {
        root.open(ctx)?;
        while let Some(row) = root.next(ctx)? {
            if ctx.cancelled() {
                root.close();
                return Ok(ExecOutcome::Cancelled);
            }
            write_one(
                &row,
                stmt,
                &col_metas,
                &db_name,
                &tbl_schema,
                &tbl_name,
                txn_id,
                table_id,
                table_obj_id,
                storage,
                catalog,
                handle,
            )?;
            count += 1;
        }
        root.close();
    }

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = count as i64;
    }
    Ok(ExecOutcome::NoRows)
}

#[allow(clippy::too_many_arguments)]
fn write_one(
    row: &[Value],
    stmt: &PhysicalInsert,
    col_metas: &[ColumnMeta],
    db_name: &str,
    tbl_schema: &str,
    tbl_name: &str,
    txn_id: TxnId,
    table_id: TableId,
    table_obj_id: ObjectId,
    storage: &dyn Storage,
    catalog: &Catalog,
    handle: &TxnHandle,
) -> SqlResult<()> {
    let mut output = vec![Value::Null; col_metas.len()];
    let mut covered: HashSet<usize> = HashSet::new();
    for (i, binding) in stmt.columns.iter().enumerate() {
        let ordinal = binding.index;
        let src = &row[i];
        let col_meta = &col_metas[ordinal];
        let converted = assign_value(src.clone(), &binding.ty, col_meta).map_err(|e| at(e, 0))?;
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
            output[ordinal] = assign_value(value, &from, col_meta).map_err(|e| at(e, 0))?;
            continue;
        }
        if col_meta.ty.nullable {
            output[ordinal] = Value::Null;
            continue;
        }
        let table_name = format!("{}.{}.{}", db_name, tbl_schema, tbl_name);
        return Err(at(
            SqlError::cannot_insert_null(&col_meta.name, &table_name, "INSERT"),
            0,
        ));
    }
    storage.insert(txn_id, table_id, &vauban_storage::Row(output))?;
    Ok(())
}

/// Evaluates the literal of a `DEFAULT` constraint, with the type it is written with.
///
/// The type is what [`assign_value`] converts from, so that an `int` column receives an
/// `I32` and not the `I64` an integer literal is parsed into.
fn eval_default(expr: &Expr) -> SqlResult<(Value, TypeInfo)> {
    match expr {
        Expr::Literal(Literal::Null, _) => Ok((Value::Null, TypeInfo::new(SqlType::Int, true))),
        Expr::Literal(Literal::Default, _) => Err(bug(
            "INSERT: DEFAULT keyword in a DEFAULT constraint is a self-reference",
        )),
        Expr::Literal(Literal::Integer(text), _) => {
            let n: i64 = text
                .parse()
                .map_err(|_| bug("INSERT: default integer literal does not parse"))?;
            Ok((Value::I64(n), TypeInfo::new(SqlType::BigInt, false)))
        }
        Expr::Literal(Literal::Str { value, unicode }, _) => {
            let ty = if *unicode {
                SqlType::NVarChar(Len::Max)
            } else {
                SqlType::VarChar(Len::Max)
            };
            Ok((
                Value::String(vauban_types::SqlString {
                    text: value.clone(),
                }),
                TypeInfo::new(ty, false),
            ))
        }
        _ => Err(bug("INSERT: default expression is not a literal")),
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

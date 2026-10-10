use std::collections::HashSet;

use vauban_binder::{BoundObjectExpression, ColumnBinding, TargetExpressions};
use vauban_catalog::{Catalog, CatalogSnapshot, ColumnId, ColumnMeta, TableMeta};
use vauban_errors::{SqlError, SqlResult};
use vauban_planner::{PhysicalInsert, PhysicalSelectInto};
use vauban_storage::{RowId, Storage, TxnId};
use vauban_txn::TxnHandle;
use vauban_types::{Decimal, SqlType, TypeInfo, Value};

use crate::context::ExecContext;
use crate::dml::assign::assign_value;
use crate::dml::fk_check::WriteChecks;
use crate::errors::at;
use crate::expr::eval_expr;
use crate::locking;
use crate::operator::build_operator;
use crate::row::{ExecOutcome, Row};

/// Runs one `INSERT`.
///
/// The catalogue snapshot, the table, its constraints and the bound expressions of the plan
/// are read once, before the first row; each row is then built and checked from them
/// (`dml/fk_check.rs`).
pub(crate) fn execute(stmt: &PhysicalInsert, ctx: &mut ExecContext<'_>) -> SqlResult<ExecOutcome> {
    let storage: &dyn Storage = ctx.storage()?;
    let txn_id = ctx.handle()?.id;
    let catalog: &Catalog = ctx.catalog()?;
    let handle: &TxnHandle = ctx.handle()?;
    let cat_snap = catalog.snapshot(handle);
    let meta = cat_snap
        .table_by_storage(stmt.table)
        .ok_or_else(|| bug("INSERT: table not found in the catalogue"))?;
    let db_name: String = cat_snap
        .database_by_id(meta.database)
        .map_or_else(|| "?".to_owned(), |db| db.name.clone());
    let target = Target {
        // The three-part name 515 prints, built once for the whole statement.
        name: format!("{}.{}.{}", db_name, meta.schema, meta.name),
        defaults: by_ordinal(&stmt.target.defaults, meta.columns.len()),
        computed: by_ordinal(&stmt.target.computed, meta.columns.len()),
        checks: WriteChecks::resolve(meta, &cat_snap, &stmt.target.checks, "INSERT", false, ctx)?,
        meta,
        cat_snap: &cat_snap,
        txn_id,
        storage,
        catalog,
        handle,
    };

    let mut root = build_operator(&stmt.source)?;
    // The type each position of the source really holds: the column's type in general,
    // but for a value that reached the column already of a matching string or binary
    // family, the type of the value itself, still at its own length. `assign_value` reads
    // it from here rather than from `stmt.columns`, whose `.ty` names the column, not the
    // value.
    let source_types: Vec<TypeInfo> = root
        .schema()
        .columns
        .iter()
        .map(|column| column.ty.clone())
        .collect();

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

    let mut count: u64 = 0;
    let mut last_identity: Option<Decimal> = None;

    if stmt.spool {
        for row in &rows {
            let (id, identity) = write_one(row, stmt, &source_types, &target, ctx)?;
            locking::write_lock(ctx, stmt.table, id)?;
            if let Some(dec) = identity {
                last_identity = Some(dec);
            }
            count += 1;
        }
    } else {
        root.open(ctx)?;
        while let Some(row) = root.next(ctx)? {
            if ctx.cancelled() {
                root.close();
                return Ok(ExecOutcome::Cancelled);
            }
            let (id, identity) = write_one(&row, stmt, &source_types, &target, ctx)?;
            locking::write_lock(ctx, stmt.table, id)?;
            if let Some(dec) = identity {
                last_identity = Some(dec);
            }
            count += 1;
        }
        root.close();
    }

    if let Some(session) = ctx.session.as_deref_mut() {
        session.rowcount = count as i64;
        session.last_identity = last_identity;
        session.identity_updated = true;
    }
    Ok(ExecOutcome::NoRows)
}

/// What an `INSERT` reads once, before its first row, and reuses for each row it writes.
struct Target<'s> {
    meta: &'s TableMeta,
    cat_snap: &'s CatalogSnapshot,
    /// `database.schema.table`, for the messages that name the table.
    name: String,
    /// The bound `DEFAULT` of each column, by ordinal; `None` for a column the plan holds
    /// no default for.
    defaults: Vec<Option<&'s BoundObjectExpression>>,
    /// The bound expression of each computed column, by ordinal.
    computed: Vec<Option<&'s BoundObjectExpression>>,
    checks: WriteChecks,
    txn_id: TxnId,
    storage: &'s dyn Storage,
    catalog: &'s Catalog,
    handle: &'s TxnHandle,
}

/// The entries of `expressions` placed at the ordinal of the column each one fills.
pub(crate) fn by_ordinal(
    expressions: &[BoundObjectExpression],
    width: usize,
) -> Vec<Option<&BoundObjectExpression>> {
    let mut placed = vec![None; width];
    for expression in expressions {
        if let Some(column) = &expression.column
            && let Some(slot) = placed.get_mut(column.index)
        {
            *slot = Some(expression);
        }
    }
    placed
}

/// Builds the storage row of one source row and writes it, answering the identifier the
/// storage gave it.
///
/// The exclusive lock of that row is taken by the caller, on the identifier this function
/// answers: a lock names a row, and the row has no identifier before it is written. Nothing
/// reads the row in between — another transaction sees neither the version nor its
/// identifier until this one commits.
///
/// A column the statement leaves out takes, in this order, the next `IDENTITY` value, its
/// `DEFAULT` evaluated by [`eval_expr`] and converted as an explicit value is
/// (`tests/insert.rs`, `a_non_literal_default_is_evaluated`), `NULL`, or the 515. The
/// computed columns are evaluated over the row so built, then the constraints are checked.
fn write_one(
    row: &[Value],
    stmt: &PhysicalInsert,
    source_types: &[TypeInfo],
    target: &Target<'_>,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<(RowId, Option<Decimal>)> {
    let col_metas = &target.meta.columns;
    let table_name = target.name.as_str();
    let mut output = vec![Value::Null; col_metas.len()];
    let mut covered: HashSet<usize> = HashSet::new();
    let mut generated_identity: Option<Decimal> = None;
    let mut explicit_identity: Option<i64> = None;
    for (i, binding) in stmt.columns.iter().enumerate() {
        let ordinal = binding.index;
        let src = &row[i];
        let col_meta = &col_metas[ordinal];
        let converted = assign_value(
            src.clone(),
            &source_types[i],
            col_meta,
            table_name,
            "INSERT",
        )
        .map_err(|e| at(e, 0))?;
        if col_meta.identity.is_some() {
            explicit_identity = Some(identity_as_i64(&converted, col_meta)?);
        }
        output[ordinal] = converted;
        covered.insert(ordinal);
    }
    for (ordinal, col_meta) in col_metas.iter().enumerate() {
        if covered.contains(&ordinal) || target.computed[ordinal].is_some() {
            continue;
        }
        if col_meta.identity.is_some() {
            let dec = target
                .catalog
                .next_identity(target.handle, target.meta.id)?;
            generated_identity = Some(dec);
            output[ordinal] = identity_value(&dec, col_meta)?;
            continue;
        }
        if let Some(default) = target.defaults[ordinal] {
            let value = eval_expr(&default.expr, None, ctx)?;
            output[ordinal] = assign_value(value, &default.expr.ty, col_meta, table_name, "INSERT")
                .map_err(|e| at(e, 0))?;
            continue;
        }
        if col_meta.default_text().is_some() {
            return Err(bug(&format!(
                "INSERT: the DEFAULT of column '{}' of table '{table_name}' has no bound \
                 expression, so the row is not written",
                col_meta.name
            )));
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
    fill_computed(
        &mut output,
        &target.computed,
        col_metas,
        table_name,
        "INSERT",
        ctx,
    )?;
    target.checks.check_row(&output, ctx)?;
    if let Some(explicit) = explicit_identity {
        target
            .catalog
            .bump_identity_after_explicit(target.handle, target.meta.id, explicit)?;
    }
    let stored = vauban_storage::Row(output);
    let id = target
        .storage
        .insert(target.txn_id, stmt.table, &stored)
        .map_err(|err| {
            crate::dml::constraints::translate_unique(
                err,
                target.meta,
                target.cat_snap,
                &stored.0,
                "INSERT",
            )
        })?;
    Ok((id, generated_identity))
}

/// Writes into `row` the value of each computed column of `computed`, evaluated over `row`
/// and converted to the type of its column.
///
/// # Errors
///
/// What evaluating an expression raises, and what [`assign_value`] raises.
pub(crate) fn fill_computed(
    row: &mut [Value],
    computed: &[Option<&BoundObjectExpression>],
    columns: &[ColumnMeta],
    table_name: &str,
    statement: &str,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<()> {
    for (ordinal, expression) in computed.iter().enumerate() {
        let (Some(expression), Some(column)) = (expression, columns.get(ordinal)) else {
            continue;
        };
        let value = eval_expr(&expression.expr, Some(&row.to_vec()), ctx)?;
        row[ordinal] = assign_value(value, &expression.expr.ty, column, table_name, statement)
            .map_err(|e| at(e, 0))?;
    }
    Ok(())
}

fn identity_as_i64(value: &Value, _col_meta: &ColumnMeta) -> SqlResult<i64> {
    Ok(match value {
        Value::I8(v) => i64::from(*v),
        Value::I16(v) => i64::from(*v),
        Value::I32(v) => i64::from(*v),
        Value::I64(v) => *v,
        Value::Decimal(dec) => i64::try_from(dec.mantissa)
            .map_err(|_| bug("INSERT: explicit identity value does not fit in i64"))?,
        _ => {
            return Err(bug(
                "INSERT: explicit identity value has an unexpected type",
            ));
        }
    })
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

// -------------------------------------------------------------------------------------------
// SELECT … INTO
// -------------------------------------------------------------------------------------------

/// Creates the table of `stmt` and writes the rows of its source into it.
pub(crate) fn execute_select_into(
    stmt: &PhysicalSelectInto,
    ctx: &mut ExecContext<'_>,
) -> SqlResult<ExecOutcome> {
    let catalog: &Catalog = ctx.catalog()?;
    let handle: &TxnHandle = ctx.handle()?;
    let meta = catalog.create_table(handle, &stmt.def)?;
    let columns: Vec<ColumnBinding> = stmt
        .def
        .columns
        .iter()
        .enumerate()
        .map(|(index, col)| ColumnBinding {
            column: ColumnId(index as i32),
            index,
            name: col.name.clone(),
            ty: col.ty.clone(),
            depth: 0,
        })
        .collect();
    let insert = PhysicalInsert {
        table: meta.storage_id,
        columns,
        source: stmt.source.clone(),
        spool: stmt.spool,
        target: TargetExpressions::default(),
    };
    execute(&insert, ctx)
}

//! Prepared-statement handles owned by a session (`sp_prepare`, `sp_execute`, `sp_unprepare`,
//! `sp_prepexec`).

use std::collections::HashMap;

use vauban_binder::{BatchVariables, BindContext, BoundExprKind, resolve_declared_type};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{DataType, ParameterDeclaration, Statement, parse_batch};
use vauban_planner::{PhysicalPlan, PhysicalStatement, PlanContext, SnapshotIndexes};
use vauban_tds::RpcParam;
use vauban_txn::{IsolationLevel, TxnHandle};
use vauban_types::{
    Collation, Len, SqlString, SqlType, TypeFamily, TypeInfo, Value, convert, implicit_result_type,
};

use crate::batch::Session;
use crate::eval_context::SessionEvalContext;
use crate::procedures::NestedParam;
use crate::procedures::{BatchProcOutcome, ProcFinish};
use crate::rpc::{DescribeKind, ProcAction, ProcParam};
use crate::sink::ResultSink;
use crate::state::SessionState;
use crate::statements::{at_statement, statement_fragment, statement_line};

const DEFAULT_SCHEMA: &str = "dbo";

/// One prepared handle and its statement text plus parameter declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Dynamic SQL text passed to `sp_prepare` / `sp_prepexec`.
    pub text: String,
    /// Declarations from the `@params` argument.
    pub params: Vec<ParameterDeclaration>,
    /// Type of each entry of `params`, resolved once when the handle is prepared: `execute`
    /// and `sp_execute` reuse it instead of resolving the declaration again.
    pub param_types: Vec<TypeInfo>,
}

/// Per-session table of prepared handles. Handles start at 1 and are not reused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreparedStatements {
    next: i32,
    entries: HashMap<i32, Prepared>,
}

impl PreparedStatements {
    /// Empty table, first handle will be `1`.
    pub fn new() -> Self {
        Self {
            next: 1,
            entries: HashMap::new(),
        }
    }

    fn allocate(&mut self, prepared: Prepared) -> i32 {
        let handle = self.next;
        self.next = self.next.saturating_add(1).max(2);
        self.entries.insert(handle, prepared);
        handle
    }

    fn remove(&mut self, handle: i32) -> Option<Prepared> {
        self.entries.remove(&handle)
    }

    fn get(&self, handle: i32) -> Option<&Prepared> {
        self.entries.get(&handle)
    }
}

/// Runs `Prepare`, `Execute` or `Unprepare` for an RPC request.
pub(crate) fn execute_prepared_action(
    session: &mut Session,
    action: ProcAction,
    finish: ProcFinish<'_>,
    rpc_params: &[RpcParam],
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    let ProcFinish::Rpc { .. } = finish else {
        return Err(SqlError::from(InternalError::Bug(
            "prepared actions are RPC-only".to_owned(),
        )));
    };
    match action {
        ProcAction::Prepare {
            statement,
            params,
            handle_arg,
            execute_with,
        } => execute_prepare(
            session,
            &statement,
            &params,
            handle_arg,
            execute_with.as_deref(),
            rpc_params,
            sink,
        ),
        ProcAction::Execute { handle, params } => {
            execute_handle(session, handle, &params, rpc_params, sink)
        }
        ProcAction::Unprepare { handle } => execute_unprepare(session, handle, sink),
        other => Err(SqlError::from(InternalError::Bug(format!(
            "execute_prepared_action called with {other:?}"
        )))),
    }
}

fn execute_prepare(
    session: &mut Session,
    statement: &str,
    declarations: &[ParameterDeclaration],
    handle_arg: usize,
    execute_with: Option<&[ProcParam]>,
    rpc_params: &[RpcParam],
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    let nested_state = session.state().clone();

    // The `@params` declarations resolve before the text is even parsed, but an unknown type
    // there still carries the same `statement_could_not_be_prepared` (8180) as a real compile
    // error of the text (`unknown_param_type_is_2715_followed_by_8180` on that shape).
    let param_types = match resolve_declaration_types(declarations) {
        Ok(types) => types,
        Err(err) => return prepare_failed(session, sink, err),
    };

    if let Err(err) = validate_prepared_text(
        session,
        statement,
        &param_types,
        declarations,
        &nested_state,
    ) {
        return prepare_failed(session, sink, err);
    }

    let prepared = Prepared {
        text: statement.to_owned(),
        params: declarations.to_vec(),
        param_types,
    };
    let handle = session.state_mut().prepared.allocate(prepared.clone());
    let handle_ty = rpc_params
        .get(handle_arg)
        .map(|param| param.ty.clone())
        .unwrap_or_else(|| TypeInfo::new(SqlType::Int, false));
    let handle_value = Value::I32(handle);

    if let Some(values) = execute_with {
        let nested_params = proc_params_to_nested(&prepared, values)?;
        let (nested, error_number) = run_nested_tracked(session, statement, &nested_params, sink)?;
        if nested.failed {
            session.state_mut().prepared.remove(handle);
            session.state_mut().last_error = error_number;
            sink.return_status(nested.return_status)?;
            emit_handle_return_value(sink, rpc_params, handle_arg, &handle_ty, &handle_value)?;
            sink.done_proc(None)?;
            return Ok(failed_outcome(error_number));
        }
        session.state_mut().rowcount = nested.rowcount;
        session.state_mut().last_error = 0;
        sink.return_status(nested.return_status)?;
        emit_handle_return_value(sink, rpc_params, handle_arg, &handle_ty, &handle_value)?;
        sink.done_proc(None)?;
        return Ok(BatchProcOutcome {
            outputs: Vec::new(),
            rowcount: nested.rowcount,
            return_status: nested.return_status,
            failed: false,
            error_number: 0,
            had_result_set: nested.had_result_set,
        });
    }

    sink.columns(&[])?;
    session.state_mut().last_error = 0;
    emit_handle_return_value(sink, rpc_params, handle_arg, &handle_ty, &handle_value)?;
    sink.return_status(0)?;
    sink.done_proc(None)?;
    Ok(BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: false,
        error_number: 0,
        had_result_set: false,
    })
}

fn execute_handle(
    session: &mut Session,
    handle: i32,
    params: &[ProcParam],
    _rpc_params: &[RpcParam],
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    let Some(prepared) = session.state().prepared.get(handle).cloned() else {
        let err = SqlError::prepared_statement_not_found(i64::from(handle));
        session.state_mut().last_error = err.number;
        sink.error(&err)?;
        sink.done_proc(None)?;
        return Ok(failed_outcome(err.number));
    };
    let nested_params = proc_params_to_nested(&prepared, params)?;
    let (nested, error_number) = run_nested_tracked(session, &prepared.text, &nested_params, sink)?;
    if nested.failed {
        session.state_mut().last_error = error_number;
        sink.return_status(nested.return_status)?;
        sink.done_proc(None)?;
        return Ok(failed_outcome(error_number));
    }
    session.state_mut().rowcount = nested.rowcount;
    session.state_mut().last_error = 0;
    sink.return_status(nested.return_status)?;
    sink.done_proc(None)?;
    Ok(BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: nested.rowcount,
        return_status: nested.return_status,
        failed: false,
        error_number: 0,
        had_result_set: nested.had_result_set,
    })
}

fn execute_unprepare(
    session: &mut Session,
    handle: i32,
    sink: &mut dyn ResultSink,
) -> SqlResult<BatchProcOutcome> {
    if session.state_mut().prepared.remove(handle).is_none() {
        let err = SqlError::prepared_statement_not_found(i64::from(handle));
        session.state_mut().last_error = err.number;
        sink.error(&err)?;
        sink.done_proc(None)?;
        return Ok(failed_outcome(err.number));
    }
    session.state_mut().last_error = 0;
    sink.return_status(0)?;
    sink.done_proc(None)?;
    Ok(BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: false,
        error_number: 0,
        had_result_set: false,
    })
}

fn failed_outcome(error_number: u32) -> BatchProcOutcome {
    BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: true,
        error_number,
        had_result_set: false,
    }
}

/// Sends `err`, then the `statement_could_not_be_prepared` (8180) that follows it whether the
/// cause is an unknown `@params` type or a compile error of the text
/// (`unknown_param_type_is_2715_followed_by_8180`, `invalid_prepare_text_emits_8180`).
fn prepare_failed(
    session: &mut Session,
    sink: &mut dyn ResultSink,
    err: SqlError,
) -> SqlResult<BatchProcOutcome> {
    sink.error(&err)?;
    // The 1205 of a binding chosen as a deadlock victim rolls the session transaction back
    // after its ERROR token (`tests/deadlock_victim.rs`,
    // `a_victim_at_binding_under_a_prepexec_is_rolled_back`).
    if vauban_errors::error_behavior(err.number).rolls_back_transaction {
        crate::procedures::roll_back_after_refusal(session, sink)?;
    }
    sink.error(&SqlError::statement_could_not_be_prepared())?;
    session.state_mut().last_error = err.number;
    sink.done_proc(None)?;
    Ok(failed_outcome(err.number))
}

fn emit_handle_return_value(
    sink: &mut dyn ResultSink,
    rpc_params: &[RpcParam],
    handle_arg: usize,
    ty: &TypeInfo,
    value: &Value,
) -> SqlResult<()> {
    let name = rpc_params
        .get(handle_arg)
        .map(|param| param.name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("handle");
    sink.return_value(name, ty, value)
}

fn proc_params_to_nested(prepared: &Prepared, params: &[ProcParam]) -> SqlResult<Vec<NestedParam>> {
    if prepared.params.is_empty() {
        if params.is_empty() {
            return Ok(Vec::new());
        }
        return Err(SqlError::no_parameter_but_arguments(""));
    }
    if params.len() < prepared.params.len() {
        let missing = &prepared.params[params.len()];
        let query = parameterized_query(&prepared.params, &prepared.text);
        return Err(SqlError::parameter_not_supplied(&query, &missing.name));
    }
    if params.len() > prepared.params.len() {
        return Err(SqlError::too_many_arguments("sp_execute"));
    }
    prepared
        .params
        .iter()
        .zip(prepared.param_types.iter())
        .zip(params.iter())
        .map(|((decl, ty), arg)| {
            Ok(NestedParam {
                name: decl.name.clone(),
                ty: ty.clone(),
                value: bound_argument(&arg.value, &arg.ty, ty)?,
                output: decl.output || arg.output,
            })
        })
        .collect()
}

fn parameterized_query(declarations: &[ParameterDeclaration], statement: &str) -> String {
    let params_text = declarations
        .iter()
        .map(|decl| decl.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if params_text.is_empty() {
        statement.to_owned()
    } else {
        format!("({params_text}){statement}")
    }
}

fn validate_prepared_text(
    session: &Session,
    text: &str,
    param_types: &[TypeInfo],
    declarations: &[ParameterDeclaration],
    state: &SessionState,
) -> SqlResult<()> {
    let batch = parse_batch(text, &state.options.parse_options())?;
    if batch.statements.is_empty() {
        return Ok(());
    }
    let mut batch_variables = BatchVariables::new();
    for (decl, ty) in declarations.iter().zip(param_types.iter()) {
        batch_variables.declare(&decl.name, ty.clone())?;
    }
    let binding = session.engine().txn.begin(IsolationLevel::ReadCommitted);
    let result = bind_prepared_statements(
        session,
        text,
        &batch.statements,
        &binding,
        state,
        &mut batch_variables,
    );
    session.engine().txn.commit(binding)?;
    result.map(|_| ())
}

/// Binds the statements of `text` in order, against the catalogue `binding` reads, and
/// answers the columns of the first one that produces a result set, `None` for a batch
/// without a result set.
///
/// Each statement waits, as the statement of a batch does, for the `Sch-M` another
/// transaction holds on a user table it names, and is bound again once the wait is over
/// (`Session::wait_for_schema`): a `sp_prepare` of a statement whose table another session
/// is altering waits for that session, and answers 1222 state 56 on the line of the
/// statement under `SET LOCK_TIMEOUT` (`tests/ddl_visibility.rs`,
/// `a_prepared_or_dynamic_statement_waits_for_an_uncommitted_alter`).
fn bind_prepared_statements(
    session: &Session,
    text: &str,
    statements: &[Statement],
    binding: &TxnHandle,
    state: &SessionState,
    batch_variables: &mut BatchVariables,
) -> SqlResult<Option<Vec<DescribedColumn>>> {
    let mut first: Option<Vec<DescribedColumn>> = None;
    let mut snapshot = crate::batch::binding_snapshot(session.engine(), state, binding);
    for (index, original) in statements.iter().enumerate() {
        if matches!(original, Statement::SetOption(_)) {
            continue;
        }
        let line = statement_line(original);
        let source = statement_fragment(text, original, statements.get(index + 1));
        let padded = format!("{}{}", "\n".repeat(line.saturating_sub(1) as usize), source);
        loop {
            let variables_before = batch_variables.clone();
            let _ = snapshot.take_resolved_tables(false);
            let physical = bind_prepared_one(session, &padded, &snapshot, state, batch_variables);
            let tables = snapshot.take_resolved_tables(false);
            match session.wait_for_schema(binding, &tables) {
                Ok(false) => {
                    if first.is_none()
                        && let PhysicalStatement::Query(plan) = &physical?
                    {
                        first = Some(described_columns(plan));
                    }
                    break;
                }
                Ok(true) => {
                    *batch_variables = variables_before;
                    snapshot = crate::batch::binding_snapshot(session.engine(), state, binding);
                }
                Err(err) => return Err(at_statement(err, line)),
            }
        }
    }
    Ok(first)
}

/// Reparses, binds, plans and compile-checks the one statement of `padded` against
/// `snapshot`, for [`bind_prepared_statements`].
fn bind_prepared_one(
    session: &Session,
    padded: &str,
    snapshot: &CatalogSnapshot,
    state: &SessionState,
    batch_variables: &mut BatchVariables,
) -> SqlResult<PhysicalStatement> {
    let reparsed = parse_batch(padded, &state.options.parse_options())?;
    let [statement] = reparsed.statements.as_slice() else {
        return Err(SqlError::from(InternalError::Bug(
            "one statement reparsed as a different statement count".to_owned(),
        )));
    };
    let options = state.binder_options();
    let ctx = BindContext {
        text: padded,
        catalog: Some(snapshot),
        database: &state.database,
        default_schema: DEFAULT_SCHEMA,
        variables: batch_variables,
        options,
    };
    let bound = vauban_binder::bind(statement, &ctx)?;
    batch_variables.declare_held_by(&bound)?;
    let indexes = SnapshotIndexes {
        storage: session.engine().storage.as_ref(),
        snapshot,
    };
    let physical = vauban_planner::plan(bound, &PlanContext { catalog: &indexes })?;
    let eval = SessionEvalContext::new(state, Some(snapshot));
    let mut exec = vauban_executor::ExecContext::scalar(&eval, state.options.to_binder());
    vauban_executor::compile(&physical, &mut exec)?;
    Ok(physical)
}

/// One column of the result set a described batch would produce.
struct DescribedColumn {
    /// Name sent to the client, empty for an expression the batch does not name.
    name: String,
    /// Type, nullability and collation sent to the client.
    ty: TypeInfo,
    /// Where the value of the column comes from.
    origin: ColumnOrigin,
}

/// Where the value of a described column comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnOrigin {
    /// A reference to a column the query reads from storage.
    StoredColumn,
    /// A value the query computes from the row it reads, such as a literal or an arithmetic
    /// expression.
    ComputedValue,
    /// A value an operator builds from several rows or from several inputs, such as an
    /// aggregate or a branch of a set operation.
    OperatorOutput,
}

/// Name of the collation the character columns report.
const DEFAULT_COLLATION_NAME: &str = "SQL_Latin1_General_CP1_CI_AS";

/// Describes the batch `text` without running its statements, and answers one row per
/// described item, in the order of the columns the caller was given.
///
/// A batch that binds and produces no result set answers no row, and so does an empty
/// batch (`tests/describe.rs`, `describing_an_empty_batch_has_no_row`). A batch the binder
/// refuses fails with the binding error.
pub(crate) fn describe_batch(
    session: &Session,
    text: &str,
    kind: DescribeKind,
) -> SqlResult<Vec<Vec<Value>>> {
    let state = session.state().clone();
    let batch = parse_batch(text, &state.options.parse_options())?;
    if batch.statements.is_empty() {
        return Ok(Vec::new());
    }
    let mut batch_variables = BatchVariables::new();
    let binding = session.engine().txn.begin(IsolationLevel::ReadCommitted);
    let described = bind_prepared_statements(
        session,
        text,
        &batch.statements,
        &binding,
        &state,
        &mut batch_variables,
    );
    session.engine().txn.commit(binding)?;
    let described = described?;
    match kind {
        DescribeKind::FirstResultSet => Ok(described
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, column)| first_result_set_row(index, column))
            .collect()),
        // A parameter the batch does not declare stops the binding above, so a batch that
        // reaches this point declares what it uses.
        DescribeKind::UndeclaredParameters => Ok(Vec::new()),
    }
}

/// The columns a plan produces, each with the origin of its value.
fn described_columns(plan: &PhysicalPlan) -> Vec<DescribedColumn> {
    plan.schema()
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| DescribedColumn {
            name: column.name.clone(),
            ty: column.ty.clone(),
            origin: column_origin(plan, index),
        })
        .collect()
}

/// Where the column at `index` of `plan` takes its value.
///
/// A projection answers on the expression it evaluates and follows a column reference into
/// its input; a scan answers on the column it reads; a grouping follows its key and answers
/// on itself for an aggregate; a join follows the operand whose columns hold the index; the
/// operators that keep the shape of their input answer on that input. The shapes pinned by
/// `tests/describe.rs` are a column read from a table, a literal, an arithmetic expression,
/// an aggregate with and without a grouping key, a branch of a `UNION ALL`, and a column
/// read through a join on either operand (`a_column_read_through_a_join_is_updateable`).
fn column_origin(plan: &PhysicalPlan, index: usize) -> ColumnOrigin {
    match plan {
        PhysicalPlan::Project { input, exprs, .. } => {
            match exprs.get(index).map(|projection| &projection.expr.kind) {
                Some(BoundExprKind::ColumnRef(binding)) => column_origin(input, binding.index),
                Some(_) => ColumnOrigin::ComputedValue,
                None => ColumnOrigin::OperatorOutput,
            }
        }
        PhysicalPlan::TableScan { .. } | PhysicalPlan::IndexSeek { .. } => {
            ColumnOrigin::StoredColumn
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Top { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::TopN { input, .. } => column_origin(input, index),
        PhysicalPlan::Distinct(input) => column_origin(input, index),
        // The columns of a grouping are its keys, then its aggregates.
        PhysicalPlan::HashAggregate {
            input, group_by, ..
        }
        | PhysicalPlan::StreamAggregate {
            input, group_by, ..
        } => match group_by.get(index).map(|key| &key.kind) {
            Some(BoundExprKind::ColumnRef(binding)) => column_origin(input, binding.index),
            Some(_) => ColumnOrigin::ComputedValue,
            None => ColumnOrigin::OperatorOutput,
        },
        PhysicalPlan::NestedLoopJoin { outer, inner, .. } => {
            join_column_origin(outer, inner, index)
        }
        // The hash table may be built from either operand: the operand whose column names
        // open the join's schema is the left one. Two operands with the same names are the
        // same shape read twice, and the build side stands for the left one.
        PhysicalPlan::HashJoin {
            build,
            probe,
            schema,
            ..
        } => {
            let build_columns = &build.schema().columns;
            let build_is_left = schema.columns.len() >= build_columns.len()
                && schema.columns[..build_columns.len()]
                    .iter()
                    .zip(build_columns)
                    .all(|(joined, built)| joined.name == built.name);
            if build_is_left {
                join_column_origin(build, probe, index)
            } else {
                join_column_origin(probe, build, index)
            }
        }
        _ => ColumnOrigin::OperatorOutput,
    }
}

/// Where the column at `index` of a join takes its value: the columns of `left` come first,
/// then those of `right`.
fn join_column_origin(left: &PhysicalPlan, right: &PhysicalPlan, index: usize) -> ColumnOrigin {
    let left_width = left.schema().columns.len();
    if index < left_width {
        column_origin(left, index)
    } else {
        column_origin(right, index - left_width)
    }
}

/// The row that describes one column of a first result set.
fn first_result_set_row(index: usize, column: &DescribedColumn) -> Vec<Value> {
    let shape = TypeShape::of(&column.ty);
    let ordinal = i32::try_from(index + 1).unwrap_or(i32::MAX);
    vec![
        Value::Bit(false),
        Value::I32(ordinal),
        text_or_null(&column.name),
        Value::Bit(column.ty.nullable),
        Value::I32(shape.system_type_id),
        Value::String(SqlString {
            text: column.ty.ty.declaration(),
        }),
        Value::I16(shape.max_length),
        Value::I8(shape.precision),
        Value::I8(shape.scale),
        shape.collation_name,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Bit(false),
        Value::Bit(false),
        Value::Bit(false),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Bit(false),
        Value::Null,
        Value::Bit(column.origin == ColumnOrigin::StoredColumn),
        Value::Bit(column.origin == ColumnOrigin::ComputedValue),
        Value::Bit(false),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::I32(shape.tds_type_id),
        Value::I32(shape.tds_length),
        shape.tds_collation_id,
        shape.tds_collation_sort_id,
    ]
}

fn text_or_null(name: &str) -> Value {
    if name.is_empty() {
        Value::Null
    } else {
        Value::String(SqlString {
            text: name.to_owned(),
        })
    }
}

/// What a described column reports about its type.
struct TypeShape {
    /// Identifier of the type, as the catalogue numbers it.
    system_type_id: i32,
    /// Storage width in bytes, `-1` for a `max` type.
    max_length: i16,
    /// Decimal precision, `0` for the types without one.
    precision: u8,
    /// Decimal scale, `0` for the types without one.
    scale: u8,
    /// Collation name, `NULL` outside the character types.
    collation_name: Value,
    /// Identifier of the TDS token that carries the type on the wire.
    tds_type_id: i32,
    /// Width the TDS token declares.
    tds_length: i32,
    /// Collation as the wire packs its locale and flags, `NULL` outside character types.
    tds_collation_id: Value,
    /// Sort identifier of the collation, `NULL` outside the character types.
    tds_collation_sort_id: Value,
}

impl TypeShape {
    fn of(ty: &TypeInfo) -> Self {
        let (system_type_id, nullable_tds_id) = type_ids(&ty.ty);
        let (max_length, precision, scale) = size_of(&ty.ty);
        let string = ty.ty.is_string();
        let collation = ty.collation.unwrap_or(Collation::DEFAULT);
        TypeShape {
            system_type_id,
            max_length,
            precision,
            scale,
            collation_name: if string {
                Value::String(SqlString {
                    text: DEFAULT_COLLATION_NAME.to_owned(),
                })
            } else {
                Value::Null
            },
            tds_type_id: if ty.nullable {
                nullable_tds_id
            } else {
                non_nullable_tds_id(&ty.ty, system_type_id)
            },
            tds_length: tds_length(&ty.ty, max_length),
            tds_collation_id: if string {
                Value::I32(packed_collation(&collation))
            } else {
                Value::Null
            },
            tds_collation_sort_id: if string {
                Value::I8(collation.sort_id)
            } else {
                Value::Null
            },
        }
    }
}

/// The locale, the comparison flags and the version of a collation packed into one number,
/// the first four of the five bytes the wire carries ([MS-TDS] 2.2.5.1.2).
fn packed_collation(collation: &Collation) -> i32 {
    let packed = (collation.lcid & 0x000F_FFFF)
        | (u32::from(collation.flags) << 20)
        | (u32::from(collation.version & 0x0F) << 28);
    packed.cast_signed()
}

/// The catalogue identifier of a type, and the identifier of the TDS token that carries it
/// when it may be `NULL`.
fn type_ids(ty: &SqlType) -> (i32, i32) {
    match ty {
        SqlType::Bit => (104, 104),
        SqlType::TinyInt => (48, 38),
        SqlType::SmallInt => (52, 38),
        SqlType::Int => (56, 38),
        SqlType::BigInt => (127, 38),
        SqlType::Decimal { .. } => (106, 106),
        SqlType::Numeric { .. } => (108, 108),
        SqlType::Float => (62, 109),
        SqlType::Real => (59, 109),
        SqlType::Money => (60, 110),
        SqlType::SmallMoney => (122, 110),
        SqlType::Char(_) => (175, 175),
        SqlType::VarChar(_) => (167, 167),
        SqlType::NChar(_) => (239, 239),
        SqlType::NVarChar(_) => (231, 231),
        SqlType::Binary(_) => (173, 173),
        SqlType::VarBinary(_) => (165, 165),
        SqlType::Date => (40, 40),
        SqlType::Time(_) => (41, 41),
        SqlType::DateTime => (61, 111),
        SqlType::SmallDateTime => (58, 111),
        SqlType::DateTime2(_) => (42, 42),
        SqlType::DateTimeOffset(_) => (43, 43),
        SqlType::UniqueIdentifier => (36, 36),
    }
}

/// The identifier of the TDS token that carries a type that cannot be `NULL`.
///
/// The token of a value that cannot be `NULL` carries the catalogue identifier of its
/// type, `bit` excepted, which has a token of its own. The families whose nullable token
/// differs from that one are pinned by `tests/describe.rs`,
/// `the_wire_token_of_a_column_follows_its_nullability`: `bit`, the integers, `float`,
/// `money` and `datetime`; the character, binary and `date` families keep a single token.
fn non_nullable_tds_id(ty: &SqlType, system_type_id: i32) -> i32 {
    match ty {
        SqlType::Bit => 50,
        _ => system_type_id,
    }
}

/// Storage width in bytes, decimal precision and decimal scale of a type.
fn size_of(ty: &SqlType) -> (i16, u8, u8) {
    match ty {
        SqlType::Bit => (1, 1, 0),
        SqlType::TinyInt => (1, 3, 0),
        SqlType::SmallInt => (2, 5, 0),
        SqlType::Int => (4, 10, 0),
        SqlType::BigInt => (8, 19, 0),
        SqlType::Decimal { precision, scale } | SqlType::Numeric { precision, scale } => {
            (decimal_bytes(*precision), *precision, *scale)
        }
        SqlType::Float => (8, 53, 0),
        SqlType::Real => (4, 24, 0),
        SqlType::Money => (8, 19, 4),
        SqlType::SmallMoney => (4, 10, 4),
        SqlType::Char(len)
        | SqlType::VarChar(len)
        | SqlType::Binary(len)
        | SqlType::VarBinary(len) => (byte_len(len, 1), 0, 0),
        SqlType::NChar(len) | SqlType::NVarChar(len) => (byte_len(len, 2), 0, 0),
        SqlType::Date => (3, 10, 0),
        SqlType::Time(scale) => (
            temporal_bytes(*scale),
            8 + u8::from(*scale > 0) + *scale,
            *scale,
        ),
        SqlType::DateTime => (8, 23, 3),
        SqlType::SmallDateTime => (4, 16, 0),
        SqlType::DateTime2(scale) => (
            temporal_bytes(*scale) + 3,
            19 + u8::from(*scale > 0) + *scale,
            *scale,
        ),
        SqlType::DateTimeOffset(scale) => (
            temporal_bytes(*scale) + 5,
            26 + u8::from(*scale > 0) + *scale,
            *scale,
        ),
        SqlType::UniqueIdentifier => (16, 0, 0),
    }
}

/// Storage width of an exact numeric, which grows in four steps with its precision.
fn decimal_bytes(precision: u8) -> i16 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

/// Storage width of the time part of a temporal type, which grows with its scale.
fn temporal_bytes(scale: u8) -> i16 {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// Storage width of a sized type, `-1` for `max`.
fn byte_len(len: &Len, per_character: i16) -> i16 {
    match len {
        Len::Fixed(n) => i16::try_from(*n)
            .unwrap_or(i16::MAX)
            .saturating_mul(per_character),
        Len::Max => -1,
    }
}

/// The width the TDS token declares, which spells `max` as the largest short width.
fn tds_length(ty: &SqlType, max_length: i16) -> i32 {
    if ty.is_exact_numeric() {
        return 17;
    }
    if max_length < 0 {
        return 65_535;
    }
    i32::from(max_length)
}

/// Resolves each declaration of `@params`, in order, or refuses the first one it cannot name
/// instead of falling back to a silent `int` (message 2715, naming the type; see
/// `unknown_param_type_is_2715_followed_by_8180`).
fn resolve_declaration_types(declarations: &[ParameterDeclaration]) -> SqlResult<Vec<TypeInfo>> {
    declarations
        .iter()
        .enumerate()
        .map(|(index, decl)| resolve_declared_type(&decl.ty, index as u32 + 1))
        .collect()
}

impl ProcParam {
    /// The type a declared procedure parameter carries, nullable, or the 2715 or 2716 that
    /// refuses its declaration; `position` is the 1-based rank in the declaration list.
    ///
    /// The resolution itself is the binder's ([`vauban_binder::resolve_declared_type`]); this
    /// entry point serves the crates that build [`ProcParam`]s without depending on the
    /// binder.
    pub fn declared_type(ty: &DataType, position: u32) -> SqlResult<TypeInfo> {
        resolve_declared_type(ty, position)
    }

    /// `value`, an argument of type `from`, converted to the `declared` type of the
    /// parameter it is bound to, the way the parameter receives it in the called text.
    ///
    /// # Errors
    ///
    /// The refusals [`bound_argument`] lists.
    pub fn bound_value(value: &Value, from: &TypeInfo, declared: &TypeInfo) -> SqlResult<Value> {
        bound_argument(value, from, declared)
    }
}

/// `value`, of type `from`, converted to the `declared` type of the parameter it is bound
/// to (`tests::arguments_take_the_declared_type`).
///
/// # Errors
///
/// - 257, state 3, from `datetime` or `smalldatetime` to a number: the pair has an explicit
///   conversion only (`tests::a_datetime_argument_to_a_number_is_257`).
/// - 206, state 2, for a pair with no implicit conversion, the argument's type named first.
/// - 8114, state 5, when the value does not convert.
///
/// A `NULL` converts to `NULL` without its pair being checked.
pub(crate) fn bound_argument(
    value: &Value,
    from: &TypeInfo,
    declared: &TypeInfo,
) -> SqlResult<Value> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    if from.ty == declared.ty {
        return Ok(value.clone());
    }
    let datetime_source = matches!(from.ty, SqlType::DateTime | SqlType::SmallDateTime);
    if datetime_source && is_number(&declared.ty) {
        return Err(SqlError::implicit_conversion_not_allowed(
            from.ty.name(),
            declared.ty.name(),
        ));
    }
    if implicit_result_type(from, declared).is_err() {
        return Err(SqlError::operand_type_clash(
            from.ty.name(),
            declared.ty.name(),
        ));
    }
    converted_or_8114(value, from, declared, ARGUMENT_FAILURE_STATE)
}

/// `value`, the final value of an `OUTPUT` parameter declared `from`, converted to the
/// type `to` of what receives it (`tests::an_output_takes_the_type_of_its_receiver`).
///
/// # Errors
///
/// 8114, state 2, when the value does not convert to `to`.
pub(crate) fn returned_output(value: &Value, from: &TypeInfo, to: &TypeInfo) -> SqlResult<Value> {
    if matches!(value, Value::Null) || from.ty == to.ty {
        return Ok(value.clone());
    }
    converted_or_8114(value, from, to, OUTPUT_FAILURE_STATE)
}

/// State of the 8114 an `OUTPUT` value that does not fit its receiver raises.
const OUTPUT_FAILURE_STATE: u8 = 2;

/// State of the 8114 an argument that does not convert to its parameter raises.
const ARGUMENT_FAILURE_STATE: u8 = 5;

/// `value` converted from `from` to `to`, or 8114 at `state`, naming the two types.
fn converted_or_8114(value: &Value, from: &TypeInfo, to: &TypeInfo, state: u8) -> SqlResult<Value> {
    convert(value, from, to, None).map_err(|_| {
        let mut err = SqlError::error_converting_data_type(from.ty.name(), to.ty.name());
        err.state = state;
        err
    })
}

/// Whether `ty` is a number: `bit`, an integer, an exact or approximate numeric, money.
fn is_number(ty: &SqlType) -> bool {
    matches!(
        ty.family(),
        TypeFamily::Bit
            | TypeFamily::Integer
            | TypeFamily::ExactNumeric
            | TypeFamily::ApproxNumeric
            | TypeFamily::Money
    )
}

fn run_nested_tracked(
    session: &mut Session,
    text: &str,
    params: &[NestedParam],
    sink: &mut dyn ResultSink,
) -> SqlResult<(crate::procedures::NestedOutcome, u32)> {
    let mut body_sink = PrepareExecSink::new(sink);
    let nested = session.run_nested(text, params, &mut body_sink)?;
    Ok((nested, body_sink.last_error()))
}

/// Swallows the nested procedure trailer for prepare/execute RPCs.
struct PrepareExecSink<'a> {
    inner: &'a mut dyn ResultSink,
    last_error: u32,
}

impl<'a> PrepareExecSink<'a> {
    fn new(inner: &'a mut dyn ResultSink) -> Self {
        Self {
            inner,
            last_error: 0,
        }
    }

    fn last_error(&self) -> u32 {
        self.last_error
    }
}

impl ResultSink for PrepareExecSink<'_> {
    fn columns(&mut self, cols: &[vauban_tds::ColumnMeta]) -> SqlResult<()> {
        self.inner.columns(cols)
    }

    fn row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.inner.row(row)
    }

    fn done(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.inner.done(rowcount, more)
    }

    fn done_in_proc(&mut self, rowcount: Option<u64>, more: bool) -> SqlResult<()> {
        self.inner.done_in_proc(rowcount, more)
    }

    fn done_proc(&mut self, _rowcount: Option<u64>) -> SqlResult<()> {
        Ok(())
    }

    fn info(&mut self, msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
        self.inner.info(msg)
    }

    fn error(&mut self, err: &SqlError) -> SqlResult<()> {
        self.last_error = err.number;
        self.inner.error(err)
    }

    fn env_change(&mut self, change: &vauban_tds::EnvChange) -> SqlResult<()> {
        self.inner.env_change(change)
    }

    fn return_value(&mut self, name: &str, ty: &TypeInfo, value: &Value) -> SqlResult<()> {
        self.inner.return_value(name, ty, value)
    }

    fn return_status(&mut self, _status: i32) -> SqlResult<()> {
        Ok(())
    }
    fn next_cur_cmd(&mut self, cur_cmd: u16) -> SqlResult<()> {
        self.inner.next_cur_cmd(cur_cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Prepared, PreparedStatements, bound_argument, parameterized_query,
        resolve_declaration_types, returned_output,
    };
    use vauban_parser::{ParseOptions, parse_parameter_declarations};
    use vauban_types::SqlType;
    use vauban_types::{Len, SqlString, TypeInfo, Value};

    #[test]
    fn handles_start_at_one_and_are_not_reused() {
        let mut table = PreparedStatements::new();
        let first = table.allocate(Prepared {
            text: "SELECT 1".into(),
            params: Vec::new(),
            param_types: Vec::new(),
        });
        let second = table.allocate(Prepared {
            text: "SELECT 2".into(),
            params: Vec::new(),
            param_types: Vec::new(),
        });
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert!(table.remove(first).is_some());
        let third = table.allocate(Prepared {
            text: "SELECT 3".into(),
            params: Vec::new(),
            param_types: Vec::new(),
        });
        assert_eq!(third, 3);
    }

    #[test]
    fn parameterized_query_lists_declaration_names() {
        let decls =
            parse_parameter_declarations("@a int, @b int", &vauban_parser::ParseOptions::default())
                .expect("declarations parse");
        assert_eq!(
            parameterized_query(&decls, "SELECT @a + @b"),
            "(@a, @b)SELECT @a + @b"
        );
    }

    #[test]
    fn declared_types_keep_scale_precision_and_numeric_name() {
        let decls = parse_parameter_declarations(
            "@a numeric(5,2), @b datetime2(3), @c float(10)",
            &ParseOptions::default(),
        )
        .expect("declarations parse");
        let types = resolve_declaration_types(&decls).expect("types resolve");
        let types: Vec<SqlType> = types.into_iter().map(|t| t.ty).collect();
        assert_eq!(
            types,
            [
                SqlType::Numeric {
                    precision: 5,
                    scale: 2
                },
                SqlType::DateTime2(3),
                SqlType::Real,
            ]
        );
    }

    #[test]
    fn length_on_int_is_2716_at_its_rank() {
        let decls = parse_parameter_declarations("@a int, @b int(10)", &ParseOptions::default())
            .expect("declarations parse");
        let err = resolve_declaration_types(&decls).unwrap_err();
        assert_eq!((err.number, err.severity, err.state), (2716, 16, 1));
        assert!(err.message.contains("#2"), "{}", err.message);
    }

    fn ty(sql_type: SqlType) -> TypeInfo {
        TypeInfo::new(sql_type, true)
    }

    fn varchar(text: &str) -> Value {
        Value::String(SqlString {
            text: text.to_owned(),
        })
    }

    #[test]
    fn arguments_take_the_declared_type() {
        let numeric = ty(SqlType::Numeric {
            precision: 5,
            scale: 2,
        });
        let value = bound_argument(&Value::I32(2), &ty(SqlType::Int), &numeric).expect("converts");
        assert!(matches!(value, Value::Decimal(_)), "{value:?}");
        let value = bound_argument(&Value::F64(0.5), &ty(SqlType::Float), &ty(SqlType::Real))
            .expect("converts");
        assert_eq!(value, Value::F32(0.5));
        let value = bound_argument(
            &varchar("12"),
            &ty(SqlType::VarChar(Len::Fixed(2))),
            &ty(SqlType::Int),
        )
        .expect("converts");
        assert_eq!(value, Value::I32(12));
        let value = bound_argument(&Value::Null, &ty(SqlType::Int), &ty(SqlType::Date))
            .expect("NULL converts");
        assert_eq!(value, Value::Null);
    }

    #[test]
    fn an_argument_that_does_not_convert_is_8114_state_5() {
        let err = bound_argument(
            &varchar("x"),
            &ty(SqlType::VarChar(Len::Fixed(1))),
            &ty(SqlType::Int),
        )
        .unwrap_err();
        assert_eq!((err.number, err.severity, err.state), (8114, 16, 5));
        assert!(err.message.contains("varchar"), "{}", err.message);
        let err =
            bound_argument(&Value::I32(300), &ty(SqlType::Int), &ty(SqlType::TinyInt)).unwrap_err();
        assert_eq!((err.number, err.state), (8114, 5));
    }

    #[test]
    fn a_datetime_argument_to_a_number_is_257() {
        let err =
            bound_argument(&Value::I32(0), &ty(SqlType::Date), &ty(SqlType::Int)).unwrap_err();
        assert_eq!((err.number, err.state), (206, 2));
        let datetime = vauban_types::DateTime {
            days: 0,
            ticks_300th: 0,
        };
        for target in [SqlType::Int, SqlType::Float] {
            let err = bound_argument(
                &Value::DateTime(datetime),
                &ty(SqlType::DateTime),
                &ty(target),
            )
            .unwrap_err();
            assert_eq!((err.number, err.state), (257, 3), "{target:?}");
        }
    }

    #[test]
    fn an_output_takes_the_type_of_its_receiver() {
        let value = returned_output(&Value::F32(0.5), &ty(SqlType::Real), &ty(SqlType::Float))
            .expect("converts");
        assert_eq!(value, Value::F64(0.5));
        let err = returned_output(&Value::I32(300), &ty(SqlType::Int), &ty(SqlType::TinyInt))
            .unwrap_err();
        assert_eq!((err.number, err.state), (8114, 2));
    }
}

//! Prepared-statement handles owned by a session (`sp_prepare`, `sp_execute`, `sp_unprepare`,
//! `sp_prepexec`).

use std::collections::HashMap;

use vauban_binder::{BatchVariables, BindContext, BoundExprKind, BoundStatement};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{DataType, ParameterDeclaration, Statement, TypeArg, parse_batch};
use vauban_planner::{PhysicalPlan, PhysicalStatement, PlanContext, StorageIndexes};
use vauban_tds::RpcParam;
use vauban_txn::IsolationLevel;
use vauban_types::{Collation, Len, SqlString, SqlType, TypeInfo, Value};

use crate::batch::Session;
use crate::eval_context::SessionEvalContext;
use crate::procedures::NestedParam;
use crate::procedures::{BatchProcOutcome, ProcFinish};
use crate::rpc::{DescribeKind, ProcAction, ProcParam};
use crate::sink::ResultSink;
use crate::state::SessionState;
use crate::statements::{statement_fragment, statement_line};

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
                value: arg.value.clone(),
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
    let snapshot = session.engine().catalog.snapshot(&binding);
    let result = bind_prepared_statements(
        session,
        text,
        &batch.statements,
        &snapshot,
        state,
        &mut batch_variables,
    );
    session.engine().txn.commit(binding)?;
    result.map(|_| ())
}

/// Binds the statements of `text` in order and answers the columns of the first one that
/// produces a result set, `None` for a batch without a result set.
fn bind_prepared_statements(
    session: &Session,
    text: &str,
    statements: &[Statement],
    snapshot: &CatalogSnapshot,
    state: &SessionState,
    batch_variables: &mut BatchVariables,
) -> SqlResult<Option<Vec<DescribedColumn>>> {
    let mut first: Option<Vec<DescribedColumn>> = None;
    for (index, original) in statements.iter().enumerate() {
        if matches!(original, Statement::SetOption(_)) {
            continue;
        }
        let line = statement_line(original);
        let source = statement_fragment(text, original, statements.get(index + 1));
        let padded = format!("{}{}", "\n".repeat(line.saturating_sub(1) as usize), source);
        let reparsed = parse_batch(&padded, &state.options.parse_options())?;
        let [statement] = reparsed.statements.as_slice() else {
            return Err(SqlError::from(InternalError::Bug(
                "one statement reparsed as a different statement count".to_owned(),
            )));
        };
        let options = state.binder_options();
        let ctx = BindContext {
            text: &padded,
            catalog: Some(snapshot),
            database: &state.database,
            default_schema: DEFAULT_SCHEMA,
            variables: batch_variables,
            options,
        };
        let bound = vauban_binder::bind(statement, &ctx)?;
        if let BoundStatement::Declare(declarations) = &bound {
            for declaration in declarations {
                batch_variables.declare(&declaration.name, declaration.ty.clone())?;
            }
        }
        let indexes = StorageIndexes(session.engine().storage.as_ref());
        let physical = vauban_planner::plan(bound, &PlanContext { catalog: &indexes })?;
        let eval = SessionEvalContext::new(state, Some(snapshot));
        let mut exec = vauban_executor::ExecContext::scalar(&eval, state.options.to_binder());
        vauban_executor::compile(&physical, &mut exec)?;
        if first.is_none()
            && let PhysicalStatement::Query(plan) = &physical
        {
            first = Some(described_columns(plan));
        }
    }
    Ok(first)
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
    let snapshot = session.engine().catalog.snapshot(&binding);
    let described = bind_prepared_statements(
        session,
        text,
        &batch.statements,
        &snapshot,
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
        .map(|(index, decl)| declaration_type(&decl.ty, index as u32 + 1))
        .collect()
}

fn declaration_type(decl: &DataType, position: u32) -> SqlResult<TypeInfo> {
    resolve_decl_type(decl, position)
}

fn resolve_decl_type(decl: &DataType, position: u32) -> SqlResult<TypeInfo> {
    let name = decl.name.to_ascii_lowercase();
    let sql_type = match name.as_str() {
        "int" | "integer" => SqlType::Int,
        "bigint" => SqlType::BigInt,
        "smallint" => SqlType::SmallInt,
        "tinyint" => SqlType::TinyInt,
        "bit" => SqlType::Bit,
        "nvarchar" | "national char varying" | "national character varying" => {
            sized_sql_type(decl, SqlType::NVarChar, 4_000, true, position)?
        }
        "varchar" | "char varying" | "character varying" => {
            sized_sql_type(decl, SqlType::VarChar, 8_000, true, position)?
        }
        "nchar" | "national char" | "national character" => {
            sized_sql_type(decl, SqlType::NChar, 4_000, false, position)?
        }
        "char" | "character" => sized_sql_type(decl, SqlType::Char, 8_000, false, position)?,
        "varbinary" | "binary varying" => {
            sized_sql_type(decl, SqlType::VarBinary, 8_000, true, position)?
        }
        "binary" => sized_sql_type(decl, SqlType::Binary, 8_000, false, position)?,
        "decimal" | "dec" | "numeric" => exact_numeric(decl, position)?,
        "float" | "double precision" => SqlType::Float,
        "real" => SqlType::Real,
        "money" => SqlType::Money,
        "smallmoney" => SqlType::SmallMoney,
        "date" => SqlType::Date,
        "datetime" => SqlType::DateTime,
        "smalldatetime" => SqlType::SmallDateTime,
        "datetime2" => SqlType::DateTime2(7),
        "datetimeoffset" => SqlType::DateTimeOffset(7),
        "time" => SqlType::Time(7),
        "uniqueidentifier" => SqlType::UniqueIdentifier,
        // Plain `sysname` is `nvarchar(128)`; a length is refused in a declaration.
        "sysname" => {
            if !decl.args.is_empty() {
                return Err(SqlError::cannot_find_data_type(position, &decl.name));
            }
            SqlType::NVarChar(Len::Fixed(128))
        }
        _ => return Err(SqlError::cannot_find_data_type(position, &decl.name)),
    };
    Ok(TypeInfo::new(sql_type, true))
}

fn sized_sql_type(
    decl: &DataType,
    make: fn(Len) -> SqlType,
    max: i64,
    allow_max: bool,
    position: u32,
) -> SqlResult<SqlType> {
    let len = match decl.args.as_slice() {
        [] => Len::Fixed(1),
        [TypeArg::Number(n)] if (1..=max).contains(n) => Len::Fixed(
            u16::try_from(*n).map_err(|_| SqlError::cannot_find_data_type(position, &decl.name))?,
        ),
        [TypeArg::Max] if allow_max => Len::Max,
        _ => return Err(SqlError::cannot_find_data_type(position, &decl.name)),
    };
    Ok(make(len))
}

fn exact_numeric(decl: &DataType, position: u32) -> SqlResult<SqlType> {
    let (precision, scale) = match decl.args.as_slice() {
        [] => (18, 0),
        [TypeArg::Number(p)] => (*p, 0),
        [TypeArg::Number(p), TypeArg::Number(s)] => (*p, *s),
        _ => return Err(SqlError::cannot_find_data_type(position, &decl.name)),
    };
    if !(1..=38).contains(&precision) || !(0..=precision).contains(&scale) {
        return Err(SqlError::cannot_find_data_type(position, &decl.name));
    }
    Ok(SqlType::Decimal {
        precision: u8::try_from(precision)
            .map_err(|_| SqlError::cannot_find_data_type(position, &decl.name))?,
        scale: u8::try_from(scale)
            .map_err(|_| SqlError::cannot_find_data_type(position, &decl.name))?,
    })
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
}

#[cfg(test)]
mod tests {
    use super::{Prepared, PreparedStatements, parameterized_query};
    use vauban_parser::parse_parameter_declarations;

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
}

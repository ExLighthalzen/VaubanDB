//! Prepared-statement handles owned by a session (`sp_prepare`, `sp_execute`, `sp_unprepare`,
//! `sp_prepexec`).

use std::collections::HashMap;

use vauban_binder::{BatchVariables, BindContext, BoundStatement};
use vauban_catalog::CatalogSnapshot;
use vauban_errors::{InternalError, SqlError, SqlResult};
use vauban_parser::{DataType, ParameterDeclaration, Statement, TypeArg, parse_batch};
use vauban_planner::{PlanContext, StorageIndexes};
use vauban_tds::RpcParam;
use vauban_txn::IsolationLevel;
use vauban_types::{Len, SqlType, TypeInfo, Value};

use crate::batch::Session;
use crate::eval_context::SessionEvalContext;
use crate::nested::NestedParam;
use crate::procedures::{BatchProcOutcome, ProcFinish};
use crate::rpc::{ProcAction, ProcParam};
use crate::sink::ResultSink;
use crate::state::SessionState;

const DEFAULT_SCHEMA: &str = "dbo";

/// One prepared handle and its statement text plus parameter declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Dynamic SQL text passed to `sp_prepare` / `sp_prepexec`.
    pub text: String,
    /// Declarations from the `@params` argument.
    pub params: Vec<ParameterDeclaration>,
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
    if let Err(err) = validate_prepared_text(session, statement, declarations, &nested_state) {
        sink.error(&err)?;
        sink.error(&SqlError::statement_could_not_be_prepared())?;
        session.state_mut().last_error = err.number;
        sink.done_proc(None)?;
        return Ok(failed_outcome(err.number));
    }

    let prepared = Prepared {
        text: statement.to_owned(),
        params: declarations.to_vec(),
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
            continues: false,
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
        continues: false,
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
        continues: false,
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
        continues: false,
    })
}

fn failed_outcome(error_number: u32) -> BatchProcOutcome {
    BatchProcOutcome {
        outputs: Vec::new(),
        rowcount: 0,
        return_status: 0,
        failed: true,
        error_number,
        continues: false,
    }
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
        .zip(params.iter())
        .map(|(decl, arg)| {
            Ok(NestedParam {
                name: decl.name.clone(),
                ty: declaration_type(&decl.ty),
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
    declarations: &[ParameterDeclaration],
    state: &SessionState,
) -> SqlResult<()> {
    let batch = parse_batch(text, &state.options.parse_options())?;
    if batch.statements.is_empty() {
        return Ok(());
    }
    let mut batch_variables = BatchVariables::new();
    for decl in declarations {
        batch_variables.declare(&decl.name, declaration_type(&decl.ty))?;
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
    result
}

fn bind_prepared_statements(
    session: &Session,
    text: &str,
    statements: &[Statement],
    snapshot: &CatalogSnapshot,
    state: &SessionState,
    batch_variables: &mut BatchVariables,
) -> SqlResult<()> {
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
        let _ = physical;
    }
    Ok(())
}

fn declaration_type(decl: &DataType) -> TypeInfo {
    resolve_decl_type(decl).unwrap_or_else(|_| TypeInfo::new(SqlType::Int, true))
}

fn resolve_decl_type(decl: &DataType) -> SqlResult<TypeInfo> {
    let name = decl.name.to_ascii_lowercase();
    let sql_type = match name.as_str() {
        "int" | "integer" => SqlType::Int,
        "bigint" => SqlType::BigInt,
        "smallint" => SqlType::SmallInt,
        "tinyint" => SqlType::TinyInt,
        "bit" => SqlType::Bit,
        "nvarchar" | "national char varying" | "national character varying" => {
            sized_sql_type(decl, SqlType::NVarChar, 4_000, true)?
        }
        "varchar" | "char varying" | "character varying" => {
            sized_sql_type(decl, SqlType::VarChar, 8_000, true)?
        }
        "nchar" | "national char" | "national character" => {
            sized_sql_type(decl, SqlType::NChar, 4_000, false)?
        }
        "char" | "character" => sized_sql_type(decl, SqlType::Char, 8_000, false)?,
        "varbinary" | "binary varying" => sized_sql_type(decl, SqlType::VarBinary, 8_000, true)?,
        "binary" => sized_sql_type(decl, SqlType::Binary, 8_000, false)?,
        "decimal" | "dec" | "numeric" => exact_numeric(decl)?,
        "float" | "double precision" => SqlType::Float,
        "real" => SqlType::Real,
        "money" => SqlType::Money,
        "smallmoney" => SqlType::SmallMoney,
        "date" => SqlType::Date,
        "datetime" => SqlType::DateTime,
        "smalldatetime" => SqlType::SmallDateTime,
        "datetime2" => SqlType::DateTime2(7),
        "time" => SqlType::Time(7),
        "uniqueidentifier" => SqlType::UniqueIdentifier,
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    Ok(TypeInfo::new(sql_type, true))
}

fn sized_sql_type(
    decl: &DataType,
    make: fn(Len) -> SqlType,
    max: i64,
    allow_max: bool,
) -> SqlResult<SqlType> {
    let len = match decl.args.as_slice() {
        [] => Len::Fixed(1),
        [TypeArg::Number(n)] if (1..=max).contains(n) => Len::Fixed(
            u16::try_from(*n).map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
        ),
        [TypeArg::Max] if allow_max => Len::Max,
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    Ok(make(len))
}

fn exact_numeric(decl: &DataType) -> SqlResult<SqlType> {
    let (precision, scale) = match decl.args.as_slice() {
        [] => (18, 0),
        [TypeArg::Number(p)] => (*p, 0),
        [TypeArg::Number(p), TypeArg::Number(s)] => (*p, *s),
        _ => return Err(SqlError::cannot_find_data_type(1, &decl.name)),
    };
    if !(1..=38).contains(&precision) || !(0..=precision).contains(&scale) {
        return Err(SqlError::cannot_find_data_type(1, &decl.name));
    }
    Ok(SqlType::Decimal {
        precision: u8::try_from(precision)
            .map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
        scale: u8::try_from(scale).map_err(|_| SqlError::cannot_find_data_type(1, &decl.name))?,
    })
}

fn statement_line(stmt: &Statement) -> u32 {
    statement_span(stmt).line
}

fn statement_span(stmt: &Statement) -> vauban_parser::Span {
    match stmt {
        Statement::Select(statement) => statement.span,
        Statement::Insert(statement) => statement.span,
        Statement::Update(statement) => statement.span,
        Statement::Delete(statement) => statement.span,
        Statement::Merge(statement) => statement.span,
        Statement::CreateDatabase(statement) => statement.span,
        Statement::AlterDatabase(statement) => statement.span,
        Statement::CreateTable(statement) => statement.span,
        Statement::AlterTable(statement) => statement.span,
        Statement::CreateIndex(statement) => statement.span,
        Statement::DropIndex(statement) => statement.span,
        Statement::CreateProcedure(statement) => statement.span,
        Statement::CreateFunction(statement) => statement.span,
        Statement::CreateView(statement) => statement.span,
        Statement::CreateTrigger(statement) => statement.span,
        Statement::CreateSequence(statement) => statement.span,
        Statement::Declare(statement) => statement.span,
        Statement::Set(statement) => statement.span,
        Statement::SetOption(statement) => statement.span,
        Statement::Execute(statement) => statement.span,
        Statement::Waitfor(statement) => statement.span,
        Statement::RaiseError(statement) => statement.span,
        Statement::Cursor(statement) => statement.span,
        Statement::Grant(statement) => statement.span,
        Statement::Break(span) | Statement::Continue(span) => *span,
        Statement::Truncate { span, .. }
        | Statement::DropDatabase { span, .. }
        | Statement::Use { span, .. }
        | Statement::DropTable { span, .. }
        | Statement::DropProcedure { span, .. }
        | Statement::DropFunction { span, .. }
        | Statement::DropView { span, .. }
        | Statement::DropTrigger { span, .. }
        | Statement::DropSequence { span, .. }
        | Statement::If { span, .. }
        | Statement::While { span, .. }
        | Statement::Block { span, .. }
        | Statement::Return { span, .. }
        | Statement::Print { span, .. }
        | Statement::BeginTransaction { span, .. }
        | Statement::Commit { span, .. }
        | Statement::Rollback { span, .. }
        | Statement::Save { span, .. }
        | Statement::TryCatch { span, .. }
        | Statement::Throw { span, .. }
        | Statement::Goto { span, .. }
        | Statement::Label { span, .. } => *span,
    }
}

fn statement_fragment<'a>(batch: &'a str, stmt: &Statement, next: Option<&Statement>) -> &'a str {
    let span = statement_span(stmt);
    let start = span.offset as usize;
    let end = next
        .map(statement_span)
        .map_or(batch.len(), |next_span| next_span.offset as usize);
    batch.get(start..end).unwrap_or(batch)
}

fn run_nested_tracked(
    session: &mut Session,
    text: &str,
    params: &[NestedParam],
    sink: &mut dyn ResultSink,
) -> SqlResult<(crate::nested::NestedOutcome, u32)> {
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
        });
        let second = table.allocate(Prepared {
            text: "SELECT 2".into(),
            params: Vec::new(),
        });
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert!(table.remove(first).is_some());
        let third = table.allocate(Prepared {
            text: "SELECT 3".into(),
            params: Vec::new(),
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

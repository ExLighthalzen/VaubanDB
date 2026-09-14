//! `Display` for statements: the batch, DML, variables, transactions, flow of control.

use std::fmt;

use crate::ast::proc::{
    CreateFunctionStatement, CreateProcedureStatement, CreateSequenceStatement,
    CreateTriggerStatement, CreateViewStatement, FunctionBody, FunctionReturns, MergeClause,
    MergeStatement, ProcParam, SequenceCache, TriggerEvent, TriggerTiming,
};
use crate::ast::stmt::{
    AssignOp, AssignTarget, Assignment, Batch, CursorAction, CursorStatement, DeclareItem,
    DeclareStatement, DeleteStatement, ExecuteArg, ExecuteStatement, ExecuteTarget, GrantAction,
    GrantStatement, InsertSource, InsertStatement, OutputClause, RaiseErrorStatement,
    SetOptionStatement, SetOptionValue, SetStatement, SetValue, Statement, UpdateStatement,
    WaitforKind, WaitforStatement,
};
use crate::display::{
    comma_separated, parenthesised_list, separated, write_block, write_string_literal,
};

/// Writes the statements separated by `";\n"`, with **no** trailing `;`.
///
/// # Contract
///
/// For any batch `s` accepted by `parse_batch`,
/// `parse_batch(&parse_batch(s)?.to_string())?` yields a `Batch` **equal** to the first
/// one. The text is not the source text: see the deviations listed on the
/// `display` module.
impl fmt::Display for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        separated(f, &self.statements, ";\n")
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Select(select) => write!(f, "{select}"),
            Self::Insert(insert) => write!(f, "{insert}"),
            Self::Update(update) => write!(f, "{update}"),
            Self::Delete(delete) => write!(f, "{delete}"),
            Self::Merge(merge) => write!(f, "{merge}"),
            Self::Truncate { table, .. } => write!(f, "TRUNCATE TABLE {table}"),
            Self::CreateDatabase(create) => write!(f, "{create}"),
            Self::AlterDatabase(alter) => write!(f, "{alter}"),
            Self::DropDatabase {
                names, if_exists, ..
            } => write_drop(f, "DATABASE", names, *if_exists),
            Self::Use { database, .. } => write!(f, "USE {database}"),
            Self::CreateTable(create) => write!(f, "{create}"),
            Self::AlterTable(alter) => write!(f, "{alter}"),
            Self::DropTable {
                names, if_exists, ..
            } => write_drop(f, "TABLE", names, *if_exists),
            Self::CreateIndex(create) => write!(f, "{create}"),
            Self::DropIndex(drop) => write!(f, "{drop}"),
            Self::CreateProcedure(create) => write!(f, "{create}"),
            Self::CreateFunction(create) => write!(f, "{create}"),
            Self::CreateView(create) => write!(f, "{create}"),
            Self::CreateTrigger(create) => write!(f, "{create}"),
            Self::CreateSequence(create) => write!(f, "{create}"),
            Self::DropProcedure {
                names, if_exists, ..
            } => write_drop(f, "PROCEDURE", names, *if_exists),
            Self::DropFunction {
                names, if_exists, ..
            } => write_drop(f, "FUNCTION", names, *if_exists),
            Self::DropView {
                names, if_exists, ..
            } => write_drop(f, "VIEW", names, *if_exists),
            Self::DropTrigger {
                names, if_exists, ..
            } => write_drop(f, "TRIGGER", names, *if_exists),
            Self::DropSequence {
                names, if_exists, ..
            } => write_drop(f, "SEQUENCE", names, *if_exists),
            Self::Declare(declare) => write!(f, "{declare}"),
            Self::Set(set) => write!(f, "{set}"),
            Self::SetOption(set) => write!(f, "{set}"),
            Self::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                write!(f, "IF {condition} {then_branch}")?;
                if let Some(else_branch) = else_branch {
                    write!(f, " ELSE {else_branch}")?;
                }
                Ok(())
            }
            Self::While {
                condition, body, ..
            } => write!(f, "WHILE {condition} {body}"),
            Self::Block { statements, .. } => write_block(f, "BEGIN", statements, "END"),
            Self::Break(_) => f.write_str("BREAK"),
            Self::Continue(_) => f.write_str("CONTINUE"),
            Self::Return { value, .. } => {
                f.write_str("RETURN")?;
                if let Some(value) = value {
                    write!(f, " {value}")?;
                }
                Ok(())
            }
            Self::Print { expr, .. } => write!(f, "PRINT {expr}"),
            Self::Execute(execute) => write!(f, "{execute}"),
            Self::BeginTransaction { name, mark, .. } => {
                f.write_str("BEGIN TRANSACTION")?;
                if let Some(name) = name {
                    write!(f, " {name}")?;
                }
                if let Some(mark) = mark {
                    f.write_str(" WITH MARK ")?;
                    write_string_literal(f, mark, false)?;
                }
                Ok(())
            }
            Self::Commit { name, .. } => write_transaction_end(f, "COMMIT", name.as_ref()),
            Self::Rollback { name, .. } => write_transaction_end(f, "ROLLBACK", name.as_ref()),
            Self::Save { name, .. } => write!(f, "SAVE TRANSACTION {name}"),
            Self::Waitfor(waitfor) => write!(f, "{waitfor}"),
            Self::TryCatch {
                try_block,
                catch_block,
                ..
            } => {
                write_block(f, "BEGIN TRY", try_block, "END TRY")?;
                f.write_str(" ")?;
                write_block(f, "BEGIN CATCH", catch_block, "END CATCH")
            }
            Self::Throw {
                number,
                message,
                state,
                ..
            } => {
                f.write_str("THROW")?;
                if let (Some(number), Some(message), Some(state)) = (number, message, state) {
                    write!(f, " {number}, {message}, {state}")?;
                }
                Ok(())
            }
            Self::RaiseError(raise) => write!(f, "{raise}"),
            Self::Goto { label, .. } => write!(f, "GOTO {label}"),
            Self::Label { name, .. } => write!(f, "{name}:"),
            Self::Cursor(cursor) => write!(f, "{cursor}"),
            Self::Grant(grant) => write!(f, "{grant}"),
        }
    }
}

/// Writes `DROP <object> [IF EXISTS] a, b`, the shape shared by every `DROP` of a list of
/// names.
fn write_drop<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    object: &str,
    names: &[T],
    if_exists: bool,
) -> fmt::Result {
    write!(f, "DROP {object} ")?;
    if if_exists {
        f.write_str("IF EXISTS ")?;
    }
    comma_separated(f, names)
}

/// Writes `COMMIT TRANSACTION [name]` or `ROLLBACK TRANSACTION [name]`: the AST records
/// neither the `TRAN` abbreviation nor the `WORK` synonym.
fn write_transaction_end(
    f: &mut fmt::Formatter<'_>,
    keyword: &str,
    name: Option<&crate::ast::expr::Ident>,
) -> fmt::Result {
    write!(f, "{keyword} TRANSACTION")?;
    if let Some(name) = name {
        write!(f, " {name}")?;
    }
    Ok(())
}

/// Writes `INSERT INTO t …`: the optional `INTO` is always written.
impl fmt::Display for InsertStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("INSERT ")?;
        if let Some(top) = &self.top {
            write!(f, "{top} ")?;
        }
        write!(f, "INTO {}", self.target)?;
        if !self.columns.is_empty() {
            f.write_str(" ")?;
            parenthesised_list(f, &self.columns)?;
        }
        if let Some(output) = &self.output {
            write!(f, " {output}")?;
        }
        write!(f, " {}", self.source)
    }
}

impl fmt::Display for InsertSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Values(rows) => {
                f.write_str("VALUES ")?;
                for (index, row) in rows.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    parenthesised_list(f, row)?;
                }
                Ok(())
            }
            Self::Query(query) => write!(f, "{query}"),
            Self::DefaultValues => f.write_str("DEFAULT VALUES"),
            Self::Execute(execute) => write!(f, "{execute}"),
        }
    }
}

impl fmt::Display for UpdateStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UPDATE ")?;
        if let Some(top) = &self.top {
            write!(f, "{top} ")?;
        }
        write!(f, "{} SET ", self.target)?;
        comma_separated(f, &self.assignments)?;
        if let Some(output) = &self.output {
            write!(f, " {output}")?;
        }
        if !self.from.is_empty() {
            f.write_str(" FROM ")?;
            comma_separated(f, &self.from)?;
        }
        if let Some(where_) = &self.where_ {
            write!(f, " WHERE {where_}")?;
        }
        Ok(())
    }
}

impl fmt::Display for DeleteStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DELETE ")?;
        if let Some(top) = &self.top {
            write!(f, "{top} ")?;
        }
        write!(f, "FROM {}", self.target)?;
        if let Some(output) = &self.output {
            write!(f, " {output}")?;
        }
        if !self.from.is_empty() {
            f.write_str(" FROM ")?;
            comma_separated(f, &self.from)?;
        }
        if let Some(where_) = &self.where_ {
            write!(f, " WHERE {where_}")?;
        }
        Ok(())
    }
}

impl fmt::Display for AssignTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Column(column) => write!(f, "{column}"),
            Self::Variable(name) => f.write_str(name),
            Self::VariableAndColumn { variable, column } => write!(f, "{variable} = {column}"),
        }
    }
}

impl fmt::Display for AssignOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Set => "=",
            Self::AddAssign => "+=",
            Self::SubAssign => "-=",
            Self::MulAssign => "*=",
            Self::DivAssign => "/=",
            Self::ModAssign => "%=",
            Self::BitAndAssign => "&=",
            Self::BitOrAssign => "|=",
            Self::BitXorAssign => "^=",
        })
    }
}

impl fmt::Display for Assignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.target, self.op, self.value)
    }
}

impl fmt::Display for OutputClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OUTPUT ")?;
        comma_separated(f, &self.items)?;
        if let Some(into) = &self.into {
            write!(f, " INTO {into}")?;
            if !self.into_columns.is_empty() {
                f.write_str(" ")?;
                parenthesised_list(f, &self.into_columns)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for DeclareStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DECLARE ")?;
        comma_separated(f, &self.items)
    }
}

impl fmt::Display for DeclareItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Variable { name, ty, default } => {
                write!(f, "{name} {ty}")?;
                if let Some(default) = default {
                    write!(f, " = {default}")?;
                }
                Ok(())
            }
            Self::TableVariable { name, definition } => write!(f, "{name} TABLE {definition}"),
            Self::Cursor { name } => write!(f, "{name} CURSOR"),
        }
    }
}

impl fmt::Display for SetValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expr(expr) => write!(f, "{expr}"),
            Self::Query(query) => write!(f, "({query})"),
        }
    }
}

impl fmt::Display for SetStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SET {} {} {}", self.target, self.op, self.value)
    }
}

impl fmt::Display for SetOptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::On => f.write_str("ON"),
            Self::Off => f.write_str("OFF"),
            Self::Value(expr) => write!(f, "{expr}"),
            Self::Word(word) => f.write_str(word),
        }
    }
}

/// Writes `SET a, b ON`: the option names of one statement share a single value, so the
/// first one alone is written.
impl fmt::Display for SetOptionStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SET")?;
        for (index, (name, _)) in self.options.iter().enumerate() {
            f.write_str(if index > 0 { ", " } else { " " })?;
            f.write_str(name)?;
        }
        if let Some((_, value)) = self.options.first() {
            write!(f, " {value}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ExecuteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Procedure(name) => write!(f, "{name}"),
            Self::Variable(name) => f.write_str(name),
            Self::Literal(expr) => write!(f, "({expr})"),
        }
    }
}

impl fmt::Display for ExecuteArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.name {
            write!(f, "{name} = ")?;
        }
        write!(f, "{}", self.value)?;
        if self.output {
            f.write_str(" OUTPUT")?;
        }
        Ok(())
    }
}

/// Writes `EXECUTE` in full, never `EXEC`; an implicit call (`p 1, 2`) writes nothing at
/// all before the name.
impl fmt::Display for ExecuteStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.implicit {
            f.write_str("EXECUTE ")?;
        }
        if let Some(return_into) = &self.return_into {
            write!(f, "{return_into} = ")?;
        }
        write!(f, "{}", self.target)?;
        if !self.args.is_empty() {
            f.write_str(" ")?;
            comma_separated(f, &self.args)?;
        }
        Ok(())
    }
}

impl fmt::Display for WaitforKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Delay => "DELAY",
            Self::Time => "TIME",
        })
    }
}

impl fmt::Display for WaitforStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WAITFOR {} {}", self.kind, self.value)
    }
}

impl fmt::Display for RaiseErrorStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RAISERROR ({}, {}, {}",
            self.message, self.severity, self.state
        )?;
        for arg in &self.args {
            write!(f, ", {arg}")?;
        }
        f.write_str(")")?;
        if !self.options.is_empty() {
            f.write_str(" WITH ")?;
            for (index, option) in self.options.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(option)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for CursorStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.action {
            CursorAction::Declare => {
                write!(f, "DECLARE {} CURSOR", self.name)?;
                for option in &self.options {
                    write!(f, " {option}")?;
                }
                if let Some(query) = &self.query {
                    write!(f, " FOR {query}")?;
                }
                Ok(())
            }
            CursorAction::Open => write!(f, "OPEN {}", self.name),
            CursorAction::Fetch => {
                write!(f, "FETCH NEXT FROM {}", self.name)?;
                if !self.into.is_empty() {
                    f.write_str(" INTO ")?;
                    for (index, target) in self.into.iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(target)?;
                    }
                }
                Ok(())
            }
            CursorAction::Close => write!(f, "CLOSE {}", self.name),
            CursorAction::Deallocate => write!(f, "DEALLOCATE {}", self.name),
        }
    }
}

impl fmt::Display for GrantAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Grant => "GRANT",
            Self::Deny => "DENY",
            Self::Revoke => "REVOKE",
        })
    }
}

/// Writes `GRANT … TO …`, `DENY … TO …` and `REVOKE … FROM …`, the preposition of each
/// statement.
impl fmt::Display for GrantStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.action)?;
        for (index, permission) in self.permissions.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(permission)?;
        }
        if let Some(on) = &self.on {
            write!(f, " ON {on}")?;
        }
        f.write_str(match self.action {
            GrantAction::Revoke => " FROM ",
            _ => " TO ",
        })?;
        comma_separated(f, &self.principals)
    }
}

impl fmt::Display for ProcParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if let Some(default) = &self.default {
            write!(f, " = {default}")?;
        }
        if self.output {
            f.write_str(" OUTPUT")?;
        }
        if self.readonly {
            f.write_str(" READONLY")?;
        }
        Ok(())
    }
}

/// Writes the `WITH` options of a procedure, a function or a view, as they were written.
fn write_with_options(f: &mut fmt::Formatter<'_>, options: &[String]) -> fmt::Result {
    if options.is_empty() {
        return Ok(());
    }
    f.write_str(" WITH ")?;
    for (index, option) in options.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        f.write_str(option)?;
    }
    Ok(())
}

/// Writes `CREATE ` then `OR ALTER ` when the statement asks for it.
fn write_create(f: &mut fmt::Formatter<'_>, or_alter: bool) -> fmt::Result {
    f.write_str("CREATE ")?;
    if or_alter {
        f.write_str("OR ALTER ")?;
    }
    Ok(())
}

impl fmt::Display for CreateProcedureStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_create(f, self.or_alter)?;
        write!(f, "PROCEDURE {}", self.name)?;
        if !self.params.is_empty() {
            f.write_str(" ")?;
            comma_separated(f, &self.params)?;
        }
        write_with_options(f, &self.with_options)?;
        f.write_str(" AS ")?;
        separated(f, &self.body, "; ")
    }
}

impl fmt::Display for FunctionReturns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scalar(ty) => write!(f, "{ty}"),
            Self::Table => f.write_str("TABLE"),
            Self::TableVariable { name, definition } => write!(f, "{name} TABLE {definition}"),
        }
    }
}

impl fmt::Display for FunctionBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Statements(statements) => write_block(f, "BEGIN", statements, "END"),
            Self::Query(query) => write!(f, "RETURN ({query})"),
        }
    }
}

impl fmt::Display for CreateFunctionStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_create(f, self.or_alter)?;
        write!(f, "FUNCTION {} ", self.name)?;
        parenthesised_list(f, &self.params)?;
        write!(f, " RETURNS {}", self.returns)?;
        write_with_options(f, &self.with_options)?;
        write!(f, " AS {}", self.body)
    }
}

impl fmt::Display for CreateViewStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_create(f, self.or_alter)?;
        write!(f, "VIEW {}", self.name)?;
        if !self.columns.is_empty() {
            f.write_str(" ")?;
            parenthesised_list(f, &self.columns)?;
        }
        write_with_options(f, &self.with_options)?;
        write!(f, " AS {}", self.query)
    }
}

impl fmt::Display for TriggerTiming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::After => "AFTER",
            Self::For => "FOR",
            Self::InsteadOf => "INSTEAD OF",
        })
    }
}

impl fmt::Display for TriggerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        })
    }
}

impl fmt::Display for CreateTriggerStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_create(f, self.or_alter)?;
        write!(
            f,
            "TRIGGER {} ON {} {} ",
            self.name, self.table, self.timing
        )?;
        comma_separated(f, &self.events)?;
        f.write_str(" AS ")?;
        separated(f, &self.body, "; ")
    }
}

impl fmt::Display for SequenceCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unspecified => Ok(()),
            Self::NoCache => f.write_str("NO CACHE"),
            Self::Size(size) => write!(f, "CACHE {size}"),
        }
    }
}

impl fmt::Display for CreateSequenceStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE SEQUENCE {}", self.name)?;
        if let Some(ty) = &self.ty {
            write!(f, " AS {ty}")?;
        }
        if let Some(start) = self.start_with {
            write!(f, " START WITH {start}")?;
        }
        if let Some(increment) = self.increment_by {
            write!(f, " INCREMENT BY {increment}")?;
        }
        if let Some(min) = self.min_value {
            write!(f, " MINVALUE {min}")?;
        }
        if let Some(max) = self.max_value {
            write!(f, " MAXVALUE {max}")?;
        }
        if self.cycle {
            f.write_str(" CYCLE")?;
        }
        if self.cache != SequenceCache::Unspecified {
            write!(f, " {}", self.cache)?;
        }
        Ok(())
    }
}

impl fmt::Display for MergeClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MatchedUpdate {
                condition,
                assignments,
            } => {
                write_when_matched(f, "WHEN MATCHED", condition.as_ref())?;
                f.write_str(" THEN UPDATE SET ")?;
                comma_separated(f, assignments)
            }
            Self::MatchedDelete { condition } => {
                write_when_matched(f, "WHEN MATCHED", condition.as_ref())?;
                f.write_str(" THEN DELETE")
            }
            Self::NotMatchedInsert {
                condition,
                columns,
                values,
            } => {
                write_when_matched(f, "WHEN NOT MATCHED", condition.as_ref())?;
                f.write_str(" THEN INSERT")?;
                if !columns.is_empty() {
                    f.write_str(" ")?;
                    parenthesised_list(f, columns)?;
                }
                match values {
                    Some(values) => {
                        f.write_str(" VALUES ")?;
                        parenthesised_list(f, values)
                    }
                    None => f.write_str(" DEFAULT VALUES"),
                }
            }
            Self::NotMatchedBySourceDelete { condition } => {
                write_when_matched(f, "WHEN NOT MATCHED BY SOURCE", condition.as_ref())?;
                f.write_str(" THEN DELETE")
            }
            Self::NotMatchedBySourceUpdate {
                condition,
                assignments,
            } => {
                write_when_matched(f, "WHEN NOT MATCHED BY SOURCE", condition.as_ref())?;
                f.write_str(" THEN UPDATE SET ")?;
                comma_separated(f, assignments)
            }
        }
    }
}

/// Writes the `WHEN …` keyword of a `MERGE` clause and its optional `AND` condition.
fn write_when_matched(
    f: &mut fmt::Formatter<'_>,
    keyword: &str,
    condition: Option<&crate::ast::expr::Expr>,
) -> fmt::Result {
    f.write_str(keyword)?;
    if let Some(condition) = condition {
        write!(f, " AND {condition}")?;
    }
    Ok(())
}

impl fmt::Display for MergeStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MERGE {} USING {} ON {}",
            self.target, self.source, self.on
        )?;
        for clause in &self.clauses {
            write!(f, " {clause}")?;
        }
        if let Some(output) = &self.output {
            write!(f, " {output}")?;
        }
        Ok(())
    }
}

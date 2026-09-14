//! `ALTER TABLE`: `ADD`/`DROP COLUMN` and `ADD`/`DROP CONSTRAINT`. The entry point below
//! and the [`DdlStatement::AlterTable`](crate::bound::DdlStatement::AlterTable) variant it
//! fills are declared, the action being the one
//! [`Catalog::alter_table`](vauban_catalog::Catalog::alter_table) takes, so that the
//! actions the catalogue adds do not reopen `bound/mod.rs`.
//!
//! `ALTER DATABASE … SET` is the other half of the pair and is **not** here: it belongs
//! with the rest of the database statements, in `ddl.rs`.

use vauban_errors::SqlResult;
use vauban_parser::AlterTableStatement;

use crate::bound::BoundStatement;
use crate::context::BindContext;
use crate::query::not_implemented;

/// Binds an `ALTER TABLE` into [`BoundStatement::Ddl`].
pub(crate) fn bind_alter_table(
    stmt: &AlterTableStatement,
    ctx: &BindContext<'_>,
) -> SqlResult<BoundStatement> {
    let _ = (stmt, ctx);
    Err(not_implemented("ALTER TABLE"))
}

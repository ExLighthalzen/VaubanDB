//! `Display` for queries, their clauses and table references.

use std::fmt;

use crate::ast::query::{
    AliasStyle, ApplyKind, CommonTableExpr, ForClause, JoinKind, OffsetFetch, OrderItem,
    PivotSource, QueryBody, QuerySpec, SelectItem, SelectStatement, SetOp, TableHint, TableRef,
    Top, UnpivotSource, With,
};
use crate::display::{comma_separated, parenthesised_list, write_string_literal};

/// Writes the clauses in the order T-SQL requires: `WITH`, the body, `ORDER BY`,
/// `OFFSET … FETCH`, then the `FOR` clause.
impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(with) = &self.with {
            write!(f, "{with} ")?;
        }
        write!(f, "{}", self.body)?;
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            comma_separated(f, &self.order_by)?;
        }
        if let Some(offset_fetch) = &self.offset_fetch {
            write!(f, " {offset_fetch}")?;
        }
        if let Some(for_clause) = &self.for_clause {
            write!(f, " {for_clause}")?;
        }
        Ok(())
    }
}

impl fmt::Display for QueryBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Select(spec) => write!(f, "{spec}"),
            Self::SetOp {
                op,
                all,
                left,
                right,
                ..
            } => {
                write!(f, "{left} {op}")?;
                if *all {
                    f.write_str(" ALL")?;
                }
                write!(f, " {right}")
            }
            Self::Nested(body, _) => write!(f, "({body})"),
        }
    }
}

impl fmt::Display for SetOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Union => "UNION",
            Self::Except => "EXCEPT",
            Self::Intersect => "INTERSECT",
        })
    }
}

/// Writes `SELECT` and its clauses on one line. `SELECT ALL` is never written: the AST
/// only records `DISTINCT`, `ALL` being the default.
impl fmt::Display for QuerySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT")?;
        if self.distinct {
            f.write_str(" DISTINCT")?;
        }
        if let Some(top) = &self.top {
            write!(f, " {top}")?;
        }
        f.write_str(" ")?;
        comma_separated(f, &self.items)?;
        if let Some(into) = &self.into {
            write!(f, " INTO {into}")?;
        }
        if !self.from.is_empty() {
            f.write_str(" FROM ")?;
            comma_separated(f, &self.from)?;
        }
        if let Some(where_) = &self.where_ {
            write!(f, " WHERE {where_}")?;
        }
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            comma_separated(f, &self.group_by)?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        Ok(())
    }
}

/// Writes the alias in the form the user wrote it, which [`AliasStyle`] records.
impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wildcard(_) => f.write_str("*"),
            Self::QualifiedWildcard(name) => write!(f, "{name}.*"),
            Self::Expr {
                expr,
                alias,
                alias_style,
            } => match alias {
                None => write!(f, "{expr}"),
                Some(alias) => match alias_style {
                    AliasStyle::As => write!(f, "{expr} AS {alias}"),
                    AliasStyle::Bare => write!(f, "{expr} {alias}"),
                    AliasStyle::Equals => write!(f, "{alias} = {expr}"),
                },
            },
        }
    }
}

/// Writes `TOP (n) [PERCENT] [WITH TIES]`, keeping the parentheses only when the user
/// wrote them.
impl fmt::Display for Top {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TOP ")?;
        if self.parenthesized {
            write!(f, "({})", self.expr)?;
        } else {
            write!(f, "{}", self.expr)?;
        }
        if self.percent {
            f.write_str(" PERCENT")?;
        }
        if self.with_ties {
            f.write_str(" WITH TIES")?;
        }
        Ok(())
    }
}

/// Writes the direction when the user wrote it, and also when `desc` is set without
/// `explicit_direction`, which would otherwise silently lose the descending order.
impl fmt::Display for OrderItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if let Some(collate) = &self.collate {
            write!(f, " COLLATE {collate}")?;
        }
        if self.explicit_direction || self.desc {
            f.write_str(if self.desc { " DESC" } else { " ASC" })?;
        }
        Ok(())
    }
}

impl fmt::Display for OffsetFetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows = if self.rows_singular { "ROW" } else { "ROWS" };
        write!(f, "OFFSET {} {rows}", self.offset)?;
        if let Some(fetch) = &self.fetch {
            let which = if self.fetch_first { "FIRST" } else { "NEXT" };
            write!(f, " FETCH {which} {fetch} {rows} ONLY")?;
        }
        Ok(())
    }
}

impl fmt::Display for JoinKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::Right => "RIGHT JOIN",
            Self::Full => "FULL JOIN",
            Self::Cross => "CROSS JOIN",
        })
    }
}

impl fmt::Display for ApplyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cross => "CROSS APPLY",
            Self::Outer => "OUTER APPLY",
        })
    }
}

/// Writes a hint as it was read: `NOLOCK`, `INDEX(1)`. Hints are never interpreted.
impl fmt::Display for TableHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.args.is_empty() {
            f.write_str("(")?;
            for (index, arg) in self.args.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(arg)?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

/// Writes a table reference. An alias is always introduced by `AS`: unlike a select item,
/// a table reference does not record whether the user wrote the keyword.
///
/// Hints are always written back with `WITH`, the deprecated spelling `t AS z (NOLOCK)`
/// not being kept either. `FROM t (NOLOCK)`, on the other hand, is not a hint list but a
/// name given arguments, and comes back as `FROM t(NOLOCK)`: the two texts do not share
/// an AST.
impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table {
                name, alias, hints, ..
            } => {
                write!(f, "{name}")?;
                if let Some(alias) = alias {
                    write!(f, " AS {alias}")?;
                }
                if !hints.is_empty() {
                    f.write_str(" WITH ")?;
                    parenthesised_list(f, hints)?;
                }
                Ok(())
            }
            Self::Derived {
                query,
                alias,
                columns,
                ..
            } => {
                write!(f, "({query})")?;
                if let Some(alias) = alias {
                    write!(f, " AS {alias}")?;
                }
                if !columns.is_empty() {
                    f.write_str(" ")?;
                    parenthesised_list(f, columns)?;
                }
                Ok(())
            }
            Self::Join {
                left,
                right,
                kind,
                on,
                ..
            } => {
                write!(f, "{left} {kind} {right}")?;
                if let Some(on) = on {
                    write!(f, " ON {on}")?;
                }
                Ok(())
            }
            Self::Variable { name, alias, .. } => {
                f.write_str(name)?;
                if let Some(alias) = alias {
                    write!(f, " AS {alias}")?;
                }
                Ok(())
            }
            Self::Apply {
                left, right, kind, ..
            } => write!(f, "{left} {kind} {right}"),
            Self::Function {
                name, args, alias, ..
            } => {
                write!(f, "{name}")?;
                parenthesised_list(f, args)?;
                if let Some(alias) = alias {
                    write!(f, " AS {alias}")?;
                }
                Ok(())
            }
            Self::Pivot(pivot) => write!(f, "{pivot}"),
            Self::Unpivot(unpivot) => write!(f, "{unpivot}"),
        }
    }
}

impl fmt::Display for PivotSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} PIVOT ({} FOR {} IN ",
            self.source, self.aggregate, self.value_column
        )?;
        parenthesised_list(f, &self.columns)?;
        f.write_str(")")?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        Ok(())
    }
}

impl fmt::Display for UnpivotSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} UNPIVOT ({} FOR {} IN ",
            self.source, self.value_column, self.name_column
        )?;
        parenthesised_list(f, &self.columns)?;
        f.write_str(")")?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        Ok(())
    }
}

/// Writes `WITH cte1, cte2`. T-SQL has no `RECURSIVE` keyword, so `recursive_allowed` is
/// never written: it exists for the binder.
impl fmt::Display for With {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WITH ")?;
        comma_separated(f, &self.ctes)
    }
}

impl fmt::Display for CommonTableExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.columns.is_empty() {
            f.write_str(" ")?;
            parenthesised_list(f, &self.columns)?;
        }
        write!(f, " AS ({})", self.query)
    }
}

impl fmt::Display for ForClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json { raw, path } => {
                f.write_str(if *raw {
                    "FOR JSON AUTO"
                } else {
                    "FOR JSON PATH"
                })?;
                if let Some(path) = path {
                    f.write_str(", ROOT(")?;
                    write_string_literal(f, path, false)?;
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::Xml { raw, path } => {
                f.write_str(if *raw { "FOR XML RAW" } else { "FOR XML PATH" })?;
                if let Some(path) = path {
                    f.write_str("(")?;
                    write_string_literal(f, path, false)?;
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::Browse => f.write_str("FOR BROWSE"),
        }
    }
}

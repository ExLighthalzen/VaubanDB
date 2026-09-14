//! `Display` for expressions, names, literals and data types.

use std::fmt::{self, Write as _};

use crate::ast::expr::{
    BinaryOp, CaseArm, ColumnRef, DataType, Expr, FrameBound, FrameUnits, Ident, InList, Literal,
    ObjectName, Over, Quantifier, TypeArg, UnaryOp, WindowFrame,
};
use crate::display::{
    comma_separated, has_identifier_shape, is_regular_identifier, parenthesised_list,
    write_bracketed, write_string_literal,
};
use crate::keyword::Keyword;
use crate::parser::expr::NILADIC_FUNCTIONS;

/// Writes the identifier bare when it can be, between brackets otherwise.
///
/// Brackets are written when the user wrote them (`quoted`) and, as a safety rule, when
/// the value is not a valid regular identifier: a bare `my col` or a reserved `select`
/// would not parse back. See `display::is_regular_identifier`.
impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.quoted || !is_regular_identifier(&self.value) {
            write_bracketed(f, &self.value)
        } else {
            f.write_str(&self.value)
        }
    }
}

/// Writes the qualifiers of `name`, each one followed by its `.`, and nothing at all when
/// the name has none. A level the user skipped keeps its dot: `srv..dbo.`.
fn write_qualifiers(f: &mut fmt::Formatter<'_>, name: &ObjectName) -> fmt::Result {
    let leading = [
        name.server.as_ref(),
        name.database.as_ref(),
        name.schema.as_ref(),
    ];
    if let Some(first) = leading.iter().position(Option::is_some) {
        for part in &leading[first..] {
            if let Some(ident) = part {
                write!(f, "{ident}")?;
            }
            f.write_str(".")?;
        }
    }
    Ok(())
}

/// Writes the parts separated by `.`, keeping an empty part for a level the user skipped:
/// a name with a server and no database is written `srv..dbo.t`.
impl fmt::Display for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_qualifiers(f, self)?;
        write!(f, "{}", self.name)
    }
}

/// Writes `name` in function-name position: the qualifiers as usual, then the name itself
/// **without** the reserved-keyword safety rule.
///
/// `LEFT`, `RIGHT`, `NULLIF`, `COALESCE`, `CONVERT` and the niladic `CURRENT_TIMESTAMP`,
/// `CURRENT_USER`, `SESSION_USER`, `SYSTEM_USER` are reserved keywords that the grammar
/// reads as function names. Writing `LEFT(x, 1)` as `[LEFT](x, 1)` would re-parse into a
/// different tree, since `Ident.quoted` would turn from `false` to `true`, and would break
/// the parse -> `Display` -> parse contract of the module. Brackets are still written when
/// the user wrote them and when the value is not spelled like an identifier at all (`my
/// fn`), which are the two cases where writing it bare would not parse back.
fn write_function_name(f: &mut fmt::Formatter<'_>, name: &ObjectName) -> fmt::Result {
    write_qualifiers(f, name)?;
    if name.name.quoted || !has_identifier_shape(&name.name.value) {
        write_bracketed(f, &name.name.value)
    } else {
        f.write_str(&name.name.value)
    }
}

/// True when `value` spells one of the five niladic functions of T-SQL, in whichever case
/// it is written: `Keyword::parse` folds the case, as it does for any keyword.
///
/// The list is [`NILADIC_FUNCTIONS`], the one the grammar reads, so that this crate spells
/// the five names once. It is not the only list in the workspace: `vauban-binder`'s
/// `call.rs` holds a second constant of the same name, the five function names as strings,
/// which this one neither reads nor feeds. A word that spells one of them is an
/// identifier by construction, so no shape check is needed on top of it.
fn is_niladic_function(value: &str) -> bool {
    Keyword::parse(value).is_some_and(|keyword| NILADIC_FUNCTIONS.contains(&keyword))
}

/// Writes the qualifier and its `.` when there is one, then the name.
///
/// A one-part, unquoted name that spells a niladic function escapes the
/// reserved-keyword safety rule of [`Ident`], for the same reason as a function name:
/// `CURRENT_TIMESTAMP` written without parentheses is read by the grammar as a
/// column reference, and writing it `[CURRENT_TIMESTAMP]` would parse back with
/// `Ident.quoted` turned from `false` to `true`, breaking the parse -> `Display` -> parse
/// contract of the module. Brackets the user wrote are kept, and a qualified `t.USER` is
/// a plain column, which keeps the safety rule.
impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(qualifier) = &self.qualifier {
            return write!(f, "{qualifier}.{}", self.name);
        }
        if !self.name.quoted && is_niladic_function(&self.name.value) {
            return f.write_str(&self.name.value);
        }
        write!(f, "{}", self.name)
    }
}

/// Restores what the lexer removed: the `$` of a money literal, the `0x` of a binary one,
/// the delimiters, the doubled `'` and the `N` of a string.
impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Default => f.write_str("DEFAULT"),
            Self::Integer(text) | Self::Decimal(text) | Self::Float(text) => f.write_str(text),
            Self::Money(text) => write!(f, "${text}"),
            Self::Str { value, unicode } => write_string_literal(f, value, *unicode),
            Self::Binary(text) => write!(f, "0x{text}"),
        }
    }
}

/// Writes the operator alone, without the surrounding spaces, which [`Expr`] adds.
///
/// [`BinaryOp::Ne`] is written `<>`: the AST does not tell `<>` from `!=`.
/// [`BinaryOp::Concat`], which no grammar rule produces, is written `+`.
impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add | Self::Concat => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::NotLt => "!<",
            Self::NotGt => "!>",
            Self::And => "AND",
            Self::Or => "OR",
        })
    }
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Not => "NOT",
            Self::BitNot => "~",
        })
    }
}

/// Writes the operand of a sign operator (`+`, `-`, `~`), keeping the two apart when the
/// operand starts with that same sign.
///
/// Gluing them would write a token the tree does not hold: `- -10` printed `--10` starts a
/// line comment, and the text no longer parses. `SELECT - -10` yields 10 and
/// `SELECT -(-10)` yields 10, while `SELECT --10` is answered with a 102 near `SELECT`,
/// the comment having eaten the rest of the line. `SELECT ++1`, `SELECT ~~1`,
/// `SELECT -+1` and `SELECT +-1` are accepted
/// glued; just the repeated sign is spaced here, for one rule instead of a table of pairs.
fn write_signed(f: &mut fmt::Formatter<'_>, sign: char, expr: &Expr) -> fmt::Result {
    f.write_char(sign)?;
    let mut operand = AfterSign {
        inner: f,
        pending: Some(sign),
    };
    write!(operand, "{expr}")
}

/// The sink of [`write_signed`]: it forwards everything to `inner`, after one space when
/// the very first character written is the sign just before it.
struct AfterSign<'a, 'b> {
    inner: &'a mut fmt::Formatter<'b>,
    /// The sign written before the operand, until its first character has been seen.
    pending: Option<char>,
}

impl fmt::Write for AfterSign<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if let Some(sign) = self.pending {
            // An empty write says nothing about the first character: keep waiting.
            let Some(first) = text.chars().next() else {
                return Ok(());
            };
            self.pending = None;
            if first == sign {
                self.inner.write_char(' ')?;
            }
        }
        self.inner.write_str(text)
    }
}

impl fmt::Display for Quantifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "ALL",
            Self::Any => "ANY",
            Self::Some => "SOME",
        })
    }
}

impl fmt::Display for TypeArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::Max => f.write_str("MAX"),
            Self::Ident(value) => f.write_str(value),
        }
    }
}

/// Writes the type name as it was written, followed by its arguments when it has any:
/// `decimal(10, 2)`, `varchar(MAX)`, `int`.
impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.args.is_empty() {
            parenthesised_list(f, &self.args)?;
        }
        Ok(())
    }
}

impl fmt::Display for CaseArm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WHEN {} THEN {}", self.when, self.then)
    }
}

/// Writes the whole `OVER (…)` clause, parentheses included.
impl fmt::Display for Over {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OVER (")?;
        let mut written = false;
        if !self.partition_by.is_empty() {
            f.write_str("PARTITION BY ")?;
            comma_separated(f, &self.partition_by)?;
            written = true;
        }
        if !self.order_by.is_empty() {
            if written {
                f.write_str(" ")?;
            }
            f.write_str("ORDER BY ")?;
            comma_separated(f, &self.order_by)?;
            written = true;
        }
        if let Some(frame) = &self.frame {
            if written {
                f.write_str(" ")?;
            }
            write!(f, "{frame}")?;
        }
        f.write_str(")")
    }
}

impl fmt::Display for FrameUnits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rows => "ROWS",
            Self::Range => "RANGE",
        })
    }
}

impl fmt::Display for FrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundedPreceding => f.write_str("UNBOUNDED PRECEDING"),
            Self::Preceding(expr) => write!(f, "{expr} PRECEDING"),
            Self::CurrentRow => f.write_str("CURRENT ROW"),
            Self::Following(expr) => write!(f, "{expr} FOLLOWING"),
            Self::UnboundedFollowing => f.write_str("UNBOUNDED FOLLOWING"),
        }
    }
}

impl fmt::Display for WindowFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.end {
            Some(end) => write!(f, "{} BETWEEN {} AND {end}", self.units, self.start),
            None => write!(f, "{} {}", self.units, self.start),
        }
    }
}

impl fmt::Display for InList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exprs(exprs) => comma_separated(f, exprs),
            Self::Subquery(query) => write!(f, "{query}"),
        }
    }
}

/// Writes an expression on one line.
///
/// No parenthesis is ever added from operator precedence: the ones the user wrote are
/// [`Expr::Nested`] nodes, and the only others are the ones the grammar requires (call
/// arguments, `IN` list, subqueries, `OVER (…)`).
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(literal, _) => write!(f, "{literal}"),
            Self::Column(column) => write!(f, "{column}"),
            Self::Variable { name, .. } => f.write_str(name),
            Self::Binary {
                op, left, right, ..
            } => write!(f, "{left} {op} {right}"),
            Self::Unary { op, expr, .. } => match op {
                UnaryOp::Not => write!(f, "NOT {expr}"),
                UnaryOp::Plus => write_signed(f, '+', expr),
                UnaryOp::Minus => write_signed(f, '-', expr),
                UnaryOp::BitNot => write_signed(f, '~', expr),
            },
            Self::Nested(expr, _) => write!(f, "({expr})"),
            Self::Function {
                name,
                args,
                star,
                distinct,
                over,
                ..
            } => {
                write_function_name(f, name)?;
                f.write_str("(")?;
                if *star {
                    f.write_str("*")?;
                } else {
                    if *distinct {
                        f.write_str("DISTINCT ")?;
                    }
                    comma_separated(f, args)?;
                }
                f.write_str(")")?;
                if let Some(over) = over {
                    write!(f, " {over}")?;
                }
                Ok(())
            }
            Self::Case {
                operand,
                arms,
                else_,
                ..
            } => {
                f.write_str("CASE")?;
                if let Some(operand) = operand {
                    write!(f, " {operand}")?;
                }
                for arm in arms {
                    write!(f, " {arm}")?;
                }
                if let Some(else_) = else_ {
                    write!(f, " ELSE {else_}")?;
                }
                f.write_str(" END")
            }
            Self::Cast { expr, ty, try_, .. } => {
                if *try_ {
                    f.write_str("TRY_")?;
                }
                write!(f, "CAST({expr} AS {ty})")
            }
            Self::Convert {
                ty,
                expr,
                style,
                try_,
                ..
            } => {
                if *try_ {
                    f.write_str("TRY_")?;
                }
                write!(f, "CONVERT({ty}, {expr}")?;
                if let Some(style) = style {
                    write!(f, ", {style}")?;
                }
                f.write_str(")")
            }
            Self::IsNull { expr, negated, .. } => {
                write!(f, "{expr} IS ")?;
                if *negated {
                    f.write_str("NOT ")?;
                }
                f.write_str("NULL")
            }
            Self::In {
                expr,
                list,
                negated,
                ..
            } => {
                write!(f, "{expr} ")?;
                if *negated {
                    f.write_str("NOT ")?;
                }
                write!(f, "IN ({list})")
            }
            Self::Like {
                expr,
                pattern,
                escape,
                negated,
                ..
            } => {
                write!(f, "{expr} ")?;
                if *negated {
                    f.write_str("NOT ")?;
                }
                write!(f, "LIKE {pattern}")?;
                if let Some(escape) = escape {
                    write!(f, " ESCAPE {escape}")?;
                }
                Ok(())
            }
            Self::Between {
                expr,
                low,
                high,
                negated,
                ..
            } => {
                write!(f, "{expr} ")?;
                if *negated {
                    f.write_str("NOT ")?;
                }
                write!(f, "BETWEEN {low} AND {high}")
            }
            Self::Exists(query, _) => write!(f, "EXISTS ({query})"),
            Self::Subquery(query, _) => write!(f, "({query})"),
            Self::Quantified {
                expr,
                op,
                quantifier,
                subquery,
                ..
            } => write!(f, "{expr} {op} {quantifier} ({subquery})"),
            Self::Collate {
                expr, collation, ..
            } => write!(f, "{expr} COLLATE {collation}"),
            Self::Assign { target, value, .. } => write!(f, "{target} = {value}"),
            Self::NextValueFor { sequence, over, .. } => {
                write!(f, "NEXT VALUE FOR {sequence}")?;
                if let Some(over) = over {
                    write!(f, " {over}")?;
                }
                Ok(())
            }
            Self::InvalidNiladic { spelling, .. } => f.write_str(spelling),
            Self::Placeholder(_) => f.write_str("?"),
        }
    }
}

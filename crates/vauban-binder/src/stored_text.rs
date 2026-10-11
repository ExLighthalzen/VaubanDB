//! The text the catalogue stores for the expression of a `DEFAULT`, of a `CHECK` or of a
//! computed column, written from the tree of the statement.
//!
//! The catalogue keeps the text alone: `sys.check_constraints.definition`,
//! `sys.default_constraints.definition` and `sys.computed_columns.definition` publish it, and
//! a statement that reads the expression parses and binds it again
//! ([`parse_stored_expression`]).

use vauban_errors::SqlResult;
use vauban_parser::{
    BinaryOp, ColumnRef, DataType, Expr, InList, Literal, ParseOptions, QueryBody, SelectItem,
    Span, Statement, TypeArg, UnaryOp, parse_batch,
};

use crate::insert::unbracket_type_names;
use crate::query::bug;

/// How the text of an object expression is read back: a `CHECK` is a predicate, a `DEFAULT`
/// and a computed column are values. The two are parsed in different positions of a `SELECT`
/// ([`parse_stored_expression`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionKind {
    /// The predicate of a `CHECK`.
    Predicate,
    /// The value of a `DEFAULT` or of a computed column.
    Value,
}

/// The text the catalogue stores for `expr`.
///
/// The normalised form of [`normalized_text`] when it reads back as one expression; for a
/// shape that function does not write (a subquery, a windowed call, a column qualified by its
/// schema), the text `Display` writes between parentheses. Read back by
/// [`parse_stored_expression`] and written again, the text is the same text (unit test
/// `the_text_of_a_stored_tree_is_the_stored_text`).
///
/// # Errors
///
/// The internal error when neither text reads back as one expression.
pub(crate) fn stored_text(expr: &Expr, kind: ExpressionKind) -> SqlResult<String> {
    if let Some(text) = normalized_text(expr)
        && parse_stored_expression(&text, kind).is_ok()
    {
        return Ok(text);
    }
    let text = display_text(expr);
    parse_stored_expression(&text, kind)?;
    Ok(text)
}

/// `Display` of `expr` between one pair of parentheses, one level of parentheses of `expr`
/// itself taken off so that the text of a tree read back from this text is this text.
fn display_text(expr: &Expr) -> String {
    match expr {
        Expr::Nested(inner, _) => format!("({inner})"),
        other => format!("({other})"),
    }
}

/// Reads back the stored text of an object expression.
///
/// A predicate is read as the `WHERE` of `SELECT 1 WHERE <text>`, a value as the one item of
/// `SELECT <text>`. A type name written between brackets, `CONVERT([int],(1))`, is kept with
/// its brackets by the parser; it is taken off here so that the tree names the type `int`,
/// as it would for `CONVERT(int, 1)` (unit test `a_bracketed_type_name_reads_back_bare`).
///
/// # Errors
///
/// The syntax error of the text, or the internal error when the text holds something else
/// than one expression.
pub fn parse_stored_expression(text: &str, kind: ExpressionKind) -> SqlResult<Expr> {
    let batch = match kind {
        ExpressionKind::Predicate => format!("SELECT 1 WHERE {text}"),
        ExpressionKind::Value => format!("SELECT {text}"),
    };
    let batch = parse_batch(&batch, &ParseOptions::default())?;
    let found = match batch.statements.as_slice() {
        [Statement::Select(select)] => match &select.body {
            QueryBody::Select(spec) if spec.from.is_empty() => match kind {
                ExpressionKind::Predicate => spec.where_.clone(),
                ExpressionKind::Value => match spec.items.as_slice() {
                    [
                        SelectItem::Expr {
                            expr, alias: None, ..
                        },
                    ] if spec.where_.is_none() => Some(expr.clone()),
                    _ => None,
                },
            },
            _ => None,
        },
        _ => None,
    };
    let mut tree = found.ok_or_else(|| {
        bug(format!(
            "bind: the stored expression {text} does not read back as one expression"
        ))
    })?;
    unbracket_type_names(&mut tree);
    Ok(tree)
}

/// Where an expression is written, which decides the parentheses around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The whole expression, inside the outer pair of parentheses.
    Top,
    /// An operand of a comparison or of `IS NULL`.
    Comparison,
    /// An operand of `+`, `-`, `&`, `|` or `^`.
    Additive,
    /// An operand of `*`, `/` or `%`.
    Multiplicative,
    /// The operand of a unary `-` or `~`.
    Sign,
    /// An operand of `AND`, `OR` or `NOT`.
    Logic,
    /// An argument of a call, a branch of a `CASE`, an operand of `LIKE` or `COLLATE`.
    Other,
}

/// The normalised text of `expr`, the form SQL Server publishes in the `definition` of a
/// `CHECK` or of a `DEFAULT`, `None` for a shape this function does not write.
///
/// The rules, frozen by the unit test `the_definition_text_is_normalised` on the forms its
/// corpus holds:
///
/// - the whole text is between one pair of parentheses, and the parentheses the statement
///   wrote are dropped: `CHECK ((a > 0))` is `([a]>(0))`;
/// - a column is between brackets, `]` doubled; a number is between parentheses, `(0)`,
///   `(-1)`, `(1.5)`, a sign before a number folded into it (`-(1)` is `(-1)`, `- -1` is
///   `(1)`); an integer outside the range of `int` gets a final point (`(2147483648.)`); a
///   `float` is written with sixteen decimals and a three-digit exponent
///   (`(1.0000000000000000e+003)`), a `money` with four decimals (`($5.0000)`); a string, a
///   binary and `NULL` are bare (`'x'`, `N'x'`, `0xAB`, `NULL`);
/// - operators are written without spaces, `AND`, `OR` and `NOT` with spaces; `!=` is `<>`,
///   `!<` is `>=`, `!>` is `<=`;
/// - an arithmetic operation inside another one is between parentheses, except a `*`, `/`
///   or `%` under an additive one (`+`, `-`, `&`, `|`, `^`): `(1)+(2)*(3)` but
///   `((1)+(2))*(3)` and `((1)+(2))+(3)`; an additive one under a comparison is between
///   parentheses too: `([a]+(1))>(0)` but `[a]*(2)>(-1)`; a unary `-` over something else
///   than a number is ` -` between parentheses under an operator, `( -[a])<(0)`, and bare
///   in a call, a `CONVERT` or a `CASE`, `abs( -[a])`, `then  -[a]`; a column qualified by
///   its table is `[t].[a]`;
/// - an `OR` under an `AND` is between parentheses, as is a right operand of the same
///   operator: `[a]=(1) OR ([a]=(2) OR [a]=(3))`; `NOT` puts parentheses around an `AND` or an
///   `OR`;
/// - `IN` is a chain of `OR` over the list read from its end (`a IN (1, 2)` is
///   `[a]=(2) OR [a]=(1)`), `BETWEEN` an `AND` of `>=` and `<=`, `NOT LIKE` is `NOT … like`;
/// - a function is in lower case, its arguments separated by a bare comma; `CURRENT_TIMESTAMP`
///   is `getdate()`, `USER`, `CURRENT_USER` and `SESSION_USER` are `user_name()`,
///   `SYSTEM_USER` is `suser_sname()`; `YEAR` is `datepart(year,…)` (`MONTH` and `DAY` are
///   written on that shape, outside the corpus), and the first argument of `datepart`,
///   `dateadd` and `datediff` is the full name of the part (`dd` is `day`);
/// - `CAST` is `CONVERT([type],…)`, `TRY_CAST` and `TRY_CONVERT` without style are
///   `TRY_CAST(… AS [type])`; `CASE`, `when`, `then`, `else`, `end`, `like`, `escape` and
///   `collate` are in lower case.
fn normalized_text(expr: &Expr) -> Option<String> {
    Some(format!("({})", write(expr, Slot::Top)?))
}

/// The text of `expr` written in `slot`. See [`normalized_text`].
fn write(expr: &Expr, slot: Slot) -> Option<String> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => write(inner, slot),
        Expr::Literal(literal, _) => literal_text(literal, false),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr: inner,
            ..
        } => match signed_literal(inner, true) {
            Some((literal, negative)) => literal_text(literal, negative),
            // ` -[a]`, between parentheses under an operator: `( -[a])<(0)`; as the whole
            // expression the outer pair stands for them: `( -((1)+(1)))`.
            None => {
                // Bare in a call, a `CONVERT` or a `CASE` too: `abs( -[a])`,
                // `then  -[a]`, `case  -[a] when`.
                let text = format!(" -{}", write(inner, Slot::Sign)?);
                Some(if matches!(slot, Slot::Top | Slot::Other) {
                    text
                } else {
                    format!("({text})")
                })
            }
        },
        Expr::Unary {
            op: UnaryOp::BitNot,
            expr: inner,
            ..
        } => {
            let text = format!("~{}", write(inner, Slot::Sign)?);
            Some(if slot == Slot::Sign {
                format!("({text})")
            } else {
                text
            })
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr: inner,
            ..
        } => Some(format!("NOT {}", negated_text(inner)?)),
        Expr::Column(column) => column_text(column),
        Expr::Variable { name, .. } => name.starts_with("@@").then(|| name.to_ascii_lowercase()),
        Expr::Binary {
            op, left, right, ..
        } => binary_text(*op, left, right, slot),
        Expr::Function {
            name,
            args,
            star: false,
            distinct: false,
            over: None,
            ..
        } if name.server.is_none() && name.database.is_none() && name.schema.is_none() => {
            function_text(&name.name.value, args)
        }
        Expr::Case {
            operand,
            arms,
            else_,
            ..
        } => {
            let mut text = String::from("case");
            if let Some(operand) = operand {
                text.push(' ');
                text.push_str(&write(operand, Slot::Other)?);
            }
            for arm in arms {
                text.push_str(" when ");
                text.push_str(&write(&arm.when, Slot::Other)?);
                text.push_str(" then ");
                text.push_str(&write(&arm.then, Slot::Other)?);
            }
            // `case when (1)=(1) then (1) else (0) end`, and two spaces before `end` without
            // an `else`: `case [a] when (1) then 'x'  end`.
            text.push(' ');
            if let Some(else_) = else_ {
                text.push_str("else ");
                text.push_str(&write(else_, Slot::Other)?);
            }
            text.push_str(" end");
            Some(text)
        }
        Expr::Cast {
            expr: inner,
            ty,
            try_,
            ..
        } => conversion_text(ty, inner, None, *try_),
        Expr::Convert {
            ty,
            expr: inner,
            style,
            try_,
            ..
        } => conversion_text(ty, inner, style.as_deref(), *try_),
        Expr::IsNull {
            expr: inner,
            negated,
            ..
        } => Some(format!(
            "{} IS {}NULL",
            write(inner, Slot::Comparison)?,
            if *negated { "NOT " } else { "" }
        )),
        Expr::In {
            expr: inner,
            list: InList::Exprs(items),
            negated,
            ..
        } => {
            let chain = in_as_chain(inner, items)?;
            if *negated {
                Some(format!("NOT {}", negated_text(&chain)?))
            } else {
                write(&chain, slot)
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            negated,
            ..
        } => {
            let mut text = format!(
                "{} like {}",
                write(inner, Slot::Other)?,
                write(pattern, Slot::Other)?
            );
            if let Some(escape) = escape {
                text.push_str(&format!(" escape {} ", write(escape, Slot::Other)?));
            }
            Some(if *negated {
                format!("NOT {text}")
            } else {
                text
            })
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
            ..
        } => {
            let range = binary(
                BinaryOp::And,
                binary(BinaryOp::Ge, (**inner).clone(), (**low).clone()),
                binary(BinaryOp::Le, (**inner).clone(), (**high).clone()),
            );
            if *negated {
                Some(format!("NOT {}", negated_text(&range)?))
            } else {
                write(&range, slot)
            }
        }
        Expr::Collate {
            expr: inner,
            collation,
            ..
        } => Some(format!(
            "({}) collate {collation}",
            write(inner, Slot::Other)?
        )),
        Expr::NextValueFor {
            sequence,
            over: None,
            ..
        } => {
            let parts: Vec<String> = [&sequence.database, &sequence.schema]
                .into_iter()
                .flatten()
                .chain(std::iter::once(&sequence.name))
                .map(|part| bracketed(&part.value))
                .collect();
            Some(format!("NEXT VALUE FOR {}", parts.join(".")))
        }
        _ => None,
    }
}

/// A binary node built from two operands, with no text behind it.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        op_span: Span::EMPTY,
        left: Box::new(left),
        right: Box::new(right),
        span: Span::EMPTY,
    }
}

/// `e IN (i1, …, in)` as `e=in OR … OR e=i1`, `None` for an empty list.
fn in_as_chain(expr: &Expr, items: &[Expr]) -> Option<Expr> {
    let mut items = items.iter().rev();
    let first = items.next()?;
    let mut chain = binary(BinaryOp::Eq, expr.clone(), first.clone());
    for item in items {
        chain = binary(
            BinaryOp::Or,
            chain,
            binary(BinaryOp::Eq, expr.clone(), item.clone()),
        );
    }
    Some(chain)
}

/// The operand of `NOT`: between parentheses when it is an `AND` or an `OR`.
fn negated_text(expr: &Expr) -> Option<String> {
    let text = write(expr, Slot::Logic)?;
    Some(if logic_operator(expr).is_some() {
        format!("({text})")
    } else {
        text
    })
}

/// `AND` or `OR` when `expr` is written as one: the operator itself, an `IN` over two items
/// or more (a chain of `OR`), a `BETWEEN` (an `AND`).
fn logic_operator(expr: &Expr) -> Option<BinaryOp> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => logic_operator(inner),
        Expr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            ..
        } => Some(*op),
        Expr::In {
            list: InList::Exprs(items),
            negated: false,
            ..
        } if items.len() > 1 => Some(BinaryOp::Or),
        Expr::Between { negated: false, .. } => Some(BinaryOp::And),
        _ => None,
    }
}

/// The text of a binary operation written in `slot`.
fn binary_text(op: BinaryOp, left: &Expr, right: &Expr, slot: Slot) -> Option<String> {
    if let Some(keyword) = match op {
        BinaryOp::And => Some("AND"),
        BinaryOp::Or => Some("OR"),
        _ => None,
    } {
        let mut left_text = write(left, Slot::Logic)?;
        if op == BinaryOp::And && logic_operator(left) == Some(BinaryOp::Or) {
            left_text = format!("({left_text})");
        }
        let mut right_text = write(right, Slot::Logic)?;
        if let Some(child) = logic_operator(right)
            && (child == op || op == BinaryOp::And)
        {
            right_text = format!("({right_text})");
        }
        return Some(format!("{left_text} {keyword} {right_text}"));
    }
    let (symbol, comparison, additive) = match op {
        BinaryOp::Eq => ("=", true, false),
        BinaryOp::Ne => ("<>", true, false),
        BinaryOp::Lt => ("<", true, false),
        BinaryOp::Le | BinaryOp::NotGt => ("<=", true, false),
        BinaryOp::Gt => (">", true, false),
        BinaryOp::Ge | BinaryOp::NotLt => (">=", true, false),
        BinaryOp::Mul => ("*", false, false),
        BinaryOp::Div => ("/", false, false),
        BinaryOp::Mod => ("%", false, false),
        BinaryOp::Add | BinaryOp::Concat => ("+", false, true),
        BinaryOp::Sub => ("-", false, true),
        BinaryOp::BitAnd => ("&", false, true),
        BinaryOp::BitOr => ("|", false, true),
        BinaryOp::BitXor => ("^", false, true),
        BinaryOp::And | BinaryOp::Or => return None,
    };
    if comparison {
        return Some(format!(
            "{}{symbol}{}",
            write(left, Slot::Comparison)?,
            write(right, Slot::Comparison)?
        ));
    }
    let operand = if additive {
        Slot::Additive
    } else {
        Slot::Multiplicative
    };
    let text = format!(
        "{}{symbol}{}",
        write(left, operand)?,
        write(right, operand)?
    );
    // `(1)+(2)*(3)`, `((1)+(2))*(3)`, `((1)+(2))+(3)`, `([a]*[a2])*[b2]`.
    let wrapped = match slot {
        Slot::Multiplicative | Slot::Sign => true,
        Slot::Additive | Slot::Comparison => additive,
        Slot::Top | Slot::Logic | Slot::Other => false,
    };
    Some(if wrapped { format!("({text})") } else { text })
}

/// The functions whose first argument names a part of a date.
const DATE_PART_FUNCTIONS: [&str; 6] = [
    "datepart",
    "dateadd",
    "datediff",
    "datediff_big",
    "datename",
    "datetrunc",
];

/// The text of a call of the unqualified function `name`.
fn function_text(name: &str, args: &[Expr]) -> Option<String> {
    let name = name.to_ascii_lowercase();
    if matches!(name.as_str(), "year" | "month" | "day")
        && let [arg] = args
    {
        return Some(format!("datepart({name},{})", write(arg, Slot::Other)?));
    }
    let mut texts = Vec::with_capacity(args.len());
    for (position, arg) in args.iter().enumerate() {
        let part = (position == 0 && DATE_PART_FUNCTIONS.contains(&name.as_str()))
            .then(|| date_part_name(arg))
            .flatten();
        texts.push(match part {
            Some(part) => part,
            None => write(arg, Slot::Other)?,
        });
    }
    Some(format!("{name}({})", texts.join(",")))
}

/// The full name of the date part an unquoted name designates, `day` for `dd`; `None` for
/// another shape of argument.
///
/// The corpus of `the_definition_text_is_normalised` holds `dd` and `year`; the other names
/// follow the abbreviation table of `DATEPART`, and a name outside it is kept in lower case.
fn date_part_name(arg: &Expr) -> Option<String> {
    let Expr::Column(ColumnRef {
        qualifier: None,
        name,
        ..
    }) = arg
    else {
        return None;
    };
    if name.quoted {
        return None;
    }
    let written = name.value.to_ascii_lowercase();
    let full = match written.as_str() {
        "yy" | "yyyy" => "year",
        "qq" | "q" => "quarter",
        "mm" | "m" => "month",
        "dy" | "y" => "dayofyear",
        "dd" | "d" => "day",
        "wk" | "ww" => "week",
        "dw" | "w" => "weekday",
        "hh" => "hour",
        "mi" | "n" => "minute",
        "ss" | "s" => "second",
        "ms" => "millisecond",
        "mcs" => "microsecond",
        "ns" => "nanosecond",
        "tz" => "tzoffset",
        "isowk" | "isoww" => "iso_week",
        other => other,
    };
    Some(full.to_owned())
}

/// `CONVERT([type],expr[,style])`, or `TRY_CAST(expr AS [type])` for a `TRY_CAST` and for a
/// `TRY_CONVERT` without style.
fn conversion_text(ty: &DataType, expr: &Expr, style: Option<&Expr>, try_: bool) -> Option<String> {
    let ty = type_text(ty)?;
    let expr = write(expr, Slot::Other)?;
    match (try_, style) {
        (true, None) => Some(format!("TRY_CAST({expr} AS {ty})")),
        (true, Some(_)) => None,
        (false, None) => Some(format!("CONVERT({ty},{expr})")),
        (false, Some(style)) => Some(format!(
            "CONVERT({ty},{expr},{})",
            write(style, Slot::Other)?
        )),
    }
}

/// `[varchar](10)`, `[decimal](10,2)`, `[nvarchar](max)`, `[int]`.
fn type_text(ty: &DataType) -> Option<String> {
    let name = ty
        .name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map_or_else(|| ty.name.clone(), |inner| inner.replace("]]", "]"));
    let mut text = bracketed(&name.to_ascii_lowercase());
    if !ty.args.is_empty() {
        let mut args = Vec::with_capacity(ty.args.len());
        for arg in &ty.args {
            args.push(match arg {
                TypeArg::Number(number) => number.to_string(),
                TypeArg::Max => "max".to_owned(),
                TypeArg::Ident(_) => return None,
            });
        }
        text.push_str(&format!("({})", args.join(",")));
    }
    Some(text)
}

/// `[name]`, `]` doubled.
fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// A column between brackets, or the call a niladic name stands for. A column qualified by
/// its table is `[t].[a]`, each part with the case it was written with; a qualifier of more
/// parts is not written.
fn column_text(column: &ColumnRef) -> Option<String> {
    if let Some(qualifier) = &column.qualifier {
        if qualifier.server.is_some() || qualifier.database.is_some() || qualifier.schema.is_some()
        {
            return None;
        }
        return Some(format!(
            "{}.{}",
            bracketed(&qualifier.name.value),
            bracketed(&column.name.value)
        ));
    }
    if !column.name.quoted {
        let niladic = match column.name.value.to_ascii_uppercase().as_str() {
            "CURRENT_TIMESTAMP" => Some("getdate()"),
            "USER" | "CURRENT_USER" | "SESSION_USER" => Some("user_name()"),
            "SYSTEM_USER" => Some("suser_sname()"),
            _ => None,
        };
        if let Some(call) = niladic {
            return Some(call.to_owned());
        }
    }
    Some(bracketed(&column.name.value))
}

/// The numeric literal `expr` is once its signs are folded, with whether it is negative:
/// `negative` is the sign already met. `None` when `expr` is not a sign over a number.
fn signed_literal(expr: &Expr, negative: bool) -> Option<(&Literal, bool)> {
    match expr {
        Expr::Nested(inner, _)
        | Expr::Unary {
            op: UnaryOp::Plus,
            expr: inner,
            ..
        } => signed_literal(inner, negative),
        Expr::Unary {
            op: UnaryOp::Minus,
            expr: inner,
            ..
        } => signed_literal(inner, !negative),
        Expr::Literal(
            literal @ (Literal::Integer(_)
            | Literal::Decimal(_)
            | Literal::Float(_)
            | Literal::Money(_)),
            _,
        ) => Some((literal, negative)),
        _ => None,
    }
}

/// The text of a literal, `negative` for a number under an odd count of `-`.
fn literal_text(literal: &Literal, negative: bool) -> Option<String> {
    match literal {
        Literal::Integer(digits) => integer_text(digits, negative),
        Literal::Decimal(text) => decimal_text(text, negative),
        Literal::Float(text) => float_text(text, negative),
        Literal::Money(text) => money_text(text, negative),
        _ if negative => None,
        Literal::Str { value, unicode } => Some(format!(
            "{}'{}'",
            if *unicode { "N" } else { "" },
            value.replace('\'', "''")
        )),
        Literal::Binary(hex) => Some(format!("0x{}", hex.to_ascii_uppercase())),
        Literal::Null => Some("NULL".to_owned()),
        Literal::Default => None,
    }
}

/// The digits of `digits` without their leading zeros, `0` for zero.
fn without_leading_zeros(digits: &str) -> &str {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// `(7)` for `007`, `(-2147483648)`, and `(2147483648.)` for an integer `int` does not hold,
/// which SQL Server reads as a `numeric`.
fn integer_text(digits: &str, negative: bool) -> Option<String> {
    let digits = without_leading_zeros(digits);
    if digits.len() > 38 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: u128 = digits.parse().ok()?;
    let sign = if negative && value != 0 { "-" } else { "" };
    let limit = if negative {
        2_147_483_648
    } else {
        2_147_483_647
    };
    let point = if value > limit { "." } else { "" };
    Some(format!("({sign}{digits}{point})"))
}

/// `(0.5)` for `.5` and for `00.5`, `(1.)` for `1.`, `(1.50)` for `1.50`.
fn decimal_text(text: &str, negative: bool) -> Option<String> {
    let (whole, fraction) = text.split_once('.')?;
    let sign = if negative { "-" } else { "" };
    Some(format!(
        "({sign}{}.{fraction})",
        without_leading_zeros(whole)
    ))
}

/// `(1.0000000000000000e+003)` for `1e3`: sixteen decimals, a signed exponent of three
/// digits.
fn float_text(text: &str, negative: bool) -> Option<String> {
    let value: f64 = text.parse().ok()?;
    let value = if negative { -value } else { value };
    let written = format!("{value:.16e}");
    let (mantissa, exponent) = written.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let sign = if exponent < 0 { '-' } else { '+' };
    Some(format!("({mantissa}e{sign}{:03})", exponent.unsigned_abs()))
}

/// `($5.0000)` for `$5`, `($-5.2500)` for `$-5.25`, `($1.2346)` for `$1.23456`: four
/// decimals, the fifth rounding away from zero.
fn money_text(text: &str, negative: bool) -> Option<String> {
    let (written_negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
        || whole.len() > 30
    {
        return None;
    }
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut kept = String::from(fraction);
    while kept.len() < 5 {
        kept.push('0');
    }
    let fourths: u128 = kept[..4].parse().ok()?;
    let round_up = kept.as_bytes()[4] >= b'5';
    let scaled = whole * 10_000 + fourths + u128::from(round_up);
    let sign = if (negative != written_negative) && scaled != 0 {
        "-"
    } else {
        ""
    };
    Some(format!(
        "(${sign}{}.{:04})",
        scaled / 10_000,
        scaled % 10_000
    ))
}

#[cfg(test)]
mod tests;

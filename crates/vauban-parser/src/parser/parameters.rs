//! Reading the parameter declaration string of `sp_executesql` and `sp_prepare`.
//!
//! The `@params` argument of `sp_executesql N'SELECT 1', N'@a int, @b nvarchar(50) OUTPUT'`
//! declares the parameters the dynamic SQL references. The grammar is a subset of the
//! one of `CREATE PROCEDURE`: a comma-separated list of `@name [AS] type [OUT | OUTPUT]`.
//!
//! The empty string is accepted and yields an empty list.
//!
//! # Error numbers of SQL Server
//!
//! SQL Server 2022 answers the following outcomes on `EXEC sp_executesql N'SELECT 1', N'<string>'`:
//!
//! | case | outcome | number | severity | state |
//! |---|---|---|---|---|
//! | `N''` (empty) | success | -- | -- | -- |
//! | `N'@a int'` | success (8178: parameter not supplied, not a parse error) | -- | -- | -- |
//! | `N'@a int, @b nvarchar(50) OUTPUT'` | success (8178: parameter not supplied) | -- | -- | -- |
//! | `N'a int'` (no `@`) | error | 102 | 15 | 1 |
//! | `N'@a'` (no type) | error | 102 | 15 | 1 |
//! | `N'@a int,'` (trailing comma) | error | 102 | 15 | 1 |
//! | `N'@a int, @a int'` (duplicate name) | error 134 at execution (not a parse error) | 134 | 15 | 1 |
//! | `N'@a int = 1'` (default value) | accepted | -- | -- | -- |
//! | `N'@a int READONLY'` | error 346 (semantic: not a table-valued param) | 346 | 15 | 1 |
//! | `N'@a foo'` (unknown type) | error 2715 (binder: type not found) | 2715 | 16 | 3 |

use vauban_errors::SqlResult;

use crate::ast::expr::DataType;
use crate::keyword::Keyword;
use crate::parser::{ParseOptions, Parser};
use crate::span::Span;
use crate::token::{Op, Punct, TokenKind};

/// One parameter declared in the `@params` string of `sp_executesql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterDeclaration {
    /// The parameter name, `@` included.
    pub name: String,
    /// The declared data type.
    pub ty: DataType,
    /// True for `OUTPUT` (or `OUT`).
    pub output: bool,
    /// Position of the whole declaration.
    pub span: Span,
}

/// Reads a parameter declaration string (`@params`) as a list of [`ParameterDeclaration`].
///
/// The grammar is:
///
/// ```text
/// parameter_list ::= /* empty */ | parameter { ',' parameter }
/// parameter       ::= '@' name [ AS ] type [ OUT | OUTPUT ] [ '=' expr ] [ READONLY ]
/// ```
///
/// A type is read by [`parse_data_type`], which handles `int`, `nvarchar(50)`,
/// `nvarchar(max)`, `decimal(10,2)`, `varbinary(16)`, `datetime2(3)`, etc.
///
/// SQL Server accepts default values (`= expr`) and `READONLY` in the parameter string,
/// but raises semantic errors later (134 for duplicate name, 346 for READONLY on
/// a non-table-valued parameter, 2715 for an unknown type). This parser accepts them
/// syntactically.
///
/// # Errors
///
/// - Syntax error 102 / 156 on a name without `@`, a missing type, or a trailing comma.
pub fn parse_parameter_declarations(
    text: &str,
    opts: &ParseOptions,
) -> SqlResult<Vec<ParameterDeclaration>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut p = Parser::new(text, opts)?;
    let mut params = Vec::new();
    loop {
        params.push(parse_one_parameter(&mut p)?);
        if p.at_eof() {
            break;
        }
        if !p.eat_punct(Punct::Comma) {
            break;
        }
        if p.at_eof() {
            return Err(p.error_here());
        }
    }
    if !p.at_eof() {
        return Err(p.error_here());
    }
    Ok(params)
}

/// Reads one `@name [AS] type [OUT | OUTPUT] [= expr] [READONLY]`.
fn parse_one_parameter(p: &mut Parser) -> SqlResult<ParameterDeclaration> {
    let start = p.mark();

    // Expect `@name` (a variable token).
    let name = parse_variable_name(p)?;

    // Optional `AS` before the type, like in CREATE PROCEDURE.
    let _ = p.eat_keyword(Keyword::As);

    // Read the type.
    let ty = crate::parser::datatype::parse_data_type(p)?;

    // Optional `OUTPUT` or `OUT`.
    let output = p.eat_keyword(Keyword::Output) || p.eat_keyword(Keyword::Out);

    // Optional `= expr` (default value). Skip the expression tokens until `,` or EOF.
    if matches!(p.peek().kind, TokenKind::Op(Op::Eq)) {
        skip_until_comma_or_end(p);
    }

    // Optional `READONLY` (checked semantically by SQL Server: 346).
    let _ = p.eat_keyword(Keyword::ReadOnly);

    Ok(ParameterDeclaration {
        name,
        ty,
        output,
        span: p.span_from(start),
    })
}

/// Skips tokens until the next `,` or the end of the input.
///
/// Used to swallow the default value expression (`= 1`, `= NULL`, `= 'abc'`, etc.)
/// without parsing it. The parser tracks nesting of `(` and `)` so that a default
/// such as `= (SELECT 1)` is skipped whole.
fn skip_until_comma_or_end(p: &mut Parser) {
    let mut depth: u32 = 0;
    loop {
        if p.at_eof() {
            return;
        }
        if depth == 0 && p.at_punct(Punct::Comma) {
            return;
        }
        let token = p.advance();
        match token.kind {
            TokenKind::Punct(Punct::LeftParen) => depth += 1,
            TokenKind::Punct(Punct::RightParen) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
}

/// Reads a variable name: `@` followed by an identifier.
///
/// # Errors
///
/// Error 102 when the token is not a variable name.
fn parse_variable_name(p: &mut Parser) -> SqlResult<String> {
    let text = p.peek().text.clone();
    if text.starts_with('@') && text.len() > 1 {
        p.advance();
        Ok(text)
    } else {
        Err(p.error_here())
    }
}

#[cfg(test)]
mod tests;

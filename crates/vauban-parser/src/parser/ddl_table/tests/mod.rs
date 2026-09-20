use super::parse_table_definition;
use crate::parser::{ParseOptions, Parser};

/// Builds a cursor over `text` with the default options.
fn cursor(text: &str) -> Parser<'static> {
    static OPTIONS: ParseOptions = ParseOptions {
        quoted_identifier: true,
    };
    match Parser::new(text, &OPTIONS) {
        Ok(parser) => parser,
        Err(error) => unreachable!("the test text lexes: {error:?}"),
    }
}

/// The entry point `flow.rs` needs for `DECLARE @t TABLE (…)`: called on the `(`, it
/// gives the cursor back after the matching `)`, with no statement around it.
#[test]
fn table_definition_is_reusable() {
    let text = "(a int NOT NULL, PRIMARY KEY (a))";
    let mut p = cursor(text);
    let definition = match parse_table_definition(&mut p) {
        Ok(definition) => definition,
        Err(error) => unreachable!("{text} should parse: {error:?}"),
    };
    assert_eq!(definition.columns.len(), 1);
    assert_eq!(definition.columns[0].name.value, "a");
    assert_eq!(definition.constraints.len(), 1);
    // Both parentheses are consumed: nothing is left for the caller.
    assert!(p.at_eof(), "{text} left {:?} unread", p.peek());
}

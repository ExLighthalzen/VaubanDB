use super::{BindContext, NoVariables, SessionOptions, VariableScope};

#[test]
fn no_variables_knows_no_variable() {
    assert!(NoVariables.type_of("@x").is_none());
    assert!(NoVariables.type_of("@@rowcount").is_none());
}

#[test]
fn scalar_context_keeps_the_text_it_was_given() {
    let ctx = BindContext::scalar("SELECT 1", SessionOptions::default());
    assert_eq!(ctx.text, "SELECT 1");
    assert!(ctx.catalog.is_none());
}

use super::{DEPTH, MAX_BIND_DEPTH, STACK_LIMIT_NUMBER, at_statement, enter};
use vauban_errors::SqlError;

#[test]
fn a_guard_leaves_the_level_it_entered() {
    assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
    {
        let _outer = enter().expect("one level fits");
        assert_eq!(DEPTH.with(std::cell::Cell::get), 1);
        let _inner = enter().expect("two levels fit");
        assert_eq!(DEPTH.with(std::cell::Cell::get), 2);
    }
    assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
}

#[test]
fn the_last_level_that_fits_is_the_limit_itself() {
    let mut guards = Vec::new();
    for level in 1..=MAX_BIND_DEPTH {
        guards.push(enter().unwrap_or_else(|_| panic!("level {level} is within {MAX_BIND_DEPTH}")));
    }
    let refused = enter().expect_err("one level past the limit is refused");
    assert_eq!(refused.number, STACK_LIMIT_NUMBER);
    assert_eq!(refused.severity, 17);
    assert_eq!(refused.state, 1);
    assert_eq!(refused.line, 0, "the line is the statement's, added later");
    assert!(
        refused
            .message
            .starts_with("Internal error: the server ran out of stack"),
        "{}",
        refused.message
    );
    drop(guards);
    assert_eq!(DEPTH.with(std::cell::Cell::get), 0);
}

#[test]
fn a_refused_level_does_not_count_itself() {
    let mut guards = Vec::new();
    for _ in 1..=MAX_BIND_DEPTH {
        guards.push(enter().expect("within the limit"));
    }
    // Ten refusals in a row must not push the counter past the limit for good.
    for _ in 0..10 {
        assert!(enter().is_err());
    }
    assert_eq!(DEPTH.with(std::cell::Cell::get), MAX_BIND_DEPTH);
    drop(guards);
    assert!(enter().is_ok(), "the next statement binds normally");
}

#[test]
fn only_the_depth_error_takes_the_statement_line() {
    let deep = at_statement(SqlError::stack_limit_reached(), 7);
    assert_eq!(deep.line, 7);
    let already_placed = at_statement(SqlError::stack_limit_reached().with_line(3), 7);
    assert_eq!(already_placed.line, 3, "a line already set is kept");
    let other = at_statement(SqlError::new(207, 16, 1, "Unknown column name 'c'."), 7);
    assert_eq!(other.line, 0, "another error keeps the line of its node");
}

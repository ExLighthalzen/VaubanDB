//! A flat text that builds a deep tree must not take the process down.
//!
//! # The two halves this file proves
//!
//! A test that only looked at the error would pass on a binary that dies **cleanly**: the
//! process would be gone all the same, and with it every other connection of the server.
//! So the deep chains are bound in a **child process**, and each test asserts both halves:
//!
//! 1. the child exits normally — no signal, no `stack overflow, aborting`;
//! 2. it printed the error 8631 the guard raises.
//!
//! Without the guard the child dies of `SIGABRT` and the parent test *fails* with the
//! signal in its message, instead of taking the whole test run down with it.
//!
//! # Why a thread of exactly 16 MiB
//!
//! The narrowest stack the server has is the one of a thread of the blocking pool, where
//! `session` runs a request: `cli` builds its runtime with
//! `thread_stack_size(REQUEST_THREAD_STACK_SIZE)`, 16 MiB. The child binds on a thread of
//! exactly that size, so that the test exercises the stack the server really has and not
//! the 8 MiB of a main thread.
//!
//! # The shapes
//!
//! Four chains that are **flat for the parser** — its Pratt loop spends no unit of its
//! depth counter on them — and comb-shaped for the binder: `+` on integers, `+` on
//! strings (a concatenation, another operator), `AND` and `OR` (predicates, a third path).
//! The flat *lists* — `IN`, the arms of a `CASE` — are checked to still bind at sizes far
//! past the limit: they are wide, not deep, and the guard must not confuse the two.

use std::process::Command;

use vauban_binder::{BindContext, SessionOptions, bind};
use vauban_errors::SqlError;
use vauban_parser::{ParseOptions, parse_batch};

/// Stack of a thread of the blocking pool of `session`, the narrowest of the server.
///
/// `vauban_session::REQUEST_THREAD_STACK_SIZE` holds the same number; it is spelled out
/// here so that a change of that constant shows up as a failure of this file rather than
/// as a test that quietly follows it.
const SERVER_THREAD_STACK: usize = 16 * 1024 * 1024;

/// Deepest tree the binder accepts, as `depth.rs` fixes it.
///
/// Written out rather than imported: `MAX_BIND_DEPTH` is `pub(crate)`, and a test that
/// spelled the same expression as the code could not notice a change of the limit.
const MAX_BIND_DEPTH: usize = 830;

/// Number of terms of the chains the child binds: far past the limit, and past the depth at
/// which the binder overflows without it.
///
/// That second half is what gives the file its meaning: on a 16 MiB thread with the guard
/// lifted, binding alone gives way at 2782 nodes in debug and at 13 806 in release, so a
/// chain shorter than the release figure would pass with or without the guard.
const DEEP: usize = 20_000;

/// Environment variable that turns the test binary into the child that binds one chain.
const CHILD_ENV: &str = "VAUBAN_BINDER_NESTING_CHILD";

/// The name of the child test, as the parent passes it to `--exact`.
const CHILD_TEST: &str = "child_binds_one_deep_chain";

/// A batch whose text is flat and whose tree is `n` nodes deep.
fn source(form: &str, n: usize) -> String {
    match form {
        "add" => format!("SELECT {};", vec!["1"; n].join(" + ")),
        "concat" => format!("SELECT {};", vec!["'a'"; n].join(" + ")),
        "and" => format!("SELECT 1 WHERE {};", vec!["1 = 1"; n].join(" AND ")),
        "or" => format!("SELECT 1 WHERE {};", vec!["1 = 1"; n].join(" OR ")),
        other => panic!("unknown shape {other}"),
    }
}

/// Parses and binds `text`, and gives back the error the binder raised, if any.
fn bind_text(text: &str) -> Result<(), SqlError> {
    let batch = parse_batch(text, &ParseOptions::default())?;
    let ctx = BindContext::scalar(text, SessionOptions::default());
    for statement in &batch.statements {
        bind(statement, &ctx)?;
    }
    Ok(())
}

/// Parses and binds `text` on a thread of [`SERVER_THREAD_STACK`], as a request is bound.
///
/// The threads `libtest` runs a test on are sized by `RUST_MIN_STACK` and by nothing this
/// file controls: a tree of [`MAX_BIND_DEPTH`] nodes can overflow one of them in a debug
/// build while the guard is doing its job, which says something about `libtest` and
/// nothing about the server. The tests here that bind a tree deeper than a handful of
/// nodes go through this function.
fn bind_on_server_stack(text: String) -> Result<(), SqlError> {
    std::thread::Builder::new()
        .stack_size(SERVER_THREAD_STACK)
        .spawn(move || bind_text(&text))
        .expect("the test can spawn a binding thread")
        .join()
        .expect("the binding thread ran to its end")
}

/// The child: binds one chain on a thread with the server's narrowest stack and says what
/// came back. Does nothing at all when the variable is not set, which is the ordinary run.
#[test]
fn child_binds_one_deep_chain() {
    let Ok(form) = std::env::var(CHILD_ENV) else {
        return;
    };
    let outcome = std::thread::Builder::new()
        .stack_size(SERVER_THREAD_STACK)
        .spawn(move || {
            let deep = match bind_text(&source(&form, DEEP)) {
                Ok(()) => "bound".to_owned(),
                Err(err) => format!("{} {} {} {}", err.number, err.severity, err.state, err.line),
            };
            // The thread that refused the deep tree must still be able to bind: the
            // counter is left where it was found, error or not.
            let after = match bind_text("SELECT 1 + 1;") {
                Ok(()) => "alive".to_owned(),
                Err(err) => format!("dead {}", err.number),
            };
            format!("{deep} / {after}")
        })
        .expect("the child can spawn its binding thread")
        .join()
        .expect("the binding thread did not panic");
    println!("NESTING_RESULT {outcome}");
}

/// Runs the child on `form` and gives its standard output, or panics with what killed it.
fn run_child(form: &str) -> String {
    let exe = std::env::current_exe().expect("the test binary knows its own path");
    let output = Command::new(exe)
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .env(CHILD_ENV, form)
        .output()
        .expect("the child test binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "binding a chain of {DEEP} {form} terms did not leave the process alive: {status}\n\
         — a child killed by a signal is a stack overflow, i.e. the whole server gone, not \
         an error sent to one client.\nstdout: {stdout}\nstderr: {stderr}",
        status = output.status,
    );
    stdout
}

#[test]
fn a_deep_arithmetic_chain_is_refused_and_the_process_lives() {
    let stdout = run_child("add");
    assert!(
        stdout.contains("NESTING_RESULT 8631 17 1 1 / alive"),
        "expected 8631, severity 17, state 1, line 1, then a live binder; got: {stdout}"
    );
}

#[test]
fn a_deep_concatenation_is_refused_and_the_process_lives() {
    let stdout = run_child("concat");
    assert!(
        stdout.contains("NESTING_RESULT 8631 17 1 1 / alive"),
        "expected 8631 then a live binder; got: {stdout}"
    );
}

#[test]
fn a_deep_and_chain_is_refused_and_the_process_lives() {
    let stdout = run_child("and");
    assert!(
        stdout.contains("NESTING_RESULT 8631 17 1 1 / alive"),
        "expected 8631 then a live binder; got: {stdout}"
    );
}

#[test]
fn a_deep_or_chain_is_refused_and_the_process_lives() {
    let stdout = run_child("or");
    assert!(
        stdout.contains("NESTING_RESULT 8631 17 1 1 / alive"),
        "expected 8631 then a live binder; got: {stdout}"
    );
}

#[test]
fn the_deepest_accepted_chain_binds_and_the_next_one_does_not() {
    // A chain of n terms is a tree of n nodes: n - 1 operators over one literal.
    bind_on_server_stack(source("add", MAX_BIND_DEPTH)).expect("a tree of exactly the limit binds");
    let refused = bind_on_server_stack(source("add", MAX_BIND_DEPTH + 1))
        .expect_err("one node more is refused");
    assert_eq!(refused.number, 8631);
    assert_eq!(refused.severity, 17);
    assert_eq!(refused.state, 1);
    assert_eq!(
        refused.message,
        SqlError::stack_limit_reached().message,
        "the text of the catalogue for 8631"
    );
}

#[test]
fn the_line_is_the_one_of_the_statement_not_of_the_node() {
    // SQL Server answers the line of the `SELECT` on a chain spread over as many lines as
    // it has terms, and on a chain written in a `WHERE` further down.
    let spread = format!("\nSELECT\n{}\n1;", "1 +\n".repeat(MAX_BIND_DEPTH + 10));
    let refused = bind_on_server_stack(spread).expect_err("the chain is too deep");
    assert_eq!(refused.number, 8631);
    assert_eq!(refused.line, 2, "the line of the SELECT, not of the term");

    let in_where = format!(
        "\n\nSELECT 1\nWHERE {} = 1;",
        vec!["1"; MAX_BIND_DEPTH + 10].join(" + ")
    );
    let refused = bind_on_server_stack(in_where).expect_err("the chain is too deep");
    assert_eq!(refused.line, 3, "the line of the SELECT, not of the WHERE");
}

#[test]
fn a_wide_list_is_not_a_deep_tree() {
    // `IN` lists, `CASE` arms and long argument lists are flat in the bound plan: the
    // guard bounds the depth of the tree, not the size of the text.
    let items = vec!["1"; 5_000].join(", ");
    bind_on_server_stack(format!("SELECT 1 WHERE 1 IN ({items});")).expect("a wide IN list binds");
    let arms = vec!["WHEN 1 = 1 THEN 1"; 5_000].join(" ");
    bind_on_server_stack(format!("SELECT CASE {arms} ELSE 0 END;")).expect("a wide CASE binds");
    let conditions = vec!["1 = 1"; MAX_BIND_DEPTH / 2].join(" AND ");
    bind_on_server_stack(format!("SELECT 1 WHERE {conditions};")).expect("a short AND chain binds");
}

#[test]
fn the_batch_goes_on_after_a_refused_statement() {
    // Two statements in one batch: the deep one is refused, and the counter it spent is
    // given back, so the next batch on the same thread binds normally.
    let deep = source("add", MAX_BIND_DEPTH + 1);
    assert_eq!(
        bind_on_server_stack(deep.clone())
            .expect_err("too deep")
            .number,
        8631,
        "the first statement is refused"
    );
    bind_text("SELECT 1 + 1 + 1;").expect("the next statement binds");
    assert_eq!(
        bind_on_server_stack(deep).expect_err("too deep").number,
        8631,
        "and the limit is still the same one"
    );
}

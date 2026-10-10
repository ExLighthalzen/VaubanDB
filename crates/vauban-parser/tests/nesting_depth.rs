//! The nesting guard: an over-nested batch is an **error**, never an abort.
//!
//! # What this file is really testing
//!
//! A stack overflow in Rust is not an error a caller can catch: the runtime prints
//! `fatal runtime error: stack overflow` and aborts the **process**. On a server that
//! means every connection dies, not just the one that sent the batch, so
//! `SELECT ((((…))))` was a remote denial of service that needed no login.
//!
//! Every test below therefore proves two things at once, and the second one is the one
//! that matters: an error came back, **and** this process is still running to read it. A
//! test that only looked at the error would pass just as well on a binary that dies
//! tidily, since a dead process fails nothing -- it fails everything.
//!
//! # Why the parse runs on a thread of its own
//!
//! The stack a batch is parsed on in production is a tokio one -- `session` hands the
//! request to `spawn_blocking`, and `cli` builds the runtime with
//! `thread_stack_size(REQUEST_THREAD_STACK_SIZE)`, so the workers and the blocking pool get
//! the 16 MiB `cli` sets (tokio's default is 2 MiB). A test
//! binary runs on nothing of the sort -- `ulimit -s` gives its
//! main thread 8 MiB here, and the threads `libtest` spawns per test are sized by
//! `RUST_MIN_STACK`, which whoever runs the suite may set -- so a guard proved on them
//! would say nothing about the server. Each test here spawns a thread of exactly
//! [`REFERENCE_STACK`] bytes and parses on that.
//!
//! On a 2 MiB stack, in a debug build, on the parser as it stood **before** the guard:
//! 143 nested parentheses, 135 nested calls, 112 nested `CASE`s and 49 nested scalar
//! subqueries were enough to abort. The same batch sent to a `vauban serve` built without
//! the guard killed the server with `SIGABRT`; the depths below are far past all of them,
//! and past what 16 MiB carry -- bisected, a derived table takes a debug server down at
//! 440 levels and a release one at 1782.
//!
//! # Why the derived table and `APPLY` are in this file
//!
//! An earlier revision listed five shapes and missed the one that costs the most
//! **stack**: a derived table. Counting units of the guard, a scalar subquery is the
//! dearest shape; counting *frames per unit*, the derived table is, because one unit buys
//! the whole uncounted chain query body -> set-operator term -> query spec -> `FROM` ->
//! join tree -> table primary -> derived table -> subquery -> select -> query body.
//! Bisected in a debug build, the deepest derived table the guard lets
//! through (216) aborts on an 8080 KiB stack and passes on 8081 KiB, against 4641 KiB for
//! the deepest subquery (108) -- so the margin on the 16 MiB reference is **2.03**, where
//! the shapes that carry a bracket alone would suggest 3.5. Enumerating shapes is only
//! worth something if the enumeration holds the dearest one.

//! # Why the prefix operators are in this file
//!
//! The first revision enumerated the shapes that carry a bracket -- parentheses, calls,
//! subqueries, `CASE`, `BEGIN…END`, derived tables, `APPLY` -- and never wrote a chain of
//! `NOT`, `-`, `+` or `~`. An axis that is not crossed is an axis that is not tried, and
//! this one could be read as being outside the guard: a debug server without the guard
//! dies between 240 and 250 `NOT`. On the parser this file tests, a prefix operator costs
//! one unit of the guard exactly like a parenthesis, and 20 000 of them come back as a 191.
//! The tests below are what makes that a fact rather than a reading of the source.
//!

//! # Why the flat chains are in this file
//!
//! The guard counts re-entries into a rule, and a chain the parser reads in a **loop**
//! re-enters nothing: such a batch goes through at any length tried here, up to 400 000
//! links, and that is the design of the guard rather than an oversight. What SQL Server's
//! own limit on a flat chain is, the shapes below say nothing about. What used to happen
//! next is that **freeing** the tree
//! recursed once per link and took the process down with it, after the parse had succeeded.
//!
//! Bisected on a 2 MiB stack, with the recursive destructor, each shape of [`combs`] was
//! freed at 18 807 links and aborted at 18 808 in a **debug** build -- 18 808 and 18 809
//! for the logical ladder, whose links alternate two operators. In **release** the limit
//! depends on the shape: 32 942 for the five expression and set-operator ladders (32 941
//! for the logical one), 26 353 for the two chains of table references. The same batches
//! parse at 400 000 links when the tree is handed to `std::mem::forget` instead of being
//! freed (tried in debug), which is what places the abort in the destructor rather than
//! in the parse. Fifty thousand `+ 1` are some 200 KiB of text, well within one TDS batch.
//!
//! Two shapes were tried and are **not** here, both because they are green without the
//! iterative destructor and an axis that is already green adds nothing: a batch of 200 000 `PRINT 1`
//! (breadth, which a `Vec` frees in a loop already) and `IF … ELSE IF …` 200 000 deep,
//! which the `IF` rule counts and refuses with 191 at its 254th `ELSE`
//! ([`else_if_chain_stops_at_its_own_limit`]).

use std::thread;

use vauban_errors::SqlError;
use vauban_parser::{
    ApplyKind, Batch, BinaryOp, Expr, JoinKind, ParseOptions, QueryBody, QuerySpec, SelectItem,
    SelectStatement, SetOp, Statement, TableRef, parse_batch,
};

/// The stack the parse is tried on: the one `cli` gives the runtime through
/// `thread_stack_size`, which its workers and its blocking pool both get.
///
/// `vauban_session::REQUEST_THREAD_STACK_SIZE` holds the same number; widening
/// this constant would weaken every test of the file, which is the point of spelling out
/// where the number comes from.
const REFERENCE_STACK: usize = 16 * 1024 * 1024;

/// Error 191, the one SQL Server answers past its own nesting limit.
const NESTED_TOO_DEEPLY: u32 = 191;

/// Parses `text` on a thread whose stack is [`REFERENCE_STACK`] bytes, and hands back
/// what the parser answered.
///
/// The thread is joined, so a parse that overflowed would take this process down with it
/// before the caller ever saw a value: reaching the `assert` **is** the survival test.
fn parse_on_reference_stack(text: String) -> Result<Batch, SqlError> {
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || parse_batch(&text, &ParseOptions::default()))
        .expect("the test can spawn a thread");
    worker.join().expect("the parse thread ran to its end")
}

/// The error of a batch that must be refused for its nesting, parsed on the reference
/// stack.
fn refused(text: String) -> SqlError {
    match parse_on_reference_stack(text) {
        Ok(batch) => unreachable!("the batch should have been refused, got {batch:?}"),
        Err(error) => error,
    }
}

/// Asserts that `error` is the 191 SQL Server sends, whole: number, severity and state.
fn assert_is_191(error: &SqlError) {
    assert_eq!(error.number, NESTED_TOO_DEEPLY, "{error:?}");
    assert_eq!(error.severity, 15, "{error:?}");
    assert_eq!(error.state, 1, "{error:?}");
    assert_eq!(
        error.message, "The statement is nested too deeply; split it into smaller queries.",
        "{error:?}"
    );
}

/// The batch of the report that killed the server, ten times deeper.
#[test]
fn deep_parentheses_are_an_error_and_not_an_abort() {
    let text = format!("SELECT {}1{}", "(".repeat(2000), ")".repeat(2000));
    assert_is_191(&refused(text));
}

/// The dearest shape of all: before the guard, 49 levels were enough to abort.
#[test]
fn deep_subqueries_are_an_error_and_not_an_abort() {
    let text = format!("SELECT {}1{}", "(SELECT ".repeat(2000), ")".repeat(2000));
    assert_is_191(&refused(text));
}

/// Nesting without a single parenthesis of its own: the guard counts rules, not tokens.
#[test]
fn deep_calls_are_an_error_and_not_an_abort() {
    let text = format!("SELECT {}1{}", "ABS(".repeat(2000), ")".repeat(2000));
    assert_is_191(&refused(text));
}

/// A shape that never goes through the expression rule at all: statements inside
/// statements, which `BEGIN…END` nests and which the statement dispatch counts.
#[test]
fn deep_blocks_are_an_error_and_not_an_abort() {
    let text = format!("{}PRINT 1 {}", "BEGIN ".repeat(2000), "END ".repeat(2000));
    assert_is_191(&refused(text));
}

/// A `CASE` inside a `CASE` inside a `CASE`.
///
/// This shape is the one the general guard does not get to see: SQL Server refuses the
/// eleventh nested `CASE` with **125** (severity 15, state 4 for a searched one), and so
/// does VaubanDB.
/// The 2000 levels are kept: what the test proves is still that the batch comes back as
/// an error on the reference stack, not as an abort.
#[test]
fn deep_case_expressions_are_an_error_and_not_an_abort() {
    let mut expr = String::from("1");
    for _ in 0..2000 {
        expr = format!("CASE WHEN 1 = 1 THEN {expr} ELSE 0 END");
    }
    let error = refused(format!("SELECT {expr}"));
    assert_eq!(
        (error.number, error.severity, error.state),
        (125, 15, 4),
        "{error:?}"
    );
    assert_eq!(
        error.message, "A CASE expression cannot be nested deeper than level 10.",
        "{error:?}"
    );
}

/// Builds `SELECT * FROM (SELECT * FROM (…(SELECT 1 AS a) AS t0…) AS t1) AS tn`, the
/// shape that costs the most stack per unit of the guard.
fn derived_tables(levels: usize) -> String {
    let mut source = String::from("(SELECT 1 AS a) AS t0");
    for level in 1..=levels {
        source = format!("(SELECT * FROM {source}) AS t{level}");
    }
    format!("SELECT * FROM {source}")
}

/// The same chain, reached through an `APPLY` rather than through a plain `FROM`.
///
/// The column names are made distinct on purpose. It changes nothing here -- the parser
/// resolves no name -- but it keeps this vector usable end to end: with two
/// columns named alike, SQL Server answers 8156 long before its nesting limit, and one
/// then exercises its name resolution instead of its stack.
fn applies(kind: &str, levels: usize) -> String {
    let mut source = String::from("(SELECT 1 AS c0) AS t0");
    for level in 1..=levels {
        source = format!(
            "(SELECT * FROM (SELECT 1 AS d{level}) AS s{level} {kind} APPLY {source}) AS t{level}"
        );
    }
    format!("SELECT * FROM {source}")
}

/// The shape the first revision of this file missed: the dearest one in stack, by far.
#[test]
fn deep_derived_tables_are_an_error_and_not_an_abort() {
    assert_is_191(&refused(derived_tables(2000)));
}

/// `CROSS APPLY` walks the same uncounted chain and costs the same.
#[test]
fn deep_cross_applies_are_an_error_and_not_an_abort() {
    assert_is_191(&refused(applies("CROSS", 2000)));
}

/// `OUTER APPLY` too: the guard hangs on the rule, not on the keyword.
#[test]
fn deep_outer_applies_are_an_error_and_not_an_abort() {
    assert_is_191(&refused(applies("OUTER", 2000)));
}

/// A derived table reached across a set operator, the longest uncounted chain of all.
#[test]
fn deep_derived_tables_under_a_set_operator_are_an_error_and_not_an_abort() {
    let mut source = String::from("(SELECT 1 AS a) AS t0");
    for level in 1..=2000 {
        source = format!("(SELECT * FROM {source} INTERSECT SELECT 1) AS t{level}");
    }
    assert_is_191(&refused(format!("SELECT * FROM {source}")));
}

/// The four prefix operators of T-SQL, spelled as a client writes them.
///
/// They are here because the guard counts **rules**, and one could read that as counting
/// only the shapes that carry a bracket, as if a chain of prefix operators went through
/// uncounted. It does not -- `parse_prefix` reads its
/// operand through the one counted door of the expression grammar, so each of these words
/// costs exactly one unit, the same as a parenthesis -- and the tests below are what says
/// so from outside the crate.
const PREFIX_OPERATORS: [&str; 4] = ["NOT", "-", "+", "~"];

/// `levels` copies of `op`, ready to be put in front of an operand.
fn prefix_chain(op: &str, levels: usize) -> String {
    format!("{op} ").repeat(levels)
}

/// The positions where a **predicate** is read, each with `{}` where the chain goes.
///
/// All four operators are legal here, `NOT` included: these are the positions that read a
/// search condition (`parse_expr`), where rank 5 is within the binding power.
const PREDICATE_POSITIONS: [(&str, &str); 4] = [
    ("WHERE", "SELECT 1 WHERE {} 1 = 1"),
    ("CASE WHEN", "SELECT CASE WHEN {} 1 = 1 THEN 1 ELSE 0 END"),
    ("IF", "IF {} 1 = 1 PRINT 1"),
    ("HAVING", "SELECT 1 AS a HAVING {} 1 = 1"),
];

/// The positions where a **value** is read, each with `{}` where the chain goes.
///
/// `NOT` is rank 5 and is refused outright in all of them, which is its own test below;
/// `-`, `+` and `~` are legal.
const VALUE_POSITIONS: [(&str, &str); 4] = [
    ("select list", "SELECT {} 1"),
    ("call argument", "SELECT ABS({} 1)"),
    ("CASE THEN", "SELECT CASE WHEN 1 = 1 THEN {} 1 ELSE 0 END"),
    ("IN list", "SELECT 1 WHERE 1 IN ({} 1)"),
];

/// The batch `template` writes with a chain of `levels` copies of `op` in its hole.
fn at_position(template: &str, op: &str, levels: usize) -> String {
    template.replace("{}", &prefix_chain(op, levels))
}

/// Asserts that the batch is refused by the nesting guard, naming the shape if it is not.
///
/// The shape is in the message because these tests cross four operators over eight
/// positions: a bare `assert_is_191` would say an error was wrong without saying which
/// batch produced it.
fn assert_shape_is_191(shape: &str, text: String) {
    let error = refused(text);
    assert_eq!(
        error.number, NESTED_TOO_DEEPLY,
        "{shape} was not refused for its nesting: {error:?}"
    );
    assert_is_191(&error);
}

/// **The prefix chain vector**, crossed over the four operators and four predicate
/// positions: five thousand prefix operators are an error, and this process is alive to
/// read it.
///
/// On the parser as it stood before the guard -- a tree with no `Parser::nested` at all
/// -- this test does not fail, it
/// **aborts**: bisected on the reference stack in a debug build, `SELECT 1 WHERE NOT ...
/// 1 = 1` parses at 249 levels and overflows at 250, and `SELECT - ... 1` at 250/251.
/// Five thousand is twenty times past that.
#[test]
fn deep_prefix_chains_in_a_predicate_are_an_error_and_not_an_abort() {
    for (position, template) in PREDICATE_POSITIONS {
        for op in PREFIX_OPERATORS {
            assert_shape_is_191(
                &format!("`{op}` in {position}"),
                at_position(template, op, 5000),
            );
        }
    }
}

/// The same chain in the positions that read a **value**, where `NOT` is not a word the
/// grammar takes.
#[test]
fn deep_prefix_chains_in_a_value_are_an_error_and_not_an_abort() {
    for (position, template) in VALUE_POSITIONS {
        for op in ["-", "+", "~"] {
            assert_shape_is_191(
                &format!("`{op}` in a {position}"),
                at_position(template, op, 5000),
            );
        }
    }
}

/// `NOT` in a value position is refused for being the wrong word, not for its depth --
/// and it is refused on the first one, so no chain of them ever reaches the guard.
///
/// The case is here so that the crossing above is not read as covering it: `SELECT NOT 1`
/// is a syntax error in T-SQL, and five thousand `NOT`s in a select list come back as the
/// same 156 as one.
#[test]
fn a_prefix_not_in_a_value_position_is_a_syntax_error_at_any_length() {
    for (position, template) in VALUE_POSITIONS {
        for levels in [1, 5000] {
            let error = refused(at_position(template, "NOT", levels));
            assert_eq!(
                error.number, 156,
                "{levels} `NOT` in a {position}: {error:?}"
            );
        }
    }
}

/// **One prefix operator costs one unit of the guard**, exactly like one parenthesis.
///
/// This is the claim that must not rot: written this way the test says nothing about the
/// value of the constant, only that
/// the three shapes reach the same depth. If a future revision of the expression ladder
/// consumed prefixes in a loop instead of re-entering the rule, the chain would run past
/// [`deepest_allowed`]'s ceiling and this test would **fail** -- not abort -- which is how
/// it behaves on the parser without the guard, where 200 levels still parse.
#[test]
fn one_prefix_operator_costs_one_unit_of_the_guard() {
    let parentheses = deepest_allowed(|n| format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n)));
    for op in ["-", "+", "~"] {
        let chain = deepest_allowed(|n| format!("SELECT {}1", prefix_chain(op, n)));
        assert_eq!(
            chain, parentheses,
            "`{op}` reaches {chain} levels where a parenthesis reaches {parentheses}: the \
             prefix operators are no longer counted like the other recursive shapes"
        );
    }
}

/// The half a test of the error alone would miss, on the prefix chain: the server answers
/// the next batch.
#[test]
fn the_parser_still_works_after_refusing_a_prefix_chain() {
    for op in PREFIX_OPERATORS {
        assert_shape_is_191(
            &format!("`{op}` in WHERE"),
            format!("SELECT 1 WHERE {}1 = 1", prefix_chain(op, 20_000)),
        );
        let batch = parse_on_reference_stack("SELECT 1".to_owned()).expect("an ordinary batch");
        assert_eq!(batch.to_string(), "SELECT 1");
    }
}

/// The deepest depth the guard lets a shape through, found by asking the guard rather
/// than by writing a number down.
///
/// Hard-coding the depth would make the test a copy of the constant; walking up to the
/// refusal makes it track whatever the constant becomes, which is the point of the test
/// that follows.
fn deepest_allowed(build: impl Fn(usize) -> String) -> usize {
    // Far past any depth 220 units of the guard could buy; reaching it means the guard
    // stopped guarding, and the assert below says so rather than looping for ever.
    const CEILING: usize = 400;
    for levels in 1..=CEILING {
        if parse_on_reference_stack(build(levels)).is_err() {
            return levels - 1;
        }
    }
    unreachable!("the guard let {CEILING} levels through: it is no longer guarding");
}

/// **The margin, held to.** The deepest batch the guard allows must fit the reference
/// stack -- and this is the test that fails if the constant is raised past what 2 MiB
/// can carry.
///
/// It is written on the derived table because that is the shape that binds: bisected in a
/// debug build, it aborts on an 8080 KiB stack and passes on 8081 KiB of the 16 384 KiB
/// available, a margin of 2.03. The eight other shapes below parse on 4641 KiB or less;
/// skipping this one would overstate the margin by a factor of nearly two.
///
/// "Fails" here means the process dies on the overflow, which is a red suite either way.
#[test]
fn the_deepest_batch_the_guard_allows_fits_the_reference_stack() {
    for (shape, build) in [
        (
            "derived table",
            Box::new(derived_tables) as Box<dyn Fn(usize) -> String>,
        ),
        ("CROSS APPLY", Box::new(|n| applies("CROSS", n))),
        ("OUTER APPLY", Box::new(|n| applies("OUTER", n))),
        (
            "parentheses",
            Box::new(|n| format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n))),
        ),
        (
            "calls",
            Box::new(|n| format!("SELECT {}1{}", "ABS(".repeat(n), ")".repeat(n))),
        ),
        (
            "subqueries",
            Box::new(|n| format!("SELECT {}1{}", "(SELECT ".repeat(n), ")".repeat(n))),
        ),
        (
            "prefix `-` in the select list",
            Box::new(|n| format!("SELECT {}1", prefix_chain("-", n))),
        ),
        (
            "prefix `NOT` in WHERE",
            Box::new(|n| format!("SELECT 1 WHERE {}1 = 1", prefix_chain("NOT", n))),
        ),
        (
            "prefix `~` in a call argument",
            Box::new(|n| format!("SELECT ABS({}1)", prefix_chain("~", n))),
        ),
    ] {
        let levels = deepest_allowed(&build);
        assert!(levels > 0, "{shape}: the guard refuses even one level");
        let parsed = parse_on_reference_stack(build(levels));
        assert!(
            parsed.is_ok(),
            "{shape}: {levels} levels are allowed but did not parse: {parsed:?}"
        );
    }
}

/// The line of the 191 is the line the nesting was written on, as SQL Server's is.
#[test]
fn the_error_carries_the_line_of_the_nesting() {
    let text = format!(
        "SELECT 1;\nSELECT {}1{}",
        "(".repeat(2000),
        ")".repeat(2000)
    );
    assert_eq!(refused(text).line, 2);
}

/// The half a test of the error alone would miss: the process outlives the refusal, and
/// the parser is in a fit state to serve the next batch.
///
/// A server that aborted on the first line would fail this test by never reaching the
/// second, which is exactly how the defect showed up over TDS.
#[test]
fn the_parser_still_works_after_refusing() {
    let deep = format!("SELECT {}1{}", "(".repeat(4000), ")".repeat(4000));
    assert_is_191(&refused(deep));
    let batch = parse_on_reference_stack("SELECT 1".to_owned()).expect("an ordinary batch");
    assert_eq!(batch.to_string(), "SELECT 1");
}

/// Nesting a client actually writes goes through untouched.
///
/// The guard is a ceiling on how deep one expression sits inside another, and these are
/// the depths real T-SQL reaches.
#[test]
fn ordinary_nesting_is_untouched() {
    for text in [
        "SELECT ((((((1))))))",
        "SELECT ABS(ABS(ABS(ABS(-1))))",
        "SELECT (SELECT (SELECT 1))",
        "SELECT CASE WHEN 1 = 1 THEN CASE WHEN 2 = 2 THEN 3 ELSE 4 END ELSE 5 END",
        "SELECT 1 WHERE EXISTS (SELECT 1 WHERE EXISTS (SELECT 1))",
        "IF 1 = 1 BEGIN IF 2 = 2 BEGIN WHILE 1 = 1 BEGIN PRINT 1 END END END",
        "SELECT * FROM (SELECT * FROM (SELECT 1 AS a) AS i) AS o",
    ] {
        let parsed = parse_on_reference_stack(text.to_owned());
        assert!(parsed.is_ok(), "{text} should parse, got {parsed:?}");
    }
}

/// Breadth is not depth: a thousand expressions **side by side** nest no deeper than one.
///
/// A counter incremented on the way in and forgotten on the way out would refuse this
/// batch; the guard's is decremented whether the rule succeeded or failed.
#[test]
fn width_is_not_depth() {
    let items = vec!["(1 + 1)"; 1000].join(", ");
    let parsed = parse_on_reference_stack(format!("SELECT {items}"));
    assert!(parsed.is_ok(), "a wide batch should parse, got {parsed:?}");
}

/// Backtracking does not leak levels either.
///
/// The grammar tries an alternative and falls back all the time -- a bare alias, a table
/// hint, an `APPLY` that turns out to be a `CROSS JOIN` -- and each attempt walks into
/// and out of the counted rules. A leak would show up as a 191 on a batch that is barely
/// nested at all, which is what the long list of tries below would trigger.
#[test]
fn backtracking_does_not_leak_levels() {
    let ors = vec!["a = 1"; 500].join(" OR ");
    let text = format!("SELECT t.a FROM (SELECT 1 AS a) AS t WHERE {ors}");
    let parsed = parse_on_reference_stack(text);
    assert!(parsed.is_ok(), "backtracking leaked a level: {parsed:?}");
}

// ---------------------------------------------------------------------------------------
// The chains the guard lets through, and the destructor that used to abort on them. See
// the section of the file header for what was tried where.
// ---------------------------------------------------------------------------------------

/// The number of links every comb of [`combs`] is built with.
///
/// Past the deepest chain the destructor of `main` could free on the reference stack in
/// **both** profiles: 2.7 times the debug limit of 18 808, and 1.5 times the highest
/// release one, 32 942. A test written at either limit would pin that number rather than
/// the property; this one asks whether the destructor recurses at all, and a destructor
/// that recursed would need half again as much stack as the deepest it ever managed.
///
/// It is not larger because the **parse** of such a chain costs more than linear time: on
/// the infix ladder, in debug, parsing 50 000 links and forgetting the tree took 0.40 s,
/// 100 000 took 1.43 s and 200 000 took 5.56 s, so each doubling roughly quadruples it.
/// Where that cost sits is no business of this file; it is why the
/// six shapes here are written at fifty thousand and not at two hundred thousand.
const COMB_LEVELS: usize = 50_000;

/// A shape whose links the parser reads in a loop: its name, how to write `n` of them, and
/// how to count them back **iteratively** from the tree.
///
/// The counter is what keeps the test honest: `is_ok()` alone would pass just as well on a
/// parser that silently stopped reading at the thousandth link.
///
/// It counts **iteratively**, and no assert below reaches for `Debug`, `Display` or
/// `PartialEq`, because all three still recurse once per link -- a separate hazard this
/// file does not close. Bisected on the reference stack, on the infix ladder: `Display` overflows
/// past 719 links in debug and past 7 315 in release, `Clone` and `PartialEq` past 696 and
/// 1 683. Formatting one of these trees in an assert message would therefore abort the
/// process on its way to reporting the failure.
type Comb = (&'static str, fn(usize) -> String, fn(&Batch) -> usize);

/// The six shapes, each with the operator its links are counted on.
///
/// A chain of `INTERSECT` is not one of them any more: past 1070 links the batch is refused
/// with 8631 ([`set_operator_chains_stop_at_their_depth`]).
fn combs() -> [Comb; 6] {
    [
        (
            "infix ladder",
            |n| format!("SELECT 1{}", " + 1".repeat(n)),
            |batch| add_links(first_item_expr(batch)),
        ),
        (
            "logical ladder",
            |n| format!("SELECT 1 WHERE 1 = 1{}", " AND 1 = 1".repeat(n)),
            |batch| and_links(where_expr(batch)),
        ),
        (
            "collation suffix",
            |n| format!("SELECT 'a'{}", " COLLATE Latin1_General_CI_AS".repeat(n)),
            |batch| collate_links(first_item_expr(batch)),
        ),
        (
            "UNION ALL chain",
            |n| format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(n)),
            |batch| union_all_links(&select_of(batch).body),
        ),
        (
            "CROSS JOIN chain",
            |n| cross_chain("JOIN", n),
            |batch| cross_join_links(first_table_ref(batch)),
        ),
        (
            "CROSS APPLY chain",
            |n| cross_chain("APPLY", n),
            |batch| cross_apply_links(first_table_ref(batch)),
        ),
    ]
}

/// `FROM t0 CROSS <kind> t1 CROSS <kind> t2 …`, with a distinct name per source.
///
/// The names are made distinct for the same reason the derived tables above have distinct
/// column names: so that the vector stays about the length of the chain, and not about a
/// correlation name written twice, if it is ever replayed against a server. What SQL Server
/// answers to either, this file does not say.
fn cross_chain(kind: &str, levels: usize) -> String {
    let mut text = String::from("SELECT * FROM t0");
    for level in 1..=levels {
        text.push_str(" CROSS ");
        text.push_str(kind);
        text.push_str(&format!(" t{level}"));
    }
    text
}

/// **The flat chain vector.** A chain ten times past what the old destructor could free is
/// parsed, counted, and **freed on the reference stack** -- and this process is still here
/// to say how many links it held.
///
/// With the derived destructor this test does not fail, it **aborts**: the whole test binary
/// dies with `fatal runtime error: stack overflow` inside the destructor, on the first
/// shape it runs, in debug and in release alike. The count is asserted because the freeing
/// is only interesting if the tree was really that deep.
#[test]
fn a_flat_chain_past_the_old_limit_is_freed_without_an_abort() {
    for (shape, write, count) in combs() {
        let links = parse_count_and_free(write(COMB_LEVELS), count);
        assert_eq!(
            links, COMB_LEVELS,
            "{shape}: {links} links were read where {COMB_LEVELS} were written"
        );
    }
}

/// The same chain, this time reached through a scalar subquery.
///
/// Crossing from one node family to another -- an expression into a `SELECT`, a query
/// specification into its `FROM` -- is left to the derived glue, because each crossing
/// costs a unit of the nesting guard and a parsed batch can only afford 32 of them. This
/// says the comb inside is still freed iteratively once a crossing sits above it.
#[test]
fn a_flat_chain_under_a_subquery_is_freed_without_an_abort() {
    let text = format!("SELECT (SELECT 1{})", " + 1".repeat(COMB_LEVELS));
    let links = parse_count_and_free(text, |batch| match first_item_expr(batch) {
        Expr::Subquery(select, _) => add_links(first_spec_item_expr(spec_of(select))),
        _ => unreachable!("a scalar subquery was expected"),
    });
    assert_eq!(links, COMB_LEVELS, "{links} links were read");
}

/// The parser answers the next batch once a comb has been freed.
///
/// The half that a test of the count alone would miss, in the shape this file uses
/// throughout: a process that aborted while freeing does not reach the second parse.
#[test]
fn the_parser_still_works_after_freeing_a_comb() {
    let links = parse_count_and_free(format!("SELECT 1{}", " + 1".repeat(COMB_LEVELS)), |batch| {
        add_links(first_item_expr(batch))
    });
    assert_eq!(links, COMB_LEVELS);
    let batch = parse_on_reference_stack("SELECT 1".to_owned()).expect("an ordinary batch");
    assert_eq!(batch.to_string(), "SELECT 1");
}

/// Parses `text` on a thread of [`REFERENCE_STACK`] bytes, counts its links with `count`,
/// and **frees the tree on that same stack**, which is the point of the exercise.
///
/// Freeing inside the thread is deliberate: a `Batch` handed back through `join` would be
/// freed on the stack of the test harness instead, which `RUST_MIN_STACK` and `ulimit -s`
/// make anybody's guess. Only the count crosses back.
fn parse_count_and_free(text: String, count: fn(&Batch) -> usize) -> usize {
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || {
            let batch = parse_batch(&text, &ParseOptions::default())
                .unwrap_or_else(|error| unreachable!("the chain should parse: {error:?}"));
            let links = count(&batch);
            drop(batch);
            links
        })
        .expect("the test can spawn a thread");
    worker.join().expect("the parse thread ran to its end")
}

/// The `SELECT` of a batch made of exactly one.
///
/// No `Debug` of the statement on the error path: the trees this file builds cannot be
/// formatted without overflowing, which is the very defect under test.
fn select_of(batch: &Batch) -> &SelectStatement {
    match batch.statements.as_slice() {
        [Statement::Select(select)] => select,
        _ => unreachable!("a batch of one SELECT was expected"),
    }
}

/// The query specification of a `SELECT` with no set operator.
fn spec_of(select: &SelectStatement) -> &QuerySpec {
    match &select.body {
        QueryBody::Select(spec) => spec,
        _ => unreachable!("a plain query specification was expected"),
    }
}

/// The expression of a select list of exactly one item.
fn first_spec_item_expr(spec: &QuerySpec) -> &Expr {
    match spec.items.as_slice() {
        [SelectItem::Expr { expr, .. }] => expr,
        _ => unreachable!("a select list of one expression was expected"),
    }
}

/// The expression of the single select item of a batch of one `SELECT`.
fn first_item_expr(batch: &Batch) -> &Expr {
    first_spec_item_expr(spec_of(select_of(batch)))
}

/// The `WHERE` predicate of a batch of one `SELECT`.
fn where_expr(batch: &Batch) -> &Expr {
    spec_of(select_of(batch))
        .where_
        .as_ref()
        .unwrap_or_else(|| unreachable!("a WHERE clause was expected"))
}

/// The single table reference of a batch of one `SELECT`.
fn first_table_ref(batch: &Batch) -> &TableRef {
    match spec_of(select_of(batch)).from.as_slice() {
        [table] => table,
        _ => unreachable!("a FROM clause of one reference was expected"),
    }
}

/// How many `+` the left spine of `expr` carries, counted without recursing.
fn add_links(mut expr: &Expr) -> usize {
    let mut links = 0;
    while let Expr::Binary {
        op: BinaryOp::Add,
        left,
        ..
    } = expr
    {
        links += 1;
        expr = left;
    }
    links
}

/// How many `AND` the left spine of `expr` carries.
fn and_links(mut expr: &Expr) -> usize {
    let mut links = 0;
    while let Expr::Binary {
        op: BinaryOp::And,
        left,
        ..
    } = expr
    {
        links += 1;
        expr = left;
    }
    links
}

/// How many `COLLATE` suffixes `expr` carries.
fn collate_links(mut expr: &Expr) -> usize {
    let mut links = 0;
    while let Expr::Collate { expr: inner, .. } = expr {
        links += 1;
        expr = inner;
    }
    links
}

/// How many `UNION ALL` nodes `body` carries, wherever they stand in the tree, counted with
/// a stack of its own rather than by recursion.
fn union_all_links(body: &QueryBody) -> usize {
    let mut links = 0;
    let mut pending = vec![body];
    while let Some(body) = pending.pop() {
        if let QueryBody::SetOp {
            op: SetOp::Union,
            all: true,
            left,
            right,
            ..
        } = body
        {
            links += 1;
            pending.push(left);
            pending.push(right);
        }
    }
    links
}

/// How many `CROSS JOIN` the left spine of `table` carries.
fn cross_join_links(mut table: &TableRef) -> usize {
    let mut links = 0;
    while let TableRef::Join {
        kind: JoinKind::Cross,
        left,
        ..
    } = table
    {
        links += 1;
        table = left;
    }
    links
}

/// How many `CROSS APPLY` the left spine of `table` carries.
fn cross_apply_links(mut table: &TableRef) -> usize {
    let mut links = 0;
    while let TableRef::Apply {
        kind: ApplyKind::Cross,
        left,
        ..
    } = table
    {
        links += 1;
        table = left;
    }
    links
}

// ---------------------------------------------------------------------------------------
// The comb the niladic diagnostic walk used to abort on, during the **parse**.
// ---------------------------------------------------------------------------------------

/// Parses `text` on a thread of [`REFERENCE_STACK`] bytes, reads `read` from the tree, and
/// frees the tree on that same stack; only what `read` gives back crosses the `join`.
///
/// The counterpart of [`parse_count_and_free`] for a reading that is not a count: the
/// shapes below are checked on the diagnostic the walk rewrote as well as on the length of
/// the chain it walked.
fn parse_read_and_free<T: Send + 'static>(text: String, read: fn(&Batch) -> T) -> T {
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || {
            let batch = parse_batch(&text, &ParseOptions::default())
                .unwrap_or_else(|error| unreachable!("the chain should parse: {error:?}"));
            let read = read(&batch);
            drop(batch);
            read
        })
        .expect("the test can spawn a thread");
    worker.join().expect("the parse thread ran to its end")
}

/// The single argument of a batch whose one select item is a call.
fn only_call_argument(batch: &Batch) -> &Expr {
    match first_item_expr(batch) {
        Expr::Function { args, .. } => match args.as_slice() {
            [argument] => argument,
            _ => unreachable!("a call of one argument was expected"),
        },
        _ => unreachable!("a call was expected"),
    }
}

/// The condition of a batch whose one select item is a `CASE` of one arm.
fn only_case_condition(batch: &Batch) -> &Expr {
    match first_item_expr(batch) {
        Expr::Case { arms, .. } => match arms.as_slice() {
            [arm] => &arm.when,
            _ => unreachable!("a CASE of one arm was expected"),
        },
        _ => unreachable!("a CASE was expected"),
    }
}

/// **The comb vector**, first shape: a comb of [`COMB_LEVELS`] links inside the
/// argument of a call, which is one of the places whose parse walks the tree again to
/// enclose the niladic diagnostics.
///
/// The guard counts one unit for the call and nothing for the links, so this
/// batch is one the parser accepts -- 200 013 bytes of text, well within one TDS batch.
/// With a recursive walk this test does not fail, it **aborts**: `visit_niladic_diagnostics`
/// recursed once per link and the process died with `fatal runtime error: stack overflow`
/// **during the parse**, in the walk that follows the reading of the argument -- tried
/// here on this shape and on the `CASE` below, in debug and in release.
#[test]
fn a_comb_in_a_call_argument_is_walked_without_an_abort() {
    let text = format!("SELECT ABS(1{})", " + 1".repeat(COMB_LEVELS));
    let links = parse_read_and_free(text, |batch| add_links(only_call_argument(batch)));
    assert_eq!(
        links, COMB_LEVELS,
        "call argument: {links} links were read where {COMB_LEVELS} were written"
    );
}

/// The second shape: the same comb in the condition of a `CASE`, whose parse encloses the
/// diagnostics of the whole expression it has just built, arms included.
#[test]
fn a_comb_in_a_case_condition_is_walked_without_an_abort() {
    let text = format!(
        "SELECT CASE WHEN 1 = 1{} THEN 1 ELSE 0 END",
        " AND 1 = 1".repeat(COMB_LEVELS)
    );
    let links = parse_read_and_free(text, |batch| and_links(only_case_condition(batch)));
    assert_eq!(
        links, COMB_LEVELS,
        "CASE condition: {links} links were read where {COMB_LEVELS} were written"
    );
}

/// The walk still **does** what it is there for once the tree is walked iteratively: the
/// invalid niladic spelling at the far end of a comb of [`COMB_LEVELS`] links comes back
/// rewritten to the 102 on `(` of the short shape
/// (`niladic.rs`, `SELECT LEN(CURRENT_TIMESTAMP())`), not to the 102 on `)` it carries
/// before the enclosing.
///
/// A test that only asked whether the parse survived would pass on a walk that visited
/// nothing at all; this one pins the node the walk had to reach.
#[test]
fn the_niladic_diagnostic_at_the_end_of_a_comb_is_still_enclosed() {
    let text = format!(
        "SELECT ABS(CURRENT_TIMESTAMP(){})",
        " + 1".repeat(COMB_LEVELS)
    );
    let (links, number, token) = parse_read_and_free(text, |batch| {
        let argument = only_call_argument(batch);
        let mut leaf = argument;
        while let Expr::Binary {
            op: BinaryOp::Add,
            left,
            ..
        } = leaf
        {
            leaf = left;
        }
        match leaf {
            Expr::InvalidNiladic {
                diagnostic_number,
                diagnostic_token,
                ..
            } => (
                add_links(argument),
                *diagnostic_number,
                diagnostic_token.clone(),
            ),
            _ => unreachable!("the deepest left operand should be the invalid niladic"),
        }
    });
    assert_eq!(links, COMB_LEVELS, "{links} links were read");
    assert_eq!(
        (number, token.as_str()),
        (102, "("),
        "the diagnostic of the comb's far end was not enclosed"
    );
}

/// The parser answers the next batch once a comb under a call has been walked.
///
/// The half that a test of the count alone would miss, in the shape this file uses
/// throughout: a process that died in the walk never reaches the second parse.
#[test]
fn the_parser_still_works_after_walking_a_comb_in_a_call() {
    let text = format!("SELECT ABS(1{})", " + 1".repeat(COMB_LEVELS));
    let links = parse_read_and_free(text, |batch| add_links(only_call_argument(batch)));
    assert_eq!(links, COMB_LEVELS);
    let batch = parse_on_reference_stack("SELECT 1".to_owned()).expect("an ordinary batch");
    assert_eq!(batch.to_string(), "SELECT 1");
}

/// `IF @i = 0 … ELSE IF @i = k …`, one `ELSE IF` per line from line 2, then a plain
/// `ELSE` when `final_else` says so.
fn else_if_chain(links: usize, final_else: bool) -> String {
    let mut text = "IF @i = 0 SET @r = 0\n".to_owned();
    for k in 1..=links {
        text.push_str(&format!("ELSE IF @i = {k} SET @r = {k}\n"));
    }
    if final_else {
        text.push_str("ELSE SET @r = -1\n");
    }
    text
}

/// How many `IF` nodes an `ELSE IF` chain was folded into, walked in a loop.
fn else_links(statement: &Statement) -> usize {
    let mut count = 0;
    let mut current = Some(statement);
    while let Some(Statement::If { else_branch, .. }) = current {
        count += 1;
        current = else_branch.as_deref();
    }
    count
}

/// A chain at the top of a batch holds 253 `ELSE`, the last plain one included, and the
/// 254th is a 191 on its own line. Under 200 enclosing `BEGIN` the chain still holds 153
/// `ELSE`, where the statement guard alone used to stop it after 19.
#[test]
fn else_if_chain_stops_at_its_own_limit() {
    for (links, final_else) in [(253, false), (252, true)] {
        let batch = parse_on_reference_stack(else_if_chain(links, final_else))
            .expect("a chain of 253 ELSE parses");
        assert_eq!(else_links(&batch.statements[0]), links + 1);
    }
    for (links, final_else, line) in [(254, false, 255), (253, true, 255), (10_000, true, 255)] {
        let error = refused(else_if_chain(links, final_else));
        assert_is_191(&error);
        assert_eq!(error.line, line, "the 254th ELSE is on line 255");
    }
    let enclosed = |links| {
        format!(
            "{}{}{}",
            "BEGIN\n".repeat(200),
            else_if_chain(links, false),
            "END\n".repeat(200)
        )
    };
    let batch = parse_on_reference_stack(enclosed(153)).expect("153 ELSE under 200 blocks");
    let mut statement = &batch.statements[0];
    for _ in 0..200 {
        match statement {
            Statement::Block { statements, .. } => statement = &statements[0],
            other => unreachable!("a block was expected, got {other:?}"),
        }
    }
    assert_eq!(else_links(statement), 154);
    let error = refused(enclosed(154));
    assert_is_191(&error);
    assert_eq!(error.line, 355, "the 154th ELSE is on line 355");
}

/// `chains` chains of `links` `ELSE IF` each, the plain `ELSE` of each opening a
/// `BEGIN … END` that holds the next chain.
fn nested_else_if_chains(chains: usize, links: usize) -> String {
    let mut text = String::new();
    for _ in 0..chains {
        text.push_str(&else_if_chain(links, false));
        text.push_str("ELSE BEGIN\n");
    }
    text.push_str("SET @r = -2\n");
    text.push_str(&"END\n".repeat(chains));
    text
}

/// Chains nested through `ELSE BEGIN … END` spend one budget: two chains of 125 parse,
/// two of 126 do not, and twenty chains of 50 -- a thousand links, which used to build a
/// tree a thousand `IF` deep -- are a 191. The deepest tree the budget lets through, a
/// full chain whose last `ELSE` holds 217 nested blocks, is printed and freed on the
/// reference stack.
#[test]
fn nested_else_if_chains_share_one_budget() {
    parse_on_reference_stack(nested_else_if_chains(2, 125)).expect("two chains of 125 parse");
    for (chains, links) in [(2, 126), (4, 250), (20, 50)] {
        assert_is_191(&refused(nested_else_if_chains(chains, links)));
    }
    let deepest = format!(
        "{}ELSE {}SET @r = 1\n{}",
        else_if_chain(252, false),
        "BEGIN\n".repeat(217),
        "END\n".repeat(217)
    );
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || {
            let batch = parse_batch(&deepest, &ParseOptions::default())
                .unwrap_or_else(|error| unreachable!("the deepest tree parses: {error:?}"));
            let printed = batch.to_string();
            let links = else_links(&batch.statements[0]);
            drop(batch);
            (links, printed.matches("BEGIN").count())
        })
        .expect("the test can spawn a thread");
    let (links, blocks) = worker.join().expect("the tree was printed and freed");
    assert_eq!((links, blocks), (253, 217));
}

/// Error 8631, the one a batch too deep for the engine is refused with.
const STACK_LIMIT_REACHED: u32 = 8631;

/// Asserts that `error` is 8631, severity 17, state 1, on `line`.
fn assert_is_8631(error: &SqlError, line: u32) {
    assert_eq!(
        (error.number, error.severity, error.state, error.line),
        (STACK_LIMIT_REACHED, 17, 1, line),
        "{error:?}"
    );
}

/// `SELECT 0 <op> SELECT 1 <op> SELECT 2 …`, `links` operators long, each operand
/// with its own number so that their order can be read back.
fn set_op_chain(op: &str, links: usize) -> String {
    let mut text = String::from("SELECT 0");
    for link in 1..=links {
        text.push_str(&format!(" {op} SELECT {link}"));
    }
    text
}

/// The depth of the set-operator tree of `body`, walked with a stack of its own.
fn set_op_depth(body: &QueryBody) -> usize {
    let mut deepest = 0;
    let mut pending = vec![(body, 0)];
    while let Some((body, depth)) = pending.pop() {
        match body {
            QueryBody::SetOp { left, right, .. } => {
                pending.push((left, depth + 1));
                pending.push((right, depth + 1));
            }
            QueryBody::Nested(inner, _) => pending.push((inner, depth)),
            QueryBody::Select(_) => deepest = deepest.max(depth),
        }
    }
    deepest
}

/// The numbers the operands of `body` select, from left to right.
fn operand_numbers(body: &QueryBody) -> Vec<String> {
    let mut numbers = Vec::new();
    let mut pending = vec![body];
    while let Some(body) = pending.pop() {
        match body {
            QueryBody::SetOp { left, right, .. } => {
                pending.push(right);
                pending.push(left);
            }
            QueryBody::Nested(inner, _) => pending.push(inner),
            QueryBody::Select(spec) => numbers.push(spec.items[0].to_string()),
        }
    }
    numbers
}

/// Fifty thousand `UNION ALL` are read as a balanced tree: every link is there, the
/// operands keep their order, the tree is 17 levels deep, and the walks that recurse once
/// per level -- `Clone`, `PartialEq`, `Display`, `Drop` -- run on the reference stack.
///
/// The parser that built this chain leaning left aborts this test in `Clone`, the walk a
/// query binder starts with.
#[test]
fn a_long_union_all_run_is_balanced() {
    let text = set_op_chain("UNION ALL", COMB_LEVELS);
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || {
            let batch = parse_batch(&text, &ParseOptions::default())
                .unwrap_or_else(|error| unreachable!("the chain parses: {error:?}"));
            let copy = batch.clone();
            let same = copy == batch;
            let printed = batch.to_string();
            let body = &select_of(&batch).body;
            let numbers = operand_numbers(body);
            let in_order = numbers
                .iter()
                .enumerate()
                .all(|(index, number)| *number == index.to_string());
            let summary = (
                union_all_links(body),
                set_op_depth(body),
                numbers.len(),
                in_order,
                same,
                printed == text,
            );
            drop(copy);
            drop(batch);
            summary
        })
        .expect("the test can spawn a thread");
    let summary = worker.join().expect("the tree was walked and freed");
    assert_eq!(
        summary,
        (COMB_LEVELS, 17, COMB_LEVELS + 1, true, true, true)
    );
}

/// A run of 31 `UNION ALL` keeps the shape it was written in, leaning left; the 32nd link
/// makes it balanced. A run of `UNION` is balanced the same way, a chain that alternates
/// `UNION` and `UNION ALL` is not, and a chain of long runs joined by `EXCEPT` costs two
/// levels per run.
#[test]
fn a_short_union_run_keeps_its_shape() {
    let depth_of = |text: String| {
        let batch = parse_on_reference_stack(text).expect("the chain parses");
        set_op_depth(&select_of(&batch).body)
    };
    assert_eq!(depth_of(set_op_chain("UNION ALL", 31)), 31);
    assert_eq!(depth_of(set_op_chain("UNION ALL", 32)), 6);
    assert_eq!(depth_of(set_op_chain("UNION", 1000)), 11);
    let alternating: String = (1..=100)
        .map(|link| {
            let op = if link % 2 == 0 { "UNION" } else { "UNION ALL" };
            format!(" {op} SELECT {link}")
        })
        .collect();
    assert_eq!(depth_of(format!("SELECT 0{alternating}")), 100);
    // The chain read before a long run sinks one level under it, not the run's depth.
    let rounds = format!(" EXCEPT SELECT 8{}", " UNION ALL SELECT 9".repeat(40)).repeat(100);
    assert_eq!(depth_of(format!("SELECT 0{rounds}")), 7 + 2 * 99);
}

/// A balanced run reads back the same: the text `Display` prints parses into the tree it
/// was printed from, also when the run follows an `EXCEPT` and is followed by one.
#[test]
fn a_balanced_run_survives_the_display_loop() {
    let text = format!(
        "SELECT 0 EXCEPT SELECT 1{} EXCEPT SELECT 2",
        " UNION ALL SELECT 3 INTERSECT SELECT 4".repeat(40)
    );
    let batch = parse_on_reference_stack(text.clone()).expect("the chain parses");
    assert_eq!(batch.to_string(), text);
    let again = parse_on_reference_stack(batch.to_string()).expect("the printed text parses");
    assert_eq!(again, batch);
    let numbers = operand_numbers(&select_of(&batch).body);
    assert_eq!(numbers.len(), 83);
    assert_eq!(numbers.first().map(String::as_str), Some("0"));
    assert_eq!(numbers.last().map(String::as_str), Some("2"));
}

/// A chain of `INTERSECT`, of `EXCEPT`, or one that alternates `UNION` and `UNION ALL`,
/// holds 1070 operators and is refused with 8631 at the 1071st, on the line of its
/// statement. The depth adds up across a derived table: two chains of 600 nested one
/// inside the other are refused. Fifty thousand links are refused the same way.
#[test]
fn set_operator_chains_stop_at_their_depth() {
    let alternating = |links: usize| -> String {
        let mut text = String::from("SELECT 0");
        for link in 1..=links {
            let op = if link % 2 == 0 { "UNION" } else { "UNION ALL" };
            text.push_str(&format!(" {op} SELECT {link}"));
        }
        text
    };
    for (shape, write) in [
        (
            "INTERSECT",
            (|n| set_op_chain("INTERSECT", n)) as fn(usize) -> String,
        ),
        ("EXCEPT", |n| set_op_chain("EXCEPT", n)),
        ("alternating UNION", alternating),
    ] {
        let batch = parse_on_reference_stack(write(1070))
            .unwrap_or_else(|error| unreachable!("{shape}: 1070 links parse: {error:?}"));
        assert_eq!(set_op_depth(&select_of(&batch).body), 1070, "{shape}");
        for links in [1071, 3500, 10_000, COMB_LEVELS] {
            assert_is_8631(&refused(write(links)), 1);
        }
    }
    let inner = set_op_chain("EXCEPT", 600);
    let nested = format!(
        "SELECT 1;\nSELECT *\nFROM ({inner}) AS t{}",
        " EXCEPT SELECT 1".repeat(600)
    );
    assert_is_8631(&refused(nested), 2);
    let side_by_side = format!(
        "SELECT * FROM ({inner}) AS t CROSS JOIN ({inner}) AS u{}",
        " EXCEPT SELECT 1".repeat(400)
    );
    parse_on_reference_stack(side_by_side).expect("breadth is not depth");
}

/// The deepest set-operator tree let through is walked by `Clone`, `PartialEq`,
/// `Display` and `Drop` on the reference stack, under 200 derived tables.
#[test]
fn the_deepest_set_operator_tree_is_walked_without_an_abort() {
    let mut source = format!("({}) AS t0", set_op_chain("EXCEPT", 1069));
    for level in 1..=200 {
        source = format!("(SELECT * FROM {source}) AS t{level}");
    }
    let text = format!("SELECT * FROM {source}");
    let worker = thread::Builder::new()
        .stack_size(REFERENCE_STACK)
        .spawn(move || {
            let batch = parse_batch(&text, &ParseOptions::default())
                .unwrap_or_else(|error| unreachable!("the tree parses: {error:?}"));
            let copy = batch.clone();
            let printed = batch.to_string();
            (copy == batch, printed == text)
        })
        .expect("the test can spawn a thread");
    assert_eq!(worker.join().expect("the tree was walked"), (true, true));
}

/// A syntax error anywhere in the batch is sent instead of the 8631 of a chain too deep,
/// before or after it.
#[test]
fn a_syntax_error_wins_over_a_set_operator_chain_too_deep() {
    let chain = set_op_chain("INTERSECT", 1100);
    let after = refused(format!("{chain};\nSELECT FROM t"));
    assert_eq!((after.number, after.line), (156, 2), "{after:?}");
    let before = refused(format!("SELECT 1 +;\n{chain}"));
    assert_eq!(before.number, 102, "{before:?}");
    assert_is_8631(&refused(format!("SELECT 1;\n{chain};\nSELECT 2")), 2);
}

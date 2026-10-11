use super::*;

/// Numbers the engine relies on: syntax and binding, conversions and arithmetic,
/// constraints, DDL and databases, transactions and concurrency, connection,
/// informational (severity <= 10).
const REQUIRED_NUMBERS: &[u32] = &[
    102, 105, 137, 148, 156, 207, 208, 209, 213, 2812, 4104, 4145, 8120, 241, 242, 245, 8114, 8115,
    8134, 8152, 2628, 515, 544, 8106, 8107, 547, 2601, 2627, 911, 1801, 2714, 3701, 4060, 226,
    1205, 1222, 3902, 3903, 3926, 3960, 3971, 3989, 18452, 18456, 5701, 5703,
];

/// The specifiers that consume an argument, longest first so that `%ls` is not read
/// inside `%.*ls`.
const SPECIFIERS: &[&str] = &[
    "%.*ls", "%S_MSG", "%I64d", "%ls", "%hs", "%ld", "%s", "%d", "%f",
];

/// The contract of each entry: severity, then the argument specifiers in template
/// order. A constructor fills them in that order; the texts around them are free.
const CONTRACT: &[(u32, u8, &[&str])] = &[
    (102, 15, &["%.*ls"]),
    (103, 15, &["%S_MSG", "%.*ls", "%d"]),
    (105, 15, &["%.*ls"]),
    (107, 15, &["%.*ls"]),
    (109, 15, &[]),
    (110, 15, &[]),
    (113, 15, &[]),
    (116, 15, &[]),
    (117, 15, &["%S_MSG", "%.*ls", "%d"]),
    (119, 15, &["%d"]),
    (120, 15, &[]),
    (121, 15, &[]),
    (127, 15, &[]),
    (128, 15, &["%.*ls"]),
    (130, 15, &[]),
    (131, 15, &["%d", "%S_MSG", "%.*ls", "%d"]),
    (133, 15, &["%.*ls"]),
    (134, 15, &["%.*ls"]),
    (135, 15, &[]),
    (136, 15, &[]),
    (137, 15, &["%.*ls"]),
    (141, 15, &[]),
    (144, 15, &[]),
    (145, 15, &[]),
    (147, 15, &[]),
    (148, 15, &["%.*ls"]),
    (151, 15, &["%.*ls"]),
    (155, 15, &["%.*ls", "%ls"]),
    (156, 15, &["%.*ls"]),
    (168, 15, &["%.*ls", "%d"]),
    (174, 15, &["%.*ls", "%d"]),
    (178, 15, &[]),
    (179, 15, &[]),
    (189, 15, &["%ls", "%d", "%d"]),
    (191, 15, &[]),
    (192, 16, &[]),
    (195, 15, &["%.*ls", "%S_MSG"]),
    (201, 16, &["%.*ls", "%.*ls"]),
    (205, 16, &[]),
    (206, 16, &["%ls", "%ls"]),
    (207, 16, &["%.*ls"]),
    (208, 16, &["%.*ls"]),
    (209, 16, &["%.*ls"]),
    (210, 16, &[]),
    (213, 16, &[]),
    (214, 16, &["%ls", "%ls"]),
    (215, 16, &["%.*ls"]),
    (217, 16, &["%d"]),
    (220, 16, &["%ls", "%ld"]),
    (226, 16, &["%ls"]),
    (232, 16, &["%ls", "%f"]),
    (234, 16, &["%ls"]),
    (235, 16, &[]),
    (237, 16, &["%ls"]),
    (241, 16, &[]),
    (242, 16, &["%ls", "%ls"]),
    (243, 16, &["%.*ls"]),
    (244, 16, &["%ls", "%.*ls", "%hs"]),
    (245, 16, &["%ls", "%.*ls", "%ls"]),
    (248, 16, &["%ls", "%.*ls"]),
    (257, 16, &["%ls", "%ls"]),
    (263, 16, &[]),
    (264, 16, &["%.*ls"]),
    (266, 16, &["%ld", "%ld"]),
    (271, 16, &["%.*ls"]),
    (281, 16, &["%d", "%ls"]),
    (289, 16, &["%ls"]),
    (292, 16, &["%ls"]),
    (295, 16, &[]),
    (319, 15, &["%.*ls"]),
    (339, 16, &[]),
    (402, 16, &["%s", "%s", "%s"]),
    (447, 16, &["%ls"]),
    (448, 16, &["%.*ls"]),
    (457, 16, &["%ls", "%ls", "%ls", "%ls", "%ls"]),
    (468, 16, &["%.*ls", "%.*ls", "%ls"]),
    (506, 16, &["%.*ls", "%ls"]),
    (512, 16, &[]),
    (515, 16, &["%.*ls", "%.*ls", "%ls"]),
    (517, 16, &["%ls"]),
    (529, 16, &["%ls", "%ls"]),
    (535, 16, &["%.*ls", "%.*ls"]),
    (536, 16, &["%ls"]),
    (537, 16, &[]),
    (544, 16, &["%.*ls"]),
    (
        547,
        16,
        &[
            "%ls", "%ls", "%.*ls", "%.*ls", "%.*ls", "%ls", "%.*ls", "%ls",
        ],
    ),
    (574, 16, &["%ls"]),
    (911, 16, &["%.*ls"]),
    (1001, 16, &["%d", "%d"]),
    (1002, 16, &["%d", "%d"]),
    (1007, 15, &["%S_MSG", "%.*ls"]),
    (1011, 15, &["%.*ls"]),
    (1012, 15, &["%.*ls", "%.*ls"]),
    (1013, 15, &["%.*ls", "%.*ls"]),
    (1014, 15, &[]),
    (1023, 15, &["%d", "%ls"]),
    (1031, 15, &[]),
    (1038, 15, &[]),
    (1046, 15, &[]),
    (1047, 15, &[]),
    (1060, 15, &[]),
    (1062, 16, &[]),
    (1065, 15, &[]),
    (1087, 15, &["%.*ls"]),
    (1088, 15, &["%.*ls"]),
    (1205, 13, &["%d", "%.*ls"]),
    (1222, 16, &[]),
    (1750, 10, &[]),
    (1754, 16, &["%.*ls", "%.*ls"]),
    (1769, 16, &["%.*ls", "%.*ls", "%.*ls"]),
    (1776, 16, &["%.*ls", "%.*ls"]),
    (1801, 16, &["%.*ls"]),
    (1902, 16, &["%S_MSG", "%.*ls", "%.*ls"]),
    (1909, 16, &["%S_MSG", "%.*ls"]),
    (1911, 16, &["%.*ls"]),
    (1913, 16, &["%.*ls", "%S_MSG", "%.*ls"]),
    (1939, 16, &["%S_MSG", "%.*ls"]),
    (2601, 14, &["%.*ls", "%.*ls", "%ls"]),
    (2627, 14, &["%ls", "%.*ls", "%.*ls", "%ls"]),
    (2628, 16, &["%.*ls", "%.*ls", "%.*ls"]),
    (2702, 16, &["%.*ls"]),
    (2705, 16, &["%.*ls", "%.*ls"]),
    (2714, 16, &["%.*ls"]),
    (2715, 16, &["%d", "%.*ls"]),
    (2716, 16, &["%d", "%.*ls"]),
    (2717, 15, &["%d", "%S_MSG", "%.*ls", "%d"]),
    (2749, 16, &["%.*ls"]),
    (2750, 16, &["%d", "%d", "%d"]),
    (2812, 16, &["%.*ls"]),
    (3623, 16, &[]),
    (3701, 11, &["%S_MSG", "%S_MSG", "%.*ls"]),
    (3708, 16, &["%S_MSG", "%S_MSG", "%.*ls", "%S_MSG"]),
    (3723, 16, &["%.*ls", "%ls"]),
    (3726, 16, &["%.*ls"]),
    (3728, 16, &["%.*ls"]),
    (3902, 16, &[]),
    (3903, 16, &[]),
    (3926, 10, &[]),
    (3952, 16, &["%.*ls"]),
    (3960, 16, &["%.*ls", "%.*ls"]),
    (3971, 16, &["%.*ls"]),
    (3989, 16, &[]),
    (4060, 11, &["%.*ls"]),
    (4063, 11, &["%.*ls", "%.*ls"]),
    (4104, 16, &["%.*ls"]),
    (4121, 16, &["%.*ls", "%.*ls"]),
    (4127, 16, &[]),
    (4145, 15, &["%.*ls"]),
    (4151, 16, &[]),
    (4701, 16, &["%.*ls"]),
    (4712, 16, &["%.*ls"]),
    (4901, 16, &["%.*ls", "%.*ls"]),
    (4902, 16, &["%.*ls"]),
    (4924, 16, &["%.*ls", "%.*ls"]),
    (5074, 16, &["%S_MSG", "%.*ls", "%S_MSG", "%.*ls"]),
    (5701, 10, &["%.*ls"]),
    (5703, 10, &["%.*ls"]),
    (7202, 11, &["%.*ls"]),
    (8101, 16, &["%.*ls"]),
    (8102, 16, &["%.*ls"]),
    (8106, 16, &["%.*ls"]),
    (8107, 16, &["%.*ls", "%.*ls", "%.*ls", "%.*ls"]),
    (8110, 16, &["%.*ls"]),
    (8111, 16, &["%.*ls"]),
    (8112, 16, &["%.*ls"]),
    (8114, 16, &["%ls", "%ls"]),
    (8115, 16, &["%ls", "%ls"]),
    (8116, 16, &["%ls", "%d", "%ls"]),
    (8117, 16, &["%ls", "%ls"]),
    (8120, 16, &["%.*ls", "%.*ls"]),
    (8121, 16, &["%.*ls", "%.*ls"]),
    (8134, 16, &[]),
    (8144, 16, &["%.*ls"]),
    (8145, 16, &["%.*ls", "%.*ls"]),
    (8146, 16, &["%.*ls"]),
    (8147, 16, &["%.*ls", "%.*ls"]),
    (8148, 16, &["%ls", "%S_MSG", "%.*ls", "%.*ls"]),
    (8152, 16, &[]),
    (8153, 10, &[]),
    (8154, 15, &["%.*ls"]),
    (8155, 15, &["%d", "%.*ls"]),
    (8158, 16, &["%.*ls"]),
    (8159, 16, &["%.*ls"]),
    (8168, 16, &["%.*ls"]),
    (8169, 16, &[]),
    (8170, 16, &[]),
    (8178, 16, &["%.*ls", "%.*ls"]),
    (8179, 16, &["%d"]),
    (8180, 16, &[]),
    (8199, 16, &[]),
    (8631, 17, &[]),
    (9806, 16, &["%.*ls", "%.*ls"]),
    (9807, 16, &["%d"]),
    (9809, 16, &["%d", "%s", "%s"]),
    (9810, 16, &["%.*ls", "%.*ls", "%s"]),
    (10709, 16, &[]),
    (15009, 16, &["%s", "%s"]),
    (15010, 16, &["%s"]),
    (18452, 14, &["%.*ls"]),
    (18456, 14, &["%.*ls", "%.*ls", "%.*ls"]),
];

/// The argument specifiers of `template`, in order; `%%` consumes nothing.
fn specifiers_of(template: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(pos) = rest.find('%') {
        let tail = &rest[pos..];
        if let Some(stripped) = tail.strip_prefix("%%") {
            rest = stripped;
        } else if let Some(spec) = SPECIFIERS.iter().find(|spec| tail.starts_with(**spec)) {
            out.push(*spec);
            rest = &tail[spec.len()..];
        } else {
            panic!("stray `%` in {template:?}");
        }
    }
    out
}

#[test]
fn catalog_specifiers_are_the_contract() {
    assert_eq!(CONTRACT.len(), CATALOG.len());
    for &(number, severity, specifiers) in CONTRACT {
        let def =
            message_template(number).unwrap_or_else(|| panic!("error {number} is in the catalog"));
        assert_eq!(def.severity, severity, "severity of {number}");
        assert_eq!(
            specifiers_of(def.template),
            specifiers,
            "specifiers of {number}"
        );
    }
}

#[test]
fn specifiers_of_reads_each_specifier_once() {
    assert_eq!(
        specifiers_of("a %.*ls b %ls c %% d %S_MSG %d"),
        ["%.*ls", "%ls", "%S_MSG", "%d"]
    );
    assert!(specifiers_of("no argument").is_empty());
}

/// 234 and 237 are two numbers for one text: a money value too large for its target.
#[test]
fn money_overflow_234_and_237_share_one_text() {
    let money = message_template(234).expect("234 is in the catalog");
    let target = message_template(237).expect("237 is in the catalog");
    assert_eq!(money.template, target.template);
    assert_eq!(money.severity, target.severity);
}

/// 3708 and 3701 are two templates, not one template with two fillings: 3708 carries
/// a fourth specifier.
#[test]
fn templates_3701_and_3708_are_distinct() {
    let missing = message_template(3701).expect("3701 is in the catalog");
    let system = message_template(3708).expect("3708 is in the catalog");
    assert_ne!(missing.template, system.template);
    assert_eq!(specifiers_of(missing.template).len(), 3);
    assert_eq!(specifiers_of(system.template).len(), 4);
}

/// 1907 has no entry: a second clustered index answers 1902, and two clustered
/// constraints in one `CREATE TABLE` answer 8112.
#[test]
fn number_1907_has_no_entry() {
    assert_eq!(message_template(1907), None);
    assert!(message_template(1902).is_some());
    assert!(message_template(8112).is_some());
}

#[test]
fn catalog_is_sorted_and_unique() {
    assert!(!CATALOG.is_empty());
    for pair in CATALOG.windows(2) {
        assert!(
            pair[0].number < pair[1].number,
            "catalog not strictly increasing between {} and {}",
            pair[0].number,
            pair[1].number
        );
    }
}

#[test]
fn catalog_severities_in_range() {
    for def in CATALOG {
        assert!(
            def.severity <= 25,
            "error {} has severity {} > 25",
            def.number,
            def.severity
        );
    }
}

#[test]
fn catalog_templates_are_non_empty_and_ascii() {
    for def in CATALOG {
        assert!(
            !def.template.is_empty(),
            "error {} has an empty template",
            def.number
        );
        assert!(
            def.template.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "error {} has a non printable-ASCII character",
            def.number
        );
        assert_eq!(
            def.template,
            def.template.trim_end(),
            "error {} has trailing whitespace",
            def.number
        );
    }
}

#[test]
fn catalog_covers_required_numbers() {
    for &number in REQUIRED_NUMBERS {
        assert!(
            message_template(number).is_some(),
            "required error {number} is missing from the catalog"
        );
    }
}

#[test]
fn message_template_returns_matching_number() {
    for def in CATALOG {
        let found = message_template(def.number).expect("the entry is found");
        assert_eq!(found.number, def.number);
        assert_eq!(found, def);
    }
}

#[test]
fn message_template_547_keeps_optional_column_suffix() {
    let def = message_template(547).expect("547 is in the catalog");
    assert!(def.template.contains("\"%.*ls\""));
    assert!(def.template.ends_with("%ls%.*ls%ls."));
}

#[test]
fn message_template_unknown_returns_none() {
    assert!(message_template(999_999).is_none());
    assert!(message_template(0).is_none());
}

/// With a successful statement on each side, these eight numbers let the surrounding
/// statements run: a following statement in the same batch still runs, and so does the
/// caller of an `EXEC('…')`/`sp_executesql` whose nested text raised one of them.
#[test]
fn batch_scope_of_constraint_and_truncation_errors_is_statement() {
    for number in [515, 544, 547, 2601, 2627, 2628, 3701, 8152] {
        let def =
            message_template(number).unwrap_or_else(|| panic!("error {number} is in the catalog"));
        assert_eq!(
            def.batch_scope(),
            BatchErrorScope::Statement,
            "error {number}"
        );
    }
}

/// The counterpart of the test above: these four numbers are raised the same way, while a
/// statement of a batch runs, and still stop the statement after them, unlike the eight
/// numbers checked there.
#[test]
fn batch_scope_of_deferred_bind_errors_stays_batch() {
    for number in [1088, 2714, 3728, 4902] {
        let def =
            message_template(number).unwrap_or_else(|| panic!("error {number} is in the catalog"));
        assert_eq!(def.batch_scope(), BatchErrorScope::Batch, "error {number}");
    }
}

/// A subquery read where one value is expected and answering several rows stops its own
/// statement: the statement written after it in the batch still runs.
#[test]
fn batch_scope_of_a_subquery_of_several_values_is_statement() {
    let def = message_template(512).expect("error 512 is in the catalog");
    assert_eq!(def.batch_scope(), BatchErrorScope::Statement);
}

/// One severity of each band of [`ErrorBehavior::of_severity`].
#[test]
fn default_behaviour_follows_the_severity() {
    let info = ErrorBehavior::of_severity(10);
    assert_eq!(info.scope, ErrorScope::Statement);
    assert!(!info.dooms_transaction && !info.xact_abort_widens);

    let syntax = ErrorBehavior::of_severity(15);
    assert_eq!(syntax.scope, ErrorScope::Level);
    assert_eq!(syntax.phase, ErrorPhase::Compilation);
    assert!(!syntax.dooms_transaction && !syntax.xact_abort_widens);

    for severity in [11, 14, 16, 19] {
        let run_time = ErrorBehavior::of_severity(severity);
        assert_eq!(run_time.scope, ErrorScope::Batch, "severity {severity}");
        assert_eq!(run_time.phase, ErrorPhase::Execution, "severity {severity}");
        assert!(run_time.dooms_transaction && run_time.xact_abort_widens);
        assert!(!run_time.rolls_back_transaction, "severity {severity}");
    }

    for severity in [20, 25] {
        assert_eq!(
            ErrorBehavior::of_severity(severity).scope,
            ErrorScope::Connection,
            "severity {severity}"
        );
    }
}

/// 8134 and 245 share severity 16 and state 1; the first stops its statement, the second
/// the batch.
#[test]
fn run_time_errors_of_one_severity_differ_in_scope() {
    assert_eq!(
        message_template(8134).map(|d| d.severity),
        message_template(245).map(|d| d.severity)
    );
    assert_eq!(error_behavior(8134).scope, ErrorScope::Statement);
    assert_eq!(error_behavior(245).scope, ErrorScope::Batch);
}

#[test]
fn the_caller_goes_on_after_a_statement_or_a_level() {
    assert!(!ErrorScope::Statement.stops_the_caller());
    assert!(!ErrorScope::Level.stops_the_caller());
    assert!(ErrorScope::Batch.stops_the_caller());
    assert!(ErrorScope::Connection.stops_the_caller());
}

#[test]
fn an_uncatalogued_number_stops_the_batch() {
    for number in [0, 50_000, 999_999] {
        assert!(message_template(number).is_none());
        let behavior = error_behavior(number);
        assert_eq!(behavior.scope, ErrorScope::Batch, "error {number}");
        assert_eq!(behavior.phase, ErrorPhase::Execution, "error {number}");
    }
}

/// Scope, phase, transaction doomed under `TRY…CATCH`, widened by `XACT_ABORT ON`, for the
/// errors whose behaviour applications rely on. Of these, 217 and 1205 roll the
/// transaction back by themselves.
#[test]
fn known_errors_carry_their_behaviour() {
    use ErrorPhase::{Compilation, Execution};
    use ErrorScope::{Batch, Level, Statement};
    let expected: &[(u32, ErrorScope, ErrorPhase, bool, bool)] = &[
        // Names resolved at compilation, or when the batch reaches the statement.
        (206, Level, Compilation, false, true),
        (207, Level, Compilation, false, true),
        (208, Level, Compilation, false, true),
        (4104, Level, Compilation, false, true),
        (4902, Level, Compilation, false, true),
        (209, Level, Compilation, false, true),
        (1011, Level, Compilation, false, true),
        (1012, Level, Compilation, false, true),
        (2715, Level, Compilation, false, true),
        (8120, Level, Compilation, false, true),
        // Collations an operator cannot resolve, found while the statement is bound.
        (457, Level, Compilation, false, true),
        (468, Level, Compilation, false, true),
        // Syntax and other compilation checks of severity 15.
        (102, Level, Compilation, false, false),
        (105, Level, Compilation, false, false),
        (133, Level, Compilation, false, false),
        (137, Level, Compilation, false, false),
        (155, Level, Compilation, false, false),
        (156, Level, Compilation, false, false),
        (178, Level, Compilation, false, false),
        (1038, Level, Compilation, false, false),
        (128, Level, Compilation, false, false),
        (1046, Level, Compilation, false, false),
        // The failing statement stops.
        (220, Statement, Execution, false, true),
        (512, Statement, Execution, false, true),
        (515, Statement, Execution, false, true),
        (547, Statement, Execution, false, true),
        (2627, Statement, Execution, false, true),
        (2812, Statement, Execution, false, true),
        (8134, Statement, Execution, false, true),
        (3701, Statement, Execution, false, false),
        (1222, Statement, Execution, false, true),
        (266, Statement, Execution, true, false),
        // The batch stops.
        (127, Batch, Execution, false, true),
        (217, Batch, Execution, true, true),
        (241, Batch, Execution, true, true),
        (245, Batch, Execution, true, true),
        (1014, Batch, Execution, true, true),
        (1031, Batch, Execution, true, true),
        (1088, Batch, Execution, true, true),
        (1205, Batch, Execution, true, true),
        (1750, Batch, Execution, true, true),
        (2714, Batch, Execution, true, true),
    ];
    for &(number, scope, phase, dooms, widens) in expected {
        assert!(message_template(number).is_some(), "error {number}");
        assert_eq!(
            error_behavior(number),
            ErrorBehavior {
                scope,
                phase,
                dooms_transaction: dooms,
                xact_abort_widens: widens,
                rolls_back_transaction: matches!(number, 217 | 1205),
            },
            "error {number}"
        );
    }
}

/// The entries that stop their statement and nothing more, with `XACT_ABORT OFF`.
#[test]
fn statement_scoped_entries() {
    let statement: Vec<u32> = CATALOG
        .iter()
        .filter(|def| def.severity > 10 && def.behavior().scope == ErrorScope::Statement)
        .map(|def| def.number)
        .collect();
    assert_eq!(
        statement,
        [
            220, 232, 237, 242, 244, 248, 266, 506, 512, 515, 517, 535, 536, 537, 544, 547, 1222,
            2601, 2627, 2628, 2812, 3623, 3701, 8115, 8134, 8152, 9806, 9807, 9809, 9810,
        ]
    );
}

/// An exception that equals the default of its severity says nothing: the entry drops it.
#[test]
fn an_exception_differs_from_the_default_of_its_severity() {
    for def in CATALOG {
        if let Some(behavior) = def.exception {
            assert_ne!(
                behavior,
                ErrorBehavior::of_severity(def.severity),
                "error {}",
                def.number
            );
        }
    }
}

/// The entries that roll the open transaction back by themselves, `XACT_ABORT OFF` included.
#[test]
fn a_call_past_the_nesting_limit_and_the_deadlock_victim_roll_their_transaction_back() {
    let rolling: Vec<u32> = CATALOG
        .iter()
        .filter(|def| def.behavior().rolls_back_transaction)
        .map(|def| def.number)
        .collect();
    assert_eq!(rolling, [217, 1205]);
    assert!(!error_behavior(1222).rolls_back_transaction);
}

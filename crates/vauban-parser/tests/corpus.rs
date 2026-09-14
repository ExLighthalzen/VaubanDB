//! The corpus of real-world batches and the parse → `Display` → parse loop over it.
//!
//! `tests/corpus/*.sql` holds whole batches written in the style of what an ORM, a
//! micro-ORM, a migration tool, a reporting query, a maintenance script or SSMS sends.
//! Each batch is legal T-SQL. The files are discovered by reading the directory: there is
//! no list to keep in step with it.
//!
//! # Batch format
//!
//! A line `-- @batch <name>` opens a batch; everything up to the next such line, or to the
//! end of the file, belongs to it -- comments included, on purpose, because SQL Server
//! counts comment lines when it reports an error line. Lines before the first
//! marker are the file header and belong to no batch. This is a format local to this
//! corpus.
//!
//! # What is compared, and what is not
//!
//! **No test here compares a re-serialisation to the source file.** Some clauses are read
//! and thrown away by the V1 grammar (`WITH (…)` and `ON [PRIMARY]` of a `CREATE TABLE` or
//! a `CREATE INDEX`, `PERSISTED` of a computed column, `OUTER` of a `LEFT OUTER JOIN`, the
//! `ALL` of a `SELECT ALL`), so the first re-serialisation legitimately differs from the
//! text of the file. What must hold is that the **AST** survives a round trip unchanged,
//! and that the re-serialisation is a **fixpoint**: printing the re-parsed tree gives the
//! same text at the first round, not after a few.
//!
//! # Batches the parser still refuses
//!
//! A batch SQL Server accepts and the parser refuses is a defect of the grammar, listed in
//! [`EXPECTED_FAILURES`] with its reason. It stays in the
//! corpus and [`corpus_parses`] asserts that it **still fails**: the day the grammar
//! accepts it, the test goes red so that the entry is removed and the loop tests start
//! covering it.

use std::fs;
use std::path::{Path, PathBuf};

use vauban_parser::{Batch, ParseOptions, Statement, parse_batch};

// ---------------------------------------------------------------------------
// Batches the parser refuses today, and why.
// ---------------------------------------------------------------------------

/// `(batch name, reason)`. Each entry is a form SQL Server 2022 accepts and the V1
/// grammar does not. The reason names the offending form.
const EXPECTED_FAILURES: &[(&str, &str)] = &[
    // Empty: the seven SSMS forms (`WITH (…) ON [PRIMARY]` after a key constraint,
    // `TEXTIMAGE_ON`, `ADD CONSTRAINT … DEFAULT … FOR`, `WITH CHECK ADD`,
    // `DROP INDEX … WITH (…)`) are read. The mechanism stays: the next refused batch goes
    // here.
];

/// The reason a batch is expected to fail, when it is.
fn expected_failure(name: &str) -> Option<&'static str> {
    EXPECTED_FAILURES
        .iter()
        .find(|(batch, _)| *batch == name)
        .map(|(_, reason)| *reason)
}

// ---------------------------------------------------------------------------
// Reading the corpus.
// ---------------------------------------------------------------------------

/// One batch of one corpus file.
struct CorpusBatch {
    /// File name, without its directory.
    file: String,
    /// The `<name>` of the `-- @batch <name>` line.
    name: String,
    /// 1-based line of the `-- @batch` marker in the file.
    line: usize,
    /// The text of the batch, marker excluded.
    text: String,
}

impl CorpusBatch {
    /// `file:line (name)`, the way every failure message names a batch.
    fn label(&self) -> String {
        format!("{}:{} ({})", self.file, self.line, self.name)
    }
}

/// The directory of the corpus, next to this file.
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

/// Every `*.sql` file of the corpus directory, in name order. Discovered, not listed.
fn corpus_files() -> Vec<PathBuf> {
    let dir = corpus_dir();
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .sql file in {}", dir.display());
    files
}

/// Cuts one file into its batches.
fn read_batches(path: &Path) -> Vec<CorpusBatch> {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("corpus file names are UTF-8")
        .to_owned();
    let content =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut batches: Vec<CorpusBatch> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if let Some(name) = line.strip_prefix("-- @batch ") {
            batches.push(CorpusBatch {
                file: file.clone(),
                name: name.trim().to_owned(),
                line: index + 1,
                text: String::new(),
            });
        } else if let Some(current) = batches.last_mut() {
            current.text.push_str(line);
            current.text.push('\n');
        }
    }
    batches
}

/// Every batch of every file.
fn load_corpus() -> Vec<CorpusBatch> {
    corpus_files()
        .iter()
        .flat_map(|path| read_batches(path))
        .collect()
}

/// `number: message (line n)`, what a failure message says of an error.
fn describe(error: &vauban_errors::SqlError) -> String {
    format!("{}: {} (line {})", error.number, error.message, error.line)
}

/// The batches the parser is expected to accept, each with its tree. Panics, naming the
/// batch, on the first one it refuses: [`corpus_parses`] is the test that lists them all.
fn accepted_batches() -> Vec<(CorpusBatch, Batch)> {
    load_corpus()
        .into_iter()
        .filter(|batch| expected_failure(&batch.name).is_none())
        .map(|batch| {
            let tree = parse_batch(&batch.text, &ParseOptions::default())
                .unwrap_or_else(|e| panic!("{} does not parse: {}", batch.label(), describe(&e)));
            (batch, tree)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shape of the corpus.
// ---------------------------------------------------------------------------

/// At least six files and thirty batches.
#[test]
fn corpus_has_at_least_six_files_and_thirty_batches() {
    let files = corpus_files();
    assert!(
        files.len() >= 6,
        "{} corpus files, expected at least 6",
        files.len()
    );
    let batches = load_corpus();
    assert!(
        batches.len() >= 30,
        "{} batches in the corpus, expected at least 30",
        batches.len()
    );
    for batch in &batches {
        assert!(
            !batch.text.trim().is_empty(),
            "{} is an empty batch",
            batch.label()
        );
    }
}

/// UTF-8 without a byte-order mark, ending with a line feed.
#[test]
fn corpus_files_are_utf8_without_bom() {
    for path in corpus_files() {
        let bytes = fs::read(&path).expect("readable corpus file");
        assert!(
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "{} starts with a UTF-8 BOM",
            path.display()
        );
        assert!(
            std::str::from_utf8(&bytes).is_ok(),
            "{} is not valid UTF-8",
            path.display()
        );
        assert!(
            bytes.last() == Some(&b'\n'),
            "{} does not end with a line feed",
            path.display()
        );
    }
}

/// At least five batches of three statements or more: the corpus is made of batches, not
/// of one-liners.
#[test]
fn corpus_has_multi_statement_batches() {
    let long: Vec<String> = accepted_batches()
        .iter()
        .filter(|(_, tree)| tree.statements.len() >= 3)
        .map(|(batch, _)| batch.name.clone())
        .collect();
    assert!(
        long.len() >= 5,
        "only {} batches with three statements or more: {long:?}",
        long.len()
    );
}

// ---------------------------------------------------------------------------
// Acceptance.
// ---------------------------------------------------------------------------

/// Every batch parses, except the ones listed in [`EXPECTED_FAILURES`], which must still
/// fail. Both kinds of surprise are collected and reported together.
#[test]
fn corpus_parses() {
    let mut failures = Vec::new();
    for batch in load_corpus() {
        let result = parse_batch(&batch.text, &ParseOptions::default());
        match (expected_failure(&batch.name), result) {
            (None, Ok(_)) => {}
            (None, Err(error)) => failures.push(format!(
                "{} does not parse: {}",
                batch.label(),
                describe(&error)
            )),
            (Some(reason), Err(_)) => {
                // Still refused, as expected. The reason is not checked against the error:
                // the entry documents the defect, it does not pin the message.
                let _ = reason;
            }
            (Some(reason), Ok(_)) => failures.push(format!(
                "{} now parses: remove it from EXPECTED_FAILURES (was: {reason})",
                batch.label()
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// The loop: parse → Display → parse.
// ---------------------------------------------------------------------------

/// The re-serialisation parses into the **same** tree. Spans compare equal by design
/// (`Span::eq` is always true), so this is a structural comparison.
#[test]
fn corpus_roundtrip_ast() {
    for (batch, tree) in accepted_batches() {
        let printed = tree.to_string();
        let reparsed = parse_batch(&printed, &ParseOptions::default()).unwrap_or_else(|e| {
            panic!(
                "{}: the re-serialisation does not parse: {}\n--- printed ---\n{printed}",
                batch.label(),
                describe(&e)
            )
        });
        assert!(
            reparsed == tree,
            "{}: the re-parsed tree differs\n--- printed ---\n{printed}\n--- original ---\n{tree:#?}\n--- reparsed ---\n{reparsed:#?}",
            batch.label()
        );
    }
}

/// Printing the re-parsed tree gives the same text as the first printing: the
/// re-serialisation is stable at the first round, not merely convergent.
#[test]
fn corpus_roundtrip_is_a_fixpoint() {
    for (batch, tree) in accepted_batches() {
        let first = tree.to_string();
        let reparsed = parse_batch(&first, &ParseOptions::default()).unwrap_or_else(|e| {
            panic!(
                "{}: the re-serialisation does not parse: {}",
                batch.label(),
                describe(&e)
            )
        });
        let second = reparsed.to_string();
        assert!(
            first == second,
            "{}: the re-serialisation is not a fixpoint\n--- first ---\n{first}\n--- second ---\n{second}",
            batch.label()
        );
    }
}

// ---------------------------------------------------------------------------
// Property of the printed text: no two tokens glue into a third.
// ---------------------------------------------------------------------------

/// The multi-character tokens the lexer builds from sign characters, read from
/// `src/lexer.rs` (`fn operator` and the comment skipping of `fn next_token`): the
/// assignment operators, the comparison operators, `::`, and the two comment openers
/// plus the block-comment closer. Anything printed by `Display` that contains one of
/// these by accident -- two neighbouring tokens written without a space between them --
/// changes the meaning of the text, and `--` even truncates it (`tests/expr.rs`).
const COMMENT_TOKENS: &[&str] = &["--", "/*", "*/"];
const COMPOUND_OPERATORS: &[&str] = &[
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<>", "<=", ">=", "!=", "!<", "!>", "::",
];

/// Replaces the inside of every `'…'` string literal, `[…]` bracketed name and `"…"`
/// quoted name by spaces, keeping every other character at its offset, so that the
/// checks below never look inside a literal.
fn blank_literals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let closer = match c {
            '\'' => '\'',
            '[' => ']',
            '"' => '"',
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push(c);
        // A doubled closer (`''`, `]]`, `""`) escapes it.
        while let Some(inner) = chars.next() {
            if inner == closer {
                if chars.peek() == Some(&closer) {
                    chars.next();
                    out.push(' ');
                    out.push(' ');
                    continue;
                }
                out.push(closer);
                break;
            }
            out.push(if inner == '\n' { '\n' } else { ' ' });
        }
    }
    out
}

/// Outside literals, the printed text contains no comment token at all, and every
/// compound operator it contains is a token of its own -- written with whitespace on both
/// sides, which is how `Display` writes every binary and assignment operator -- never
/// the tail of one token glued to the head of the next.
#[test]
fn corpus_display_never_glues_tokens_into_another() {
    let mut failures = Vec::new();
    for (batch, tree) in accepted_batches() {
        let printed = tree.to_string();
        let blanked = blank_literals(&printed);
        for token in COMMENT_TOKENS {
            if let Some(offset) = blanked.find(token) {
                failures.push(format!(
                    "{}: `{token}` at byte {offset} of the printed text: {}",
                    batch.label(),
                    excerpt(&printed, offset)
                ));
            }
        }
        for op in COMPOUND_OPERATORS {
            let mut from = 0;
            while let Some(found) = blanked[from..].find(op) {
                let offset = from + found;
                let before = blanked[..offset].chars().next_back();
                let after = blanked[offset + op.len()..].chars().next();
                let spaced = |c: Option<char>| c.is_none_or(char::is_whitespace);
                if !(spaced(before) && spaced(after)) {
                    failures.push(format!(
                        "{}: `{op}` glued to a neighbour at byte {offset} of the printed text: {}",
                        batch.label(),
                        excerpt(&printed, offset)
                    ));
                }
                from = offset + op.len();
            }
        }
    }
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

/// A short window of `text` around `offset`, for a failure message.
fn excerpt(text: &str, offset: usize) -> String {
    let start = text[..offset]
        .char_indices()
        .rev()
        .nth(20)
        .map_or(0, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .nth(20)
        .map_or(text.len(), |(i, _)| offset + i);
    format!("…{}…", &text[start..end]).replace('\n', "⏎")
}

/// The guard above must catch a `--` formed by two glued minus signs.
#[test]
fn blank_literals_keeps_offsets_and_hides_literals() {
    let text = "SELECT '--' AS [a--b], \"x/*y\", - -1 --";
    let blanked = blank_literals(text);
    assert_eq!(blanked.len(), text.len());
    assert_eq!(blanked, "SELECT '  ' AS [    ], \"    \", - -1 --");
    // Seven characters in, seven out: the doubled quote becomes two spaces.
    assert_eq!(blank_literals("'it''s'"), "'     '");
}

// ---------------------------------------------------------------------------
// Coverage of the statement kinds.
// ---------------------------------------------------------------------------

/// The name of the variant of `statement`, for the ones the corpus must cover.
fn kind_name(statement: &Statement) -> &'static str {
    match statement {
        Statement::Select(_) => "Select",
        Statement::Insert(_) => "Insert",
        Statement::Update(_) => "Update",
        Statement::Delete(_) => "Delete",
        Statement::Truncate { .. } => "Truncate",
        Statement::CreateTable(_) => "CreateTable",
        Statement::AlterTable(_) => "AlterTable",
        Statement::DropTable { .. } => "DropTable",
        Statement::CreateIndex(_) => "CreateIndex",
        Statement::DropIndex(_) => "DropIndex",
        Statement::CreateDatabase(_) => "CreateDatabase",
        Statement::AlterDatabase(_) => "AlterDatabase",
        Statement::DropDatabase { .. } => "DropDatabase",
        Statement::Use { .. } => "Use",
        Statement::Declare(_) => "Declare",
        Statement::Set(_) => "Set",
        Statement::SetOption(_) => "SetOption",
        Statement::If { .. } => "If",
        Statement::While { .. } => "While",
        Statement::Block { .. } => "Block",
        Statement::Break(_) => "Break",
        Statement::Continue(_) => "Continue",
        Statement::Return { .. } => "Return",
        Statement::BeginTransaction { .. } => "BeginTransaction",
        Statement::Commit { .. } => "Commit",
        Statement::Rollback { .. } => "Rollback",
        Statement::Save { .. } => "Save",
        Statement::Print { .. } => "Print",
        Statement::Execute(_) => "Execute",
        _ => "other",
    }
}

/// Records the kind of `statement` and of every statement nested in it (`IF` branches,
/// `WHILE` body, `BEGIN … END` block).
fn collect_kinds<'a>(statement: &Statement, seen: &mut Vec<&'a str>)
where
    'static: 'a,
{
    let kind = kind_name(statement);
    if !seen.contains(&kind) {
        seen.push(kind);
    }
    match statement {
        Statement::If {
            then_branch,
            else_branch,
            ..
        } => {
            collect_kinds(then_branch, seen);
            if let Some(else_branch) = else_branch {
                collect_kinds(else_branch, seen);
            }
        }
        Statement::While { body, .. } => collect_kinds(body, seen),
        Statement::Block { statements, .. } => {
            for nested in statements {
                collect_kinds(nested, seen);
            }
        }
        _ => {}
    }
}

/// The corpus produces every statement kind of the V1 subset at least once, nested ones
/// included. The failure names the missing kinds.
#[test]
fn corpus_covers_every_statement_kind() {
    const REQUIRED: &[&str] = &[
        "Select",
        "Insert",
        "Update",
        "Delete",
        "Truncate",
        "CreateTable",
        "AlterTable",
        "DropTable",
        "CreateIndex",
        "CreateDatabase",
        "Use",
        "Declare",
        "Set",
        "SetOption",
        "If",
        "While",
        "Block",
        "BeginTransaction",
        "Commit",
        "Rollback",
        "Print",
        "Execute",
    ];
    let mut seen = Vec::new();
    for (_, tree) in accepted_batches() {
        for statement in &tree.statements {
            collect_kinds(statement, &mut seen);
        }
    }
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|kind| !seen.contains(kind))
        .collect();
    assert!(
        missing.is_empty(),
        "statement kinds missing from the corpus: {missing:?} (seen: {seen:?})"
    );
}

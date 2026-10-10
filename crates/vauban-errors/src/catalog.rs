//! The static catalogue of the errors VaubanDB raises.
//!
//! One entry per error number, with its default severity, its message template and what it
//! stops ([`ErrorBehavior`]: scope, phase, effect on the transaction). The
//! number, the severity and the state of an error are those SQL Server sends for the same
//! situation, because applications compare them; the text is VaubanDB's own wording.
//!
//! Templates use printf-like specifiers (`%.*ls`, `%ls`, `%s`, `%hs`, `%d`, `%ld`,
//! `%I64d`, `%f`, `%S_MSG`, `%%`), substituted in order by the named constructors of
//! `constructors.rs` through `format::format_message`. The count and the order of the
//! specifiers of a template are part of its contract: `tests::catalog_specifiers_are_the_contract`
//! lists them per number, and a constructor fills them in that order.
//!
//! The catalogue does not carry the `state`: it depends on the code path that raises the
//! error and is set by each constructor. Likewise, `severity` is the *default* severity of
//! the number; a constructor may send another one (`format::from_catalog_with_severity`),
//! and 5701 is emitted with severity 0.

/// The definition of one error: number, default severity, message template and what the
/// error stops.
///
/// `template` keeps its specifiers unsubstituted. See the module documentation for what
/// the catalogue does and does not carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDef {
    /// Error number.
    pub number: u32,
    /// Default severity, `0..=25`.
    pub severity: u8,
    /// Message template in English, specifiers left as-is.
    pub template: &'static str,
    /// What the error stops, when it differs from the default of its severity
    /// ([`ErrorBehavior::of_severity`]); `None` keeps that default. Read it through
    /// [`ErrorDef::behavior`].
    pub exception: Option<ErrorBehavior>,
}

/// How far an error stops the code that raised it.
///
/// A procedure, a dynamic text (`EXEC('…')`, `sp_executesql`) and a batch run on its own
/// are each a level; a call runs one level below its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorScope {
    /// The failing statement stops; the next statement of its level runs
    /// (8134: `SELECT 1; SELECT 1/0; SELECT 2;` answers both rows).
    Statement,
    /// The level of the failing statement stops; the caller of a procedure or of a dynamic
    /// text runs its next statement. For a batch run on its own, the level is the batch
    /// (208 on a table the batch has not created: `EXEC('SELECT * FROM dbo.missing');
    /// SELECT 2;` answers the 2).
    Level,
    /// The batch stops, with the procedures and dynamic texts it called
    /// (245: `EXEC('SELECT CAST(''abc'' AS int)'); SELECT 2;` does not answer the 2).
    Batch,
    /// The connection is closed.
    Connection,
}

impl ErrorScope {
    /// Whether an error of this scope, raised inside a procedure or a dynamic text, stops
    /// the caller too (`tests::the_caller_goes_on_after_a_statement_or_a_level`).
    pub const fn stops_the_caller(self) -> bool {
        matches!(self, Self::Batch | Self::Connection)
    }
}

/// When an error is raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    /// While the statement is compiled: parsed, or its names resolved, including a name
    /// resolved late because the batch creates its object. A `TRY` block of the level
    /// that compiles the statement does not catch it; the `TRY` block of a caller does.
    Compilation,
    /// While the statement runs.
    Execution,
}

/// What an error does to the code that raised it and to the open transaction.
///
/// The default comes from the severity ([`ErrorBehavior::of_severity`]); an entry of the
/// catalogue that behaves otherwise carries its own value in [`ErrorDef::exception`].
/// Severity and state do not separate the families: 8134 and 245 are both severity 16,
/// state 1, while 8134 stops its statement and 245 stops the batch
/// (`tests::run_time_errors_of_one_severity_differ_in_scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorBehavior {
    /// What the error stops, with `SET XACT_ABORT OFF`.
    pub scope: ErrorScope,
    /// When the error is raised. The session does not read it yet: it describes the
    /// reference behaviour that a `TRY…CATCH` will follow.
    pub phase: ErrorPhase,
    /// Whether the error, caught by `TRY…CATCH` inside a transaction, leaves that
    /// transaction uncommittable (`XACT_STATE()` answers -1 in the `CATCH` block).
    pub dooms_transaction: bool,
    /// Whether `SET XACT_ABORT ON` widens the error to the batch, the callers included,
    /// and rolls the transaction back, in the reference behaviour. The session does not
    /// read it yet: under `XACT_ABORT ON` it stops the batch after an error of either
    /// scope (`xact_abort_stops_a_statement_scoped_runtime_error`), where this field
    /// says `false` too.
    pub xact_abort_widens: bool,
    /// Whether the error rolls the open transaction back by itself, `SET XACT_ABORT OFF`
    /// included: the victim of a deadlock (1205) loses its transaction and its writes, and
    /// `@@TRANCOUNT` reads 0 afterwards (`vauban-session`, `tests/deadlock_victim.rs`). A
    /// fatal error closes the connection, which rolls back on its own, so its default
    /// leaves this to `false`. In the reference behaviour a `TRY…CATCH` dooms that
    /// transaction instead, and the rollback comes when the batch ends; the session has no
    /// `TRY…CATCH` yet.
    pub rolls_back_transaction: bool,
}

/// An error that stops its statement while it runs: constraints, truncation, arithmetic.
const STATEMENT: ErrorBehavior = ErrorBehavior {
    scope: ErrorScope::Statement,
    phase: ErrorPhase::Execution,
    dooms_transaction: false,
    xact_abort_widens: true,
    rolls_back_transaction: false,
};

/// A statement that fails to bind, at compilation or when the batch reaches it: a name it
/// cannot resolve (208, 207, 4104, 4902), an ambiguous one (209, 1011, 1012), an unknown
/// type (2715), a column outside `GROUP BY` (8120), an operand type clash (206). Unlike a
/// syntax error, `SET XACT_ABORT ON` widens it to the caller in the reference behaviour.
const BINDING: ErrorBehavior = ErrorBehavior {
    scope: ErrorScope::Level,
    phase: ErrorPhase::Compilation,
    dooms_transaction: false,
    xact_abort_widens: true,
    rolls_back_transaction: false,
};

/// The default of severities 11 to 14 and 16 to 19. Entries of another severity carry it
/// as their exception when they stop the batch: 1014, 1031 and 1088 raised while a statement
/// runs (`DECLARE @n float = NULL; SELECT TOP (@n) PERCENT 1;` is 1014), and 1750, which
/// follows the error of a constraint `CREATE TABLE` could not create.
const BATCH: ErrorBehavior = ErrorBehavior {
    scope: ErrorScope::Batch,
    phase: ErrorPhase::Execution,
    dooms_transaction: true,
    xact_abort_widens: true,
    rolls_back_transaction: false,
};

impl ErrorBehavior {
    /// The behaviour of an error of `severity` whose entry leaves `exception` to `None`.
    ///
    /// - `0..=10`: an informational message; it stops nothing.
    /// - `15`: a syntax error, found while the statement is compiled. It stops its level
    ///   and leaves the transaction committable; in the reference behaviour,
    ///   `SET XACT_ABORT ON` does not widen it (see [`ErrorBehavior::xact_abort_widens`]
    ///   for what the session does today).
    /// - `11..=14` and `16..=19`: a run-time error that stops the batch and dooms the
    ///   transaction. This default is provisional for the compilation errors of these
    ///   severities whose entry leaves `exception` to `None`: the binding errors characterised so far
    ///   stop their level instead and carry their own exception
    ///   (`tests::known_errors_carry_their_behaviour`).
    /// - `20..`: a fatal error; it closes the connection.
    ///
    /// `tests::default_behaviour_follows_the_severity` pins one severity of each band.
    pub const fn of_severity(severity: u8) -> Self {
        match severity {
            0..=10 => Self {
                scope: ErrorScope::Statement,
                phase: ErrorPhase::Execution,
                dooms_transaction: false,
                xact_abort_widens: false,
                rolls_back_transaction: false,
            },
            15 => Self {
                scope: ErrorScope::Level,
                phase: ErrorPhase::Compilation,
                dooms_transaction: false,
                xact_abort_widens: false,
                rolls_back_transaction: false,
            },
            20.. => Self {
                scope: ErrorScope::Connection,
                phase: ErrorPhase::Execution,
                dooms_transaction: true,
                xact_abort_widens: true,
                rolls_back_transaction: false,
            },
            _ => BATCH,
        }
    }
}

/// What SQL Server stops after an error raised while executing a statement, in two
/// families: the statement, or more.
///
/// The two-way view of [`ErrorScope`] that the interpreter of the executor reads:
/// [`ErrorScope::Statement`] maps to `Statement`, the three wider scopes to `Batch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchErrorScope {
    /// Only the failing statement stops; a following statement in the batch still runs.
    Statement,
    /// The failing statement and everything after it in the batch stop.
    Batch,
}

impl ErrorDef {
    /// What this error stops, its phase and its effect on the transaction: the entry's
    /// [`ErrorDef::exception`], or the default of its severity.
    pub const fn behavior(&self) -> ErrorBehavior {
        match self.exception {
            Some(behavior) => behavior,
            None => ErrorBehavior::of_severity(self.severity),
        }
    }

    /// [`ErrorDef::behavior`] read as a [`BatchErrorScope`].
    pub const fn batch_scope(&self) -> BatchErrorScope {
        match self.behavior().scope {
            ErrorScope::Statement => BatchErrorScope::Statement,
            ErrorScope::Level | ErrorScope::Batch | ErrorScope::Connection => {
                BatchErrorScope::Batch
            }
        }
    }
}

/// What an error of `number` stops, read from the catalogue without building the error.
///
/// A number the catalogue does not hold, such as the internal error 50000, stops the
/// batch (`tests::an_uncatalogued_number_stops_the_batch`).
pub fn error_behavior(number: u32) -> ErrorBehavior {
    message_template(number).map_or(BATCH, ErrorDef::behavior)
}

/// Looks up the catalogue entry for `number`.
///
/// Returns `None` for numbers VaubanDB does not raise. Binary search: the catalogue is
/// sorted by number, which `tests::catalog_is_sorted_and_unique` enforces.
pub fn message_template(number: u32) -> Option<&'static ErrorDef> {
    CATALOG
        .binary_search_by_key(&number, |def| def.number)
        .ok()
        .map(|index| &CATALOG[index])
}

/// The catalogue, sorted by strictly increasing `number`.
///
/// Keep it sorted when adding entries.
const CATALOG: &[ErrorDef] = &[
    ErrorDef {
        number: 102,
        severity: 15,
        template: "Syntax error near '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 103,
        severity: 15,
        template: "The %S_MSG beginning with '%.*ls' exceeds the maximum length of %d.",
        exception: None,
    },
    ErrorDef {
        number: 105,
        severity: 15,
        template: "Quotation mark left open after the string '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 107,
        severity: 15,
        template: "The column prefix '%.*ls' matches no table or alias of the query.",
        exception: None,
    },
    ErrorDef {
        number: 109,
        severity: 15,
        template: "The INSERT column list names more columns than the VALUES row supplies; the two counts have to match.",
        exception: None,
    },
    ErrorDef {
        number: 110,
        severity: 15,
        template: "The INSERT column list names fewer columns than the VALUES row supplies; the two counts have to match.",
        exception: None,
    },
    ErrorDef {
        number: 113,
        severity: 15,
        template: "Comment is not closed: '*/' expected.",
        exception: None,
    },
    ErrorDef {
        number: 116,
        severity: 15,
        template: "A subquery not introduced by EXISTS must select a single expression.",
        exception: None,
    },
    ErrorDef {
        number: 117,
        severity: 15,
        template: "The %S_MSG name '%.*ls' has too many prefixes; at most %d are allowed.",
        exception: None,
    },
    ErrorDef {
        number: 119,
        severity: 15,
        template: "Parameter number %d and the ones after it have to use the '@name = value' form; once that form has been used, a positional argument may not follow it.",
        exception: None,
    },
    ErrorDef {
        number: 120,
        severity: 15,
        template: "The select list of the INSERT supplies fewer items than its column list; the two counts have to match.",
        exception: None,
    },
    ErrorDef {
        number: 121,
        severity: 15,
        template: "The select list of the INSERT supplies more items than its column list; the two counts have to match.",
        exception: None,
    },
    ErrorDef {
        number: 127,
        severity: 15,
        template: "The row count of TOP or FETCH cannot be negative.",
        // A negative row count read from a variable stops the batch, and the transaction
        // stays committable: `DECLARE @n int = -1; SELECT TOP (@n) 1;`.
        exception: Some(ErrorBehavior {
            dooms_transaction: false,
            ..BATCH
        }),
    },
    ErrorDef {
        number: 130,
        severity: 15,
        template: "An aggregate function cannot be applied to an expression that holds an aggregate or a subquery.",
        exception: None,
    },
    ErrorDef {
        number: 131,
        severity: 15,
        template: "Size %d of the %S_MSG '%.*ls' is larger than any data type allows (%d).",
        exception: None,
    },
    ErrorDef {
        number: 133,
        severity: 15,
        template: "GOTO names the label '%.*ls', which this batch does not declare.",
        exception: None,
    },
    ErrorDef {
        number: 134,
        severity: 15,
        template: "The variable '%.*ls' is already declared; a batch or a procedure declares each name once.",
        exception: None,
    },
    ErrorDef {
        number: 135,
        severity: 15,
        template: "BREAK is written where no WHILE loop encloses it.",
        exception: None,
    },
    ErrorDef {
        number: 136,
        severity: 15,
        template: "CONTINUE is written where no WHILE loop encloses it.",
        exception: None,
    },
    ErrorDef {
        number: 137,
        severity: 15,
        template: "The scalar variable \"%.*ls\" is not declared.",
        exception: None,
    },
    ErrorDef {
        number: 141,
        severity: 15,
        template: "A SELECT that assigns variables cannot also return rows.",
        exception: None,
    },
    ErrorDef {
        number: 144,
        severity: 15,
        template: "A GROUP BY expression cannot hold an aggregate or a subquery.",
        exception: None,
    },
    ErrorDef {
        number: 145,
        severity: 15,
        template: "With SELECT DISTINCT, each ORDER BY item has to be in the select list.",
        exception: None,
    },
    ErrorDef {
        number: 147,
        severity: 15,
        template: "An aggregate cannot appear in a WHERE clause, except inside a subquery of a HAVING clause or a select list, aggregating an outer reference.",
        exception: None,
    },
    ErrorDef {
        number: 148,
        severity: 15,
        template: "WAITFOR received a time string '%.*ls' that is not valid.",
        exception: None,
    },
    ErrorDef {
        number: 151,
        severity: 15,
        template: "'%.*ls' cannot be read as a money value.",
        exception: None,
    },
    ErrorDef {
        number: 155,
        severity: 15,
        template: "'%.*ls' is not a known %ls option.",
        exception: None,
    },
    ErrorDef {
        number: 156,
        severity: 15,
        template: "Syntax error near the keyword '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 168,
        severity: 15,
        template: "The floating point literal '%.*ls' cannot be represented in %d bytes.",
        exception: None,
    },
    ErrorDef {
        number: 174,
        severity: 15,
        template: "The function %.*ls takes exactly %d argument(s).",
        exception: None,
    },
    ErrorDef {
        number: 178,
        severity: 15,
        template: "A return value needs a procedure to return from; this statement is not inside one.",
        exception: None,
    },
    ErrorDef {
        number: 179,
        severity: 15,
        template: "The OUTPUT option cannot be used on a constant argument of a procedure.",
        exception: None,
    },
    ErrorDef {
        number: 189,
        severity: 15,
        template: "The function %ls takes between %d and %d arguments.",
        exception: None,
    },
    ErrorDef {
        number: 191,
        severity: 15,
        template: "The statement is nested too deeply; split it into smaller queries.",
        exception: None,
    },
    ErrorDef {
        number: 192,
        severity: 16,
        template: "Scale cannot exceed precision.",
        exception: None,
    },
    ErrorDef {
        number: 195,
        severity: 15,
        template: "'%.*ls' is not a known %S_MSG.",
        exception: None,
    },
    ErrorDef {
        number: 201,
        severity: 16,
        template: "Procedure '%.*ls' was called without the parameter '%.*ls' it requires.",
        exception: None,
    },
    ErrorDef {
        number: 205,
        severity: 16,
        template: "Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions.",
        exception: None,
    },
    ErrorDef {
        number: 206,
        severity: 16,
        template: "Type mismatch: %ls cannot be combined with %ls.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 207,
        severity: 16,
        template: "Unknown column name '%.*ls'.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 208,
        severity: 16,
        template: "Unknown object name '%.*ls'.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 209,
        severity: 16,
        template: "Column name '%.*ls' is ambiguous.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 210,
        severity: 16,
        template: "A binary or varbinary value could not be converted to datetime.",
        exception: None,
    },
    ErrorDef {
        number: 213,
        severity: 16,
        template: "The supplied values do not match the columns of the table.",
        exception: None,
    },
    ErrorDef {
        number: 214,
        severity: 16,
        template: "The parameter '%ls' has to be of type '%ls' for this procedure.",
        exception: None,
    },
    ErrorDef {
        number: 215,
        severity: 16,
        template: "Object '%.*ls' is not a function and takes no parameters; a table hint needs the WITH keyword.",
        exception: None,
    },
    ErrorDef {
        number: 220,
        severity: 16,
        template: "Value out of range for data type %ls: %ld.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 226,
        severity: 16,
        template: "%ls is not allowed inside a multi-statement transaction.",
        exception: None,
    },
    ErrorDef {
        number: 232,
        severity: 16,
        template: "Value out of range for type %ls: %f.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 234,
        severity: 16,
        template: "A money value does not fit in the result type %ls.",
        exception: None,
    },
    ErrorDef {
        number: 235,
        severity: 16,
        template: "The character value is not a valid money literal and could not be converted.",
        exception: None,
    },
    ErrorDef {
        number: 237,
        severity: 16,
        template: "A money value does not fit in the result type %ls.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 241,
        severity: 16,
        template: "The character string could not be converted to a date or time.",
        exception: None,
    },
    ErrorDef {
        number: 242,
        severity: 16,
        template: "Converting %ls to %ls produced a value outside the target range.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 243,
        severity: 16,
        template: "%.*ls is not a known system type.",
        exception: None,
    },
    ErrorDef {
        number: 244,
        severity: 16,
        template: "The %ls value '%.*ls' does not fit in an %hs column; a wider integer type is needed.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 245,
        severity: 16,
        template: "The %ls value '%.*ls' could not be converted to data type %ls.",
        exception: None,
    },
    ErrorDef {
        number: 248,
        severity: 16,
        template: "The %ls value '%.*ls' does not fit in an int column.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 257,
        severity: 16,
        template: "No implicit conversion from %ls to %ls; use CONVERT explicitly.",
        exception: None,
    },
    ErrorDef {
        number: 263,
        severity: 16,
        template: "No table to select from.",
        exception: None,
    },
    ErrorDef {
        number: 264,
        severity: 16,
        template: "Column '%.*ls' is named more than once in the column list of the INSERT or the SET clause of the UPDATE; a column takes one value per statement.",
        exception: None,
    },
    ErrorDef {
        number: 266,
        severity: 16,
        template: "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before %ld, after %ld).",
        // Raised in the caller once the call returns, and the caller goes on:
        // `EXEC('BEGIN TRAN'); SELECT 2;` answers the 2. The reference behaviour keeps
        // going under `XACT_ABORT ON` as well, which the session does not follow yet.
        exception: Some(ErrorBehavior {
            dooms_transaction: true,
            xact_abort_widens: false,
            ..STATEMENT
        }),
    },
    ErrorDef {
        number: 271,
        severity: 16,
        template: "The column \"%.*ls\" cannot be modified: it is computed, or it comes out of a UNION.",
        exception: None,
    },
    ErrorDef {
        number: 281,
        severity: 16,
        template: "Style %d is not defined for converting %ls to a character string.",
        exception: None,
    },
    ErrorDef {
        number: 289,
        severity: 16,
        template: "Data type %ls cannot be built from these arguments: at least one value is out of range.",
        exception: None,
    },
    ErrorDef {
        number: 292,
        severity: 16,
        template: "A smallmoney value does not fit in the result type %ls.",
        exception: None,
    },
    ErrorDef {
        number: 295,
        severity: 16,
        template: "The character string could not be converted to smalldatetime.",
        exception: None,
    },
    ErrorDef {
        number: 339,
        severity: 16,
        template: "DEFAULT and NULL cannot be given as explicit identity values.",
        exception: None,
    },
    ErrorDef {
        number: 402,
        severity: 16,
        template: "The types %s and %s cannot be combined by the %s operator.",
        exception: None,
    },
    ErrorDef {
        number: 447,
        severity: 16,
        template: "COLLATE cannot apply to an expression of type %ls.",
        exception: None,
    },
    ErrorDef {
        number: 448,
        severity: 16,
        template: "Unknown collation '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 506,
        severity: 16,
        template: "The escape character \"%.*ls\" of the %ls predicate must be a single character.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 512,
        severity: 16,
        template: "The subquery returned several values, which is not allowed after a comparison operator or as an expression.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 515,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' does not accept NULL; %ls fails.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 517,
        severity: 16,
        template: "The addition overflowed the '%ls' column.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 529,
        severity: 16,
        template: "No explicit conversion exists from %ls to %ls.",
        exception: None,
    },
    ErrorDef {
        number: 535,
        severity: 16,
        template: "%.*ls overflowed: too many dateparts separate the two instants. Call %.*ls with a coarser datepart.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 536,
        severity: 16,
        template: "The length given to the %ls function is not valid.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 537,
        severity: 16,
        template: "The length given to LEFT or SUBSTRING is not valid.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 544,
        severity: 16,
        template: "An explicit value for the identity column of table '%.*ls' requires IDENTITY_INSERT ON.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 547,
        severity: 16,
        template: "%ls violates the %ls constraint \"%.*ls\" in database \"%.*ls\", table \"%.*ls\"%ls%.*ls%ls.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 574,
        severity: 16,
        template: "%ls is not allowed inside a user transaction.",
        exception: None,
    },
    ErrorDef {
        number: 911,
        severity: 16,
        template: "No database named '%.*ls' exists; check the spelling of the name.",
        exception: None,
    },
    ErrorDef {
        number: 1001,
        severity: 16,
        template: "Line %d: the length or precision %d is not valid.",
        exception: None,
    },
    ErrorDef {
        number: 1002,
        severity: 16,
        template: "Line %d: the scale %d is not valid.",
        exception: None,
    },
    ErrorDef {
        number: 1007,
        severity: 15,
        template: "The %S_MSG '%.*ls' exceeds the numeric range (precision is limited to 38).",
        exception: None,
    },
    ErrorDef {
        number: 1011,
        severity: 15,
        template: "Correlation name '%.*ls' is used more than once in the FROM clause.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 1012,
        severity: 15,
        template: "Correlation name '%.*ls' is also the exposed name of table '%.*ls'; give one of them another alias.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 1013,
        severity: 15,
        template: "\"%.*ls\" and \"%.*ls\" expose the same name in the FROM clause; give them distinct aliases.",
        exception: None,
    },
    ErrorDef {
        number: 1014,
        severity: 15,
        template: "The value of the TOP or FETCH clause is not valid.",
        exception: Some(BATCH),
    },
    ErrorDef {
        number: 1023,
        severity: 15,
        template: "Parameter %d of %ls is not valid.",
        exception: None,
    },
    ErrorDef {
        number: 1031,
        severity: 15,
        template: "A percentage has to lie between 0 and 100.",
        exception: Some(BATCH),
    },
    ErrorDef {
        number: 1038,
        severity: 15,
        template: "An object or column name is empty: each SELECT INTO column needs a name, and an alias written \"\" or [] is not allowed.",
        exception: None,
    },
    ErrorDef {
        number: 1047,
        severity: 15,
        template: "The locking hints contradict each other.",
        exception: None,
    },
    ErrorDef {
        number: 1060,
        severity: 15,
        template: "The row count of TOP or FETCH has to be an integer.",
        exception: None,
    },
    ErrorDef {
        number: 1062,
        severity: 16,
        template: "TOP WITH TIES requires an ORDER BY clause.",
        exception: None,
    },
    ErrorDef {
        number: 1065,
        severity: 15,
        template: "NOLOCK and READUNCOMMITTED cannot be applied to the target table of INSERT, UPDATE, DELETE or MERGE.",
        exception: None,
    },
    ErrorDef {
        number: 1087,
        severity: 15,
        template: "The table variable \"%.*ls\" is not declared.",
        exception: None,
    },
    ErrorDef {
        number: 1088,
        severity: 15,
        template: "Object \"%.*ls\" was not found: it does not exist or is not accessible.",
        exception: Some(BATCH),
    },
    ErrorDef {
        number: 1205,
        severity: 13,
        template: "Process %d was chosen as the victim of a deadlock on %.*ls resources; run the transaction again.",
        // The victim of a deadlock: its batch stops, with the procedures and dynamic texts
        // it called, and its transaction is rolled back under `XACT_ABORT OFF` as under `ON`.
        exception: Some(ErrorBehavior {
            rolls_back_transaction: true,
            ..BATCH
        }),
    },
    ErrorDef {
        number: 1222,
        severity: 16,
        template: "The lock could not be acquired before the timeout.",
        // A wait for a lock that ran out of time stops its statement; the transaction stays
        // open and committable: `BEGIN TRAN; UPDATE t1 WITH (NOWAIT) SET v = 1; SELECT
        // @@TRANCOUNT;` answers 1 while another transaction holds the row.
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 1750,
        severity: 10,
        template: "The constraint or index was not created; the previous errors say why.",
        exception: Some(BATCH),
    },
    ErrorDef {
        number: 1754,
        severity: 16,
        template: "A DEFAULT cannot sit on an IDENTITY column. Table '%.*ls', column '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 1769,
        severity: 16,
        template: "Foreign key '%.*ls' names the column '%.*ls', which the referencing table '%.*ls' does not have.",
        exception: None,
    },
    ErrorDef {
        number: 1776,
        severity: 16,
        template: "The referenced table '%.*ls' has no primary or unique key matching the columns of foreign key '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 1801,
        severity: 16,
        template: "A database named '%.*ls' exists already; pick another name.",
        exception: None,
    },
    ErrorDef {
        number: 1902,
        severity: 16,
        template: "The %S_MSG '%.*ls' can have a single clustered index; drop '%.*ls' first.",
        exception: None,
    },
    ErrorDef {
        number: 1909,
        severity: 16,
        template: "The %S_MSG cannot repeat a column: '%.*ls' is listed twice.",
        exception: None,
    },
    ErrorDef {
        number: 1911,
        severity: 16,
        template: "The target table or view has no column named '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 1913,
        severity: 16,
        template: "An index or statistics named '%.*ls' exists already on %S_MSG '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 1939,
        severity: 16,
        template: "The %S_MSG requires the view '%.*ls' to be schema bound.",
        exception: None,
    },
    ErrorDef {
        number: 2601,
        severity: 14,
        template: "Duplicate key in object '%.*ls' for unique index '%.*ls': the value %ls exists already.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 2627,
        severity: 14,
        template: "The %ls constraint '%.*ls' rejects a duplicate key in object '%.*ls': the value %ls exists already.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 2628,
        severity: 16,
        template: "Data too long for table '%.*ls', column '%.*ls': the value '%.*ls' would be cut.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 2702,
        severity: 16,
        template: "No database named '%.*ls' exists.",
        exception: None,
    },
    ErrorDef {
        number: 2705,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' is defined twice; column names have to be unique.",
        exception: None,
    },
    ErrorDef {
        number: 2714,
        severity: 16,
        template: "An object named '%.*ls' exists already in the database.",
        exception: None,
    },
    ErrorDef {
        number: 2715,
        severity: 16,
        template: "Column, parameter or variable #%d: unknown data type %.*ls.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 2716,
        severity: 16,
        template: "Column, parameter or variable #%d: data type %.*ls does not accept a length.",
        exception: None,
    },
    ErrorDef {
        number: 2717,
        severity: 15,
        template: "Size %d of the %S_MSG '%.*ls' is larger than the maximum (%d).",
        exception: None,
    },
    ErrorDef {
        number: 2749,
        severity: 16,
        template: "Identity column '%.*ls' has to be a non-nullable int, bigint, smallint, tinyint, or decimal or numeric of scale 0.",
        exception: None,
    },
    ErrorDef {
        number: 2750,
        severity: 16,
        template: "Column or parameter #%d: precision %d exceeds the maximum of %d.",
        exception: None,
    },
    ErrorDef {
        number: 2812,
        severity: 16,
        template: "Unknown stored procedure '%.*ls'.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 3623,
        severity: 16,
        template: "The floating point operation is undefined.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 3701,
        severity: 11,
        template: "Unable to %S_MSG the %S_MSG '%.*ls': it does not exist or is not accessible.",
        // In the reference behaviour, `SET XACT_ABORT ON; DROP TABLE dbo.missing; SELECT 2;`
        // answers the 2; the session does not read `xact_abort_widens` yet.
        exception: Some(ErrorBehavior {
            xact_abort_widens: false,
            ..STATEMENT
        }),
    },
    ErrorDef {
        number: 3708,
        severity: 16,
        template: "Unable to %S_MSG the %S_MSG '%.*ls': it is a system %S_MSG.",
        exception: None,
    },
    ErrorDef {
        number: 3723,
        severity: 16,
        template: "Index '%.*ls' enforces a %ls constraint and cannot be dropped directly.",
        exception: None,
    },
    ErrorDef {
        number: 3726,
        severity: 16,
        template: "Object '%.*ls' is referenced by a FOREIGN KEY constraint and cannot be dropped.",
        exception: None,
    },
    ErrorDef {
        number: 3728,
        severity: 16,
        template: "'%.*ls' is not the name of a constraint here.",
        exception: None,
    },
    ErrorDef {
        number: 3902,
        severity: 16,
        template: "COMMIT TRANSACTION without a matching BEGIN TRANSACTION.",
        exception: None,
    },
    ErrorDef {
        number: 3903,
        severity: 16,
        template: "ROLLBACK TRANSACTION without a matching BEGIN TRANSACTION.",
        exception: None,
    },
    ErrorDef {
        number: 3926,
        severity: 10,
        template: "The session transaction was committed or aborted elsewhere.",
        exception: None,
    },
    ErrorDef {
        number: 3952,
        severity: 16,
        template: "Database '%.*ls' does not allow snapshot isolation; enable it with ALTER DATABASE.",
        exception: None,
    },
    ErrorDef {
        number: 3960,
        severity: 16,
        template: "Update conflict under snapshot isolation: a row of table '%.*ls' in database '%.*ls' was changed by another transaction. Retry or change the isolation level.",
        exception: None,
    },
    ErrorDef {
        number: 3971,
        severity: 16,
        template: "Could not resume the transaction. Desc:%.*ls.",
        exception: None,
    },
    ErrorDef {
        number: 3989,
        severity: 16,
        template: "The request cannot start without a valid transaction descriptor.",
        exception: None,
    },
    ErrorDef {
        number: 4060,
        severity: 11,
        template: "The database \"%.*ls\" requested at login could not be opened; login refused.",
        exception: None,
    },
    ErrorDef {
        number: 4063,
        severity: 11,
        template: "The database \"%.*ls\" requested at login could not be opened; the default database \"%.*ls\" is used instead.",
        exception: None,
    },
    ErrorDef {
        number: 4104,
        severity: 16,
        template: "The qualified name \"%.*ls\" matches nothing in scope.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 4121,
        severity: 16,
        template: "Neither a column \"%.*ls\" nor a user-defined function or aggregate \"%.*ls\" was found, or the name is ambiguous.",
        exception: None,
    },
    ErrorDef {
        number: 4127,
        severity: 16,
        template: "COALESCE needs at least one argument other than the NULL constant.",
        exception: None,
    },
    ErrorDef {
        number: 4145,
        severity: 15,
        template: "A condition is expected near '%.*ls', but the expression is not boolean.",
        exception: None,
    },
    ErrorDef {
        number: 4151,
        severity: 16,
        template: "The first argument of NULLIF cannot be the NULL constant: its type has to be known.",
        exception: None,
    },
    ErrorDef {
        number: 4701,
        severity: 16,
        template: "Object \"%.*ls\" was not found: it does not exist or is not accessible.",
        exception: None,
    },
    ErrorDef {
        number: 4712,
        severity: 16,
        template: "Table '%.*ls' is referenced by a FOREIGN KEY constraint and cannot be truncated.",
        exception: None,
    },
    ErrorDef {
        number: 4901,
        severity: 16,
        template: "A column added to a non-empty table has to be nullable, have a DEFAULT, or be an identity or timestamp column. Column '%.*ls' cannot be added to table '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 4902,
        severity: 16,
        template: "Object \"%.*ls\" was not found: it does not exist or this login lacks permissions.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 4924,
        severity: 16,
        template: "ALTER TABLE DROP COLUMN could not run: column '%.*ls' is missing from table '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 5074,
        severity: 16,
        template: "%S_MSG '%.*ls' still depends on %S_MSG '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 5701,
        severity: 10,
        template: "Database context is now '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 5703,
        severity: 10,
        template: "Language setting is now %.*ls.",
        exception: None,
    },
    ErrorDef {
        number: 7202,
        severity: 11,
        template: "Server '%.*ls' is not registered in sys.servers; check the name or add it with sp_addlinkedserver.",
        exception: None,
    },
    ErrorDef {
        number: 8101,
        severity: 16,
        template: "An explicit value for the identity column of table '%.*ls' requires a column list and IDENTITY_INSERT ON.",
        exception: None,
    },
    ErrorDef {
        number: 8102,
        severity: 16,
        template: "Identity column '%.*ls' cannot be updated.",
        exception: None,
    },
    ErrorDef {
        number: 8106,
        severity: 16,
        template: "Table '%.*ls' lacks an IDENTITY column; SET IDENTITY_INSERT cannot run on it.",
        exception: None,
    },
    ErrorDef {
        number: 8107,
        severity: 16,
        template: "A session holds IDENTITY_INSERT for one table at a time; '%.*ls.%.*ls.%.*ls' already has it, and '%.*ls' is refused.",
        exception: None,
    },
    ErrorDef {
        number: 8110,
        severity: 16,
        template: "Table '%.*ls' can have a single PRIMARY KEY constraint.",
        exception: None,
    },
    ErrorDef {
        number: 8111,
        severity: 16,
        template: "A PRIMARY KEY of table '%.*ls' cannot include a nullable column.",
        exception: None,
    },
    ErrorDef {
        number: 8112,
        severity: 16,
        template: "Constraints of table '%.*ls' can create a single clustered index.",
        exception: None,
    },
    ErrorDef {
        number: 8114,
        severity: 16,
        template: "Data type %ls could not be converted to %ls.",
        exception: None,
    },
    ErrorDef {
        number: 8115,
        severity: 16,
        template: "Converting %ls to data type %ls overflowed.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 8116,
        severity: 16,
        template: "Data type %ls is not accepted for argument %d of the %ls function.",
        exception: None,
    },
    ErrorDef {
        number: 8117,
        severity: 16,
        template: "Data type %ls is not accepted by the %ls operator.",
        exception: None,
    },
    ErrorDef {
        number: 8120,
        severity: 16,
        template: "Column '%.*ls.%.*ls' of the select list is neither aggregated nor part of GROUP BY.",
        exception: Some(BINDING),
    },
    ErrorDef {
        number: 8121,
        severity: 16,
        template: "Column '%.*ls.%.*ls' of the HAVING clause is neither aggregated nor part of GROUP BY.",
        exception: None,
    },
    ErrorDef {
        number: 8134,
        severity: 16,
        template: "Division by zero.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 8144,
        severity: 16,
        template: "Procedure %.*ls was called with more arguments than it declares.",
        exception: None,
    },
    ErrorDef {
        number: 8145,
        severity: 16,
        template: "%.*ls is not a parameter declared by procedure %.*ls.",
        exception: None,
    },
    ErrorDef {
        number: 8146,
        severity: 16,
        template: "Procedure %.*ls declares no parameter and was called with arguments.",
        exception: None,
    },
    ErrorDef {
        number: 8147,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' is nullable and cannot be an IDENTITY column.",
        exception: None,
    },
    ErrorDef {
        number: 8148,
        severity: 16,
        template: "A second %ls %S_MSG is refused for column '%.*ls' of table '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 8152,
        severity: 16,
        template: "Data too long: the string or binary value would be cut.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 8153,
        severity: 10,
        template: "Warning: an aggregate or SET operation ignored a NULL value.",
        exception: None,
    },
    ErrorDef {
        number: 8154,
        severity: 15,
        template: "The reference to table '%.*ls' is ambiguous.",
        exception: None,
    },
    ErrorDef {
        number: 8155,
        severity: 15,
        template: "Column %d of '%.*ls' has no name.",
        exception: None,
    },
    ErrorDef {
        number: 8158,
        severity: 16,
        template: "The alias list of the derived table '%.*ls' names fewer columns than the query inside it produces.",
        exception: None,
    },
    ErrorDef {
        number: 8159,
        severity: 16,
        template: "The alias list of the derived table '%.*ls' names more columns than the query inside it produces.",
        exception: None,
    },
    ErrorDef {
        number: 8168,
        severity: 16,
        template: "The name '%.*ls' is used more than once for a constraint, column, index or trigger here; names have to be unique.",
        exception: None,
    },
    ErrorDef {
        number: 8169,
        severity: 16,
        template: "The character string could not be converted to uniqueidentifier.",
        exception: None,
    },
    ErrorDef {
        number: 8170,
        severity: 16,
        template: "A uniqueidentifier value does not fit in the char result.",
        exception: None,
    },
    ErrorDef {
        number: 8178,
        severity: 16,
        template: "The parameterized query '%.*ls' was called without its parameter '%.*ls'.",
        exception: None,
    },
    ErrorDef {
        number: 8179,
        severity: 16,
        template: "No prepared statement of this session has the handle %d.",
        exception: None,
    },
    ErrorDef {
        number: 8180,
        severity: 16,
        template: "The statement could not be prepared.",
        exception: None,
    },
    ErrorDef {
        number: 8199,
        severity: 16,
        template: "The procedure name in EXECUTE must be a string literal or a char, varchar, nchar, or nvarchar variable.",
        exception: None,
    },
    ErrorDef {
        number: 8631,
        severity: 17,
        template: "Internal error: the server ran out of stack; the query is probably nested too deeply and needs simplifying.",
        exception: None,
    },
    ErrorDef {
        number: 9806,
        severity: 16,
        template: "Datepart %.*ls cannot be used with the date function %.*ls.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 9807,
        severity: 16,
        template: "The input string does not match style %d; change the string or the style.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 9809,
        severity: 16,
        template: "Style %d is not defined for converting %s to %s.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 9810,
        severity: 16,
        template: "Datepart %.*ls cannot be used with the date function %.*ls on data type %s.",
        exception: Some(STATEMENT),
    },
    ErrorDef {
        number: 10709,
        severity: 16,
        template: "The rows of a table value constructor have to supply the same number of columns.",
        exception: None,
    },
    ErrorDef {
        number: 15009,
        severity: 16,
        template: "The object '%s' is not in database '%s', or this operation does not accept it.",
        exception: None,
    },
    ErrorDef {
        number: 15010,
        severity: 16,
        template: "No database named '%s' exists; give a valid database name.",
        exception: None,
    },
    ErrorDef {
        number: 18452,
        severity: 14,
        template: "Login refused: integrated authentication does not accept a login from an untrusted domain.%.*ls",
        exception: None,
    },
    ErrorDef {
        number: 18456,
        severity: 14,
        template: "Login refused for user '%.*ls'.%.*ls%.*ls",
        exception: None,
    },
];

#[cfg(test)]
mod tests;

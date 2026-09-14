//! The static catalogue of the errors VaubanDB raises.
//!
//! One entry per error number, with its default severity and its message template. The
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

/// The definition of one error: number, default severity and message template.
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
}

/// What SQL Server stops after an error raised while executing a statement.
///
/// Compilation and binding errors never reach this decision: they stop the batch before
/// its first statement runs. `SET XACT_ABORT ON` also overrides this value in `session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchErrorScope {
    /// Only the failing statement stops; a following statement in the batch still runs.
    Statement,
    /// The failing statement and everything after it in the batch stop.
    Batch,
}

impl ErrorDef {
    /// What stops after an error VaubanDB can raise at run time.
    ///
    /// Neither severity nor state separates the families: 8134 and 245 are both severity 16,
    /// state 1, while 8134 continues and 245 stops. With a successful statement before and
    /// after the failing one, the statement-scoped numbers are 220, 232, 237, 242, 244, 248,
    /// 506, 517, 535, 537, 3623, 8115, 8134, 9806, 9807, 9809 and 9810 (`SELECT 1; SELECT
    /// DATEDIFF(iso_week, CAST('2020-01-01' AS date), CAST('2020-01-01' AS date));
    /// SELECT 2;` keeps both surrounding rows). Error 536 is statement-scoped when it
    /// reaches execution; a foldable length is caught at compilation and stops the batch on
    /// its own. Batch-scoped numbers include 127, 234, 235, 241, 245, 281, 289, 292, 295,
    /// 1014, 8114, 8169 and 8170. Unknown and internal errors default to the safer batch
    /// scope.
    pub const fn batch_scope(&self) -> BatchErrorScope {
        match self.number {
            220 | 232 | 237 | 242 | 244 | 248 | 506 | 517 | 535 | 536 | 537 | 3623 | 8115
            | 8134 | 9806 | 9807 | 9809 | 9810 => BatchErrorScope::Statement,
            _ => BatchErrorScope::Batch,
        }
    }
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
    },
    ErrorDef {
        number: 103,
        severity: 15,
        template: "The %S_MSG beginning with '%.*ls' exceeds the maximum length of %d.",
    },
    ErrorDef {
        number: 105,
        severity: 15,
        template: "Quotation mark left open after the string '%.*ls'.",
    },
    ErrorDef {
        number: 107,
        severity: 15,
        template: "The column prefix '%.*ls' matches no table or alias of the query.",
    },
    ErrorDef {
        number: 109,
        severity: 15,
        template: "The INSERT column list names more columns than the VALUES row supplies; the two counts have to match.",
    },
    ErrorDef {
        number: 110,
        severity: 15,
        template: "The INSERT column list names fewer columns than the VALUES row supplies; the two counts have to match.",
    },
    ErrorDef {
        number: 113,
        severity: 15,
        template: "Comment is not closed: '*/' expected.",
    },
    ErrorDef {
        number: 116,
        severity: 15,
        template: "A subquery not introduced by EXISTS must select a single expression.",
    },
    ErrorDef {
        number: 117,
        severity: 15,
        template: "The %S_MSG name '%.*ls' has too many prefixes; at most %d are allowed.",
    },
    ErrorDef {
        number: 120,
        severity: 15,
        template: "The select list of the INSERT supplies fewer items than its column list; the two counts have to match.",
    },
    ErrorDef {
        number: 121,
        severity: 15,
        template: "The select list of the INSERT supplies more items than its column list; the two counts have to match.",
    },
    ErrorDef {
        number: 127,
        severity: 15,
        template: "The row count of TOP or FETCH cannot be negative.",
    },
    ErrorDef {
        number: 130,
        severity: 15,
        template: "An aggregate function cannot be applied to an expression that holds an aggregate or a subquery.",
    },
    ErrorDef {
        number: 131,
        severity: 15,
        template: "Size %d of the %S_MSG '%.*ls' is larger than any data type allows (%d).",
    },
    ErrorDef {
        number: 134,
        severity: 15,
        template: "The variable '%.*ls' is already declared; a batch or a procedure declares each name once.",
    },
    ErrorDef {
        number: 137,
        severity: 15,
        template: "The scalar variable \"%.*ls\" is not declared.",
    },
    ErrorDef {
        number: 141,
        severity: 15,
        template: "A SELECT that assigns variables cannot also return rows.",
    },
    ErrorDef {
        number: 144,
        severity: 15,
        template: "A GROUP BY expression cannot hold an aggregate or a subquery.",
    },
    ErrorDef {
        number: 145,
        severity: 15,
        template: "With SELECT DISTINCT, each ORDER BY item has to be in the select list.",
    },
    ErrorDef {
        number: 147,
        severity: 15,
        template: "An aggregate cannot appear in a WHERE clause, except inside a subquery of a HAVING clause or a select list, aggregating an outer reference.",
    },
    ErrorDef {
        number: 148,
        severity: 15,
        template: "WAITFOR received a time string '%.*ls' that is not valid.",
    },
    ErrorDef {
        number: 151,
        severity: 15,
        template: "'%.*ls' cannot be read as a money value.",
    },
    ErrorDef {
        number: 155,
        severity: 15,
        template: "'%.*ls' is not a known %ls option.",
    },
    ErrorDef {
        number: 156,
        severity: 15,
        template: "Syntax error near the keyword '%.*ls'.",
    },
    ErrorDef {
        number: 168,
        severity: 15,
        template: "The floating point literal '%.*ls' cannot be represented in %d bytes.",
    },
    ErrorDef {
        number: 174,
        severity: 15,
        template: "The function %.*ls takes exactly %d argument(s).",
    },
    ErrorDef {
        number: 189,
        severity: 15,
        template: "The function %ls takes between %d and %d arguments.",
    },
    ErrorDef {
        number: 191,
        severity: 15,
        template: "The statement is nested too deeply; split it into smaller queries.",
    },
    ErrorDef {
        number: 192,
        severity: 16,
        template: "Scale cannot exceed precision.",
    },
    ErrorDef {
        number: 195,
        severity: 15,
        template: "'%.*ls' is not a known %S_MSG.",
    },
    ErrorDef {
        number: 205,
        severity: 16,
        template: "Each side of a UNION, INTERSECT or EXCEPT must select the same number of expressions.",
    },
    ErrorDef {
        number: 206,
        severity: 16,
        template: "Type mismatch: %ls cannot be combined with %ls.",
    },
    ErrorDef {
        number: 207,
        severity: 16,
        template: "Unknown column name '%.*ls'.",
    },
    ErrorDef {
        number: 208,
        severity: 16,
        template: "Unknown object name '%.*ls'.",
    },
    ErrorDef {
        number: 209,
        severity: 16,
        template: "Column name '%.*ls' is ambiguous.",
    },
    ErrorDef {
        number: 210,
        severity: 16,
        template: "A binary or varbinary value could not be converted to datetime.",
    },
    ErrorDef {
        number: 213,
        severity: 16,
        template: "The supplied values do not match the columns of the table.",
    },
    ErrorDef {
        number: 215,
        severity: 16,
        template: "Object '%.*ls' is not a function and takes no parameters; a table hint needs the WITH keyword.",
    },
    ErrorDef {
        number: 220,
        severity: 16,
        template: "Value out of range for data type %ls: %ld.",
    },
    ErrorDef {
        number: 226,
        severity: 16,
        template: "%ls is not allowed inside a multi-statement transaction.",
    },
    ErrorDef {
        number: 232,
        severity: 16,
        template: "Value out of range for type %ls: %f.",
    },
    ErrorDef {
        number: 234,
        severity: 16,
        template: "A money value does not fit in the result type %ls.",
    },
    ErrorDef {
        number: 235,
        severity: 16,
        template: "The character value is not a valid money literal and could not be converted.",
    },
    ErrorDef {
        number: 237,
        severity: 16,
        template: "A money value does not fit in the result type %ls.",
    },
    ErrorDef {
        number: 241,
        severity: 16,
        template: "The character string could not be converted to a date or time.",
    },
    ErrorDef {
        number: 242,
        severity: 16,
        template: "Converting %ls to %ls produced a value outside the target range.",
    },
    ErrorDef {
        number: 243,
        severity: 16,
        template: "%.*ls is not a known system type.",
    },
    ErrorDef {
        number: 244,
        severity: 16,
        template: "The %ls value '%.*ls' does not fit in an %hs column; a wider integer type is needed.",
    },
    ErrorDef {
        number: 245,
        severity: 16,
        template: "The %ls value '%.*ls' could not be converted to data type %ls.",
    },
    ErrorDef {
        number: 248,
        severity: 16,
        template: "The %ls value '%.*ls' does not fit in an int column.",
    },
    ErrorDef {
        number: 257,
        severity: 16,
        template: "No implicit conversion from %ls to %ls; use CONVERT explicitly.",
    },
    ErrorDef {
        number: 263,
        severity: 16,
        template: "No table to select from.",
    },
    ErrorDef {
        number: 264,
        severity: 16,
        template: "Column '%.*ls' is named more than once in the column list of the INSERT or the SET clause of the UPDATE; a column takes one value per statement.",
    },
    ErrorDef {
        number: 266,
        severity: 16,
        template: "The transaction count changed across EXECUTE: BEGIN and COMMIT are unbalanced (before %ld, after %ld).",
    },
    ErrorDef {
        number: 271,
        severity: 16,
        template: "The column \"%.*ls\" cannot be modified: it is computed, or it comes out of a UNION.",
    },
    ErrorDef {
        number: 281,
        severity: 16,
        template: "Style %d is not defined for converting %ls to a character string.",
    },
    ErrorDef {
        number: 289,
        severity: 16,
        template: "Data type %ls cannot be built from these arguments: at least one value is out of range.",
    },
    ErrorDef {
        number: 292,
        severity: 16,
        template: "A smallmoney value does not fit in the result type %ls.",
    },
    ErrorDef {
        number: 295,
        severity: 16,
        template: "The character string could not be converted to smalldatetime.",
    },
    ErrorDef {
        number: 339,
        severity: 16,
        template: "DEFAULT and NULL cannot be given as explicit identity values.",
    },
    ErrorDef {
        number: 402,
        severity: 16,
        template: "The types %s and %s cannot be combined by the %s operator.",
    },
    ErrorDef {
        number: 447,
        severity: 16,
        template: "COLLATE cannot apply to an expression of type %ls.",
    },
    ErrorDef {
        number: 448,
        severity: 16,
        template: "Unknown collation '%.*ls'.",
    },
    ErrorDef {
        number: 506,
        severity: 16,
        template: "The escape character \"%.*ls\" of the %ls predicate must be a single character.",
    },
    ErrorDef {
        number: 512,
        severity: 16,
        template: "The subquery returned several values, which is not allowed after a comparison operator or as an expression.",
    },
    ErrorDef {
        number: 515,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' does not accept NULL; %ls fails.",
    },
    ErrorDef {
        number: 517,
        severity: 16,
        template: "The addition overflowed the '%ls' column.",
    },
    ErrorDef {
        number: 529,
        severity: 16,
        template: "No explicit conversion exists from %ls to %ls.",
    },
    ErrorDef {
        number: 535,
        severity: 16,
        template: "%.*ls overflowed: too many dateparts separate the two instants. Call %.*ls with a coarser datepart.",
    },
    ErrorDef {
        number: 536,
        severity: 16,
        template: "The length given to the %ls function is not valid.",
    },
    ErrorDef {
        number: 537,
        severity: 16,
        template: "The length given to LEFT or SUBSTRING is not valid.",
    },
    ErrorDef {
        number: 544,
        severity: 16,
        template: "An explicit value for the identity column of table '%.*ls' requires IDENTITY_INSERT ON.",
    },
    ErrorDef {
        number: 547,
        severity: 16,
        template: "%ls violates the %ls constraint \"%.*ls\" in database \"%.*ls\", table \"%.*ls\"%ls%.*ls%ls.",
    },
    ErrorDef {
        number: 574,
        severity: 16,
        template: "%ls is not allowed inside a user transaction.",
    },
    ErrorDef {
        number: 911,
        severity: 16,
        template: "No database named '%.*ls' exists; check the spelling of the name.",
    },
    ErrorDef {
        number: 1001,
        severity: 16,
        template: "Line %d: the length or precision %d is not valid.",
    },
    ErrorDef {
        number: 1002,
        severity: 16,
        template: "Line %d: the scale %d is not valid.",
    },
    ErrorDef {
        number: 1007,
        severity: 15,
        template: "The %S_MSG '%.*ls' exceeds the numeric range (precision is limited to 38).",
    },
    ErrorDef {
        number: 1011,
        severity: 15,
        template: "Correlation name '%.*ls' is used more than once in the FROM clause.",
    },
    ErrorDef {
        number: 1012,
        severity: 15,
        template: "Correlation name '%.*ls' is also the exposed name of table '%.*ls'; give one of them another alias.",
    },
    ErrorDef {
        number: 1013,
        severity: 15,
        template: "\"%.*ls\" and \"%.*ls\" expose the same name in the FROM clause; give them distinct aliases.",
    },
    ErrorDef {
        number: 1014,
        severity: 15,
        template: "The value of the TOP or FETCH clause is not valid.",
    },
    ErrorDef {
        number: 1023,
        severity: 15,
        template: "Parameter %d of %ls is not valid.",
    },
    ErrorDef {
        number: 1031,
        severity: 15,
        template: "A percentage has to lie between 0 and 100.",
    },
    ErrorDef {
        number: 1038,
        severity: 15,
        template: "An object or column name is empty: each SELECT INTO column needs a name, and an alias written \"\" or [] is not allowed.",
    },
    ErrorDef {
        number: 1047,
        severity: 15,
        template: "The locking hints contradict each other.",
    },
    ErrorDef {
        number: 1060,
        severity: 15,
        template: "The row count of TOP or FETCH has to be an integer.",
    },
    ErrorDef {
        number: 1062,
        severity: 16,
        template: "TOP WITH TIES requires an ORDER BY clause.",
    },
    ErrorDef {
        number: 1065,
        severity: 15,
        template: "NOLOCK and READUNCOMMITTED cannot be applied to the target table of INSERT, UPDATE, DELETE or MERGE.",
    },
    ErrorDef {
        number: 1087,
        severity: 15,
        template: "The table variable \"%.*ls\" is not declared.",
    },
    ErrorDef {
        number: 1088,
        severity: 15,
        template: "Object \"%.*ls\" was not found: it does not exist or is not accessible.",
    },
    ErrorDef {
        number: 1205,
        severity: 13,
        template: "Process %d was chosen as the victim of a deadlock on %.*ls resources; run the transaction again.",
    },
    ErrorDef {
        number: 1222,
        severity: 16,
        template: "The lock could not be acquired before the timeout.",
    },
    ErrorDef {
        number: 1750,
        severity: 10,
        template: "The constraint or index was not created; the previous errors say why.",
    },
    ErrorDef {
        number: 1769,
        severity: 16,
        template: "Foreign key '%.*ls' names the column '%.*ls', which the referencing table '%.*ls' does not have.",
    },
    ErrorDef {
        number: 1776,
        severity: 16,
        template: "The referenced table '%.*ls' has no primary or unique key matching the columns of foreign key '%.*ls'.",
    },
    ErrorDef {
        number: 1801,
        severity: 16,
        template: "A database named '%.*ls' exists already; pick another name.",
    },
    ErrorDef {
        number: 1902,
        severity: 16,
        template: "The %S_MSG '%.*ls' can have a single clustered index; drop '%.*ls' first.",
    },
    ErrorDef {
        number: 1909,
        severity: 16,
        template: "The %S_MSG cannot repeat a column: '%.*ls' is listed twice.",
    },
    ErrorDef {
        number: 1911,
        severity: 16,
        template: "The target table or view has no column named '%.*ls'.",
    },
    ErrorDef {
        number: 1913,
        severity: 16,
        template: "An index or statistics named '%.*ls' exists already on %S_MSG '%.*ls'.",
    },
    ErrorDef {
        number: 1939,
        severity: 16,
        template: "The %S_MSG requires the view '%.*ls' to be schema bound.",
    },
    ErrorDef {
        number: 2601,
        severity: 14,
        template: "Duplicate key in object '%.*ls' for unique index '%.*ls': the value %ls exists already.",
    },
    ErrorDef {
        number: 2627,
        severity: 14,
        template: "The %ls constraint '%.*ls' rejects a duplicate key in object '%.*ls': the value %ls exists already.",
    },
    ErrorDef {
        number: 2628,
        severity: 16,
        template: "Data too long for table '%.*ls', column '%.*ls': the value '%.*ls' would be cut.",
    },
    ErrorDef {
        number: 2702,
        severity: 16,
        template: "No database named '%.*ls' exists.",
    },
    ErrorDef {
        number: 2705,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' is defined twice; column names have to be unique.",
    },
    ErrorDef {
        number: 2714,
        severity: 16,
        template: "An object named '%.*ls' exists already in the database.",
    },
    ErrorDef {
        number: 2715,
        severity: 16,
        template: "Column, parameter or variable #%d: unknown data type %.*ls.",
    },
    ErrorDef {
        number: 2717,
        severity: 15,
        template: "Size %d of the %S_MSG '%.*ls' is larger than the maximum (%d).",
    },
    ErrorDef {
        number: 2749,
        severity: 16,
        template: "Identity column '%.*ls' has to be a non-nullable int, bigint, smallint, tinyint, or decimal or numeric of scale 0.",
    },
    ErrorDef {
        number: 2750,
        severity: 16,
        template: "Column or parameter #%d: precision %d exceeds the maximum of %d.",
    },
    ErrorDef {
        number: 2812,
        severity: 16,
        template: "Unknown stored procedure '%.*ls'.",
    },
    ErrorDef {
        number: 3623,
        severity: 16,
        template: "The floating point operation is undefined.",
    },
    ErrorDef {
        number: 3701,
        severity: 11,
        template: "Unable to %S_MSG the %S_MSG '%.*ls': it does not exist or is not accessible.",
    },
    ErrorDef {
        number: 3708,
        severity: 16,
        template: "Unable to %S_MSG the %S_MSG '%.*ls': it is a system %S_MSG.",
    },
    ErrorDef {
        number: 3723,
        severity: 16,
        template: "Index '%.*ls' enforces a %ls constraint and cannot be dropped directly.",
    },
    ErrorDef {
        number: 3726,
        severity: 16,
        template: "Object '%.*ls' is referenced by a FOREIGN KEY constraint and cannot be dropped.",
    },
    ErrorDef {
        number: 3902,
        severity: 16,
        template: "COMMIT TRANSACTION without a matching BEGIN TRANSACTION.",
    },
    ErrorDef {
        number: 3903,
        severity: 16,
        template: "ROLLBACK TRANSACTION without a matching BEGIN TRANSACTION.",
    },
    ErrorDef {
        number: 3952,
        severity: 16,
        template: "Database '%.*ls' does not allow snapshot isolation; enable it with ALTER DATABASE.",
    },
    ErrorDef {
        number: 3960,
        severity: 16,
        template: "Update conflict under snapshot isolation: a row of table '%.*ls' in database '%.*ls' was changed by another transaction. Retry or change the isolation level.",
    },
    ErrorDef {
        number: 4060,
        severity: 11,
        template: "The database \"%.*ls\" requested at login could not be opened; login refused.",
    },
    ErrorDef {
        number: 4063,
        severity: 11,
        template: "The database \"%.*ls\" requested at login could not be opened; the default database \"%.*ls\" is used instead.",
    },
    ErrorDef {
        number: 4104,
        severity: 16,
        template: "The qualified name \"%.*ls\" matches nothing in scope.",
    },
    ErrorDef {
        number: 4121,
        severity: 16,
        template: "Neither a column \"%.*ls\" nor a user-defined function or aggregate \"%.*ls\" was found, or the name is ambiguous.",
    },
    ErrorDef {
        number: 4127,
        severity: 16,
        template: "COALESCE needs at least one argument other than the NULL constant.",
    },
    ErrorDef {
        number: 4145,
        severity: 15,
        template: "A condition is expected near '%.*ls', but the expression is not boolean.",
    },
    ErrorDef {
        number: 4151,
        severity: 16,
        template: "The first argument of NULLIF cannot be the NULL constant: its type has to be known.",
    },
    ErrorDef {
        number: 4701,
        severity: 16,
        template: "Object \"%.*ls\" was not found: it does not exist or is not accessible.",
    },
    ErrorDef {
        number: 4712,
        severity: 16,
        template: "Table '%.*ls' is referenced by a FOREIGN KEY constraint and cannot be truncated.",
    },
    ErrorDef {
        number: 4901,
        severity: 16,
        template: "A column added to a non-empty table has to be nullable, have a DEFAULT, or be an identity or timestamp column. Column '%.*ls' cannot be added to table '%.*ls'.",
    },
    ErrorDef {
        number: 5701,
        severity: 10,
        template: "Database context is now '%.*ls'.",
    },
    ErrorDef {
        number: 5703,
        severity: 10,
        template: "Language setting is now %.*ls.",
    },
    ErrorDef {
        number: 7202,
        severity: 11,
        template: "Server '%.*ls' is not registered in sys.servers; check the name or add it with sp_addlinkedserver.",
    },
    ErrorDef {
        number: 8101,
        severity: 16,
        template: "An explicit value for the identity column of table '%.*ls' requires a column list and IDENTITY_INSERT ON.",
    },
    ErrorDef {
        number: 8102,
        severity: 16,
        template: "Identity column '%.*ls' cannot be updated.",
    },
    ErrorDef {
        number: 8110,
        severity: 16,
        template: "Table '%.*ls' can have a single PRIMARY KEY constraint.",
    },
    ErrorDef {
        number: 8111,
        severity: 16,
        template: "A PRIMARY KEY of table '%.*ls' cannot include a nullable column.",
    },
    ErrorDef {
        number: 8112,
        severity: 16,
        template: "Constraints of table '%.*ls' can create a single clustered index.",
    },
    ErrorDef {
        number: 8114,
        severity: 16,
        template: "Data type %ls could not be converted to %ls.",
    },
    ErrorDef {
        number: 8115,
        severity: 16,
        template: "Converting %ls to data type %ls overflowed.",
    },
    ErrorDef {
        number: 8116,
        severity: 16,
        template: "Data type %ls is not accepted for argument %d of the %ls function.",
    },
    ErrorDef {
        number: 8117,
        severity: 16,
        template: "Data type %ls is not accepted by the %ls operator.",
    },
    ErrorDef {
        number: 8120,
        severity: 16,
        template: "Column '%.*ls.%.*ls' of the select list is neither aggregated nor part of GROUP BY.",
    },
    ErrorDef {
        number: 8121,
        severity: 16,
        template: "Column '%.*ls.%.*ls' of the HAVING clause is neither aggregated nor part of GROUP BY.",
    },
    ErrorDef {
        number: 8134,
        severity: 16,
        template: "Division by zero.",
    },
    ErrorDef {
        number: 8147,
        severity: 16,
        template: "Column '%.*ls' of table '%.*ls' is nullable and cannot be an IDENTITY column.",
    },
    ErrorDef {
        number: 8152,
        severity: 16,
        template: "Data too long: the string or binary value would be cut.",
    },
    ErrorDef {
        number: 8153,
        severity: 10,
        template: "Warning: an aggregate or SET operation ignored a NULL value.",
    },
    ErrorDef {
        number: 8154,
        severity: 15,
        template: "The reference to table '%.*ls' is ambiguous.",
    },
    ErrorDef {
        number: 8155,
        severity: 15,
        template: "Column %d of '%.*ls' has no name.",
    },
    ErrorDef {
        number: 8168,
        severity: 16,
        template: "The name '%.*ls' is used more than once for a constraint, column, index or trigger here; names have to be unique.",
    },
    ErrorDef {
        number: 8169,
        severity: 16,
        template: "The character string could not be converted to uniqueidentifier.",
    },
    ErrorDef {
        number: 8170,
        severity: 16,
        template: "A uniqueidentifier value does not fit in the char result.",
    },
    ErrorDef {
        number: 8631,
        severity: 17,
        template: "Internal error: the server ran out of stack; the query is probably nested too deeply and needs simplifying.",
    },
    ErrorDef {
        number: 9806,
        severity: 16,
        template: "Datepart %.*ls cannot be used with the date function %.*ls.",
    },
    ErrorDef {
        number: 9807,
        severity: 16,
        template: "The input string does not match style %d; change the string or the style.",
    },
    ErrorDef {
        number: 9809,
        severity: 16,
        template: "Style %d is not defined for converting %s to %s.",
    },
    ErrorDef {
        number: 9810,
        severity: 16,
        template: "Datepart %.*ls cannot be used with the date function %.*ls on data type %s.",
    },
    ErrorDef {
        number: 10709,
        severity: 16,
        template: "The rows of a table value constructor have to supply the same number of columns.",
    },
    ErrorDef {
        number: 18452,
        severity: 14,
        template: "Login refused: integrated authentication does not accept a login from an untrusted domain.%.*ls",
    },
    ErrorDef {
        number: 18456,
        severity: 14,
        template: "Login refused for user '%.*ls'.%.*ls%.*ls",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers the engine relies on: syntax and binding, conversions and arithmetic,
    /// constraints, DDL and databases, transactions and concurrency, connection,
    /// informational (severity <= 10).
    const REQUIRED_NUMBERS: &[u32] = &[
        102, 105, 137, 148, 156, 207, 208, 209, 213, 2812, 4104, 4145, 8120, 241, 242, 245, 8114,
        8115, 8134, 8152, 2628, 515, 544, 547, 2601, 2627, 911, 1801, 2714, 3701, 4060, 226, 1205,
        1222, 3902, 3903, 3960, 18452, 18456, 5701, 5703,
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
        (120, 15, &[]),
        (121, 15, &[]),
        (127, 15, &[]),
        (130, 15, &[]),
        (131, 15, &["%d", "%S_MSG", "%.*ls", "%d"]),
        (134, 15, &["%.*ls"]),
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
        (189, 15, &["%ls", "%d", "%d"]),
        (191, 15, &[]),
        (192, 16, &[]),
        (195, 15, &["%.*ls", "%S_MSG"]),
        (205, 16, &[]),
        (206, 16, &["%ls", "%ls"]),
        (207, 16, &["%.*ls"]),
        (208, 16, &["%.*ls"]),
        (209, 16, &["%.*ls"]),
        (210, 16, &[]),
        (213, 16, &[]),
        (215, 16, &["%.*ls"]),
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
        (339, 16, &[]),
        (402, 16, &["%s", "%s", "%s"]),
        (447, 16, &["%ls"]),
        (448, 16, &["%.*ls"]),
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
        (1047, 15, &[]),
        (1060, 15, &[]),
        (1062, 16, &[]),
        (1065, 15, &[]),
        (1087, 15, &["%.*ls"]),
        (1088, 15, &["%.*ls"]),
        (1205, 13, &["%d", "%.*ls"]),
        (1222, 16, &[]),
        (1750, 10, &[]),
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
        (2717, 15, &["%d", "%S_MSG", "%.*ls", "%d"]),
        (2749, 16, &["%.*ls"]),
        (2750, 16, &["%d", "%d", "%d"]),
        (2812, 16, &["%.*ls"]),
        (3623, 16, &[]),
        (3701, 11, &["%S_MSG", "%S_MSG", "%.*ls"]),
        (3708, 16, &["%S_MSG", "%S_MSG", "%.*ls", "%S_MSG"]),
        (3723, 16, &["%.*ls", "%ls"]),
        (3726, 16, &["%.*ls"]),
        (3902, 16, &[]),
        (3903, 16, &[]),
        (3952, 16, &["%.*ls"]),
        (3960, 16, &["%.*ls", "%.*ls"]),
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
        (5701, 10, &["%.*ls"]),
        (5703, 10, &["%.*ls"]),
        (7202, 11, &["%.*ls"]),
        (8101, 16, &["%.*ls"]),
        (8102, 16, &["%.*ls"]),
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
        (8147, 16, &["%.*ls", "%.*ls"]),
        (8152, 16, &[]),
        (8153, 10, &[]),
        (8154, 15, &["%.*ls"]),
        (8155, 15, &["%d", "%.*ls"]),
        (8168, 16, &["%.*ls"]),
        (8169, 16, &[]),
        (8170, 16, &[]),
        (8631, 17, &[]),
        (9806, 16, &["%.*ls", "%.*ls"]),
        (9807, 16, &["%d"]),
        (9809, 16, &["%d", "%s", "%s"]),
        (9810, 16, &["%.*ls", "%.*ls", "%s"]),
        (10709, 16, &[]),
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
            let def = message_template(number)
                .unwrap_or_else(|| panic!("error {number} is in the catalog"));
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
}

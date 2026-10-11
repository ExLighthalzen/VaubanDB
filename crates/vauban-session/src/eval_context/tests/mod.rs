use std::sync::Arc;

use vauban_catalog::{ColumnDef, QualifiedName, TableDef};
use vauban_storage::MemoryStorage;
use vauban_txn::IsolationLevel;
use vauban_types::{SqlType, TypeInfo};

use super::*;
use crate::server::Engine;

fn state() -> SessionState {
    let mut state = SessionState::new(57);
    state.login = "sa".into();
    state.app_name = "sqlcmd".into();
    state.hostname = "WORKSTATION".into();
    state.rowcount = 3;
    state.last_error = 8134;
    state
}

#[test]
fn reads_the_session_state() {
    let state = state();
    let ctx = SessionEvalContext::new(&state, None);
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.spid(), 57);
    assert_eq!(ctx.current_database(), "master");
    assert_eq!(ctx.rowcount(), 3);
    assert_eq!(ctx.login_name(), Some("sa"));
    assert_eq!(ctx.app_name(), Some("sqlcmd"));
    assert_eq!(ctx.host_name(), Some("WORKSTATION"));
    assert_eq!(ctx.datefirst(), 7);
    assert_eq!(ctx.language(), "us_english");
    assert_eq!(
        ctx.server_name(),
        "vauban",
        "the default of a state built by hand"
    );
}

/// The name of the state is the one answered, and a session started elsewhere answers
/// elsewhere: the counter-proof of a constant answer is that the two names below differ
/// from each other and from the default `vauban`.
#[test]
fn server_name_comes_from_the_session_state() {
    for name in ["VAUBAN-NODE-2", "sql-01\\INSTANCE"] {
        let mut state = state();
        state.server_name = name.into();
        let ctx = SessionEvalContext::new(&state, None);
        let ctx: &dyn EvalContext = &ctx;
        assert_eq!(ctx.server_name(), name);
        assert_ne!(ctx.server_name(), "vauban");
    }
}

#[test]
fn login7_host_and_application_preserve_empty_space_and_128_characters() {
    // SQL Server 2022: empty, one ASCII space and 128 ASCII characters survive
    // HOST_NAME()/APP_NAME(); an empty username is rejected before evaluation.
    for text in [String::new(), " ".to_owned(), "x".repeat(128)] {
        let mut state = SessionState::new(51);
        state.hostname = text.clone();
        state.app_name = text.clone();
        state.login = "sa".into();
        let ctx = SessionEvalContext::new(&state, None);
        assert_eq!(ctx.host_name(), Some(text.as_str()));
        assert_eq!(ctx.app_name(), Some(text.as_str()));
        assert_eq!(ctx.login_name(), Some("sa"));
    }
    let state = SessionState::new(51);
    assert_eq!(SessionEvalContext::new(&state, None).login_name(), None);
}

#[test]
fn error_and_trancount_are_session_variables() {
    let mut state = state();
    state.trancount = 2;
    let ctx = SessionEvalContext::new(&state, None);
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.variable("@@ERROR"), Some(Value::I32(8134)));
    assert_eq!(ctx.variable("@@TRANCOUNT"), Some(Value::I32(2)));
    // T-SQL names are case-insensitive; the parser hands the text as written.
    assert_eq!(ctx.variable("@@error"), Some(Value::I32(8134)));
    assert_eq!(ctx.variable("@@ROWCOUNT"), None, "own method on the trait");
    assert_eq!(ctx.variable("@@FETCH_STATUS"), None);
}

#[test]
fn version_banner_and_edition_come_from_the_session_state() {
    let mut state = state();
    state.version_banner = "CustomBanner".into();
    state.edition = "Custom Edition".into();
    let ctx = SessionEvalContext::new(&state, None);
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.version_banner(), Some("CustomBanner"));
    assert_eq!(ctx.edition(), Some("Custom Edition"));
}

#[test]
fn the_clock_is_read_once_and_never_again() {
    let state = SessionState::new(51);
    let ctx = SessionEvalContext::new(&state, None);
    let first = ctx.now_local();
    // Enough 100 ns ticks pass here for a clock read per call to differ.
    for _ in 0..10_000 {
        assert_eq!(ctx.now_local(), first);
    }
    assert_eq!(ctx.now_utc(), first);
}

#[test]
fn two_contexts_read_two_instants() {
    // The freeze is per context, that is per statement: a second statement of the same
    // batch reads the clock again. Monotonic, not necessarily different: the assertion
    // is on the order, which a per-call read would also satisfy — what distinguishes
    // the two is `the_clock_is_read_once_and_never_again` above.
    let state = SessionState::new(51);
    let first = SessionEvalContext::new(&state, None).now_utc();
    let second = SessionEvalContext::new(&state, None).now_utc();
    assert!(
        second.date.days > first.date.days || second.time.ticks_100ns >= first.time.ticks_100ns
    );
}

#[test]
fn the_clock_lands_in_this_century() {
    // Guards the epoch conversion: a wrong origin lands centuries away. 2020-01-01 and
    // 2200-01-01 in days since 0001-01-01.
    let state = SessionState::new(51);
    let now = SessionEvalContext::new(&state, None).now_utc();
    assert!(now.date.days > days_from_civil(2020, 1, 1), "{now:?}");
    assert!(now.date.days < days_from_civil(2200, 1, 1), "{now:?}");
    assert!(
        now.time.ticks_100ns < SECONDS_PER_DAY * TICKS_PER_SECOND,
        "{now:?}"
    );
}

#[test]
fn the_clock_can_be_given_by_the_caller() {
    let state = SessionState::new(51);
    let noon = DateTime2 {
        date: Date {
            days: days_from_civil(2026, 9, 10),
        },
        time: Time {
            ticks_100ns: 12 * 3600 * TICKS_PER_SECOND,
        },
    };
    let ctx = SessionEvalContext::with_clock(&state, noon);
    assert_eq!(ctx.now_local(), noon);
    assert_eq!(ctx.now_utc(), noon);
}

// The metadata functions read the catalogue of the statement.

/// The table the tests of `OBJECT_ID` resolve, `dbo.t` of `master` with one `int`
/// column (module documentation).
fn table_def() -> TableDef {
    TableDef {
        name: QualifiedName {
            database: "master".to_owned(),
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
        },
        columns: vec![ColumnDef {
            name: "a".to_owned(),
            ty: TypeInfo::new(SqlType::Int, false),
            default: None,
            identity: None,
            computed: None,
        }],
        constraints: Vec::new(),
    }
}

/// An engine bootstrapped by `Engine::new` with `dbo.t` created in `master`, and the
/// identifier the catalogue gave that table.
fn engine_with_a_table() -> (Engine, i32) {
    let engine = Engine::new(Arc::new(MemoryStorage::new()));
    let created = {
        let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
        let meta = engine
            .catalog
            .create_table(&handle, &table_def())
            .expect("the table is created");
        engine
            .txn
            .commit(handle)
            .expect("the creating transaction commits");
        meta.id.0
    };
    (engine, created)
}

/// A snapshot of [`engine_with_a_table`] and the identifier of its table — what
/// `batch.rs` hands to [`SessionEvalContext::new`] while it binds a batch.
fn catalogue_with_a_table() -> (CatalogSnapshot, i32) {
    let (engine, created) = engine_with_a_table();
    let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
    let snapshot = engine.catalog.snapshot(&handle);
    engine
        .txn
        .commit(handle)
        .expect("the reading transaction commits");
    (snapshot, created)
}

#[test]
fn object_id_of_created_table_is_some() {
    let (snapshot, created) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    // Each form resolves (module documentation); here each answers the identifier
    // `create_table` handed out, which is more than `is_some`.
    for written in [
        "dbo.t",
        "[dbo].[t]",
        "\"dbo\".\"t\"",
        "t",
        "DBO.T",
        "master.dbo.t",
        ".t",
        ".dbo.t",
        "..dbo.t",
        "[].[].dbo.t",
        ".master.dbo.t",
        "[master].[dbo].[t]",
        "dbo.t ",
        "[dbo].[t ]",
    ] {
        assert_eq!(ctx.object_id(written), Some(created), "{written}");
    }
}

#[test]
fn object_name_roundtrip() {
    let (snapshot, created) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    // The object part alone, `OBJECT_NAME` being a `sysname` and not a three-part name.
    assert_eq!(ctx.object_name(created), Some("t".to_owned()));
    // The name comes back with the case it was created with, not the case written:
    // `OBJECT_NAME(OBJECT_ID('DBO.T'))` is `t` (`CatalogSnapshot`).
    let resolved = ctx.object_id("DBO.T").expect("the table resolves");
    assert_eq!(resolved, created);
    assert_eq!(ctx.object_name(resolved), Some("t".to_owned()));
}

#[test]
fn object_id_unknown_is_none() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    // The forms below resolve to nothing, that is NULL (module documentation): an
    // absent table, a written server part, five parts, a blank inside an undelimited
    // part, a character outside the delimiters of a part, an empty object part, a
    // delimiter left open, another schema, and a database this snapshot does not hold.
    for written in [
        "dbo.no_such_table",
        "no_such_table",
        "srv.master.dbo.t",
        "a.b.master.dbo.t",
        " dbo.t ",
        "dbo . t",
        "[dbo].[t] ",
        "[dbo].[t]\t",
        "[dbo].[t]x",
        "[dbo] .[t]",
        " [dbo].[t]",
        "[dbo] .t",
        "dbo.",
        "",
        "[dbo].[t",
        "sys.t",
        "no_such_db.dbo.t",
    ] {
        assert_eq!(ctx.object_id(written), None, "{written}");
    }
    // An identifier far outside the range this snapshot holds: NULL, as on SQL Server.
    assert_eq!(ctx.object_name(-100_000), None);
    assert_eq!(ctx.object_name(i32::MAX), None);
    // An identifier outside both numberings: NULL here and on SQL Server, where `-1`
    // names `sys.objects` here (see `object_name` above).
    assert_eq!(ctx.object_name(-987_654_321), None);
    assert_eq!(ctx.object_name(-1), Some("objects".to_owned()));
}

#[test]
fn db_id_master_is_some() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    assert_eq!(state.database, "master", "the session starts in `master`");
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    let master = ctx.database_id(None).expect("`master` has an identifier");
    // Named, and named in another case: `CatalogSnapshot::database` compares under the
    // default collation.
    assert_eq!(ctx.database_id(Some("master")), Some(master));
    assert_eq!(ctx.database_id(Some("MASTER")), Some(master));
    // Another database of the bootstrap carries an identifier of its own.
    let tempdb = ctx
        .database_id(Some("tempdb"))
        .expect("`tempdb` is in the bootstrap");
    assert_ne!(tempdb, master);
    assert_eq!(ctx.database_id(Some("no_such_db")), None);
}

#[test]
fn database_name_answers_any_database_of_the_snapshot() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    let master = ctx.database_id(None).expect("`master` has an identifier");
    assert_eq!(ctx.database_name(master), Some("master".to_owned()));
    // Another database of the same instance answers its own name, read from `master`.
    let tempdb = ctx
        .database_id(Some("tempdb"))
        .expect("`tempdb` is in the bootstrap");
    assert_eq!(ctx.database_name(tempdb), Some("tempdb".to_owned()));
    assert_eq!(tempdb, 2, "`DB_NAME(2)` is `tempdb`");
    // `DB_NAME(0)` is the current database, `master` for this session.
    assert_eq!(ctx.database_name(0), Some("master".to_owned()));
    // `DB_NAME(-1)` and an identifier no database carries: NULL.
    assert_eq!(ctx.database_name(-1), None);
    assert_eq!(ctx.database_name(999), None);
    assert_eq!(ctx.database_name(i32::MAX), None);
    // From another current database, `0` follows it and `1` still names `master`.
    let mut in_tempdb = state.clone();
    in_tempdb.database = "tempdb".into();
    let ctx = SessionEvalContext::new(&in_tempdb, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.database_name(0), Some("tempdb".to_owned()));
    assert_eq!(ctx.database_name(master), Some("master".to_owned()));
}

/// `SCHEMA_NAME(1)`, `(3)`, `(4)` and `(99)` answer `dbo`, `INFORMATION_SCHEMA`, `sys`
/// and `NULL` (`schema_name_from_identifier`).
#[test]
fn schema_name_from_identifier() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.schema_name(1), Some("dbo".to_owned()));
    assert_eq!(ctx.schema_name(3), Some("INFORMATION_SCHEMA".to_owned()));
    assert_eq!(ctx.schema_name(4), Some("sys".to_owned()));
    assert_eq!(ctx.schema_name(99), None);
}

/// `SCHEMA_ID()` and `SCHEMA_ID('dbo')` answer 1, the number of the default schema, with
/// a catalogue as without one: this answer is the session's, not the snapshot's, and
/// SQL Server answers 1 to both forms.
#[test]
fn schema_id_of_the_default_schema_is_one() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    let with_catalogue = SessionEvalContext::new(&state, Some(&snapshot));
    let with_catalogue: &dyn EvalContext = &with_catalogue;
    assert_eq!(with_catalogue.schema_id(None), Some(1));
    assert_eq!(with_catalogue.schema_id(Some("dbo")), Some(1));
    let without = SessionEvalContext::new(&state, None);
    let without: &dyn EvalContext = &without;
    assert_eq!(without.schema_id(None), Some(1));
    assert_eq!(without.schema_id(Some("dbo")), Some(1));
}

/// The comparison is the default collation's: case folded and trailing blanks trimmed,
/// a leading blank kept. `SCHEMA_ID('DBO')` and `SCHEMA_ID('dbo ')` are 1 on SQL Server,
/// `SCHEMA_ID(' dbo')` is NULL there; the leading blank is the vector that separates
/// "trailing blanks trimmed" from "blanks trimmed".
#[test]
fn schema_id_compares_under_the_collation() {
    let state = state();
    let ctx = SessionEvalContext::new(&state, None);
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.schema_id(Some("DBO")), Some(1));
    assert_eq!(ctx.schema_id(Some("dbo ")), Some(1));
    assert_eq!(ctx.schema_id(Some(" dbo")), None);
    assert_eq!(ctx.schema_id(Some("[dbo]")), None);
}

/// A schema that is not the default one is not resolved here, the bound of the module
/// documentation: SQL Server answers 2, 3 and 4 for `guest`, `INFORMATION_SCHEMA` and
/// `sys` where this context answers `None` for the three, as for `no_such_schema`,
/// which neither server holds (the four assertions below).
#[test]
fn schema_id_of_another_schema_is_not_resolved_yet() {
    let (snapshot, _) = catalogue_with_a_table();
    let state = state();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    for name in ["sys", "INFORMATION_SCHEMA", "guest", "no_such_schema"] {
        assert_eq!(ctx.schema_id(Some(name)), None, "{name}");
    }
}

#[test]
fn a_system_view_resolves_and_names_itself() {
    // `sys.tables` is an entry of `views/` with a negative identifier, and it
    // resolves from a session whose database is not `master`, as on SQL Server.
    let (snapshot, created) = catalogue_with_a_table();
    let mut state = state();
    state.database = "msdb".to_owned();
    let ctx = SessionEvalContext::new(&state, Some(&snapshot));
    let ctx: &dyn EvalContext = &ctx;
    let view = ctx.object_id("sys.tables").expect("`sys.tables` resolves");
    assert!(view < 0, "a system view carries a negative id: {view}");
    assert_eq!(ctx.object_name(view), Some("tables".to_owned()));
    // The table of `master` is out of reach of this session unless its database is
    // written, the schema `dbo` of `msdb` holding no `t`.
    assert_eq!(ctx.object_id("dbo.t"), None);
    assert_eq!(ctx.object_id("master.dbo.t"), Some(created));
}

#[test]
fn the_deferred_catalogue_is_read_on_the_first_call_and_not_before() {
    // What `batch.rs` builds for a statement: the snapshot is taken from the transaction
    // of the statement, and only if something asks for it. A statement that calls no
    // metadata function leaves the cell empty.
    let (engine, created) = engine_with_a_table();
    let handle = engine.txn.begin(IsolationLevel::ReadCommitted);
    let state = state();
    let ctx = SessionEvalContext::deferred(&state, &engine.catalog, &handle);
    let built = |ctx: &SessionEvalContext<'_>| match &ctx.catalog {
        Some(CatalogSource::Deferred { built, .. }) => built.get().is_some(),
        _ => unreachable!("`deferred` builds a `Deferred` source"),
    };
    assert!(!built(&ctx), "nothing has asked for the catalogue yet");
    assert_eq!(ctx.rowcount(), 3, "the session state needs no catalogue");
    assert!(!built(&ctx));
    assert_eq!(ctx.object_id("dbo.t"), Some(created));
    assert!(built(&ctx), "the first call read the catalogue");
    // Read once: the second call answers from the same snapshot.
    assert_eq!(ctx.object_name(created), Some("t".to_owned()));
    engine
        .txn
        .commit(handle)
        .expect("the reading transaction commits");
}

#[test]
fn without_a_catalogue_nothing_resolves() {
    // The counter-proof of the four catalogue methods above: a context built without a
    // snapshot answers NULL for the seven calls below.
    let state = state();
    let ctx = SessionEvalContext::new(&state, None);
    let ctx: &dyn EvalContext = &ctx;
    assert_eq!(ctx.object_id("dbo.t"), None);
    assert_eq!(ctx.object_id("sys.tables"), None);
    assert_eq!(ctx.object_name(1), None);
    assert_eq!(ctx.database_id(None), None);
    assert_eq!(ctx.database_id(Some("master")), None);
    assert_eq!(ctx.database_name(1), None);
    assert_eq!(ctx.schema_name(1), None);
}

#[test]
fn a_written_name_is_split_on_its_undelimited_dots() {
    // The splitting `object_id` resolves through. An empty schema or database part is
    // `None`, so `.t` and `..t` name `t` of the default schema of the current database,
    // as on SQL Server.
    assert_eq!(
        WrittenName::parse("t"),
        Some(WrittenName {
            server: None,
            database: None,
            schema: None,
            object: "t".to_owned(),
        })
    );
    assert_eq!(
        WrittenName::parse("[my db].[my schema].[my table]"),
        Some(WrittenName {
            server: None,
            database: Some("my db".to_owned()),
            schema: Some("my schema".to_owned()),
            object: "my table".to_owned(),
        })
    );
    assert_eq!(WrittenName::parse(".t").and_then(|name| name.schema), None);
    assert_eq!(
        WrittenName::parse("..t").and_then(|name| name.database),
        None
    );
    assert_eq!(
        WrittenName::parse("..t").map(|name| name.object),
        Some("t".to_owned())
    );
    assert_eq!(
        WrittenName::parse("srv.db.dbo.t").and_then(|name| name.server),
        Some("srv".to_owned())
    );
    // A dot inside a delimiter is part of the name, and a doubled closing delimiter
    // stands for itself: `[dbo]].[t]` is the single name `dbo].[t`, which resolves to
    // nothing on SQL Server too.
    assert_eq!(
        WrittenName::parse("[a.b]").map(|name| name.object),
        Some("a.b".to_owned())
    );
    assert_eq!(
        WrittenName::parse("[dbo]].[t]").map(|name| name.object),
        Some("dbo].[t".to_owned())
    );
    // Five parts and a delimiter left open give `None`, NULL on SQL Server.
    assert_eq!(WrittenName::parse("a.b.c.d.e"), None);
    assert_eq!(WrittenName::parse("[dbo].[t"), None);
    assert_eq!(WrittenName::parse("\"dbo"), None);
    // An empty server part is read as "not written", like an empty database and an empty
    // schema.
    assert_eq!(
        WrittenName::parse("..dbo.t"),
        Some(WrittenName {
            server: None,
            database: None,
            schema: Some("dbo".to_owned()),
            object: "t".to_owned(),
        })
    );
    assert_eq!(
        WrittenName::parse("[].[].dbo.t"),
        WrittenName::parse("..dbo.t")
    );
    // A delimited part fills its part: a character before the opening delimiter or after
    // the closing one refuses the whole name, where the same character inside the
    // delimiter is part of it (table of `split_parts`).
    for refused in [
        "[dbo].[t] ",
        "[dbo].[t]\t",
        "[dbo].[t]x",
        "[dbo] .[t]",
        " [dbo].[t]",
        "[dbo] .t",
        "[a][b]",
    ] {
        assert_eq!(WrittenName::parse(refused), None, "{refused}");
    }
    assert_eq!(
        WrittenName::parse("[dbo].[t ]").map(|name| name.object),
        Some("t ".to_owned())
    );
    assert_eq!(
        WrittenName::parse("dbo.t ").map(|name| name.object),
        Some("t ".to_owned())
    );
}

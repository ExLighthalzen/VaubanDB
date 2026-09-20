use super::*;

#[test]
fn static_context_returns_its_fixed_values() {
    let now = DateTime2 {
        date: Date { days: 730_119 },
        time: Time { ticks_100ns: 42 },
    };
    let ctx = StaticContext {
        now,
        spid: 57,
        database: "master".to_owned(),
        server_name: "VAUBAN".to_owned(),
        rowcount: 3,
        ..StaticContext::default()
    };
    let dyn_ctx: &dyn EvalContext = &ctx;
    assert_eq!(dyn_ctx.now_local(), now);
    assert_eq!(dyn_ctx.now_utc(), now);
    assert_eq!(dyn_ctx.spid(), 57);
    assert_eq!(dyn_ctx.current_database(), "master");
    assert_eq!(dyn_ctx.server_name(), "VAUBAN");
    assert_eq!(dyn_ctx.rowcount(), 3);
    assert_eq!(dyn_ctx.last_identity(), None);
    assert_eq!(dyn_ctx.object_id("dbo.t"), None);
    assert_eq!(dyn_ctx.object_name(1), None);
    assert_eq!(dyn_ctx.variable("@@ERROR"), None);
}

#[test]
fn static_context_default_is_neutral() {
    let ctx = StaticContext::default();
    assert_eq!(ctx.spid(), 0);
    assert_eq!(ctx.rowcount(), 0);
    assert_eq!(ctx.current_database(), "");
    assert_eq!(ctx.server_name(), "");
    assert_eq!(ctx.now_local().date.days, 0);
    assert_eq!(ctx.now_local().time.ticks_100ns, 0);
}

/// A context that implements the methods without a default and nothing else: the
/// defaulted methods must still answer.
struct MinimalContext;

impl EvalContext for MinimalContext {
    fn now_local(&self) -> DateTime2 {
        DateTime2 {
            date: Date { days: 0 },
            time: Time { ticks_100ns: 0 },
        }
    }

    fn now_utc(&self) -> DateTime2 {
        self.now_local()
    }

    fn rowcount(&self) -> i64 {
        0
    }

    fn last_identity(&self) -> Option<Decimal> {
        None
    }

    fn spid(&self) -> i16 {
        0
    }

    fn current_database(&self) -> &str {
        "master"
    }

    fn server_name(&self) -> &str {
        "VAUBAN"
    }

    fn object_id(&self, _name: &str) -> Option<i32> {
        None
    }

    fn object_name(&self, _id: i32) -> Option<String> {
        None
    }

    fn variable(&self, name: &str) -> Option<Value> {
        match name {
            "@@ERROR" => Some(Value::I32(0)),
            _ => None,
        }
    }
}

#[test]
fn variable_is_returned_by_value() {
    let ctx: &dyn EvalContext = &MinimalContext;
    // The caller owns the value: the context computed it and kept nothing.
    let value: Option<Value> = ctx.variable("@@ERROR");
    assert_eq!(value, Some(Value::I32(0)));
    assert_eq!(ctx.variable("@@TRANCOUNT"), None);
}

#[test]
fn eval_context_defaults() {
    let ctx: &dyn EvalContext = &MinimalContext;
    assert_eq!(ctx.host_name(), None);
    assert_eq!(ctx.app_name(), None);
    assert_eq!(ctx.login_name(), None);
    assert_eq!(ctx.user_name(), None);
    assert_eq!(ctx.database_id(None), None);
    assert_eq!(ctx.database_id(Some("master")), None);
    assert_eq!(ctx.database_name(1), None);
    assert_eq!(ctx.schema_name(1), None);
    assert_eq!(ctx.schema_id(None), None);
    assert_eq!(ctx.schema_id(Some("dbo")), None);
    assert_eq!(ctx.scope_identity(), None);
    assert_eq!(ctx.ident_current("dbo.t"), None);
    assert_eq!(ctx.datefirst(), 7);
    assert_eq!(ctx.language(), "us_english");
}

#[test]
fn static_context_new_accessors() {
    let ctx = StaticContext {
        host_name: Some("WORKSTATION".to_owned()),
        app_name: Some("sqlcmd".to_owned()),
        login_name: Some("sa".to_owned()),
        user_name: Some("dbo".to_owned()),
        datefirst: 1,
        ..StaticContext::default()
    };
    let dyn_ctx: &dyn EvalContext = &ctx;
    assert_eq!(dyn_ctx.host_name(), Some("WORKSTATION"));
    assert_eq!(dyn_ctx.app_name(), Some("sqlcmd"));
    assert_eq!(dyn_ctx.login_name(), Some("sa"));
    assert_eq!(dyn_ctx.user_name(), Some("dbo"));
    assert_eq!(dyn_ctx.datefirst(), 1);

    let empty = StaticContext::default();
    let dyn_empty: &dyn EvalContext = &empty;
    assert_eq!(dyn_empty.host_name(), None);
    assert_eq!(dyn_empty.app_name(), None);
    assert_eq!(dyn_empty.login_name(), None);
    assert_eq!(dyn_empty.user_name(), None);
    assert_eq!(dyn_empty.datefirst(), 7);
    // Not backed by a field: the trait defaults answer.
    assert_eq!(dyn_empty.language(), "us_english");
    assert_eq!(dyn_empty.scope_identity(), None);
}

/// The three transaction methods answer the value of a session outside a transaction
/// on a context that overrides nothing, and on `StaticContext::default()`.
///
/// The three values are those of SQL Server outside a transaction and without `SET
/// LOCK_TIMEOUT`: `XACT_STATE()` 0, `@@LOCK_TIMEOUT` -1, `@@TRANCOUNT` 0. `-1` is the
/// one that is not a zero: a default of 0 would mean "give up as soon as a lock is
/// held", the opposite of the actual default.
#[test]
fn defaults_are_neutral() {
    let minimal: &dyn EvalContext = &MinimalContext;
    assert_eq!(minimal.xact_state(), 0);
    assert_eq!(minimal.lock_timeout(), -1);
    assert_eq!(minimal.trancount(), 0);

    let fallback = StaticContext::default();
    let static_ctx: &dyn EvalContext = &fallback;
    assert_eq!(static_ctx.xact_state(), 0);
    assert_eq!(static_ctx.lock_timeout(), -1);
    assert_eq!(static_ctx.trancount(), 0);
}

/// The three fields are read back, each with a value the defaults do not hold: a
/// context built with them answers them and not the `0 / -1 / 0` above.
#[test]
fn static_context_carries_the_transaction_state() {
    let ctx = StaticContext {
        xact_state: -1,
        lock_timeout: 5_000,
        trancount: 2,
        ..StaticContext::default()
    };
    let dyn_ctx: &dyn EvalContext = &ctx;
    assert_eq!(dyn_ctx.xact_state(), -1);
    assert_eq!(dyn_ctx.lock_timeout(), 5_000);
    assert_eq!(dyn_ctx.trancount(), 2);
}

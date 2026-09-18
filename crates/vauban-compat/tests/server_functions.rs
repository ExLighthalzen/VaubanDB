//! Integration tests for the server functions registered by `compat`.

use vauban_compat::register_functions;
use vauban_sysfn::{EvalArgs, EvalContext, FunctionDef, lookup};
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

struct Databases;

impl EvalContext for Databases {
    fn now_local(&self) -> vauban_types::DateTime2 {
        vauban_types::DateTime2 {
            date: vauban_types::Date { days: 0 },
            time: vauban_types::Time { ticks_100ns: 0 },
        }
    }

    fn now_utc(&self) -> vauban_types::DateTime2 {
        self.now_local()
    }

    fn rowcount(&self) -> i64 {
        0
    }

    fn last_identity(&self) -> Option<vauban_types::Decimal> {
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

    fn object_id(&self, name: &str) -> Option<i32> {
        if name.eq_ignore_ascii_case("sys.tables") {
            Some(1)
        } else {
            None
        }
    }

    fn object_name(&self, _id: i32) -> Option<String> {
        None
    }

    fn variable(&self, _name: &str) -> Option<Value> {
        None
    }

    fn database_id(&self, name: Option<&str>) -> Option<i32> {
        match name.unwrap_or("master") {
            "master" | "MASTER" => Some(1),
            _ => None,
        }
    }

    fn schema_id(&self, name: Option<&str>) -> Option<i32> {
        match name {
            Some("dbo") | Some("DBO") => Some(1),
            None => Some(1),
            _ => None,
        }
    }
}

fn eval(def: &FunctionDef, values: &[Value], ctx: &dyn EvalContext) -> Value {
    let types: Vec<TypeInfo> = values
        .iter()
        .map(|_| TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false))
        .collect();
    let result = (def.return_type)(&types).expect("return_type must succeed");
    let args = EvalArgs {
        values,
        types: &types,
        result: &result,
    };
    (def.eval)(&args, ctx).expect("eval must succeed")
}

fn string(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

#[test]
fn lookup_finds_all_seven_functions_with_return_types() {
    register_functions();
    let expected = [
        ("CONNECTIONPROPERTY", SqlType::NVarChar(Len::Fixed(128)), 1),
        ("SESSIONPROPERTY", SqlType::Int, 1),
        ("HAS_DBACCESS", SqlType::Int, 1),
        ("ORIGINAL_LOGIN", SqlType::NVarChar(Len::Fixed(128)), 0),
        ("IS_SRVROLEMEMBER", SqlType::Int, 1),
        ("IS_MEMBER", SqlType::Int, 1),
        ("HAS_PERMS_BY_NAME", SqlType::Int, 3),
    ];
    for (name, ty, arity) in expected {
        let def = lookup(name).unwrap_or_else(|| panic!("{name} must be registered"));
        assert_eq!(def.name, name);
        let arg_types = vec![TypeInfo::new(SqlType::NVarChar(Len::Fixed(20)), false); arity];
        let result = (def.return_type)(&arg_types).expect("return_type must succeed");
        assert_eq!(result.ty, ty, "{name}");
        assert!(result.nullable, "{name}");
    }
}

#[test]
fn has_dbaccess_and_has_perms_by_name_use_the_catalogue() {
    register_functions();
    let ctx: &dyn EvalContext = &Databases;
    let has_dbaccess = lookup("HAS_DBACCESS").expect("registered");
    assert_eq!(eval(has_dbaccess, &[string("master")], ctx), Value::I32(1));
    assert_eq!(eval(has_dbaccess, &[string("nosuchdb")], ctx), Value::Null);

    let has_perms = lookup("HAS_PERMS_BY_NAME").expect("registered");
    assert_eq!(
        eval(
            has_perms,
            &[string("master"), string("DATABASE"), string("SELECT"),],
            ctx
        ),
        Value::I32(1)
    );
    assert_eq!(
        eval(
            has_perms,
            &[Value::Null, string("SERVER"), string("VIEW ANY DATABASE"),],
            ctx
        ),
        Value::Null
    );
    assert_eq!(
        eval(
            has_perms,
            &[string("sys.tables"), string("OBJECT"), string("SELECT")],
            ctx
        ),
        Value::I32(1)
    );
    assert_eq!(
        eval(
            has_perms,
            &[string("dbo"), string("SCHEMA"), string("SELECT")],
            ctx
        ),
        Value::I32(1)
    );
    assert_eq!(
        eval(
            has_perms,
            &[string("nosuch"), string("OBJECT"), string("SELECT")],
            ctx
        ),
        Value::I32(0)
    );
}

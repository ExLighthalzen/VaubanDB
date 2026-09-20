use super::{compound_operator, target_source};
use crate::bound::ColumnBinding;
use crate::star::Source;
use vauban_catalog::ColumnId;
use vauban_parser::{AssignOp, BinaryOp, Ident, ObjectName, Span};
use vauban_types::{SqlType, TypeInfo};

fn ident(value: &str) -> Ident {
    Ident {
        value: value.to_owned(),
        quoted: false,
    }
}

fn name(parts: &[&str]) -> ObjectName {
    let mut name = ObjectName {
        server: None,
        database: None,
        schema: None,
        name: ident(parts[parts.len() - 1]),
        span: Span::EMPTY,
    };
    let mut rest = parts[..parts.len() - 1].iter().rev();
    name.schema = rest.next().map(|part| ident(part));
    name.database = rest.next().map(|part| ident(part));
    name.server = rest.next().map(|part| ident(part));
    name
}

fn source(alias: &str) -> Source {
    let columns = [ColumnBinding {
        column: ColumnId(1),
        index: 0,
        name: "a".to_owned(),
        ty: TypeInfo::new(SqlType::Int, false),
    }];
    Source::new(alias, Some(&name(&["dbo", "t"])), &columns, "dbo", "master")
}

/// A one-part target matches a source by its exposed name, in upper case as in lower
/// case; a qualified one matches nothing, and neither does a name no source answers to.
#[test]
fn a_one_part_target_matches_the_exposed_name_of_a_source() {
    let sources = [source("x"), source("t")];
    assert!(target_source(&name(&["x"]), &sources).is_some());
    assert!(target_source(&name(&["X"]), &sources).is_some());
    assert_eq!(
        target_source(&name(&["t"]), &sources).map(Source::exposed_name),
        Some("t")
    );
    assert!(target_source(&name(&["dbo", "t"]), &sources).is_none());
    assert!(target_source(&name(&["master", "dbo", "t"]), &sources).is_none());
    assert!(target_source(&name(&["y"]), &sources).is_none());
    assert!(target_source(&name(&["x"]), &[]).is_none());
}

/// Each of the eight compound operators stands for one arithmetic operator; `=` alone
/// stands for no operator.
#[test]
fn each_compound_operator_has_its_arithmetic() {
    assert!(compound_operator(AssignOp::Set).is_none());
    let pairs = [
        (AssignOp::AddAssign, BinaryOp::Add),
        (AssignOp::SubAssign, BinaryOp::Sub),
        (AssignOp::MulAssign, BinaryOp::Mul),
        (AssignOp::DivAssign, BinaryOp::Div),
        (AssignOp::ModAssign, BinaryOp::Mod),
        (AssignOp::BitAndAssign, BinaryOp::BitAnd),
        (AssignOp::BitOrAssign, BinaryOp::BitOr),
        (AssignOp::BitXorAssign, BinaryOp::BitXor),
    ];
    for (op, expected) in pairs {
        assert_eq!(compound_operator(op), Some(expected), "{op:?}");
    }
}

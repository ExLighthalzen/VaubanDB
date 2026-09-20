use super::{ColumnBinding, LogicalPlan, OutputColumn, OutputSchema};
use vauban_catalog::{ColumnId, TableId};
use vauban_types::{SqlType, TypeInfo};

/// `LogicalPlan::schema` reads the `schema` field of a `Scan` and consults nothing else:
/// it takes no catalogue, so what a `Scan` publishes is what was written into the node
/// when the statement was bound.
#[test]
fn scan_schema_is_the_declared_schema() {
    let declared = OutputSchema {
        columns: vec![
            OutputColumn {
                name: "id".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
            },
            OutputColumn {
                name: "label".to_owned(),
                ty: TypeInfo::new(SqlType::Int, true),
            },
        ],
    };
    let scan = LogicalPlan::Scan {
        table: TableId(7),
        columns: vec![
            ColumnBinding {
                column: ColumnId(1),
                index: 0,
                name: "id".to_owned(),
                ty: TypeInfo::new(SqlType::Int, false),
                depth: 0,
            },
            ColumnBinding {
                column: ColumnId(4),
                index: 1,
                name: "label".to_owned(),
                ty: TypeInfo::new(SqlType::Int, true),
                depth: 0,
            },
        ],
        alias: "t".to_owned(),
        schema: declared.clone(),
        hints: super::LockHints::default(),
    };

    let schema = scan.schema();
    assert_eq!(schema.columns.len(), declared.columns.len());
    assert_eq!(schema.columns[0].name, "id");
    assert_eq!(schema.columns[0].ty, TypeInfo::new(SqlType::Int, false));
    assert_eq!(schema.columns[1].name, "label");
    assert_eq!(schema.columns[1].ty, TypeInfo::new(SqlType::Int, true));

    // A node that does not change the shape of its input delegates to it, so the same
    // schema comes back through a `Filter` above the `Scan`.
    let limited = LogicalPlan::Limit {
        input: Box::new(scan),
        top: super::BoundTop {
            expr: super::BoundExpr {
                kind: super::BoundExprKind::Literal(vauban_types::Value::I32(1)),
                ty: TypeInfo::new(SqlType::Int, false),
                line: 1,
            },
            percent: false,
            with_ties: false,
        },
    };
    assert_eq!(limited.schema().columns[1].name, "label");
}

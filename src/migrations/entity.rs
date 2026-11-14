//! The table a SeaORM entity describes, in migration terms.

use std::collections::BTreeMap;

use sea_orm::sea_query::{
    ColumnType, Expr, ForeignKeyAction, Keyword, StringLen, TableRef, Value as SqlValue,
};
use sea_orm::{DbBackend, EntityTrait, Schema};

use super::{Action, Column, ForeignKey, Index, Table, Type, Value};

/// A model's table, and the columns it marks `renamed_from` an older name.
#[derive(Clone, Debug)]
pub(crate) struct EntityTable {
    pub(crate) table: Table,
    /// New column name to old.
    pub(crate) renamed_from: BTreeMap<String, String>,
}

/// The table SeaORM would create for `E`.
pub(crate) fn table_of<E: EntityTrait + Default>() -> Result<EntityTable, String> {
    // SQLite keeps native enums as `ColumnType::Enum`, so they can be recognised; the other
    // backends rewrite them into database-specific types.
    let schema = Schema::new(DbBackend::Sqlite);
    let create = schema.create_table_from_entity(E::default());
    let name = create
        .get_table_name()
        .and_then(table_name)
        .ok_or_else(|| "a model has no table name".to_owned())?;
    let mut table = Table::new(&name);
    let mut renamed_from = BTreeMap::new();

    for def in create.get_columns() {
        let column_name = def.get_column_name();
        let spec = def.get_column_spec();
        let kind = def
            .get_column_type()
            .ok_or_else(|| format!("`{name}.{column_name}` has no type"))
            .and_then(|t| column_type(t).map_err(|e| format!("`{name}.{column_name}`: {e}")))?;
        let default = match &spec.default {
            Some(expr) => Some(
                default_value(expr)
                    .ok_or_else(|| format!("`{name}.{column_name}` has a default that migrations cannot write down; use `default_expr = \"...\"` with plain SQL"))?,
            ),
            None => None,
        };
        if let Some(old) = spec.comment.as_deref().and_then(renamed) {
            renamed_from.insert(column_name.clone(), old);
        }
        table.columns.push(Column {
            name: column_name,
            kind,
            null: spec.nullable != Some(false),
            unique: spec.unique,
            primary_key: spec.primary_key,
            auto_increment: spec.auto_increment,
            default,
        });
    }

    for index in create.get_indexes() {
        if index.is_primary_key() {
            table.primary_key = index.get_index_spec().get_column_names();
        }
    }

    for statement in create.get_foreign_key_create_stmts() {
        let key = statement.get_foreign_key();
        let columns = key.get_columns();
        let to_table = key
            .get_ref_table()
            .and_then(table_name)
            .ok_or_else(|| format!("a foreign key of `{name}` has no target table"))?;
        table.foreign_keys.push(ForeignKey {
            name: key
                .get_name()
                .map_or_else(|| format!("fk-{name}-{}", columns.join("-")), str::to_owned),
            columns,
            to_table,
            to_columns: key.get_ref_columns(),
            on_delete: key.get_on_delete().map(action),
            on_update: key.get_on_update().map(action),
        });
    }

    for statement in schema.create_index_from_entity(E::default()) {
        let spec = statement.get_index_spec();
        let columns = spec.get_column_names();
        table.indexes.push(Index {
            name: spec.get_name().map_or_else(
                || format!("idx-{name}-{}", columns.join("-")),
                str::to_owned,
            ),
            columns,
            unique: statement.is_unique_key(),
        });
    }

    Ok(EntityTable {
        table,
        renamed_from,
    })
}

fn table_name(table: &TableRef) -> Option<String> {
    match table {
        TableRef::Table(name, _) => Some(name.1.to_string()),
        _ => None,
    }
}

/// SeaORM writes `renamed_from = "old"` into the column comment.
fn renamed(comment: &str) -> Option<String> {
    let rest = comment.split("renamed_from \"").nth(1)?;
    Some(rest.split('"').next()?.to_owned())
}

fn action(action: ForeignKeyAction) -> Action {
    match action {
        ForeignKeyAction::Restrict => Action::Restrict,
        ForeignKeyAction::Cascade => Action::Cascade,
        ForeignKeyAction::SetNull => Action::SetNull,
        ForeignKeyAction::NoAction => Action::NoAction,
        ForeignKeyAction::SetDefault => Action::SetDefault,
        _ => Action::NoAction,
    }
}

fn length(len: StringLen) -> Option<u32> {
    match len {
        StringLen::N(n) => Some(n),
        StringLen::Max | StringLen::None => None,
    }
}

fn column_type(kind: &ColumnType) -> Result<Type, String> {
    Ok(match kind {
        ColumnType::Char(n) => Type::Char(*n),
        ColumnType::String(n) => Type::String(length(*n)),
        ColumnType::Text => Type::Text,
        ColumnType::Blob => Type::Blob,
        ColumnType::TinyInteger => Type::TinyInteger,
        ColumnType::SmallInteger => Type::SmallInteger,
        ColumnType::Integer => Type::Integer,
        ColumnType::BigInteger => Type::BigInteger,
        ColumnType::TinyUnsigned => Type::TinyUnsigned,
        ColumnType::SmallUnsigned => Type::SmallUnsigned,
        ColumnType::Unsigned => Type::Unsigned,
        ColumnType::BigUnsigned => Type::BigUnsigned,
        ColumnType::Float => Type::Float,
        ColumnType::Double => Type::Double,
        ColumnType::Decimal(p) => Type::Decimal(*p),
        ColumnType::DateTime => Type::DateTime,
        ColumnType::Timestamp => Type::Timestamp,
        ColumnType::TimestampWithTimeZone => Type::TimestampTz,
        ColumnType::Time => Type::Time,
        ColumnType::Date => Type::Date,
        ColumnType::Year => Type::Year,
        ColumnType::Binary(n) => Type::Binary(*n),
        ColumnType::VarBinary(n) => Type::VarBinary(length(*n)),
        ColumnType::Boolean => Type::Boolean,
        ColumnType::Json => Type::Json,
        ColumnType::JsonBinary => Type::JsonBinary,
        ColumnType::Uuid => Type::Uuid,
        ColumnType::Custom(name) => Type::Custom(name.to_string()),
        ColumnType::Enum { .. } => {
            return Err(
                "database enums are not supported by migrations yet; store the enum \
                        as a string (`db_type = \"String(StringLen::None)\"`)"
                    .into(),
            );
        }
        ColumnType::Array(_) => return Err("array columns are not supported yet".into()),
        ColumnType::Cidr => Type::Custom("cidr".into()),
        ColumnType::Inet => Type::Custom("inet".into()),
        ColumnType::MacAddr => Type::Custom("macaddr".into()),
        ColumnType::LTree => Type::Custom("ltree".into()),
        ColumnType::Money(_) => Type::Custom("money".into()),
        ColumnType::Interval(..) => Type::Custom("interval".into()),
        ColumnType::Bit(n) => Type::Custom(format!("bit({})", n.unwrap_or(1))),
        ColumnType::VarBit(n) => Type::Custom(format!("varbit({n})")),
        ColumnType::Vector(n) => Type::Custom(match n {
            Some(n) => format!("vector({n})"),
            None => "vector".into(),
        }),
        _ => return Err("this column type is not supported by migrations yet".into()),
    })
}

fn default_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Value(value) | Expr::Constant(value) => literal(value),
        Expr::Keyword(Keyword::CurrentTimestamp) => Some(Value::Now),
        Expr::Keyword(Keyword::CurrentDate) => Some(Value::sql("CURRENT_DATE")),
        Expr::Keyword(Keyword::CurrentTime) => Some(Value::sql("CURRENT_TIME")),
        Expr::Custom(sql) => Some(Value::sql(sql.to_string())),
        _ => None,
    }
}

fn literal(value: &SqlValue) -> Option<Value> {
    Some(match value {
        SqlValue::Bool(Some(b)) => Value::Bool(*b),
        SqlValue::TinyInt(Some(n)) => Value::Int((*n).into()),
        SqlValue::SmallInt(Some(n)) => Value::Int((*n).into()),
        SqlValue::Int(Some(n)) => Value::Int((*n).into()),
        SqlValue::BigInt(Some(n)) => Value::Int(*n),
        SqlValue::TinyUnsigned(Some(n)) => Value::Int((*n).into()),
        SqlValue::SmallUnsigned(Some(n)) => Value::Int((*n).into()),
        SqlValue::Unsigned(Some(n)) => Value::Int((*n).into()),
        SqlValue::BigUnsigned(Some(n)) => Value::Int(i64::try_from(*n).ok()?),
        SqlValue::Float(Some(n)) => Value::Float((*n).into()),
        SqlValue::Double(Some(n)) => Value::Float(*n),
        SqlValue::String(Some(s)) => Value::Text(s.clone()),
        SqlValue::Char(Some(c)) => Value::Text(c.to_string()),
        _ => return None,
    })
}

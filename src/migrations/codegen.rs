//! Writes migrations as Rust source, the way `makemigrations` saves them.

use std::fmt::Write as _;

use super::{Action, Column, ForeignKey, Index, Operation, Table, Type, Value};

/// A migration file: `pub fn migration() -> Migration` building these operations.
pub(crate) fn render_migration(name: &str, operations: &[Operation]) -> String {
    let mut out = format!(
        "//! Migration {name}, written by `cargo run -- makemigrations`.\n\
         //!\n\
         //! It can be edited until it has been applied somewhere, for instance to change data\n\
         //! with `.sql_reversible(...)` or `.run(...)`.\n\
         \n\
         use cadmus::migrations::*;\n\
         \n\
         pub fn migration() -> Migration {{\n    Migration::new({})",
        string(name)
    );
    for operation in operations {
        out.push_str("\n        ");
        out.push_str(&call(operation));
    }
    out.push_str("\n}\n");
    out
}

/// `src/migrations/mod.rs`: the modules, and `all()` listing them in order.
pub(crate) fn render_list(modules: &[String]) -> String {
    let mut out = String::from(
        "//! The project's database migrations, oldest first.\n\
         //!\n\
         //! `cargo run -- makemigrations` adds a file here and rewrites this list;\n\
         //! `cargo run -- migrate` applies the migrations the database has not had yet.\n\
         \n\
         use cadmus::migrations::Migration;\n",
    );
    if !modules.is_empty() {
        out.push('\n');
    }
    for module in modules {
        let _ = writeln!(out, "mod {module};");
    }
    out.push_str(
        "\n/// Every migration, in the order they apply.\npub fn all() -> Vec<Migration> {\n",
    );
    if modules.is_empty() {
        out.push_str("    Vec::new()\n");
    } else {
        out.push_str("    vec![\n");
        for module in modules {
            let _ = writeln!(out, "        {module}::migration(),");
        }
        out.push_str("    ]\n");
    }
    out.push_str("}\n");
    out
}

fn call(operation: &Operation) -> String {
    match operation {
        Operation::CreateTable(t) => format!(".create_table({})", table(t, 12)),
        Operation::DropTable(t) => format!(".drop_table({})", table(t, 12)),
        Operation::RenameTable { from, to } => {
            format!(".rename_table({}, {})", string(from), string(to))
        }
        Operation::AddColumn {
            table,
            column: c,
            fill: None,
        } => format!(".add_column({}, {})", string(table), column(c)),
        Operation::AddColumn {
            table,
            column: c,
            fill: Some(fill),
        } => format!(
            ".add_column_filled({}, {}, {})",
            string(table),
            column(c),
            value(fill)
        ),
        Operation::DropColumn { table, column: c } => {
            format!(".drop_column({}, {})", string(table), column(c))
        }
        Operation::RenameColumn { table, from, to } => format!(
            ".rename_column({}, {}, {})",
            string(table),
            string(from),
            string(to)
        ),
        Operation::AlterColumn { table, from, to } => format!(
            ".alter_column(\n            {},\n            {},\n            {},\n        )",
            string(table),
            column(from),
            column(to)
        ),
        Operation::AddIndex { table, index: i } => {
            format!(".add_index({}, {})", string(table), index(i))
        }
        Operation::DropIndex { table, index: i } => {
            format!(".drop_index({}, {})", string(table), index(i))
        }
        Operation::AddForeignKey { table, key } => {
            format!(".add_foreign_key({}, {})", string(table), foreign_key(key))
        }
        Operation::DropForeignKey { table, key } => {
            format!(".drop_foreign_key({}, {})", string(table), foreign_key(key))
        }
        Operation::Sql {
            forward,
            backward: None,
        } => format!(".sql({})", string(forward)),
        Operation::Sql {
            forward,
            backward: Some(backward),
        } => format!(".sql_reversible({}, {})", string(forward), string(backward)),
        // Never produced by `makemigrations`: code is written by hand.
        Operation::Run { .. } => "/* Rust code */".to_owned(),
    }
}

fn table(t: &Table, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let inner = " ".repeat(indent + 4);
    let mut out = format!("\n{pad}Table::new({})", string(&t.name));
    for c in &t.columns {
        let _ = write!(out, "\n{inner}.column({})", column(c));
    }
    if !t.primary_key.is_empty() {
        let _ = write!(out, "\n{inner}.primary_key({})", strings(&t.primary_key));
    }
    for i in &t.indexes {
        let _ = write!(out, "\n{inner}.index({})", index(i));
    }
    for k in &t.foreign_keys {
        let _ = write!(out, "\n{inner}.foreign_key({})", foreign_key(k));
    }
    let _ = write!(out, ",\n{}", " ".repeat(indent - 4));
    out
}

fn column(c: &Column) -> String {
    let mut out = format!("Column::new({}, {})", string(&c.name), kind(&c.kind));
    if c.null {
        out.push_str(".null()");
    }
    if c.primary_key {
        out.push_str(".primary_key()");
    }
    if c.auto_increment {
        out.push_str(".auto_increment()");
    }
    if c.unique {
        out.push_str(".unique()");
    }
    if let Some(default) = &c.default {
        let _ = write!(out, ".default({})", value(default));
    }
    out
}

fn index(i: &Index) -> String {
    let mut out = format!("Index::new({}, {})", string(&i.name), strings(&i.columns));
    if i.unique {
        out.push_str(".unique()");
    }
    out
}

fn foreign_key(k: &ForeignKey) -> String {
    let mut out = format!(
        "ForeignKey::new({}, {}, {}, {})",
        string(&k.name),
        strings(&k.columns),
        string(&k.to_table),
        strings(&k.to_columns)
    );
    if let Some(a) = k.on_delete {
        let _ = write!(out, ".on_delete({})", action(a));
    }
    if let Some(a) = k.on_update {
        let _ = write!(out, ".on_update({})", action(a));
    }
    out
}

fn action(a: Action) -> &'static str {
    match a {
        Action::Restrict => "Action::Restrict",
        Action::Cascade => "Action::Cascade",
        Action::SetNull => "Action::SetNull",
        Action::NoAction => "Action::NoAction",
        Action::SetDefault => "Action::SetDefault",
    }
}

fn option<T: std::fmt::Debug>(value: Option<T>) -> String {
    match value {
        Some(v) => format!("Some({v:?})"),
        None => "None".to_owned(),
    }
}

fn kind(t: &Type) -> String {
    match t {
        Type::TinyInteger => "Type::TinyInteger".into(),
        Type::SmallInteger => "Type::SmallInteger".into(),
        Type::Integer => "Type::Integer".into(),
        Type::BigInteger => "Type::BigInteger".into(),
        Type::TinyUnsigned => "Type::TinyUnsigned".into(),
        Type::SmallUnsigned => "Type::SmallUnsigned".into(),
        Type::Unsigned => "Type::Unsigned".into(),
        Type::BigUnsigned => "Type::BigUnsigned".into(),
        Type::Float => "Type::Float".into(),
        Type::Double => "Type::Double".into(),
        Type::Decimal(p) => format!("Type::Decimal({})", option(*p)),
        Type::Boolean => "Type::Boolean".into(),
        Type::Char(n) => format!("Type::Char({})", option(*n)),
        Type::String(n) => format!("Type::String({})", option(*n)),
        Type::Text => "Type::Text".into(),
        Type::Date => "Type::Date".into(),
        Type::Time => "Type::Time".into(),
        Type::DateTime => "Type::DateTime".into(),
        Type::Timestamp => "Type::Timestamp".into(),
        Type::TimestampTz => "Type::TimestampTz".into(),
        Type::Year => "Type::Year".into(),
        Type::Json => "Type::Json".into(),
        Type::JsonBinary => "Type::JsonBinary".into(),
        Type::Uuid => "Type::Uuid".into(),
        Type::Binary(n) => format!("Type::Binary({n})"),
        Type::VarBinary(n) => format!("Type::VarBinary({})", option(*n)),
        Type::Blob => "Type::Blob".into(),
        Type::Custom(name) => format!("Type::custom({})", string(name)),
    }
}

pub(crate) fn value(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        // A bare literal is an `i32`, so larger numbers need the suffix.
        Value::Int(n) if i32::try_from(*n).is_ok() => n.to_string(),
        Value::Int(n) => format!("{n}_i64"),
        // `{:?}` keeps the decimal point, so the literal stays a float.
        Value::Float(n) => format!("{n:?}"),
        Value::Text(s) => string(s),
        Value::Now => "Value::Now".into(),
        Value::Sql(s) => format!("Value::sql({})", string(s)),
    }
}

/// A Rust string literal.
fn string(s: &str) -> String {
    format!("{s:?}")
}

fn strings(list: &[String]) -> String {
    let items: Vec<String> = list.iter().map(|s| string(s)).collect();
    format!("[{}]", items.join(", "))
}

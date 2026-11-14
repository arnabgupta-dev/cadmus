//! The SQL each operation runs on a given database.

use std::fmt::Write as _;

use sea_orm::DbBackend;
use sea_orm::sea_query::{
    Alias, ColumnDef, ColumnType, Expr, ForeignKey as SqlForeignKey, ForeignKeyAction,
    ForeignKeyCreateStatement, Index as SqlIndex, MysqlQueryBuilder, PostgresQueryBuilder,
    SchemaStatementBuilder, SqliteQueryBuilder, StringLen, Table as SqlTable,
};

use super::{Action, Column, ForeignKey, Index, Operation, RunFn, State, Table, Type, Value};

/// One thing a migration does on the database.
#[derive(Clone)]
pub(crate) enum Step {
    Sql(String),
    Run(RunFn),
}

/// The steps for `operation`, given the tables before and after it.
pub(crate) fn plan(
    operation: &Operation,
    before: &State,
    after: &State,
    backend: DbBackend,
) -> Result<Vec<Step>, String> {
    let table_before = |name: &str| {
        before
            .tables
            .get(name)
            .ok_or_else(|| format!("there is no table `{name}`"))
    };
    let table_after = |name: &str| {
        after
            .tables
            .get(name)
            .ok_or_else(|| format!("there is no table `{name}`"))
    };
    let sqlite = backend == DbBackend::Sqlite;
    let rebuild_of = |name: &str, fill: Option<(&str, &Value)>| -> Result<Vec<String>, String> {
        Ok(rebuild(table_before(name)?, table_after(name)?, fill))
    };
    let sql: Vec<String> = match operation {
        Operation::CreateTable(table) => create_table(backend, table),
        Operation::DropTable(table) => vec![render(
            backend,
            &SqlTable::drop().table(Alias::new(&table.name)).take(),
        )],
        Operation::RenameTable { from, to } => vec![render(
            backend,
            &SqlTable::rename()
                .table(Alias::new(from), Alias::new(to))
                .take(),
        )],
        Operation::AddColumn {
            table,
            column,
            fill,
        } => {
            if sqlite {
                let simple = fill.is_none()
                    && !column.unique
                    && !column.primary_key
                    && !column.auto_increment
                    && match &column.default {
                        None => column.null,
                        Some(Value::Now | Value::Sql(_)) => false,
                        Some(_) => true,
                    };
                if simple {
                    vec![add_column(backend, table, column)]
                } else {
                    rebuild_of(table, fill.as_ref().map(|f| (column.name.as_str(), f)))?
                }
            } else {
                let mut added = column.clone();
                if let Some(fill) = fill {
                    added.default = Some(fill.clone());
                }
                let mut sql = vec![add_column(backend, table, &added)];
                if added.default != column.default {
                    sql.push(set_default(backend, table, column));
                }
                sql
            }
        }
        Operation::DropColumn { table, column } => {
            if sqlite {
                rebuild_of(table, None)?
            } else {
                vec![render(
                    backend,
                    &SqlTable::alter()
                        .table(Alias::new(table))
                        .drop_column(Alias::new(&column.name))
                        .take(),
                )]
            }
        }
        Operation::RenameColumn { table, from, to } => vec![render(
            backend,
            &SqlTable::alter()
                .table(Alias::new(table))
                .rename_column(Alias::new(from), Alias::new(to))
                .take(),
        )],
        Operation::AlterColumn { table, from, to } => {
            if sqlite {
                rebuild_of(table, None)?
            } else {
                alter_column(backend, table, from, to)?
            }
        }
        Operation::AddIndex { table, index } => vec![create_index(backend, table, index)],
        Operation::DropIndex { table, index } => {
            let mut statement = SqlIndex::drop();
            statement.name(&index.name).table(Alias::new(table));
            vec![render(backend, &statement)]
        }
        Operation::AddForeignKey { table, key } => {
            if sqlite {
                rebuild_of(table, None)?
            } else {
                vec![render(backend, &foreign_key(table, key))]
            }
        }
        Operation::DropForeignKey { table, key } => {
            if sqlite {
                rebuild_of(table, None)?
            } else {
                let mut statement = SqlForeignKey::drop();
                statement.name(&key.name).table(Alias::new(table));
                vec![render(backend, &statement)]
            }
        }
        Operation::Sql { forward, .. } => vec![forward.clone()],
        Operation::Run { forward, .. } => return Ok(vec![Step::Run(forward.clone())]),
    };
    Ok(sql.into_iter().map(Step::Sql).collect())
}

fn render<S: SchemaStatementBuilder>(backend: DbBackend, statement: &S) -> String {
    match backend {
        DbBackend::Postgres => statement.to_string(PostgresQueryBuilder),
        DbBackend::MySql => statement.to_string(MysqlQueryBuilder),
        _ => statement.to_string(SqliteQueryBuilder),
    }
}

/// A quoted identifier.
pub(crate) fn quote(backend: DbBackend, name: &str) -> String {
    if backend == DbBackend::MySql {
        format!("`{}`", name.replace('`', "``"))
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A default or fill value as SQL.
pub(crate) fn value_sql(backend: DbBackend, value: &Value) -> String {
    match value {
        Value::Bool(true) => "TRUE".into(),
        Value::Bool(false) => "FALSE".into(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => format!("{n:?}"),
        Value::Text(s) => {
            let mut escaped = s.replace('\'', "''");
            if backend == DbBackend::MySql {
                escaped = escaped.replace('\\', "\\\\");
            }
            format!("'{escaped}'")
        }
        Value::Now => "CURRENT_TIMESTAMP".into(),
        // Expression defaults need parentheses on SQLite and MySQL.
        Value::Sql(s) => format!("({s})"),
    }
}

fn length(n: Option<u32>) -> StringLen {
    n.map_or(StringLen::None, StringLen::N)
}

fn column_type(kind: &Type) -> ColumnType {
    match kind {
        Type::TinyInteger => ColumnType::TinyInteger,
        Type::SmallInteger => ColumnType::SmallInteger,
        Type::Integer => ColumnType::Integer,
        Type::BigInteger => ColumnType::BigInteger,
        Type::TinyUnsigned => ColumnType::TinyUnsigned,
        Type::SmallUnsigned => ColumnType::SmallUnsigned,
        Type::Unsigned => ColumnType::Unsigned,
        Type::BigUnsigned => ColumnType::BigUnsigned,
        Type::Float => ColumnType::Float,
        Type::Double => ColumnType::Double,
        Type::Decimal(p) => ColumnType::Decimal(*p),
        Type::Boolean => ColumnType::Boolean,
        Type::Char(n) => ColumnType::Char(*n),
        Type::String(n) => ColumnType::String(length(*n)),
        Type::Text => ColumnType::Text,
        Type::Date => ColumnType::Date,
        Type::Time => ColumnType::Time,
        Type::DateTime => ColumnType::DateTime,
        Type::Timestamp => ColumnType::Timestamp,
        Type::TimestampTz => ColumnType::TimestampWithTimeZone,
        Type::Year => ColumnType::Year,
        Type::Json => ColumnType::Json,
        Type::JsonBinary => ColumnType::JsonBinary,
        Type::Uuid => ColumnType::Uuid,
        Type::Binary(n) => ColumnType::Binary(*n),
        Type::VarBinary(n) => ColumnType::VarBinary(length(*n)),
        Type::Blob => ColumnType::Blob,
        Type::Custom(name) => ColumnType::custom(name.clone()),
    }
}

/// The column as SQL: `unique` and `primary key` only where asked for.
fn column_def(backend: DbBackend, column: &Column, unique: bool, primary_key: bool) -> ColumnDef {
    let mut kind = column_type(&column.kind);
    // SQLite numbers rows only in an `integer primary key`.
    if backend == DbBackend::Sqlite && column.auto_increment && column.primary_key {
        kind = ColumnType::Integer;
    }
    let mut def = ColumnDef::new_with_type(Alias::new(&column.name), kind);
    if column.null {
        def.null();
    } else {
        def.not_null();
    }
    if column.primary_key && primary_key {
        def.primary_key();
    }
    if column.auto_increment {
        def.auto_increment();
    }
    if column.unique && unique {
        def.unique_key();
    }
    if let Some(value) = &column.default {
        def.default(default_expr(value));
    }
    def
}

/// A default for `CREATE TABLE` and `ADD COLUMN`: literals as values, and SQL as written
/// (sea-query puts it in the parentheses that SQLite and MySQL need).
fn default_expr(value: &Value) -> Expr {
    match value {
        Value::Bool(b) => Expr::val(*b),
        Value::Int(n) => Expr::val(*n),
        Value::Float(n) => Expr::val(*n),
        Value::Text(s) => Expr::val(s.clone()),
        Value::Now => Expr::current_timestamp(),
        Value::Sql(s) => Expr::cust(s.clone()),
    }
}

fn action(action: Action) -> ForeignKeyAction {
    match action {
        Action::Restrict => ForeignKeyAction::Restrict,
        Action::Cascade => ForeignKeyAction::Cascade,
        Action::SetNull => ForeignKeyAction::SetNull,
        Action::NoAction => ForeignKeyAction::NoAction,
        Action::SetDefault => ForeignKeyAction::SetDefault,
    }
}

fn foreign_key(table: &str, key: &ForeignKey) -> ForeignKeyCreateStatement {
    let mut statement = SqlForeignKey::create();
    statement
        .name(&key.name)
        .from_tbl(Alias::new(table))
        .to_tbl(Alias::new(&key.to_table));
    for column in &key.columns {
        statement.from_col(Alias::new(column));
    }
    for column in &key.to_columns {
        statement.to_col(Alias::new(column));
    }
    if let Some(a) = key.on_delete {
        statement.on_delete(action(a));
    }
    if let Some(a) = key.on_update {
        statement.on_update(action(a));
    }
    statement
}

fn create_index(backend: DbBackend, table: &str, index: &Index) -> String {
    let mut statement = SqlIndex::create();
    statement.name(&index.name).table(Alias::new(table));
    for column in &index.columns {
        statement.col(Alias::new(column));
    }
    if index.unique {
        statement.unique();
    }
    render(backend, &statement)
}

/// `CREATE TABLE` for the table, without its indexes.
fn create_table_only(backend: DbBackend, table: &Table, name: &str) -> String {
    let mut statement = SqlTable::create();
    statement.table(Alias::new(name));
    for column in &table.columns {
        statement.col(column_def(backend, column, true, true));
    }
    if !table.primary_key.is_empty() {
        let mut key = SqlIndex::create();
        key.name(format!("pk-{}", table.name));
        for column in &table.primary_key {
            key.col(Alias::new(column));
        }
        statement.primary_key(&mut key);
    }
    for key in &table.foreign_keys {
        statement.foreign_key(&mut foreign_key(&table.name, key));
    }
    render(backend, &statement)
}

fn create_table(backend: DbBackend, table: &Table) -> Vec<String> {
    let mut sql = vec![create_table_only(backend, table, &table.name)];
    for index in &table.indexes {
        sql.push(create_index(backend, &table.name, index));
    }
    sql
}

fn add_column(backend: DbBackend, table: &str, column: &Column) -> String {
    render(
        backend,
        &SqlTable::alter()
            .table(Alias::new(table))
            .add_column(column_def(backend, column, true, true))
            .take(),
    )
}

/// Sets the column's default to what `column` says, or drops it.
fn set_default(backend: DbBackend, table: &str, column: &Column) -> String {
    let change = match &column.default {
        Some(value) => format!("SET DEFAULT {}", value_sql(backend, value)),
        None => "DROP DEFAULT".into(),
    };
    format!(
        "ALTER TABLE {} ALTER COLUMN {} {change}",
        quote(backend, table),
        quote(backend, &column.name)
    )
}

/// The name PostgreSQL gives a one-column unique constraint.
fn unique_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_key")
}

fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn add_unique(backend: DbBackend, table: &str, column: &str) -> String {
    if backend == DbBackend::Postgres {
        format!(
            "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({})",
            quote(backend, table),
            quote(backend, &unique_name(table, column)),
            quote(backend, column)
        )
    } else {
        format!(
            "ALTER TABLE {} ADD UNIQUE INDEX {} ({})",
            quote(backend, table),
            quote(backend, column),
            quote(backend, column)
        )
    }
}

fn drop_unique(backend: DbBackend, table: &str, column: &str) -> String {
    if backend == DbBackend::Postgres {
        // The constraint keeps the name it was created with, which a renamed table or column
        // no longer matches, so find it by its column.
        format!(
            "DO $$ DECLARE found text; BEGIN \
             SELECT con.conname INTO found FROM pg_constraint con \
             JOIN pg_class rel ON rel.oid = con.conrelid \
             JOIN pg_attribute att ON att.attrelid = rel.oid AND att.attnum = con.conkey[1] \
             WHERE con.contype = 'u' AND array_length(con.conkey, 1) = 1 \
             AND rel.relname = {} AND att.attname = {} AND pg_table_is_visible(rel.oid) LIMIT 1; \
             IF found IS NOT NULL THEN \
             EXECUTE format('ALTER TABLE %I DROP CONSTRAINT %I', {}, found); \
             END IF; END $$",
            sql_string(table),
            sql_string(column),
            sql_string(table)
        )
    } else {
        format!(
            "ALTER TABLE {} DROP INDEX {}",
            quote(backend, table),
            quote(backend, column)
        )
    }
}

fn alter_column(
    backend: DbBackend,
    table: &str,
    from: &Column,
    to: &Column,
) -> Result<Vec<String>, String> {
    if from.primary_key != to.primary_key {
        return Err(format!(
            "`{table}.{}` changes whether it is the primary key, which migrations cannot do on \
             this database; write that step with `.sql(...)`",
            to.name
        ));
    }
    let t = quote(backend, table);
    let c = quote(backend, &to.name);
    let mut sql = Vec::new();
    if backend == DbBackend::Postgres {
        if from.auto_increment != to.auto_increment {
            return Err(format!(
                "`{table}.{}` changes whether it numbers rows itself, which migrations cannot \
                 do on PostgreSQL; write that step with `.sql(...)`",
                to.name
            ));
        }
        if from.kind != to.kind {
            let alter = render(
                backend,
                &SqlTable::alter()
                    .table(Alias::new(table))
                    .modify_column(ColumnDef::new_with_type(
                        Alias::new(&to.name),
                        column_type(&to.kind),
                    ))
                    .take(),
            );
            let kind = alter.split(" TYPE ").nth(1).unwrap_or_default().to_owned();
            // A default of the old type may not convert, so it is set again afterwards.
            if from.default.is_some() {
                sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT"));
            }
            sql.push(format!("{alter} USING {c}::{kind}"));
        }
        if from.null != to.null {
            let change = if to.null {
                "DROP NOT NULL"
            } else {
                "SET NOT NULL"
            };
            sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} {change}"));
        }
        if from.default != to.default || (from.kind != to.kind && to.default.is_some()) {
            sql.push(set_default(backend, table, to));
        }
    } else if from.kind != to.kind
        || from.null != to.null
        || from.default != to.default
        || from.auto_increment != to.auto_increment
    {
        sql.push(render(
            backend,
            &SqlTable::alter()
                .table(Alias::new(table))
                .modify_column(column_def(backend, to, false, false))
                .take(),
        ));
    }
    if from.unique != to.unique {
        sql.push(if to.unique {
            add_unique(backend, table, &to.name)
        } else {
            drop_unique(backend, table, &to.name)
        });
    }
    Ok(sql)
}

/// SQLite cannot change or drop most things in place, so the table is created anew under a
/// temporary name, the rows copied across, the old table dropped and the new one renamed.
fn rebuild(before: &Table, after: &Table, fill: Option<(&str, &Value)>) -> Vec<String> {
    let backend = DbBackend::Sqlite;
    let temporary = format!("__hera_new_{}", after.name);
    let mut sql = vec![create_table_only(backend, after, &temporary)];
    let mut targets = Vec::new();
    let mut sources = Vec::new();
    for column in &after.columns {
        let source = if before.find(&column.name).is_some() {
            Some(quote(backend, &column.name))
        } else {
            fill.filter(|(name, _)| *name == column.name)
                .map(|(_, value)| value_sql(backend, value))
        };
        if let Some(source) = source {
            targets.push(quote(backend, &column.name));
            sources.push(source);
        }
    }
    let mut copy = format!("INSERT INTO {} (", quote(backend, &temporary));
    let _ = write!(
        copy,
        "{}) SELECT {} FROM {}",
        targets.join(", "),
        sources.join(", "),
        quote(backend, &before.name)
    );
    sql.push(copy);
    sql.push(format!("DROP TABLE {}", quote(backend, &before.name)));
    sql.push(format!(
        "ALTER TABLE {} RENAME TO {}",
        quote(backend, &temporary),
        quote(backend, &after.name)
    ));
    for index in &after.indexes {
        sql.push(create_index(backend, &after.name, index));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::Migration;

    fn sql(migration: &Migration, base: &[Migration], backend: DbBackend) -> Vec<String> {
        let state = State::after(base).unwrap();
        crate::migrations::runner::steps(migration.operations(), state, backend)
            .unwrap()
            .into_iter()
            .map(|step| match step {
                Step::Sql(sql) => sql,
                Step::Run(_) => "code".into(),
            })
            .collect()
    }

    fn base() -> Vec<Migration> {
        vec![
            Migration::new("0001").create_table(
                Table::new("post")
                    .column(
                        Column::new("id", Type::Integer)
                            .primary_key()
                            .auto_increment(),
                    )
                    .column(Column::new("title", Type::String(None)))
                    .column(Column::new("views", Type::Integer).null())
                    .index(Index::new("idx-post-title", ["title"])),
            ),
        ]
    }

    #[test]
    fn changing_a_column_on_postgres_converts_it_and_sets_each_property() {
        let change = Migration::new("0002").alter_column(
            "post",
            Column::new("views", Type::Integer).null(),
            Column::new("views", Type::BigInteger).default(0).unique(),
        );
        assert_eq!(
            sql(&change, &base(), DbBackend::Postgres),
            [
                r#"ALTER TABLE "post" ALTER COLUMN "views" TYPE bigint USING "views"::bigint"#,
                r#"ALTER TABLE "post" ALTER COLUMN "views" SET NOT NULL"#,
                r#"ALTER TABLE "post" ALTER COLUMN "views" SET DEFAULT 0"#,
                r#"ALTER TABLE "post" ADD CONSTRAINT "post_views_key" UNIQUE ("views")"#,
            ]
        );
        // Undoing it finds the unique constraint by its column, whatever it is called.
        let back = Migration::new("0002").alter_column(
            "post",
            Column::new("views", Type::BigInteger).default(0).unique(),
            Column::new("views", Type::Integer).null(),
        );
        let mut later = base();
        later.push(change);
        let undo = sql(&back, &later, DbBackend::Postgres);
        assert!(undo.last().unwrap().starts_with("DO $$"), "{undo:?}");
        assert!(undo.last().unwrap().contains("att.attname = 'views'"));
    }

    #[test]
    fn changing_a_column_on_mysql_restates_it() {
        let change = Migration::new("0002").alter_column(
            "post",
            Column::new("views", Type::Integer).null(),
            Column::new("views", Type::BigInteger).default(0).unique(),
        );
        assert_eq!(
            sql(&change, &base(), DbBackend::MySql),
            [
                "ALTER TABLE `post` MODIFY COLUMN `views` bigint NOT NULL DEFAULT 0",
                "ALTER TABLE `post` ADD UNIQUE INDEX `views` (`views`)",
            ]
        );
    }

    #[test]
    fn sqlite_rebuilds_the_table_for_changes_it_cannot_make_in_place() {
        let change = Migration::new("0002").add_column_filled(
            "post",
            Column::new("slug", Type::String(None)).unique(),
            "it's",
        );
        assert_eq!(
            sql(&change, &base(), DbBackend::Sqlite),
            [
                r#"CREATE TABLE "__hera_new_post" ( "id" integer NOT NULL PRIMARY KEY AUTOINCREMENT, "title" varchar NOT NULL, "views" integer NULL, "slug" varchar NOT NULL UNIQUE )"#,
                r#"INSERT INTO "__hera_new_post" ("id", "title", "views", "slug") SELECT "id", "title", "views", 'it''s' FROM "post""#,
                r#"DROP TABLE "post""#,
                r#"ALTER TABLE "__hera_new_post" RENAME TO "post""#,
                r#"CREATE INDEX "idx-post-title" ON "post" ("title")"#,
            ]
        );
        // A plain new column is added in place.
        let simple =
            Migration::new("0002").add_column("post", Column::new("note", Type::Text).null());
        assert_eq!(
            sql(&simple, &base(), DbBackend::Sqlite),
            [r#"ALTER TABLE "post" ADD COLUMN "note" text NULL"#]
        );
    }

    #[test]
    fn filling_existing_rows_leaves_no_default_behind() {
        let change =
            Migration::new("0002").add_column_filled("post", Column::new("slug", Type::Text), "x");
        assert_eq!(
            sql(&change, &base(), DbBackend::Postgres),
            [
                r#"ALTER TABLE "post" ADD COLUMN "slug" text NOT NULL DEFAULT 'x'"#,
                r#"ALTER TABLE "post" ALTER COLUMN "slug" DROP DEFAULT"#,
            ]
        );
    }

    #[test]
    fn mysql_text_escapes_backslashes() {
        assert_eq!(
            value_sql(DbBackend::MySql, &Value::Text(r"a\'b".into())),
            r"'a\\''b'"
        );
        assert_eq!(
            value_sql(DbBackend::Postgres, &Value::Text(r"a\'b".into())),
            r"'a\''b'"
        );
    }
}

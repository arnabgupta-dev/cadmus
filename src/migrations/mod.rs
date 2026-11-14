//! Versioned migrations, in the style of Django.
//!
//! `cargo run -- makemigrations` compares the models with the tables the existing migrations
//! build up, and writes the difference as a new Rust file in `src/migrations/`. `cargo run --
//! migrate` applies the migrations a database has not had yet, in order, and records each one
//! in the `cadmus_migration` table. Because migration files describe changes rather than SQL,
//! the same files work on SQLite, PostgreSQL and MySQL.
//!
//! A generated migration looks like this, and can be edited before it is applied anywhere:
//!
//! ```
//! use cadmus::migrations::*;
//!
//! pub fn migration() -> Migration {
//!     Migration::new("0002_post_slug")
//!         .add_column_filled("post", Column::new("slug", Type::String(Some(200))), "")
//!         .rename_column("post", "body", "content")
//!         .sql_reversible(
//!             "UPDATE post SET slug = lower(title)",
//!             "UPDATE post SET slug = ''",
//!         )
//! }
//! ```
//!
//! The built-in tables (accounts, and uploads when they are configured) have migrations of
//! their own, which `migrate` applies before the project's.

mod codegen;
mod diff;
mod entity;
pub(crate) mod runner;
mod sql;

pub(crate) mod builtin;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sea_orm::{DatabaseTransaction, DbErr};

pub(crate) use codegen::{render_list, render_migration};
pub(crate) use diff::{Answers, Detect, Questions, describe, diff};
pub(crate) use entity::{EntityTable, table_of};
pub use runner::MigrationEvent;

/// A migration file written by [`App::make_migration`](crate::App::make_migration).
#[derive(Clone, Debug)]
pub struct NewMigration {
    /// Its name, such as `0002_post_slug`.
    pub name: String,
    /// The file written.
    pub path: std::path::PathBuf,
    /// One line per change, for people.
    pub changes: Vec<String>,
}

/// Rust code run by a migration, given the migration's transaction.
pub type RunFn =
    Arc<dyn for<'a> Fn(&'a DatabaseTransaction) -> BoxFuture<'a, Result<(), DbErr>> + Send + Sync>;

/// One migration: a name such as `0002_post_slug` and the changes it makes, in order.
#[derive(Clone)]
pub struct Migration {
    name: String,
    operations: Vec<Operation>,
}

impl Migration {
    /// An empty migration. Its name orders it among the others and is recorded in the database
    /// once it is applied, so do not rename a migration that has been applied somewhere.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            operations: Vec::new(),
        }
    }

    /// The migration's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    fn push(mut self, operation: Operation) -> Self {
        self.operations.push(operation);
        self
    }

    /// Creates a table, with its indexes and foreign keys.
    #[must_use]
    pub fn create_table(self, table: Table) -> Self {
        self.push(Operation::CreateTable(table))
    }

    /// Drops a table. The whole definition is kept so the migration can be reversed (the
    /// rows cannot come back).
    #[must_use]
    pub fn drop_table(self, table: Table) -> Self {
        self.push(Operation::DropTable(table))
    }

    /// Renames a table.
    #[must_use]
    pub fn rename_table(self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.push(Operation::RenameTable {
            from: from.into(),
            to: to.into(),
        })
    }

    /// Adds a column. A required column with no default needs
    /// [`add_column_filled`](Self::add_column_filled) if the table may have rows.
    #[must_use]
    pub fn add_column(self, table: impl Into<String>, column: Column) -> Self {
        self.push(Operation::AddColumn {
            table: table.into(),
            column,
            fill: None,
        })
    }

    /// Adds a column, giving the rows already in the table `fill` as its value.
    #[must_use]
    pub fn add_column_filled(
        self,
        table: impl Into<String>,
        column: Column,
        fill: impl Into<Value>,
    ) -> Self {
        self.push(Operation::AddColumn {
            table: table.into(),
            column,
            fill: Some(fill.into()),
        })
    }

    /// Drops a column. Its definition is kept so the migration can be reversed.
    #[must_use]
    pub fn drop_column(self, table: impl Into<String>, column: Column) -> Self {
        self.push(Operation::DropColumn {
            table: table.into(),
            column,
        })
    }

    /// Renames a column, keeping its data.
    #[must_use]
    pub fn rename_column(
        self,
        table: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        self.push(Operation::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
        })
    }

    /// Changes a column's type, nullability, default or uniqueness, from `from` to `to`
    /// (both are needed so the change can be reversed). Both must have the same name.
    #[must_use]
    pub fn alter_column(self, table: impl Into<String>, from: Column, to: Column) -> Self {
        self.push(Operation::AlterColumn {
            table: table.into(),
            from,
            to,
        })
    }

    /// Adds an index.
    #[must_use]
    pub fn add_index(self, table: impl Into<String>, index: Index) -> Self {
        self.push(Operation::AddIndex {
            table: table.into(),
            index,
        })
    }

    /// Drops an index.
    #[must_use]
    pub fn drop_index(self, table: impl Into<String>, index: Index) -> Self {
        self.push(Operation::DropIndex {
            table: table.into(),
            index,
        })
    }

    /// Adds a foreign key.
    #[must_use]
    pub fn add_foreign_key(self, table: impl Into<String>, key: ForeignKey) -> Self {
        self.push(Operation::AddForeignKey {
            table: table.into(),
            key,
        })
    }

    /// Drops a foreign key.
    #[must_use]
    pub fn drop_foreign_key(self, table: impl Into<String>, key: ForeignKey) -> Self {
        self.push(Operation::DropForeignKey {
            table: table.into(),
            key,
        })
    }

    /// Runs SQL as written. A migration with SQL that has no reverse cannot be rolled back.
    /// The SQL is the same on every database, so keep to what they all accept, or use
    /// [`run`](Self::run) and check the backend.
    #[must_use]
    pub fn sql(self, forward: impl Into<String>) -> Self {
        self.push(Operation::Sql {
            forward: forward.into(),
            backward: None,
        })
    }

    /// Runs `forward` when the migration is applied and `backward` when it is rolled back.
    #[must_use]
    pub fn sql_reversible(self, forward: impl Into<String>, backward: impl Into<String>) -> Self {
        self.push(Operation::Sql {
            forward: forward.into(),
            backward: Some(backward.into()),
        })
    }

    /// Runs Rust code inside the migration's transaction, for changes to the data:
    ///
    /// ```
    /// # use cadmus::migrations::*;
    /// use sea_orm::ConnectionTrait;
    ///
    /// Migration::new("0003_titles").run(|db| {
    ///     Box::pin(async move {
    ///         db.execute_unprepared("UPDATE post SET title = trim(title)").await?;
    ///         Ok(())
    ///     })
    /// });
    /// ```
    ///
    /// A migration with code that has no reverse cannot be rolled back.
    #[must_use]
    pub fn run<F>(self, forward: F) -> Self
    where
        F: for<'a> Fn(&'a DatabaseTransaction) -> BoxFuture<'a, Result<(), DbErr>>
            + Send
            + Sync
            + 'static,
    {
        self.push(Operation::Run {
            forward: Arc::new(forward),
            backward: None,
        })
    }

    /// Runs `forward` when the migration is applied and `backward` when it is rolled back.
    #[must_use]
    pub fn run_reversible<F, B>(self, forward: F, backward: B) -> Self
    where
        F: for<'a> Fn(&'a DatabaseTransaction) -> BoxFuture<'a, Result<(), DbErr>>
            + Send
            + Sync
            + 'static,
        B: for<'a> Fn(&'a DatabaseTransaction) -> BoxFuture<'a, Result<(), DbErr>>
            + Send
            + Sync
            + 'static,
    {
        self.push(Operation::Run {
            forward: Arc::new(forward),
            backward: Some(Arc::new(backward)),
        })
    }

    pub(crate) fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// The migration's number: the digits its name starts with.
    pub(crate) fn number(&self) -> Option<u32> {
        let digits: String = self.name.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }
}

impl fmt::Debug for Migration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Migration")
            .field("name", &self.name)
            .field("operations", &self.operations)
            .finish()
    }
}

impl PartialEq for Migration {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.operations == other.operations
    }
}

/// One change in a migration.
#[derive(Clone)]
pub(crate) enum Operation {
    CreateTable(Table),
    DropTable(Table),
    RenameTable {
        from: String,
        to: String,
    },
    AddColumn {
        table: String,
        column: Column,
        fill: Option<Value>,
    },
    DropColumn {
        table: String,
        column: Column,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    AlterColumn {
        table: String,
        from: Column,
        to: Column,
    },
    AddIndex {
        table: String,
        index: Index,
    },
    DropIndex {
        table: String,
        index: Index,
    },
    AddForeignKey {
        table: String,
        key: ForeignKey,
    },
    DropForeignKey {
        table: String,
        key: ForeignKey,
    },
    Sql {
        forward: String,
        backward: Option<String>,
    },
    Run {
        forward: RunFn,
        backward: Option<RunFn>,
    },
}

impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateTable(t) => f.debug_tuple("CreateTable").field(t).finish(),
            Self::DropTable(t) => f.debug_tuple("DropTable").field(t).finish(),
            Self::RenameTable { from, to } => write!(f, "RenameTable({from} -> {to})"),
            Self::AddColumn {
                table,
                column,
                fill,
            } => write!(f, "AddColumn({table}, {column:?}, fill {fill:?})"),
            Self::DropColumn { table, column } => write!(f, "DropColumn({table}, {column:?})"),
            Self::RenameColumn { table, from, to } => {
                write!(f, "RenameColumn({table}: {from} -> {to})")
            }
            Self::AlterColumn { table, from, to } => {
                write!(f, "AlterColumn({table}: {from:?} -> {to:?})")
            }
            Self::AddIndex { table, index } => write!(f, "AddIndex({table}, {index:?})"),
            Self::DropIndex { table, index } => write!(f, "DropIndex({table}, {index:?})"),
            Self::AddForeignKey { table, key } => write!(f, "AddForeignKey({table}, {key:?})"),
            Self::DropForeignKey { table, key } => write!(f, "DropForeignKey({table}, {key:?})"),
            Self::Sql { forward, backward } => write!(f, "Sql({forward:?}, {backward:?})"),
            Self::Run { backward, .. } => write!(f, "Run(reversible: {})", backward.is_some()),
        }
    }
}

impl PartialEq for Operation {
    fn eq(&self, other: &Self) -> bool {
        use Operation as O;
        let same_fn = |a: &RunFn, b: &RunFn| Arc::ptr_eq(a, b);
        match (self, other) {
            (O::CreateTable(a), O::CreateTable(b)) | (O::DropTable(a), O::DropTable(b)) => a == b,
            (O::RenameTable { from: a, to: b }, O::RenameTable { from: c, to: d }) => {
                a == c && b == d
            }
            (
                O::AddColumn {
                    table: a,
                    column: b,
                    fill: c,
                },
                O::AddColumn {
                    table: d,
                    column: e,
                    fill: g,
                },
            ) => a == d && b == e && c == g,
            (
                O::DropColumn {
                    table: a,
                    column: b,
                },
                O::DropColumn {
                    table: c,
                    column: d,
                },
            ) => a == c && b == d,
            (
                O::RenameColumn {
                    table: a,
                    from: b,
                    to: c,
                },
                O::RenameColumn {
                    table: d,
                    from: e,
                    to: g,
                },
            ) => a == d && b == e && c == g,
            (
                O::AlterColumn {
                    table: a,
                    from: b,
                    to: c,
                },
                O::AlterColumn {
                    table: d,
                    from: e,
                    to: g,
                },
            ) => a == d && b == e && c == g,
            (O::AddIndex { table: a, index: b }, O::AddIndex { table: c, index: d })
            | (O::DropIndex { table: a, index: b }, O::DropIndex { table: c, index: d }) => {
                a == c && b == d
            }
            (O::AddForeignKey { table: a, key: b }, O::AddForeignKey { table: c, key: d })
            | (O::DropForeignKey { table: a, key: b }, O::DropForeignKey { table: c, key: d }) => {
                a == c && b == d
            }
            (
                O::Sql {
                    forward: a,
                    backward: b,
                },
                O::Sql {
                    forward: c,
                    backward: d,
                },
            ) => a == c && b == d,
            (
                O::Run {
                    forward: a,
                    backward: b,
                },
                O::Run {
                    forward: c,
                    backward: d,
                },
            ) => {
                same_fn(a, c)
                    && match (b, d) {
                        (Some(b), Some(d)) => same_fn(b, d),
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

impl Operation {
    /// The operation that undoes this one, if there is one.
    pub(crate) fn reverse(&self) -> Option<Self> {
        Some(match self.clone() {
            Self::CreateTable(table) => Self::DropTable(table),
            Self::DropTable(table) => Self::CreateTable(table),
            Self::RenameTable { from, to } => Self::RenameTable { from: to, to: from },
            Self::AddColumn { table, column, .. } => Self::DropColumn { table, column },
            // Rows get an empty value of the column's type, as Django does.
            Self::DropColumn { table, column } => Self::AddColumn {
                fill: if column.needs_fill() {
                    column.kind.empty_value()
                } else {
                    None
                },
                table,
                column,
            },
            Self::RenameColumn { table, from, to } => Self::RenameColumn {
                table,
                from: to,
                to: from,
            },
            Self::AlterColumn { table, from, to } => Self::AlterColumn {
                table,
                from: to,
                to: from,
            },
            Self::AddIndex { table, index } => Self::DropIndex { table, index },
            Self::DropIndex { table, index } => Self::AddIndex { table, index },
            Self::AddForeignKey { table, key } => Self::DropForeignKey { table, key },
            Self::DropForeignKey { table, key } => Self::AddForeignKey { table, key },
            Self::Sql { forward, backward } => Self::Sql {
                forward: backward?,
                backward: Some(forward),
            },
            Self::Run { forward, backward } => Self::Run {
                forward: backward?,
                backward: Some(forward),
            },
        })
    }
}

/// A table: columns in order, then indexes and foreign keys.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    /// Set only for a primary key over several columns; a single-column key is marked on the
    /// column.
    pub(crate) primary_key: Vec<String>,
    pub(crate) indexes: Vec<Index>,
    pub(crate) foreign_keys: Vec<ForeignKey>,
}

impl Table {
    /// A table with no columns yet.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// Adds a column.
    #[must_use]
    pub fn column(mut self, column: Column) -> Self {
        self.columns.push(column);
        self
    }

    /// Makes several columns together the primary key.
    #[must_use]
    pub fn primary_key<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.primary_key = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Adds an index.
    #[must_use]
    pub fn index(mut self, index: Index) -> Self {
        self.indexes.push(index);
        self
    }

    /// Adds a foreign key.
    #[must_use]
    pub fn foreign_key(mut self, key: ForeignKey) -> Self {
        self.foreign_keys.push(key);
        self
    }

    pub(crate) fn find(&self, column: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == column)
    }
}

/// A column. Columns are required (`NOT NULL`) unless [`null`](Self::null) is called.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub(crate) name: String,
    pub(crate) kind: Type,
    pub(crate) null: bool,
    pub(crate) unique: bool,
    pub(crate) primary_key: bool,
    pub(crate) auto_increment: bool,
    pub(crate) default: Option<Value>,
}

impl Column {
    /// A required column of this type.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: Type) -> Self {
        Self {
            name: name.into(),
            kind,
            null: false,
            unique: false,
            primary_key: false,
            auto_increment: false,
            default: None,
        }
    }

    /// Allows `NULL`.
    #[must_use]
    pub fn null(mut self) -> Self {
        self.null = true;
        self
    }

    /// No two rows may have the same value.
    #[must_use]
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Makes this column the primary key.
    #[must_use]
    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self
    }

    /// Numbers new rows automatically.
    #[must_use]
    pub fn auto_increment(mut self) -> Self {
        self.auto_increment = true;
        self
    }

    /// The value new rows get when none is given.
    #[must_use]
    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Whether two columns are defined alike, whatever their names.
    pub(crate) fn same_definition(&self, other: &Self) -> bool {
        Self {
            name: String::new(),
            ..self.clone()
        } == Self {
            name: String::new(),
            ..other.clone()
        }
    }

    /// Whether rows already in the table need a value for this column when it is added.
    pub(crate) fn needs_fill(&self) -> bool {
        !self.null && self.default.is_none() && !self.auto_increment
    }
}

/// A column type, independent of the database. Each database stores it in its nearest type,
/// as SeaORM does.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Type {
    /// 8-bit integer.
    TinyInteger,
    /// 16-bit integer.
    SmallInteger,
    /// 32-bit integer.
    Integer,
    /// 64-bit integer.
    BigInteger,
    /// Unsigned 8-bit integer.
    TinyUnsigned,
    /// Unsigned 16-bit integer.
    SmallUnsigned,
    /// Unsigned 32-bit integer.
    Unsigned,
    /// Unsigned 64-bit integer.
    BigUnsigned,
    /// 32-bit floating point.
    Float,
    /// 64-bit floating point.
    Double,
    /// Exact decimal, with optional (precision, scale).
    Decimal(Option<(u32, u32)>),
    /// True or false.
    Boolean,
    /// Fixed-length text.
    Char(Option<u32>),
    /// Text with an optional maximum length (`varchar`).
    String(Option<u32>),
    /// Text of any length.
    Text,
    /// A date.
    Date,
    /// A time of day.
    Time,
    /// A date and time without a time zone.
    DateTime,
    /// A timestamp.
    Timestamp,
    /// A timestamp with a time zone.
    TimestampTz,
    /// A year.
    Year,
    /// JSON.
    Json,
    /// JSON stored in binary form (`jsonb` on PostgreSQL).
    JsonBinary,
    /// A UUID.
    Uuid,
    /// Fixed-length bytes.
    Binary(u32),
    /// Bytes with an optional maximum length.
    VarBinary(Option<u32>),
    /// Bytes of any length.
    Blob,
    /// A database type written out by name, used as is on every database.
    Custom(String),
}

impl Type {
    /// A database type written out by name, such as `citext`.
    #[must_use]
    pub fn custom(name: impl Into<String>) -> Self {
        Self::Custom(name.into())
    }

    /// The value given to existing rows when a dropped required column comes back.
    fn empty_value(&self) -> Option<Value> {
        Some(match self {
            Self::TinyInteger
            | Self::SmallInteger
            | Self::Integer
            | Self::BigInteger
            | Self::TinyUnsigned
            | Self::SmallUnsigned
            | Self::Unsigned
            | Self::BigUnsigned
            | Self::Year => Value::Int(0),
            Self::Float | Self::Double | Self::Decimal(_) => Value::Float(0.0),
            Self::Boolean => Value::Bool(false),
            Self::Char(_) | Self::String(_) | Self::Text => Value::Text(String::new()),
            Self::Date | Self::Time | Self::DateTime | Self::Timestamp | Self::TimestampTz => {
                Value::Now
            }
            Self::Json | Self::JsonBinary => Value::Text("{}".into()),
            Self::Uuid => Value::Text("00000000-0000-0000-0000-000000000000".into()),
            Self::Binary(_) | Self::VarBinary(_) | Self::Blob | Self::Custom(_) => return None,
        })
    }
}

/// A default value, or the value given to existing rows when a column is added.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// True or false.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number with a fraction.
    Float(f64),
    /// Text.
    Text(String),
    /// The current date and time (`CURRENT_TIMESTAMP`).
    Now,
    /// An SQL expression, used as written.
    Sql(String),
}

impl Value {
    /// An SQL expression, used as written.
    #[must_use]
    pub fn sql(expression: impl Into<String>) -> Self {
        Self::Sql(expression.into())
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Int(value.into())
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

/// An index on one or more columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub(crate) name: String,
    pub(crate) columns: Vec<String>,
    pub(crate) unique: bool,
}

impl Index {
    /// An index on these columns.
    #[must_use]
    pub fn new<I, S>(name: impl Into<String>, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            name: name.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            unique: false,
        }
    }

    /// No two rows may have the same values in these columns.
    #[must_use]
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}

/// A foreign key: columns of this table that refer to columns of another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub(crate) name: String,
    pub(crate) columns: Vec<String>,
    pub(crate) to_table: String,
    pub(crate) to_columns: Vec<String>,
    pub(crate) on_delete: Option<Action>,
    pub(crate) on_update: Option<Action>,
}

impl ForeignKey {
    /// `columns` refer to `to_columns` of `to_table`.
    #[must_use]
    pub fn new<C, S, R, T>(name: impl Into<String>, columns: C, to_table: R, to_columns: T) -> Self
    where
        C: IntoIterator<Item = S>,
        T: IntoIterator<Item = S>,
        S: Into<String>,
        R: Into<String>,
    {
        Self {
            name: name.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            to_table: to_table.into(),
            to_columns: to_columns.into_iter().map(Into::into).collect(),
            on_delete: None,
            on_update: None,
        }
    }

    /// What happens to these rows when the row they refer to is deleted.
    #[must_use]
    pub fn on_delete(mut self, action: Action) -> Self {
        self.on_delete = Some(action);
        self
    }

    /// What happens to these rows when the key they refer to changes.
    #[must_use]
    pub fn on_update(mut self, action: Action) -> Self {
        self.on_update = Some(action);
        self
    }

    /// Whether two keys do the same thing, whatever their names.
    pub(crate) fn same_definition(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.to_table == other.to_table
            && self.to_columns == other.to_columns
            && self.on_delete == other.on_delete
            && self.on_update == other.on_update
    }
}

/// What a foreign key does when the row it refers to is deleted or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Refuse the change.
    Restrict,
    /// Delete or update these rows too.
    Cascade,
    /// Set these columns to `NULL`.
    SetNull,
    /// Like `Restrict`, checked at the end of the statement.
    NoAction,
    /// Set these columns to their default.
    SetDefault,
}

/// The tables that a list of migrations builds, worked out without a database.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct State {
    pub(crate) tables: BTreeMap<String, Table>,
}

impl State {
    /// The state after `migrations`, in order.
    pub(crate) fn after(migrations: &[Migration]) -> Result<Self, String> {
        let mut state = Self::default();
        for migration in migrations {
            for operation in migration.operations() {
                state
                    .apply(operation)
                    .map_err(|e| format!("migration {}: {e}", migration.name()))?;
            }
        }
        Ok(state)
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut Table, String> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| format!("there is no table `{name}` at this point"))
    }

    /// Applies one operation; fails if it does not fit, such as dropping a missing column.
    pub(crate) fn apply(&mut self, operation: &Operation) -> Result<(), String> {
        match operation {
            Operation::CreateTable(table) => {
                if self.tables.contains_key(&table.name) {
                    return Err(format!("table `{}` already exists", table.name));
                }
                self.tables.insert(table.name.clone(), table.clone());
            }
            Operation::DropTable(table) => {
                if self.tables.remove(&table.name).is_none() {
                    return Err(format!("there is no table `{}` to drop", table.name));
                }
            }
            Operation::RenameTable { from, to } => {
                if self.tables.contains_key(to) {
                    return Err(format!("table `{to}` already exists"));
                }
                let mut table = self
                    .tables
                    .remove(from)
                    .ok_or_else(|| format!("there is no table `{from}` to rename"))?;
                table.name.clone_from(to);
                self.tables.insert(to.clone(), table);
                // Keys that referred to the old name now refer to the new one, as the
                // databases do.
                for other in self.tables.values_mut() {
                    for key in &mut other.foreign_keys {
                        if &key.to_table == from {
                            key.to_table.clone_from(to);
                        }
                    }
                }
            }
            Operation::AddColumn { table, column, .. } => {
                let t = self.table_mut(table)?;
                if t.find(&column.name).is_some() {
                    return Err(format!("column `{table}.{}` already exists", column.name));
                }
                t.columns.push(column.clone());
            }
            Operation::DropColumn { table, column } => {
                let t = self.table_mut(table)?;
                let before = t.columns.len();
                t.columns.retain(|c| c.name != column.name);
                if t.columns.len() == before {
                    return Err(format!(
                        "there is no column `{table}.{}` to drop",
                        column.name
                    ));
                }
                t.indexes.retain(|i| !i.columns.contains(&column.name));
                t.foreign_keys.retain(|k| !k.columns.contains(&column.name));
            }
            Operation::RenameColumn { table, from, to } => {
                let t = self.table_mut(table)?;
                if t.find(to).is_some() {
                    return Err(format!("column `{table}.{to}` already exists"));
                }
                let column = t
                    .columns
                    .iter_mut()
                    .find(|c| &c.name == from)
                    .ok_or_else(|| format!("there is no column `{table}.{from}` to rename"))?;
                column.name.clone_from(to);
                let rename = |names: &mut Vec<String>| {
                    for name in names {
                        if name == from {
                            name.clone_from(to);
                        }
                    }
                };
                rename(&mut t.primary_key);
                for index in &mut t.indexes {
                    rename(&mut index.columns);
                }
                for key in &mut t.foreign_keys {
                    rename(&mut key.columns);
                }
                for other in self.tables.values_mut() {
                    for key in &mut other.foreign_keys {
                        if &key.to_table == table {
                            rename(&mut key.to_columns);
                        }
                    }
                }
            }
            Operation::AlterColumn { table, from, to } => {
                if from.name != to.name {
                    return Err(format!(
                        "alter_column on `{table}` changes the name from `{}` to `{}`; use \
                         rename_column for that",
                        from.name, to.name
                    ));
                }
                let t = self.table_mut(table)?;
                let column = t
                    .columns
                    .iter_mut()
                    .find(|c| c.name == to.name)
                    .ok_or_else(|| format!("there is no column `{table}.{}` to alter", to.name))?;
                *column = to.clone();
            }
            Operation::AddIndex { table, index } => {
                let t = self.table_mut(table)?;
                if t.indexes.iter().any(|i| i.name == index.name) {
                    return Err(format!("index `{}` already exists", index.name));
                }
                t.indexes.push(index.clone());
            }
            Operation::DropIndex { table, index } => {
                let t = self.table_mut(table)?;
                let before = t.indexes.len();
                t.indexes.retain(|i| i.name != index.name);
                if t.indexes.len() == before {
                    return Err(format!("there is no index `{}` to drop", index.name));
                }
            }
            Operation::AddForeignKey { table, key } => {
                let t = self.table_mut(table)?;
                if t.foreign_keys.iter().any(|k| k.name == key.name) {
                    return Err(format!("foreign key `{}` already exists", key.name));
                }
                t.foreign_keys.push(key.clone());
            }
            Operation::DropForeignKey { table, key } => {
                let t = self.table_mut(table)?;
                let before = t.foreign_keys.len();
                t.foreign_keys.retain(|k| k.name != key.name);
                if t.foreign_keys.len() == before {
                    return Err(format!("there is no foreign key `{}` to drop", key.name));
                }
            }
            Operation::Sql { .. } | Operation::Run { .. } => {}
        }
        Ok(())
    }
}

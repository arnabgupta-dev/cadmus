//! Applies and rolls back migrations, and records which ones a database has had.

use std::collections::BTreeSet;
use std::fmt;

use futures_util::future::BoxFuture;
use sea_orm::sea_query::{Alias, ColumnDef, Expr, ExprTrait, Order, Query, Table as SqlTable};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, Statement,
    TransactionTrait,
};

use super::sql::{Step, plan};
use super::{Migration, Operation, State};
use crate::error::AdminError;

/// Where applied migrations are recorded.
pub(crate) const HISTORY: &str = "cadmus_migration";

/// Brings a database from before migrations up to date with SeaORM's schema sync.
pub(crate) type LegacySync<'a> =
    Box<dyn Fn() -> BoxFuture<'a, Result<(), DbErr>> + Send + Sync + 'a>;

/// One app's migrations: the built-in accounts, uploads, or the project.
pub(crate) struct AppMigrations<'a> {
    pub(crate) label: &'static str,
    pub(crate) migrations: &'a [Migration],
    /// For a built-in app: a table that shows the database predates migrations, and the sync
    /// that brings such a database up to date.
    pub(crate) legacy: Option<(&'static str, LegacySync<'a>)>,
}

/// What `migrate` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationEvent {
    /// A migration was applied.
    Applied {
        /// `auth`, `uploads` or `app`.
        app: String,
        /// The migration's name.
        name: String,
    },
    /// A migration was recorded as applied without running it, because its tables were
    /// already there (or `--fake` was given).
    Faked {
        /// `auth`, `uploads` or `app`.
        app: String,
        /// The migration's name.
        name: String,
    },
    /// A migration was rolled back.
    Unapplied {
        /// `auth`, `uploads` or `app`.
        app: String,
        /// The migration's name.
        name: String,
    },
    /// Tables that predate migrations were brought up to date, and the app's migrations
    /// recorded as applied.
    Upgraded {
        /// `auth` or `uploads`.
        app: String,
    },
    /// The project has no migrations, so its tables were created or extended to match the
    /// models.
    Synced,
    /// The models have changes that no migration makes yet; one line per change.
    Unmigrated(Vec<String>),
}

impl fmt::Display for MigrationEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Applied { app, name } => write!(f, "applied {app}.{name}"),
            Self::Faked { app, name } => write!(
                f,
                "marked {app}.{name} as applied (its tables were already there)"
            ),
            Self::Unapplied { app, name } => write!(f, "rolled back {app}.{name}"),
            Self::Upgraded { app } => write!(
                f,
                "brought the existing {app} tables up to date and marked its migrations applied"
            ),
            Self::Synced => write!(
                f,
                "created missing tables and columns for the models (the project has no \
                 migrations; run makemigrations to start keeping them)"
            ),
            Self::Unmigrated(changes) => {
                writeln!(
                    f,
                    "the models have changes that no migration makes yet; run makemigrations:"
                )?;
                for change in changes {
                    writeln!(f, "  {change}")?;
                }
                Ok(())
            }
        }
    }
}

fn error(message: impl Into<String>) -> AdminError {
    AdminError::Migration(message.into())
}

/// Creates the history table if it is missing.
pub(crate) async fn ensure_history(db: &DatabaseConnection) -> Result<(), AdminError> {
    let statement = SqlTable::create()
        .table(Alias::new(HISTORY))
        .if_not_exists()
        .col(
            ColumnDef::new(Alias::new("id"))
                .integer()
                .not_null()
                .auto_increment()
                .primary_key(),
        )
        .col(ColumnDef::new(Alias::new("app")).string_len(100).not_null())
        .col(
            ColumnDef::new(Alias::new("name"))
                .string_len(200)
                .not_null(),
        )
        .col(
            ColumnDef::new(Alias::new("applied"))
                .timestamp_with_time_zone()
                .not_null(),
        )
        .take();
    db.execute(&statement).await?;
    Ok(())
}

/// The names of `app`'s applied migrations.
pub(crate) async fn applied(
    db: &DatabaseConnection,
    app: &str,
) -> Result<BTreeSet<String>, AdminError> {
    let query = Query::select()
        .column(Alias::new("name"))
        .from(Alias::new(HISTORY))
        .and_where(Expr::col(Alias::new("app")).eq(app))
        .order_by(Alias::new("id"), Order::Asc)
        .take();
    let rows = db.query_all(&query).await?;
    rows.iter()
        .map(|row| row.try_get::<String>("", "name").map_err(AdminError::from))
        .collect()
}

async fn record<C: ConnectionTrait>(db: &C, app: &str, name: &str) -> Result<(), DbErr> {
    let insert = Query::insert()
        .into_table(Alias::new(HISTORY))
        .columns([Alias::new("app"), Alias::new("name"), Alias::new("applied")])
        .values_panic([app.into(), name.into(), chrono::Utc::now().into()])
        .take();
    db.execute(&insert).await.map(|_| ())
}

async fn forget<C: ConnectionTrait>(db: &C, app: &str, name: &str) -> Result<(), DbErr> {
    let delete = Query::delete()
        .from_table(Alias::new(HISTORY))
        .and_where(Expr::col(Alias::new("app")).eq(app))
        .and_where(Expr::col(Alias::new("name")).eq(name))
        .take();
    db.execute(&delete).await.map(|_| ())
}

/// Whether `table` exists with all of `columns`.
pub(crate) async fn has_table(db: &DatabaseConnection, table: &str, columns: &[String]) -> bool {
    let mut query = Query::select();
    query.from(Alias::new(table)).and_where(Expr::cust("1 = 0"));
    if columns.is_empty() {
        query.expr(Expr::cust("1"));
    }
    for column in columns {
        query.column(Alias::new(column));
    }
    db.query_all(&query).await.is_ok()
}

/// Applies `app`'s migrations up to (not including) index `upto`.
pub(crate) async fn forward(
    db: &DatabaseConnection,
    app: &AppMigrations<'_>,
    upto: usize,
    fake: bool,
    events: &mut Vec<MigrationEvent>,
) -> Result<(), AdminError> {
    let done = applied(db, app.label).await?;
    let label = app.label.to_owned();
    if let Some(unknown) = done
        .iter()
        .find(|name| !app.migrations.iter().any(|m| m.name() == name.as_str()))
    {
        return Err(error(format!(
            "the database has had migration {label}.{unknown}, which this code does not have; \
             it was made from another version of the code"
        )));
    }

    if done.is_empty() && !fake && upto > 0 {
        if let Some((probe, sync)) = &app.legacy {
            if has_table(db, probe, &[]).await {
                sync().await?;
                for migration in app.migrations {
                    record(db, app.label, migration.name()).await?;
                }
                events.push(MigrationEvent::Upgraded { app: label });
                return Ok(());
            }
        } else if let Some(first) = app.migrations.first()
            && already_there(db, first).await?
        {
            record(db, app.label, first.name()).await?;
            events.push(MigrationEvent::Faked {
                app: label.clone(),
                name: first.name().to_owned(),
            });
        }
    }

    let done = applied(db, app.label).await?;
    let backend = db.get_database_backend();
    for (i, migration) in app.migrations.iter().enumerate().take(upto) {
        if done.contains(migration.name()) {
            continue;
        }
        if fake {
            record(db, app.label, migration.name()).await?;
            events.push(MigrationEvent::Faked {
                app: label.clone(),
                name: migration.name().to_owned(),
            });
            continue;
        }
        let state = State::after(&app.migrations[..i]).map_err(error)?;
        let steps = steps(migration.operations(), state, backend)
            .map_err(|e| error(format!("{label}.{}: {e}", migration.name())))?;
        execute(db, steps, Finish::Record(app.label, migration.name()))
            .await
            .map_err(|e| error(format!("{label}.{} failed: {e}", migration.name())))?;
        events.push(MigrationEvent::Applied {
            app: label.clone(),
            name: migration.name().to_owned(),
        });
    }
    Ok(())
}

/// Rolls back `app`'s applied migrations from the last down to index `keep`, so that only
/// the first `keep` stay applied.
pub(crate) async fn backward(
    db: &DatabaseConnection,
    app: &AppMigrations<'_>,
    keep: usize,
    fake: bool,
    events: &mut Vec<MigrationEvent>,
) -> Result<(), AdminError> {
    let done = applied(db, app.label).await?;
    let backend = db.get_database_backend();
    let label = app.label.to_owned();
    for i in (keep..app.migrations.len()).rev() {
        let migration = &app.migrations[i];
        if !done.contains(migration.name()) {
            continue;
        }
        if !fake {
            let state = State::after(&app.migrations[..=i]).map_err(error)?;
            let reversed = reversed(migration).map_err(|e| {
                error(format!(
                    "{label}.{} cannot be rolled back: {e}",
                    migration.name()
                ))
            })?;
            let steps = steps(&reversed, state, backend)
                .map_err(|e| error(format!("{label}.{}: {e}", migration.name())))?;
            execute(db, steps, Finish::Forget(app.label, migration.name()))
                .await
                .map_err(|e| {
                    error(format!(
                        "rolling back {label}.{} failed: {e}",
                        migration.name()
                    ))
                })?;
        } else {
            forget(db, app.label, migration.name()).await?;
        }
        events.push(MigrationEvent::Unapplied {
            app: label.clone(),
            name: migration.name().to_owned(),
        });
    }
    Ok(())
}

/// The operations that undo `migration`, last first.
pub(crate) fn reversed(migration: &Migration) -> Result<Vec<Operation>, String> {
    migration
        .operations()
        .iter()
        .rev()
        .map(|op| {
            op.reverse()
                .ok_or_else(|| "it runs SQL or code that has no reverse".to_owned())
        })
        .collect()
}

/// The steps for `operations`, starting from `state`.
pub(crate) fn steps(
    operations: &[Operation],
    mut state: State,
    backend: DbBackend,
) -> Result<Vec<Step>, String> {
    let mut all = Vec::new();
    for operation in operations {
        let before = state.clone();
        state.apply(operation)?;
        all.extend(plan(operation, &before, &state, backend)?);
    }
    Ok(all)
}

/// Whether the first migration's tables are all in the database already, as they are when a
/// project that used schema sync starts keeping migrations. Some but not all is an error.
async fn already_there(db: &DatabaseConnection, first: &Migration) -> Result<bool, AdminError> {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for operation in first.operations() {
        match operation {
            Operation::CreateTable(table) => {
                let columns: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
                if has_table(db, &table.name, &columns).await {
                    present.push(table.name.clone());
                } else {
                    missing.push(table.name.clone());
                }
            }
            Operation::AddIndex { .. } | Operation::AddForeignKey { .. } => {}
            // Anything else is not an initial migration.
            _ => return Ok(false),
        }
    }
    if present.is_empty() {
        return Ok(false);
    }
    if missing.is_empty() {
        return Ok(true);
    }
    Err(error(format!(
        "{} is the first migration, and the database has some of its tables ({}) but not \
         others ({}), or not all their columns. Bring the tables in line by hand, then run \
         `migrate --fake {}` to record it as applied",
        first.name(),
        present.join(", "),
        missing.join(", "),
        first.name()
    )))
}

enum Finish<'a> {
    Record(&'a str, &'a str),
    Forget(&'a str, &'a str),
}

/// Runs the steps and records the change in one transaction.
///
/// On SQLite, rebuilding a table that others refer to only works with foreign keys off, and
/// that setting cannot change inside a transaction. So the transaction's connection commits
/// its (empty) transaction, turns the checks off and begins again; at the end the keys are
/// checked, and the setting restored the same way.
async fn execute(
    db: &DatabaseConnection,
    steps: Vec<Step>,
    finish: Finish<'_>,
) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let txn = db.begin().await?;
    let mut keys_off = false;
    if backend == DbBackend::Sqlite {
        let on = txn
            .query_one_raw(Statement::from_string(backend, "PRAGMA foreign_keys"))
            .await?
            .and_then(|row| row.try_get_by_index::<i32>(0).ok())
            == Some(1);
        if on {
            txn.execute_unprepared("COMMIT").await?;
            txn.execute_unprepared("PRAGMA foreign_keys = OFF").await?;
            txn.execute_unprepared("BEGIN").await?;
            keys_off = true;
        }
    }
    let result = run(&txn, steps, &finish, backend).await;
    let restore = |end: &'static str| {
        let txn = &txn;
        async move {
            if keys_off {
                txn.execute_unprepared(end).await?;
                txn.execute_unprepared("PRAGMA foreign_keys = ON").await?;
                txn.execute_unprepared("BEGIN").await?;
            }
            Ok::<(), DbErr>(())
        }
    };
    match result {
        Ok(()) => {
            restore("COMMIT").await?;
            txn.commit().await
        }
        Err(e) => {
            let _ = restore("ROLLBACK").await;
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}

async fn run(
    txn: &DatabaseTransaction,
    steps: Vec<Step>,
    finish: &Finish<'_>,
    backend: DbBackend,
) -> Result<(), DbErr> {
    for step in steps {
        match step {
            Step::Sql(sql) => {
                txn.execute_unprepared(&sql)
                    .await
                    .map_err(|e| DbErr::Custom(format!("{e}\n  while running: {sql}")))?;
            }
            Step::Run(code) => code(txn).await?,
        }
    }
    if backend == DbBackend::Sqlite {
        let broken = txn
            .query_all_raw(Statement::from_string(backend, "PRAGMA foreign_key_check"))
            .await?;
        if !broken.is_empty() {
            let tables: BTreeSet<String> = broken
                .iter()
                .filter_map(|row| row.try_get_by_index::<String>(0).ok())
                .collect();
            return Err(DbErr::Custom(format!(
                "rows in {} refer to rows that no longer exist",
                tables.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    match finish {
        Finish::Record(app, name) => record(txn, app, name).await,
        Finish::Forget(app, name) => forget(txn, app, name).await,
    }
}

/// The SQL a migration runs on `backend`, for `sqlmigrate`.
pub(crate) fn sql_of(
    migrations: &[Migration],
    index: usize,
    backend: DbBackend,
    backwards: bool,
) -> Result<Vec<String>, String> {
    let migration = &migrations[index];
    let steps = if backwards {
        let state = State::after(&migrations[..=index])?;
        steps(&reversed(migration)?, state, backend)?
    } else {
        let state = State::after(&migrations[..index])?;
        steps(migration.operations(), state, backend)?
    };
    Ok(steps
        .into_iter()
        .map(|step| match step {
            Step::Sql(sql) => format!("{sql};"),
            Step::Run(_) => "-- (Rust code)".to_owned(),
        })
        .collect())
}

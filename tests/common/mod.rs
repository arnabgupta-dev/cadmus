//! Shared by the integration tests: the database every test runs against.
//!
//! SQLite in memory by default. Set `TEST_DATABASE_URL` to a PostgreSQL URL (and build with
//! `--features postgres`) to run the same tests there.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A fresh, empty database for one test.
///
/// On PostgreSQL that is a new schema, so tests can run in parallel against one server
/// without seeing each other's tables.
pub async fn database() -> DatabaseConnection {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        // One connection: every pooled connection to `:memory:` would otherwise be its own
        // database.
        let mut options = ConnectOptions::new("sqlite::memory:");
        options.max_connections(1);
        return Database::connect(options).await.unwrap();
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let schema = format!(
        "test_{}_{}_{nanos}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let setup = Database::connect(&url).await.unwrap();
    setup
        .execute_unprepared(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    setup.close().await.unwrap();

    let mut options = ConnectOptions::new(url);
    options.set_schema_search_path(schema).max_connections(4);
    Database::connect(options).await.unwrap()
}

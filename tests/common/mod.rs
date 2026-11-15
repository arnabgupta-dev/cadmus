//! Shared by the integration tests: the database every test runs against.
//!
//! SQLite in memory by default. Set `TEST_DATABASE_URL` to a PostgreSQL or MySQL URL to run
//! the same tests there.

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
    if url.starts_with("mysql:") {
        // MySQL's schemas are databases: make one and connect to it.
        let setup = Database::connect(&url).await.unwrap();
        setup
            .execute_unprepared(&format!("CREATE DATABASE {schema}"))
            .await
            .unwrap();
        setup.close().await.unwrap();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(base, _)| base);
        let mut options = ConnectOptions::new(format!("{base}/{schema}"));
        options.max_connections(4);
        return Database::connect(options).await.unwrap();
    }
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

/// What a login token carries to show which password it was issued under, as Cadmus
/// computes it: for tests that make their own tokens.
#[allow(dead_code)]
pub async fn password_print(db: &DatabaseConnection, username: &str) -> String {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use sha2::Digest;
    let account = cadmus::user::Entity::find()
        .filter(cadmus::user::Column::Username.eq(username))
        .one(db)
        .await
        .unwrap()
        .expect("the account exists");
    sha2::Sha256::digest(account.password_hash.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

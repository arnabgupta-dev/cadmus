//! Versioned migrations: writing them from model changes, applying, rolling back, and
//! adopting databases made before migrations.
//!
//! The files `makemigrations` writes for the two versions of the models below are checked in
//! under `tests/migrations/`, compiled here, and compared with what is written now. After a
//! deliberate change to the generated code, refresh them with
//! `UPDATE_FIXTURES=1 cargo test --test migrations`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use heraapi::migrations::{Column, Migration, MigrationEvent, Type};
use heraapi::{App, AuthConfig, ModelOptions};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

mod common;

// Kept exactly as `makemigrations` writes them, so not formatted.
#[rustfmt::skip]
#[path = "migrations/m0001_initial.rs"]
mod m0001;
#[rustfmt::skip]
#[path = "migrations/m0002_blog_changes.rs"]
mod m0002;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

/// The first version of the models.
mod v1 {
    pub mod author {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "author")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod post {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "post")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub body: String,
            pub views: i32,
            pub author_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::author::Entity",
                from = "Column::AuthorId",
                to = "super::author::Column::Id"
            )]
            Author,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }
}

/// The second version: a new optional column, a renamed column, a longer title with an
/// index, a new column with a default, a dropped column, and a new table.
mod v2 {
    pub mod author {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "author")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
            pub email: Option<String>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod post {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "post")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            #[sea_orm(column_type = "String(StringLen::N(200))", indexed)]
            pub title: String,
            #[sea_orm(renamed_from = "body")]
            pub content: String,
            #[sea_orm(default_value = "draft")]
            pub status: String,
            pub author_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::author::Entity",
                from = "Column::AuthorId",
                to = "super::author::Column::Id"
            )]
            Author,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }

    pub mod comment {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "comment")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub post_id: i32,
            #[sea_orm(column_type = "Text")]
            pub text: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::post::Entity",
                from = "Column::PostId",
                to = "super::post::Column::Id",
                on_delete = "Cascade"
            )]
            Post,
        }

        impl ActiveModelBehavior for ActiveModel {}
    }
}

fn app_v1(db: &DatabaseConnection) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<v1::author::Entity>(ModelOptions::new())
        .model::<v1::post::Entity>(ModelOptions::new())
}

fn app_v2(db: &DatabaseConnection) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<v2::author::Entity>(ModelOptions::new())
        .model::<v2::post::Entity>(ModelOptions::new())
        .model::<v2::comment::Entity>(ModelOptions::new())
}

static NEXT: AtomicU32 = AtomicU32::new(0);

/// An empty directory for one test's migration files.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hera-migrations-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("migrations")
        .join(name)
}

/// Compares a written file with its checked-in copy, or refreshes the copy.
fn matches_fixture(written: &std::path::Path, name: &str) {
    let text = std::fs::read_to_string(written).unwrap();
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(fixture(name), &text).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(fixture(name)).unwrap();
    assert_eq!(
        text, expected,
        "{name} differs from what makemigrations writes now"
    );
}

async fn query_strings(db: &DatabaseConnection, sql: &str) -> Vec<String> {
    db.query_all_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .iter()
        .map(|row| {
            row.try_get_by_index::<String>(0)
                .or_else(|_| row.try_get_by_index::<i32>(0).map(|n| n.to_string()))
                .or_else(|_| row.try_get_by_index::<i64>(0).map(|n| n.to_string()))
                .unwrap()
        })
        .collect()
}

async fn works(db: &DatabaseConnection, sql: &str) -> bool {
    db.execute_unprepared(sql).await.is_ok()
}

#[tokio::test]
async fn makemigrations_writes_the_changes_as_rust() {
    let db = common::database().await;
    let dir = scratch_dir();

    let first = app_v1(&db).make_migration(&dir, None).unwrap().unwrap();
    assert_eq!(first.name, "0001_initial");
    assert_eq!(first.changes, ["+ table author", "+ table post"]);
    matches_fixture(&first.path, "m0001_initial.rs");
    let list = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(list.contains("mod m0001_initial;"), "{list}");
    assert!(list.contains("m0001_initial::migration(),"));

    // The second version, built with the first migration, gets the difference.
    let v2 = app_v2(&db).migrations(vec![m0001::migration()]);
    let second = v2
        .make_migration(&dir, Some("blog changes"))
        .unwrap()
        .unwrap();
    assert_eq!(second.name, "0002_blog_changes");
    assert_eq!(
        second.changes,
        [
            "> column post.body renamed to content",
            "+ table comment",
            "+ column author.email",
            "+ column post.status",
            "~ column post.title",
            "- column post.views (its data will be deleted)",
            "+ index idx-post-title",
        ]
    );
    matches_fixture(&second.path, "m0002_blog_changes.rs");
    let list = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    assert!(list.contains("mod m0001_initial;\nmod m0002_blog_changes;"));

    // The compiled files are exactly what was written, so nothing is left to do.
    let current = app_v2(&db).migrations(vec![m0001::migration(), m0002::migration()]);
    assert!(current.pending_changes().unwrap().is_empty());
    assert!(current.make_migration(&dir, None).unwrap().is_none());

    // Files the app was not built with are refused rather than numbered over.
    let error = app_v2(&db).make_migration(&dir, None).unwrap_err();
    assert!(error.to_string().contains("does not load them"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_required_column_without_a_default_needs_a_value_for_existing_rows() {
    mod later {
        pub mod author {
            use sea_orm::entity::prelude::*;

            #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
            #[sea_orm(table_name = "author")]
            pub struct Model {
                #[sea_orm(primary_key)]
                pub id: i32,
                pub name: String,
                pub country: String,
            }

            #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
            pub enum Relation {}

            impl ActiveModelBehavior for ActiveModel {}
        }
    }
    let db = common::database().await;
    let dir = scratch_dir();
    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<later::author::Entity>(ModelOptions::new())
        .model::<v1::post::Entity>(ModelOptions::new())
        .migrations(vec![m0001::migration()]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("m0001_initial.rs"), "").unwrap();
    let error = app.make_migration(&dir, None).unwrap_err().to_string();
    assert!(error.contains("`author.country` is required"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn migrations_apply_roll_back_and_apply_again_keeping_the_data() {
    let db = common::database().await;
    let v1 = app_v1(&db).migrations(vec![m0001::migration()]);
    let events = v1.migrate_to(None, false).await.unwrap();
    assert!(events.contains(&MigrationEvent::Applied {
        app: "app".into(),
        name: "0001_initial".into()
    }));
    assert!(events.contains(&MigrationEvent::Applied {
        app: "auth".into(),
        name: "0001_initial".into()
    }));
    assert!(works(&db, "INSERT INTO author (id, name) VALUES (1, 'Ada')").await);
    assert!(
        works(
            &db,
            "INSERT INTO post (id, title, body, views, author_id) VALUES (1, 'Hello', 'First post', 7, 1)"
        )
        .await
    );
    // The foreign key is there.
    assert!(
        !works(
            &db,
            "INSERT INTO post (id, title, body, views, author_id) VALUES (2, 'x', 'y', 0, 99)"
        )
        .await
    );

    let v2 = app_v2(&db).migrations(vec![m0001::migration(), m0002::migration()]);
    let events = v2.migrate_to(None, false).await.unwrap();
    assert_eq!(
        events,
        [MigrationEvent::Applied {
            app: "app".into(),
            name: "0002_blog_changes".into()
        }]
    );
    assert_eq!(
        query_strings(&db, "SELECT content FROM post").await,
        ["First post"]
    );
    assert_eq!(
        query_strings(&db, "SELECT status FROM post").await,
        ["draft"]
    );
    assert!(!works(&db, "SELECT views FROM post").await);
    assert!(
        works(
            &db,
            "INSERT INTO comment (post_id, text) VALUES (1, 'Nice')"
        )
        .await
    );
    // Deleting the post deletes its comments (on_delete = Cascade), and the post's other
    // foreign key survived the changes.
    assert!(
        !works(
            &db,
            "INSERT INTO post (title, content, author_id) VALUES ('x', 'y', 99)"
        )
        .await
    );
    // Running again does nothing.
    assert!(v2.migrate_to(None, false).await.unwrap().is_empty());

    // Back to the first version: the rename is undone and the dropped column comes back
    // with an empty value.
    let events = v2.migrate_to(Some("0001"), false).await.unwrap();
    assert_eq!(
        events,
        [MigrationEvent::Unapplied {
            app: "app".into(),
            name: "0002_blog_changes".into()
        }]
    );
    assert_eq!(
        query_strings(&db, "SELECT body FROM post").await,
        ["First post"]
    );
    assert_eq!(query_strings(&db, "SELECT views FROM post").await, ["0"]);
    assert!(!works(&db, "SELECT 1 FROM comment").await);

    // And forward again, then all the way back.
    v2.migrate_to(None, false).await.unwrap();
    assert_eq!(
        query_strings(&db, "SELECT content FROM post").await,
        ["First post"]
    );
    v2.migrate_to(Some("zero"), false).await.unwrap();
    assert!(!works(&db, "SELECT 1 FROM post").await);
    assert!(!works(&db, "SELECT 1 FROM author").await);
    // The built-in tables stay.
    assert!(works(&db, "SELECT 1 FROM auth_user").await);
}

#[tokio::test]
async fn changing_a_table_others_refer_to_keeps_their_rows() {
    let db = common::database().await;
    let base = vec![m0001::migration(), m0002::migration()];
    app_v2(&db)
        .migrations(base.clone())
        .migrate()
        .await
        .unwrap();
    db.execute_unprepared("INSERT INTO author (id, name) VALUES (1, 'ada')")
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO post (id, title, content, author_id) VALUES (1, 'Hello', 'First', 1)",
    )
    .await
    .unwrap();
    db.execute_unprepared("INSERT INTO comment (post_id, text) VALUES (1, 'Nice')")
        .await
        .unwrap();

    // Hand-written: a type change on `post`, which `comment` refers to (SQLite rebuilds the
    // table), SQL, and Rust code changing data.
    let mut migrations = base;
    migrations.push(
        Migration::new("0003_by_hand")
            .alter_column(
                "post",
                Column::new("content", Type::String(None)),
                Column::new("content", Type::Text),
            )
            .sql_reversible(
                "UPDATE author SET name = upper(name)",
                "UPDATE author SET name = lower(name)",
            )
            .run_reversible(
                |db| {
                    Box::pin(async move {
                        db.execute_unprepared("UPDATE post SET title = 'Hi'")
                            .await?;
                        Ok(())
                    })
                },
                |db| {
                    Box::pin(async move {
                        db.execute_unprepared("UPDATE post SET title = 'Hello'")
                            .await?;
                        Ok(())
                    })
                },
            ),
    );
    let app = app_v2(&db).migrations(migrations);
    let events = app.migrate_to(None, false).await.unwrap();
    assert!(events.contains(&MigrationEvent::Applied {
        app: "app".into(),
        name: "0003_by_hand".into()
    }));
    // The models still say `content` is a string, so `migrate` points that out.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, MigrationEvent::Unmigrated(changes) if changes == &["~ column post.content"]))
    );
    assert_eq!(query_strings(&db, "SELECT name FROM author").await, ["ADA"]);
    assert_eq!(query_strings(&db, "SELECT title FROM post").await, ["Hi"]);
    assert_eq!(
        query_strings(&db, "SELECT text FROM comment").await,
        ["Nice"]
    );
    // The comment's key still points at the rebuilt table.
    assert!(
        !works(
            &db,
            "INSERT INTO comment (post_id, text) VALUES (42, 'orphan')"
        )
        .await
    );

    app.migrate_to(Some("0002_blog_changes"), false)
        .await
        .unwrap();
    assert_eq!(query_strings(&db, "SELECT name FROM author").await, ["ada"]);
    assert_eq!(
        query_strings(&db, "SELECT title FROM post").await,
        ["Hello"]
    );
    assert_eq!(
        query_strings(&db, "SELECT text FROM comment").await,
        ["Nice"]
    );
}

#[tokio::test]
async fn sql_without_a_reverse_cannot_be_rolled_back() {
    let db = common::database().await;
    let mut migrations = vec![m0001::migration()];
    migrations.push(Migration::new("0002_data").sql("UPDATE author SET name = name"));
    let app = app_v1(&db).migrations(migrations);
    app.migrate().await.unwrap();
    let error = app.migrate_to(Some("0001"), false).await.unwrap_err();
    assert!(
        error.to_string().contains("cannot be rolled back"),
        "{error}"
    );
    // `--fake` forgets it without running anything.
    let events = app.migrate_to(Some("0001"), true).await.unwrap();
    assert_eq!(
        events,
        [MigrationEvent::Unapplied {
            app: "app".into(),
            name: "0002_data".into()
        }]
    );
}

#[tokio::test]
async fn a_failed_migration_changes_nothing_where_the_database_allows() {
    let db = common::database().await;
    let mut migrations = vec![m0001::migration()];
    migrations.push(
        Migration::new("0002_broken")
            .add_column("author", Column::new("bio", Type::Text).null())
            .sql("UPDATE no_such_table SET x = 1"),
    );
    let app = app_v1(&db).migrations(migrations);
    let error = app.migrate().await.unwrap_err().to_string();
    assert!(error.contains("0002_broken failed"), "{error}");
    assert!(error.contains("no_such_table"), "{error}");
    // MySQL commits each change of a table's structure as it goes; the others undo all.
    if db.get_database_backend() != DbBackend::MySql {
        assert!(!works(&db, "SELECT bio FROM author").await);
    }
    assert!(works(&db, "SELECT name FROM author").await);
}

#[tokio::test]
async fn projects_that_kept_tables_in_sync_adopt_migrations() {
    let db = common::database().await;
    let events = app_v1(&db).migrate_to(None, false).await.unwrap();
    assert!(events.contains(&MigrationEvent::Synced));
    db.execute_unprepared("INSERT INTO author (id, name) VALUES (1, 'Ada')")
        .await
        .unwrap();

    // The first migration's tables are already there, so it is only recorded.
    let events = app_v1(&db)
        .migrations(vec![m0001::migration()])
        .migrate_to(None, false)
        .await
        .unwrap();
    assert_eq!(
        events,
        [MigrationEvent::Faked {
            app: "app".into(),
            name: "0001_initial".into()
        }]
    );
    // Later migrations run as usual, on the synced tables.
    app_v2(&db)
        .migrations(vec![m0001::migration(), m0002::migration()])
        .migrate()
        .await
        .unwrap();
    assert_eq!(query_strings(&db, "SELECT name FROM author").await, ["Ada"]);
    assert!(works(&db, "SELECT status FROM post").await);
}

#[tokio::test]
async fn databases_from_before_migrations_get_the_built_in_tables_brought_up_to_date() {
    let db = common::database().await;
    // An old database: accounts, but not the tables added since.
    db.get_schema_builder()
        .register(heraapi::user::Entity)
        .sync(&db)
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO auth_user (username, password_hash, is_active, is_staff, is_superuser) \
         VALUES ('root', 'x', TRUE, TRUE, TRUE)",
    )
    .await
    .unwrap();

    let events = app_v1(&db).migrate_to(None, false).await.unwrap();
    assert!(events.contains(&MigrationEvent::Upgraded { app: "auth".into() }));
    assert!(works(&db, "SELECT digest FROM auth_recovery_code").await);
    assert!(works(&db, "SELECT provider FROM auth_identity").await);
    assert_eq!(
        query_strings(&db, "SELECT username FROM auth_user").await,
        ["root"]
    );
    assert_eq!(
        query_strings(&db, "SELECT name FROM hera_migration WHERE app = 'auth'").await,
        ["0001_initial"]
    );
    // Next time there is nothing to do for them.
    let events = app_v1(&db).migrate_to(None, false).await.unwrap();
    assert!(!events.iter().any(|e| matches!(
        e,
        MigrationEvent::Upgraded { .. } | MigrationEvent::Applied { .. }
    )));
}

#[tokio::test]
async fn a_database_ahead_of_the_code_is_refused() {
    let db = common::database().await;
    app_v2(&db)
        .migrations(vec![m0001::migration(), m0002::migration()])
        .migrate()
        .await
        .unwrap();
    let error = app_v1(&db)
        .migrations(vec![m0001::migration()])
        .migrate()
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("which this code does not have"), "{error}");
}

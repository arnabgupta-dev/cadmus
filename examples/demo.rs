//! Runnable demo: `cargo run --example demo`, then open http://127.0.0.1:3000/admin
//!
//! Every `ADMIN_*` variable documented on `UiSettings::from_env`, `AuthConfig::from_env` and
//! `EnvUserStore::from_env` is honoured. With no auth variables set, the demo generates a
//! throwaway secret and password and prints the password once.

use axum::Router;
use cadmus::{
    Admin, AdminUser, AuthConfig, EnvUserStore, ModelOptions, SeaOrmResource, UiSettings,
    hash_password,
};
use sea_orm::{ConnectOptions, ConnectionTrait, Database};

mod post {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "post")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        #[sea_orm(column_type = "Text")]
        pub body: String,
        pub published: bool,
        pub views: i32,
        pub published_at: Option<DateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod author {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "author")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        pub email: String,
        pub api_key: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

fn random_hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = ConnectOptions::new("sqlite::memory:");
    options.max_connections(1);
    let db = Database::connect(options).await?;
    db.execute_unprepared(
        "CREATE TABLE post (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL,
            body TEXT NOT NULL, published BOOLEAN NOT NULL, views INTEGER NOT NULL,
            published_at DATETIME);
         CREATE TABLE author (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
            email TEXT NOT NULL, api_key TEXT NOT NULL DEFAULT '');
         INSERT INTO post (title, body, published, views) VALUES
            ('Hello admin', 'Generated from the entity definition.', 1, 3);
         INSERT INTO author (name, email, api_key) VALUES ('Ada', 'ada@example.com', 'k-123');",
    )
    .await?;

    let (auth, users) = if std::env::var_os("ADMIN_JWT_SECRET").is_some() {
        (AuthConfig::from_env()?, EnvUserStore::from_env()?)
    } else {
        let password = random_hex(12);
        println!("demo login: admin / {password}");
        let user = AdminUser {
            username: "admin".into(),
            password_hash: hash_password(&password)?,
            totp_secret: None,
        };
        // Plain HTTP on localhost, so the cookie cannot be `Secure` here.
        (
            AuthConfig::new(random_hex(32))?.cookie_secure(false),
            EnvUserStore::new(user)?,
        )
    };

    let admin =
        Admin::builder()
            .settings(UiSettings::from_env()?)
            .auth(auth, users)
            .register(SeaOrmResource::<post::Entity>::new(db.clone()).options(
                ModelOptions::new().label("Posts").list_display([
                    "id",
                    "title",
                    "published",
                    "views",
                ]),
            ))
            .register(
                SeaOrmResource::<author::Entity>::new(db).options(
                    ModelOptions::new()
                        .label("Authors")
                        .hidden_fields(["api_key"]),
                ),
            )
            .build()?;

    let app = Router::new().merge(admin.router());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("listening on http://127.0.0.1:3000/admin");
    axum::serve(listener, app).await?;
    Ok(())
}

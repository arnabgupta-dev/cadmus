//! A whole app through PgBouncer: migrations, accounts, and many API requests at once over a
//! pool of connections. Runs only when `PGBOUNCER_TEST_URL` names a PgBouncer in front of an
//! empty PostgreSQL database, e.g.
//! `PGBOUNCER_TEST_URL=postgres://cadmus:test@127.0.0.1:6439/cadmus cargo test --test pgbouncer`.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cadmus::{App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::{ConnectOptions, Database};
use serde_json::{Value, json};
use tower::ServiceExt;

mod note {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "note")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub text: String,
        pub views: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_works_through_pgbouncer() {
    let Ok(url) = std::env::var("PGBOUNCER_TEST_URL") else {
        eprintln!("PGBOUNCER_TEST_URL is not set; skipped");
        return;
    };
    let mut options = ConnectOptions::new(url);
    options.max_connections(8);
    let db = Database::connect(options).await.unwrap();
    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<note::Entity>(ModelOptions::new().search_fields(["text"]));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();

    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "root", "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    let (status, body) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bearer = format!("Bearer {}", body["token"].as_str().unwrap());

    // Many requests at once, so that the pool's connections share PgBouncer's few server
    // connections, each with the statements it has prepared.
    let mut tasks = Vec::new();
    for i in 0..200 {
        let (app, bearer) = (app.clone(), bearer.clone());
        tasks.push(tokio::spawn(async move {
            let request = match i % 4 {
                0 => Request::post("/api/note")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "text": format!("note {i}"), "views": i }).to_string(),
                    )),
                1 => Request::get("/api/note?per_page=5").body(Body::empty()),
                2 => Request::get(format!("/api/note?q=note&views__gte={i}")).body(Body::empty()),
                _ => Request::get("/api/note?sort=-views").body(Body::empty()),
            };
            let mut request = request.unwrap();
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, bearer.parse().unwrap());
            call(&app, request).await
        }));
    }
    let mut failures = Vec::new();
    for task in tasks {
        let (status, body) = task.await.unwrap();
        if !status.is_success() {
            failures.push(format!("{status}: {body}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failed, e.g. {}",
        failures.len(),
        failures[0]
    );

    let request = Request::get("/api/note?per_page=1")
        .header(header::AUTHORIZATION, &bearer)
        .body(Body::empty())
        .unwrap();
    let (_, body) = call(&app, request).await;
    assert_eq!(body["meta"]["total"], 50, "{body}");
}

//! Model behaviour that differs between databases: every column type the generator offers,
//! searching, and `migrate` on a table that already has rows.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cadmus::{App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

/// One column of every type `cadmus generate model` can produce, required and nullable.
mod sample {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sample")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub name: String,
        #[sea_orm(column_type = "Text")]
        pub notes: String,
        pub count: i32,
        pub big: i64,
        pub ratio: f64,
        pub active: bool,
        pub seen_at: DateTime,
        pub born: Date,
        pub extra: Json,
        pub maybe_name: Option<String>,
        pub maybe_count: Option<i32>,
        pub maybe_big: Option<i64>,
        pub maybe_ratio: Option<f64>,
        pub maybe_active: Option<bool>,
        pub maybe_seen_at: Option<DateTime>,
        pub maybe_born: Option<Date>,
        pub maybe_extra: Option<Json>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The first version of a model...
mod task_v1 {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "task")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// ...and the same model after a developer added two fields.
mod task_v2 {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "task")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub title: String,
        pub priority: Option<i32>,
        pub due: Option<Date>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection) -> App {
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
}

async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn token(app: &Router) -> String {
    let body = json!({ "username": "root", "password": PASSWORD });
    let (status, reply) = call(app, "", "POST", "/api/auth/token", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["token"].as_str().unwrap().to_owned()
}

/// Titles of the rows a search returns, in id order.
async fn search(app: &Router, token: &str, model: &str, q: &str) -> Vec<String> {
    let (status, reply) = call(app, token, "GET", &format!("/api/{model}?q={q}"), None).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    reply["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_owned())
        .collect()
}

fn full_row(name: &str) -> Value {
    json!({
        "name": name,
        "notes": "long\ntext with 'quotes' and ünïcödé",
        "count": -42,
        "big": 9_007_199_254_740_993_i64,
        "ratio": 0.125,
        "active": true,
        "seen_at": "2026-03-04T05:06:07",
        "born": "1999-12-31",
        "extra": { "tags": ["a", "b"], "depth": { "n": 1 } },
    })
}

#[tokio::test]
async fn every_generated_column_type_round_trips() {
    let db = common::database().await;
    let app = new_app(&db).model::<sample::Entity>(ModelOptions::new());
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let token = token(&app).await;

    // Required columns only: the nullable ones come back as null.
    let (status, row) = call(&app, &token, "POST", "/api/sample", Some(full_row("first"))).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let id = row["id"].as_i64().unwrap();
    assert_eq!(row["name"], "first");
    assert_eq!(row["notes"], "long\ntext with 'quotes' and ünïcödé");
    assert_eq!(row["count"], -42);
    assert_eq!(row["big"], 9_007_199_254_740_993_i64);
    assert_eq!(row["ratio"], 0.125);
    assert_eq!(row["active"], true);
    assert_eq!(row["seen_at"], "2026-03-04T05:06:07");
    assert_eq!(row["born"], "1999-12-31");
    assert_eq!(
        row["extra"],
        json!({ "tags": ["a", "b"], "depth": { "n": 1 } })
    );
    for column in [
        "maybe_name",
        "maybe_count",
        "maybe_big",
        "maybe_ratio",
        "maybe_active",
        "maybe_seen_at",
        "maybe_born",
        "maybe_extra",
    ] {
        assert_eq!(row[column], Value::Null, "{column}");
    }

    // Fill the nullable ones, then read the row back on its own and in a list.
    let patch = json!({
        "maybe_name": "set",
        "maybe_count": 7,
        "maybe_big": -9_007_199_254_740_993_i64,
        "maybe_ratio": -2.5,
        "maybe_active": false,
        "maybe_seen_at": "2001-02-03T04:05:06",
        "maybe_born": "2024-02-29",
        "maybe_extra": [1, "two", null],
        "active": false,
    });
    let path = format!("/api/sample/{id}");
    let (status, reply) = call(&app, &token, "PATCH", &path, Some(patch.clone())).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (status, row) = call(&app, &token, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{row}");
    for (column, expected) in patch.as_object().unwrap() {
        assert_eq!(&row[column], expected, "{column}");
    }
    // Untouched by the patch.
    assert_eq!(row["count"], -42);
    assert_eq!(row["extra"]["tags"][1], "b");

    let (status, list) = call(&app, &token, "GET", "/api/sample", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["meta"]["total"], 1);
    assert_eq!(list["data"][0], row);

    // And back to null.
    let (status, reply) = call(
        &app,
        &token,
        "PATCH",
        &path,
        Some(json!({ "maybe_name": null, "maybe_extra": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let (_, row) = call(&app, &token, "GET", &path, None).await;
    assert_eq!(row["maybe_name"], Value::Null);
    assert_eq!(row["maybe_extra"], Value::Null);
    assert_eq!(row["maybe_count"], 7);

    // Values of the wrong type are refused with the field named, on every database.
    for (column, bad) in [
        ("count", json!("many")),
        ("big", json!(1.5)),
        ("active", json!("maybe")),
        ("seen_at", json!("yesterday")),
        ("born", json!("2024-02-30")),
    ] {
        let (status, reply) =
            call(&app, &token, "PATCH", &path, Some(json!({ column: bad }))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{column}: {reply}"
        );
        assert_eq!(reply["error"]["field"], column);
    }

    let (status, _) = call(&app, &token, "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, &token, "GET", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_behaves_the_same_on_every_database() {
    let db = common::database().await;
    let app = new_app(&db).model::<sample::Entity>(ModelOptions::new());
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let router = app.router().unwrap();
    let token = token(&router).await;

    for (name, count) in [
        ("Alpha Report", 100),
        ("alphabet soup", 2100),
        ("100% done", 3),
        ("under_score", 4),
    ] {
        let mut row = full_row(name);
        row["count"] = json!(count);
        row["notes"] = json!("n");
        let (status, reply) = call(&router, &token, "POST", "/api/sample", Some(row)).await;
        assert_eq!(status, StatusCode::CREATED, "{reply}");
    }

    // Case does not matter.
    assert_eq!(
        search(&router, &token, "sample", "ALPHA").await,
        ["Alpha Report", "alphabet soup"]
    );
    assert_eq!(
        search(&router, &token, "sample", "report").await,
        ["Alpha Report"]
    );
    // `%` and `_` are ordinary characters, not wildcards.
    assert_eq!(
        search(&router, &token, "sample", "100%25").await,
        ["100% done"]
    );
    assert_eq!(
        search(&router, &token, "sample", "r_s").await,
        ["under_score"]
    );
    assert_eq!(
        search(&router, &token, "sample", "%25").await,
        ["100% done"]
    );
    // By default only text columns are searched, so a number matches nothing.
    assert!(search(&router, &token, "sample", "2100").await.is_empty());

    // A model may name non-text columns to search.
    let app = new_app(&db).model::<sample::Entity>(
        ModelOptions::new().search_fields(["name", "count", "active", "born"]),
    );
    let router = app.router().unwrap();
    assert_eq!(
        search(&router, &token, "sample", "100").await,
        ["Alpha Report", "alphabet soup", "100% done"]
    );
    assert_eq!(
        search(&router, &token, "sample", "2100").await,
        ["alphabet soup"]
    );
    assert_eq!(search(&router, &token, "sample", "1999-12").await.len(), 4);
}

#[tokio::test]
async fn migrate_adds_new_columns_and_keeps_existing_rows() {
    let db = common::database().await;
    let v1 = new_app(&db).model::<task_v1::Entity>(ModelOptions::new());
    v1.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let router = v1.router().unwrap();
    let token = token(&router).await;
    let (status, reply) = call(
        &router,
        &token,
        "POST",
        "/api/task",
        Some(json!({ "title": "old row" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reply}");

    let v2 = new_app(&db).model::<task_v2::Entity>(ModelOptions::new());
    v2.migrate().await.unwrap();
    // Nothing left to do the second time.
    v2.migrate().await.unwrap();
    let router = v2.router().unwrap();

    let (status, row) = call(&router, &token, "GET", "/api/task/1", None).await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(
        row,
        json!({ "id": 1, "title": "old row", "priority": null, "due": null })
    );

    let body = json!({ "title": "new row", "priority": 3, "due": "2027-01-02" });
    let (status, row) = call(&router, &token, "POST", "/api/task", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(
        row,
        json!({ "id": 2, "title": "new row", "priority": 3, "due": "2027-01-02" })
    );
}

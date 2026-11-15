//! Your own routes: public unless a handler takes `CurrentUser`, which tells it who sent the
//! request and refuses everyone else; and account stores that do not say what accounts may do.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use cadmus::{
    Action, AdminError, AdminUser, ApiResult, App, AuthConfig, CurrentUser, ModelOptions,
    UserStore, change_password, create_superuser, hash_password, new_api_key,
};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

mod note {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "note")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub text: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

async fn whoami(user: CurrentUser) -> Json<Value> {
    Json(json!({
        "username": user.username(),
        "superuser": user.is_superuser(),
        "csrf": user.csrf_token(),
    }))
}

async fn greeting(user: Option<CurrentUser>) -> String {
    match user {
        Some(user) => format!("hello {}", user.username()),
        None => "hello anyone".into(),
    }
}

async fn edit_note(user: CurrentUser) -> ApiResult<&'static str> {
    user.require("note", Action::Change)?;
    Ok("changed")
}

fn routes() -> Router {
    let protected = get(whoami).post(whoami);
    Router::new()
        .route("/public", get(|| async { "open" }))
        .route("/greeting", get(greeting))
        .route("/whoami", protected.clone())
        .route("/notes/edit", get(edit_note).post(edit_note))
        // Under the admin's path, where the browser sends the session cookie.
        .route("/admin/extra/whoami", protected)
}

fn new_app(db: &DatabaseConnection) -> App {
    App::new(
        db.clone(),
        AuthConfig::new(SECRET)
            .unwrap()
            .cookie_secure(false)
            .api_keys(true),
    )
    .model::<note::Entity>(ModelOptions::new())
    .routes(routes())
}

async fn site() -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = new_app(&db);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    (app.router().unwrap(), db)
}

struct Reply {
    status: StatusCode,
    set_cookie: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        set_cookie: parts
            .headers
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned()),
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn fetch(app: &Router, method: Method, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn token(app: &Router, username: &str) -> String {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": username, "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    let reply = send(app, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.json()["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn own_routes_are_public_unless_they_ask_who_is_calling() {
    let (app, _db) = site().await;

    assert_eq!(fetch(&app, Method::GET, "/public", &[]).await.body, "open");
    let anonymous = fetch(&app, Method::GET, "/whoami", &[]).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        anonymous.json()["error"]["code"],
        "unauthorized",
        "{}",
        anonymous.body
    );
    let forged = fetch(
        &app,
        Method::GET,
        "/whoami",
        &[("authorization", "Bearer a.b.c")],
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);

    let bearer = format!("Bearer {}", token(&app, "root").await);
    let me = fetch(&app, Method::GET, "/whoami", &[("authorization", &bearer)]).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.json()["username"], "root");
    assert_eq!(me.json()["superuser"], true);
    // Login tokens need no CSRF token, so there is none to hand out.
    assert_eq!(me.json()["csrf"], Value::Null);
    let changed = fetch(&app, Method::POST, "/whoami", &[("authorization", &bearer)]).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);

    let greeting = fetch(&app, Method::GET, "/greeting", &[]).await;
    assert_eq!(greeting.body, "hello anyone");
    let greeting = fetch(
        &app,
        Method::GET,
        "/greeting",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(greeting.body, "hello root");
}

#[tokio::test]
async fn cookie_sessions_must_send_the_csrf_token_to_change_anything() {
    let (app, _db) = site().await;
    let login = Request::post("/admin/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "username=root&password={}",
            PASSWORD.replace(' ', "+")
        )))
        .unwrap();
    let login = send(&app, login).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
    let cookie = login.set_cookie.unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();

    let me = fetch(
        &app,
        Method::GET,
        "/admin/extra/whoami",
        &[("cookie", &cookie)],
    )
    .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    let csrf = me.json()["csrf"].as_str().unwrap().to_owned();

    let forged = fetch(
        &app,
        Method::POST,
        "/admin/extra/whoami",
        &[("cookie", &cookie)],
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.body);
    let wrong = [("cookie", cookie.as_str()), ("x-csrf-token", "nope")];
    let wrong = fetch(&app, Method::POST, "/admin/extra/whoami", &wrong).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let sent = [("cookie", cookie.as_str()), ("x-csrf-token", csrf.as_str())];
    let sent = fetch(&app, Method::POST, "/admin/extra/whoami", &sent).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
}

#[tokio::test]
async fn api_keys_work_on_own_routes_and_read_only_ones_only_read() {
    let (app, db) = site().await;
    let full = new_api_key(&db, "root", "script", None, false)
        .await
        .unwrap();
    let read_only = new_api_key(&db, "root", "viewer", None, true)
        .await
        .unwrap();

    let me = fetch(&app, Method::GET, "/whoami", &[("x-api-key", &full)]).await;
    assert_eq!(me.json()["username"], "root", "{}", me.body);
    let bearer = format!("Bearer {full}");
    let changed = fetch(
        &app,
        Method::POST,
        "/notes/edit",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);

    let viewer = [("x-api-key", read_only.as_str())];
    let me = fetch(&app, Method::GET, "/whoami", &viewer).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(
        me.json()["superuser"],
        false,
        "a read-only key is no superuser"
    );
    let refused = fetch(&app, Method::POST, "/whoami", &viewer).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    // Even a GET handler that checks for a change permission says no.
    let refused = fetch(&app, Method::GET, "/notes/edit", &viewer).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

/// Accounts kept elsewhere, by a store that only finds them and says nothing about what
/// they may do.
struct Forgetful {
    hash: String,
}

#[async_trait]
impl UserStore for Forgetful {
    async fn find(&self, username: &str) -> Result<Option<AdminUser>, AdminError> {
        Ok((username == "sam").then(|| AdminUser {
            username: "sam".into(),
            password_hash: self.hash.clone(),
            totp_secret: None,
        }))
    }
}

#[tokio::test]
async fn a_store_without_access_rules_grants_nothing() {
    let db = common::database().await;
    let app = new_app(&db).users(Forgetful {
        hash: hash_password(PASSWORD).unwrap(),
    });
    app.migrate().await.unwrap();
    let app = app.router().unwrap();

    // Sam may log in...
    let bearer = format!("Bearer {}", token(&app, "sam").await);
    let me = fetch(&app, Method::GET, "/whoami", &[("authorization", &bearer)]).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.json()["superuser"], false);
    // ...but do nothing: not in the generated API, not in routes that check.
    let list = fetch(
        &app,
        Method::GET,
        "/api/note",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(list.status, StatusCode::FORBIDDEN, "{}", list.body);
    let edit = fetch(
        &app,
        Method::POST,
        "/notes/edit",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(edit.status, StatusCode::FORBIDDEN, "{}", edit.body);
}

#[tokio::test]
async fn changing_the_password_ends_logins_made_with_the_old_one() {
    let (app, db) = site().await;
    let old = format!("Bearer {}", token(&app, "root").await);
    let ok = fetch(&app, Method::GET, "/api/note", &[("authorization", &old)]).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);

    change_password(&db, "root", "a different long password")
        .await
        .unwrap();
    for path in ["/api/note", "/whoami"] {
        let refused = fetch(&app, Method::GET, path, &[("authorization", &old)]).await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{path}: {}",
            refused.body
        );
    }
    // Changing it back does not bring the old token back to life either way: it is the
    // hash, salted afresh, that counts.
    change_password(&db, "root", PASSWORD).await.unwrap();
    let refused = fetch(&app, Method::GET, "/whoami", &[("authorization", &old)]).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    let new = format!("Bearer {}", token(&app, "root").await);
    let ok = fetch(&app, Method::GET, "/whoami", &[("authorization", &new)]).await;
    assert_eq!(ok.status, StatusCode::OK);
}

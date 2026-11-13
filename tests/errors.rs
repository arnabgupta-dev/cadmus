//! Central error handling: the same error is JSON under the API path, a page under the admin
//! path and plain text elsewhere; unknown URLs, wrong methods and panics are covered too.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use heraapi::{AdminError, ApiResult, App, AuthConfig, ModelOptions, create_superuser};
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

mod note {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
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

fn out_of_stock() -> AdminError {
    AdminError::custom(StatusCode::CONFLICT, "out_of_stock", "no items left")
}

async fn typed() -> ApiResult<Json<u8>> {
    Err(out_of_stock().into())
}

async fn untyped() -> Result<String, AdminError> {
    Err(out_of_stock())
}

async fn boom() -> &'static str {
    panic!("handler blew up")
}

async fn internal() -> Result<String, AdminError> {
    Err(std::io::Error::other("disk path /secret/location").into())
}

async fn app() -> Router {
    let db = common::database().await;
    let routes = Router::new()
        .route("/shop/typed", get(typed))
        .route("/shop/untyped", get(untyped))
        .route("/shop/boom", get(boom))
        .route("/shop/internal", get(internal))
        .route("/api/shop/untyped", get(untyped))
        .route("/api/shop/boom", get(boom))
        .route("/admin/shop/extra/untyped", get(untyped))
        .route("/admin/shop/extra/boom", get(boom));
    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<note::Entity>(ModelOptions::new().label("Notes"))
        .routes(routes);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    app.router().unwrap()
}

struct Reply {
    status: StatusCode,
    content_type: String,
    allow: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        assert!(
            self.content_type.starts_with("application/json"),
            "expected JSON, got `{}`: {}",
            self.content_type,
            self.body
        );
        serde_json::from_str(&self.body).unwrap()
    }

    fn is_page(&self) -> bool {
        self.content_type.starts_with("text/html") && self.body.contains("<main id=\"main\"")
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let header = |name| {
        response
            .headers()
            .get(name)
            .map(|v: &header::HeaderValue| v.to_str().unwrap().to_owned())
    };
    let content_type = header(header::CONTENT_TYPE).unwrap_or_default();
    let allow = header(header::ALLOW);
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        allow,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn fetch(app: &Router, method: &str, path: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn login(app: &Router) -> String {
    let request = Request::post("/admin/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "username=root&password={}",
            PASSWORD.replace(' ', "+")
        )))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn custom_errors_keep_their_status_code_and_message() {
    let app = app().await;

    let reply = fetch(&app, "GET", "/shop/typed", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.json(),
        serde_json::json!({
            "error": { "code": "out_of_stock", "message": "no items left", "field": null }
        })
    );

    // Outside the API and admin paths an `AdminError` stays plain text.
    let reply = fetch(&app, "GET", "/shop/untyped", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.content_type.starts_with("text/plain"));
    assert_eq!(reply.body, "no items left");
}

#[tokio::test]
async fn everything_under_the_api_path_is_json() {
    let app = app().await;

    // A handler of the application that returned the plain error type.
    let reply = fetch(&app, "GET", "/api/shop/untyped", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "out_of_stock");

    // No route at all.
    let reply = fetch(&app, "GET", "/api/a/b/c/d", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"]["code"], "not_found");

    // A route, but not for this method; the Allow header survives.
    let reply = fetch(&app, "DELETE", "/api/note", None).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(reply.json()["error"]["code"], "method_not_allowed");
    let allow = reply.allow.expect("405 carries Allow");
    assert!(allow.contains("GET") && allow.contains("POST"), "{allow}");

    // Errors the API already rendered are passed through unchanged.
    let reply = fetch(&app, "GET", "/api/note", None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn admin_errors_are_pages() {
    let app = app().await;
    let cookie = login(&app).await;

    // Unknown model: the handler's own 404.
    let reply = fetch(&app, "GET", "/admin/nothing", Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.is_page(), "{}", reply.body);
    assert!(reply.body.contains("Not Found") && reply.body.contains("Error 404"));
    // Signed in, so the page has the navigation and the log-out form.
    assert!(reply.body.contains("Notes") && reply.body.contains("Log out"));

    // Unknown record.
    let reply = fetch(&app, "GET", "/admin/note/999", Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.is_page());

    // No route at all, and nobody signed in: still a page, without the navigation.
    let reply = fetch(&app, "GET", "/admin/a/b/c/d", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.is_page());
    assert!(!reply.body.contains("Log out"));

    // Wrong method.
    let reply = fetch(&app, "DELETE", "/admin/note", Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(reply.is_page());
    assert!(reply.allow.is_some());

    // A write without the CSRF token.
    let request = Request::post("/admin/note/new")
        .header(header::COOKIE, &cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("text=hello"))
        .unwrap();
    let reply = send(&app, request).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.is_page() && reply.body.contains("CSRF"));

    // A route of the application mounted under the admin path.
    let reply = fetch(&app, "GET", "/admin/shop/extra/untyped", Some(&cookie)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.is_page() && reply.body.contains("no items left"));
}

#[tokio::test]
async fn panics_become_500s_and_the_server_keeps_answering() {
    let app = app().await;

    let reply = fetch(&app, "GET", "/shop/boom", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "internal error");

    let reply = fetch(&app, "GET", "/api/shop/boom", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.json()["error"]["code"], "internal");
    assert!(!reply.body.contains("blew up"));

    let reply = fetch(&app, "GET", "/admin/shop/extra/boom", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.is_page());
    assert!(!reply.body.contains("blew up"));

    let reply = fetch(&app, "GET", "/shop/typed", None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn internal_details_are_not_sent_and_other_urls_are_left_alone() {
    let app = app().await;

    let reply = fetch(&app, "GET", "/shop/internal", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body, "internal error");

    // Not ours: axum's own empty 404, for the application to style as it likes.
    let reply = fetch(&app, "GET", "/somewhere/else", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, "");

    // A prefix that merely starts with the same letters is not the API.
    let reply = fetch(&app, "GET", "/apiary", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, "");
}

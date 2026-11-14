//! End-to-end tests against an in-memory SQLite database, driving the router directly.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use cadmus::{
    Admin, AdminError, AdminUser, App, AuthConfig, EnvUserStore, ModelOptions, SeaOrmResource,
    UiSettings, change_password, create_superuser, hash_password,
};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend};
use tower::ServiceExt;

mod common;

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
        pub rating: Option<f64>,
        pub created_at: Option<DateTime>,
        pub secret: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
// RFC 6238 test secret, base32.
const TOTP_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

async fn database() -> DatabaseConnection {
    let db = common::database().await;
    // Hand-written rather than migrated, so the tests also cover a table the app did not
    // create, with a database-side default the entity does not know about.
    let (id, real, timestamp, secret) = match db.get_database_backend() {
        DbBackend::Postgres => (
            "SERIAL PRIMARY KEY",
            "DOUBLE PRECISION",
            "TIMESTAMP",
            "TEXT",
        ),
        // MySQL allows a plain default only on a VARCHAR, not on TEXT.
        DbBackend::MySql => (
            "INTEGER PRIMARY KEY AUTO_INCREMENT",
            "DOUBLE",
            "DATETIME",
            "VARCHAR(255)",
        ),
        _ => (
            "INTEGER PRIMARY KEY AUTOINCREMENT",
            "DOUBLE",
            "DATETIME",
            "TEXT",
        ),
    };
    db.execute_unprepared(&format!(
        "CREATE TABLE post (
            id {id},
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            published BOOLEAN NOT NULL,
            views INTEGER NOT NULL,
            rating {real},
            created_at {timestamp},
            secret {secret} NOT NULL DEFAULT 'hunter2-default'
        )"
    ))
    .await
    .unwrap();
    db
}

fn users(totp: bool) -> EnvUserStore {
    EnvUserStore::new(AdminUser {
        username: "root".into(),
        password_hash: hash_password(PASSWORD).unwrap(),
        totp_secret: totp.then(|| TOTP_SECRET.to_owned()),
    })
    .unwrap()
}

async fn app_with(settings: UiSettings, auth: AuthConfig, totp: bool) -> Router {
    let db = database().await;
    let posts = SeaOrmResource::<post::Entity>::new(db)
        .options(ModelOptions::new().label("Posts").hidden_fields(["secret"]));
    Admin::builder()
        .settings(settings)
        .auth(auth, users(totp))
        .register(posts)
        .build()
        .unwrap()
        .router()
}

/// Obtains a bearer token through the token endpoint.
async fn bearer(app: &Router) -> String {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"username":"root","password":"{PASSWORD}"}}"#
        )))
        .unwrap();
    let (response, body) = send(app, request).await;
    assert_eq!(response.status(), StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["token"].as_str().unwrap().to_owned()
}

/// Sends a JSON API request with a bearer token; returns status, headers and parsed body.
async fn api(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (Response<()>, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
        .unwrap();
    let (response, text) = send(app, request).await;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (response, json)
}

async fn api_get(app: &Router, path: &str) -> (Response<()>, String) {
    let token = bearer(app).await;
    let request = Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

async fn app() -> Router {
    app_with(UiSettings::new(), AuthConfig::new(SECRET).unwrap(), false).await
}

async fn send(app: &Router, request: Request<Body>) -> (Response<()>, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        Response::from_parts(parts, ()),
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::get(path);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn post_form(path: &str, cookie: Option<&str>, body: &str) -> Request<Body> {
    let mut builder =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

/// Logs in and returns `(cookie header value, csrf token)`.
async fn login(app: &Router) -> (String, String) {
    let body = format!("username=root&password={}", PASSWORD.replace(' ', "+"));
    let (response, _) = send(app, post_form("/admin/login", None, &body)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set_cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_owned();

    let (_, html) = send(app, get("/admin/post/new", Some(&cookie))).await;
    let marker = "name=\"_csrf\" value=\"";
    let start = html.find(marker).expect("form carries a CSRF token") + marker.len();
    let csrf = html[start..].split('"').next().unwrap().to_owned();
    (cookie, csrf)
}

async fn create_post(
    app: &Router,
    cookie: &str,
    csrf: &str,
    title: &str,
    views: &str,
) -> StatusCode {
    let body =
        format!("_csrf={csrf}&title={title}&body=hello&published=true&views={views}&rating=");
    send(app, post_form("/admin/post/new", Some(cookie), &body))
        .await
        .0
        .status()
}

fn totp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"12345678901234567890").unwrap();
    mac.update(&(secs / 30).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let chunk: [u8; 4] = digest[offset..offset + 4].try_into().unwrap();
    format!(
        "{:06}",
        (u32::from_be_bytes(chunk) & 0x7fff_ffff) % 1_000_000
    )
}

#[tokio::test]
async fn anonymous_requests_are_turned_away() {
    let app = app().await;
    for path in [
        "/admin",
        "/admin/",
        "/admin/post",
        "/admin/post/new",
        "/admin/post/1",
    ] {
        let (response, body) = send(&app, get(path, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{path}");
        assert_eq!(
            response.headers()[header::LOCATION],
            "/admin/login",
            "{path}"
        );
        assert!(body.is_empty(), "{path} leaked a body");
    }
    let (response, _) = send(&app, get("/api/post", None)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Writes without a session must not reach the database either.
    let (response, _) = send(
        &app,
        post_form("/admin/post/new", None, "title=x&body=y&views=1"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, json) = api_get(&app, "/api/post").await;
    assert!(json.contains("\"total\":0"), "{json}");
}

#[tokio::test]
async fn wrong_credentials_are_rejected() {
    let app = app().await;
    for body in [
        "username=root&password=wrong",
        "username=nobody&password=whatever",
    ] {
        let (response, html) = send(&app, post_form("/admin/login", None, body)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(html.contains("Invalid credentials."));
    }
    let (response, _) = send(
        &app,
        get("/admin/post", Some("admin_token=forged.token.value")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn crud_round_trip() {
    let app = app().await;
    let (cookie, csrf) = login(&app).await;

    assert_eq!(
        create_post(&app, &cookie, &csrf, "First+post", "7").await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        create_post(&app, &cookie, &csrf, "Second", "9").await,
        StatusCode::SEE_OTHER
    );

    let (response, html) = send(&app, get("/admin/post", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        html.contains("First post") && html.contains("Second"),
        "{html}"
    );
    assert!(html.contains("<span class=\"num\">2</span> rows"));

    let (_, html) = send(&app, get("/admin/post?q=Seco", Some(&cookie))).await;
    assert!(html.contains("Second") && !html.contains("First post"));
    assert!(html.contains("<span class=\"num\">1</span> row matching"));

    let (response, html) = send(&app, get("/admin/post/1", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(html.contains("value=\"First post\""), "{html}");

    // Unchecked checkbox (no `published` key) must store false.
    let body = format!("_csrf={csrf}&title=Renamed&body=changed&views=8&rating=4.5");
    let (response, _) = send(&app, post_form("/admin/post/1", Some(&cookie), &body)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, json) = api_get(&app, "/api/post/1").await;
    let row: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(row["title"], "Renamed");
    assert_eq!(row["views"], 8);
    assert_eq!(row["rating"], 4.5);
    assert!(row["published"] == false || row["published"] == 0, "{row}");

    let body = format!("_csrf={csrf}");
    let (response, _) = send(
        &app,
        post_form("/admin/post/1/delete", Some(&cookie), &body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (response, _) = send(&app, get("/admin/post/1", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let (response, _) = send(
        &app,
        post_form("/admin/post/1/delete", Some(&cookie), &body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn writes_without_a_valid_csrf_token_are_refused() {
    let app = app().await;
    let (cookie, csrf) = login(&app).await;
    assert_eq!(
        create_post(&app, &cookie, &csrf, "Keep", "1").await,
        StatusCode::SEE_OTHER
    );

    assert_eq!(
        create_post(&app, &cookie, "wrong", "Evil", "1").await,
        StatusCode::FORBIDDEN
    );
    let (response, _) = send(
        &app,
        post_form("/admin/post/1", Some(&cookie), "title=Evil&body=x&views=1"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let (response, _) = send(&app, post_form("/admin/post/1/delete", Some(&cookie), "")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let (_, json) = api_get(&app, "/api/post").await;
    assert!(
        json.contains("\"total\":1") && json.contains("Keep") && !json.contains("Evil"),
        "{json}"
    );
}

#[tokio::test]
async fn invalid_input_is_reported_and_not_saved() {
    let app = app().await;
    let (cookie, csrf) = login(&app).await;

    let body = format!("_csrf={csrf}&title=Bad&body=x&views=not-a-number");
    let (response, html) = send(&app, post_form("/admin/post/new", Some(&cookie), &body)).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("invalid value for `views`"), "{html}");
    // The form comes back as typed.
    assert!(html.contains("value=\"Bad\""));

    // Required non-text field left empty.
    let body = format!("_csrf={csrf}&title=Bad&body=x&views=");
    let (response, _) = send(&app, post_form("/admin/post/new", Some(&cookie), &body)).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let (_, json) = api_get(&app, "/api/post").await;
    assert!(json.contains("\"total\":0"), "{json}");

    let (response, _) = send(&app, get("/admin/nope", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let (response, _) = send(&app, get("/admin/post/abc", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hidden_fields_never_leave_the_server_and_html_is_escaped() {
    let app = app().await;
    let (cookie, csrf) = login(&app).await;
    let title = "%3Cscript%3Ealert(1)%3C%2Fscript%3E";
    assert_eq!(
        create_post(&app, &cookie, &csrf, title, "1").await,
        StatusCode::SEE_OTHER
    );

    for path in ["/admin/post", "/admin/post/1", "/api/post", "/api/post/1"] {
        let (response, body) = if path.starts_with("/api") {
            api_get(&app, path).await
        } else {
            send(&app, get(path, Some(&cookie))).await
        };
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(
            !body.contains("hunter2-default"),
            "{path} leaked the hidden column"
        );
        assert!(
            !body.to_lowercase().contains("secret"),
            "{path} mentions the hidden column"
        );
    }
    let (_, html) = send(&app, get("/admin/post", Some(&cookie))).await;
    assert!(
        !html.contains("<script>alert(1)"),
        "stored markup was rendered unescaped"
    );
    assert!(html.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn bearer_tokens_work_for_the_api_and_tampering_is_rejected() {
    let app = app().await;
    let request = |body: String| {
        Request::post("/api/auth/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let (response, _) = send(
        &app,
        request(r#"{"username":"root","password":"wrong"}"#.to_owned()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (response, body) = send(
        &app,
        request(format!(r#"{{"username":"root","password":"{PASSWORD}"}}"#)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = json["token"].as_str().unwrap();
    assert_eq!(json["expires_in"], 3600);

    let with = |token: &str| {
        Request::get("/api/post")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&app, with(token)).await.0.status(), StatusCode::OK);

    let mut tampered = token.to_owned();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(
        send(&app, with(&tampered)).await.0.status(),
        StatusCode::UNAUTHORIZED
    );

    // A token signed with a different secret is worthless here.
    let foreign = app_with(
        UiSettings::new(),
        AuthConfig::new("f".repeat(32)).unwrap(),
        false,
    )
    .await;
    assert_eq!(
        send(&foreign, with(token)).await.0.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn totp_is_enforced_when_the_user_has_a_secret() {
    let app = app_with(UiSettings::new(), AuthConfig::new(SECRET).unwrap(), true).await;
    let password = PASSWORD.replace(' ', "+");

    let body = format!("username=root&password={password}");
    assert_eq!(
        send(&app, post_form("/admin/login", None, &body))
            .await
            .0
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let body = format!("username=root&password={password}&totp=000000x");
    assert_eq!(
        send(&app, post_form("/admin/login", None, &body))
            .await
            .0
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // Right code, wrong password.
    let body = format!("username=root&password=wrong&totp={}", totp_now());
    assert_eq!(
        send(&app, post_form("/admin/login", None, &body))
            .await
            .0
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let body = format!("username=root&password={password}&totp={}", totp_now());
    assert_eq!(
        send(&app, post_form("/admin/login", None, &body))
            .await
            .0
            .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn totp_required_refuses_users_without_a_second_factor() {
    let auth = AuthConfig::new(SECRET).unwrap().totp_required(true);
    let app = app_with(UiSettings::new(), auth, false).await;
    let body = format!("username=root&password={}", PASSWORD.replace(' ', "+"));
    let (response, _) = send(&app, post_form("/admin/login", None, &body)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ui_settings_from_env_style_lookup_change_the_page() {
    let dir = std::env::temp_dir().join(format!("cadmus-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "{% extends \"base.html\" %}{% block content %}CUSTOM INDEX for {{ user }}{% endblock %}",
    )
    .unwrap();
    let dir_text = dir.to_string_lossy().into_owned();

    let settings = UiSettings::from_lookup(|key| match key {
        "ADMIN_SITE_TITLE" => Some("Acme Back Office".into()),
        "ADMIN_ACCENT_COLOR" => Some("#0a7f5a".into()),
        "ADMIN_THEME" => Some("dark".into()),
        "ADMIN_FOOTER_TEXT" => Some("Internal use only".into()),
        "ADMIN_NAV_LINKS" => Some("Docs=/docs, Status=https://status.example".into()),
        "ADMIN_CUSTOM_CSS_URL" => Some("/static/admin.css".into()),
        "ADMIN_TEMPLATE_DIR" => Some(dir_text.clone()),
        _ => None,
    })
    .unwrap();
    let app = app_with(settings, AuthConfig::new(SECRET).unwrap(), false).await;
    let (cookie, _) = login(&app).await;

    let (_, html) = send(&app, get("/admin/", Some(&cookie))).await;
    // The template engine writes `/` in attribute values as an entity; browsers decode it.
    let html = html.replace("&#x2f;", "/");
    assert!(html.contains("Acme Back Office"));
    assert!(html.contains("--accent: #0a7f5a"));
    assert!(html.contains("data-theme=\"dark\""));
    assert!(html.contains("Internal use only"));
    assert!(html.contains("href=\"/docs\"") && html.contains("https://status.example"));
    assert!(html.contains("href=\"/static/admin.css\""));
    assert!(
        html.contains("CUSTOM INDEX for root"),
        "template override was not used: {html}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_configuration_is_refused() {
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    // A value that would break out of the stylesheet.
    assert!(
        UiSettings::from_lookup(lookup(&[(
            "ADMIN_ACCENT_COLOR",
            "red; } body { display:none"
        )]))
        .is_err()
    );
    assert!(UiSettings::from_lookup(lookup(&[("ADMIN_THEME", "neon")])).is_err());
    assert!(UiSettings::from_lookup(lookup(&[("ADMIN_PAGE_SIZE", "0")])).is_err());

    assert!(AuthConfig::from_lookup(lookup(&[])).is_err());
    assert!(AuthConfig::from_lookup(lookup(&[("ADMIN_JWT_SECRET", "short")])).is_err());
    assert!(EnvUserStore::from_lookup(lookup(&[("ADMIN_USERNAME", "root")])).is_err());
    assert!(
        EnvUserStore::from_lookup(lookup(&[
            ("ADMIN_USERNAME", "root"),
            ("ADMIN_PASSWORD_HASH", "plaintext")
        ]))
        .is_err()
    );

    // No auth configured: refuse to build rather than serve an open admin.
    assert!(Admin::builder().build().is_err());
    let auth = || AuthConfig::new(SECRET).unwrap();
    assert!(
        Admin::builder()
            .auth(auth(), users(false))
            .base_path("admin")
            .build()
            .is_err()
    );
    assert!(
        Admin::builder()
            .auth(auth(), users(false))
            .base_path("/")
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn rest_api_full_crud() {
    let app = app().await;
    let token = bearer(&app).await;

    let body = r#"{"title":"Hello","body":"text","published":true,"views":3,"rating":null}"#;
    let (response, row) = api(&app, &token, "POST", "/api/post", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()[header::LOCATION], "/api/post/1");
    assert_eq!(row["id"], 1);
    assert_eq!(row["title"], "Hello");
    assert!(row.get("secret").is_none(), "hidden column returned: {row}");

    let (response, page) = api(&app, &token, "GET", "/api/post?per_page=100000", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(page["meta"]["total"], 1);
    assert_eq!(page["meta"]["per_page"], 100, "per_page must be capped");
    assert_eq!(page["data"][0]["title"], "Hello");

    // PATCH changes only what it names.
    let (response, row) = api(
        &app,
        &token,
        "PATCH",
        "/api/post/1",
        Some(r#"{"views":10}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(row["views"], 10);
    assert_eq!(row["title"], "Hello");
    assert!(row["published"] == true || row["published"] == 1, "{row}");

    // PUT replaces: every required field must be present.
    let (response, error) = api(
        &app,
        &token,
        "PUT",
        "/api/post/1",
        Some(r#"{"title":"Only title"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "invalid_field");
    assert!(error["error"]["field"].is_string());
    let body = r#"{"title":"Replaced","body":"b","published":false,"views":1}"#;
    let (response, row) = api(&app, &token, "PUT", "/api/post/1", Some(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(row["title"], "Replaced");
    assert!(row["rating"].is_null());

    let (response, _) = api(&app, &token, "DELETE", "/api/post/1", None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (response, error) = api(&app, &token, "GET", "/api/post/1", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error["error"]["code"], "not_found");
    let (response, _) = api(&app, &token, "DELETE", "/api/post/1", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rest_api_rejects_bad_writes_and_stores_nothing() {
    let app = app().await;
    let token = bearer(&app).await;
    let valid = r#"{"title":"Keep","body":"b","published":true,"views":1}"#;
    assert_eq!(
        api(&app, &token, "POST", "/api/post", Some(valid))
            .await
            .0
            .status(),
        StatusCode::CREATED
    );

    let cases = [
        // (method, path, body, status, error code)
        (
            "POST",
            "/api/post",
            "not json",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "POST",
            "/api/post",
            "[1,2]",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "POST",
            "/api/post",
            r#"{"title":"x","body":"b","published":true,"views":"many"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "POST",
            "/api/post",
            r#"{"title":"x","body":"b","published":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "POST",
            "/api/post",
            r#"{"title":"x","body":"b","published":"maybe","views":1}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "POST",
            "/api/post",
            r#"{"title":"x","body":"b","published":true,"views":1,"nope":1}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        // The hidden column must look exactly like a column that does not exist.
        (
            "POST",
            "/api/post",
            r#"{"title":"x","body":"b","published":true,"views":1,"secret":"s"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "POST",
            "/api/post",
            r#"{"id":99,"title":"x","body":"b","published":true,"views":1}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "PATCH",
            "/api/post/1",
            r#"{"id":2}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "PATCH",
            "/api/post/1",
            r#"{"title":null}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
        ),
        (
            "PATCH",
            "/api/post/999",
            r#"{"views":2}"#,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "PATCH",
            "/api/nope/1",
            r#"{"views":2}"#,
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ];
    for (method, path, body, status, code) in cases {
        let (response, error) = api(&app, &token, method, path, Some(body)).await;
        assert_eq!(response.status(), status, "{method} {path} {body}");
        assert_eq!(error["error"]["code"], code, "{method} {path} {body}");
    }
    let (_, error) = api(&app, &token, "POST", "/api/post", Some(r#"{"secret":"s"}"#)).await;
    let (_, unknown) = api(&app, &token, "POST", "/api/post", Some(r#"{"nope":"s"}"#)).await;
    assert_eq!(error["error"]["message"], unknown["error"]["message"]);

    let (_, page) = api(&app, &token, "GET", "/api/post", None).await;
    assert_eq!(page["meta"]["total"], 1);
    assert_eq!(page["data"][0]["title"], "Keep");
    assert_eq!(page["data"][0]["views"], 1);
}

#[tokio::test]
async fn rest_api_needs_a_bearer_token() {
    let app = app().await;
    let (cookie, _) = login(&app).await;
    let body = r#"{"title":"x","body":"b","published":true,"views":1}"#;

    for (method, path) in [
        ("GET", "/api/post"),
        ("POST", "/api/post"),
        ("DELETE", "/api/post/1"),
    ] {
        // No credentials at all.
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::from(body))
            .unwrap();
        let (response, text) = send(&app, request).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        assert!(text.contains("\"code\":\"unauthorized\""), "{text}");

        // A browser session cookie is not accepted: the API has no CSRF token.
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &cookie)
            .body(Body::from(body))
            .unwrap();
        let (response, _) = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (_, json) = api_get(&app, "/api/post").await;
    assert!(json.contains("\"total\":0"), "{json}");
}

mod note {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "note")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        #[sea_orm(unique)]
        pub text: String,
        pub done: bool,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[tokio::test]
async fn app_migrates_registered_models_and_uses_database_accounts() {
    let db = common::database().await;

    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<note::Entity>(ModelOptions::new().label("Notes"));
    app.migrate().await.unwrap();
    // Running it again must be a no-op, not an error.
    app.migrate().await.unwrap();
    let router = app.router().unwrap();

    // No accounts yet: nobody gets in.
    let token_request = |password: &str| {
        Request::post("/api/auth/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"username":"root","password":"{password}"}}"#
            )))
            .unwrap()
    };
    assert_eq!(
        send(&router, token_request(PASSWORD)).await.0.status(),
        StatusCode::UNAUTHORIZED
    );

    assert!(matches!(
        create_superuser(&db, "root", "short").await,
        Err(AdminError::BadInput { .. })
    ));
    assert!(matches!(
        create_superuser(&db, "bad name!", PASSWORD).await,
        Err(AdminError::BadInput { .. })
    ));
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    assert!(matches!(
        create_superuser(&db, "root", PASSWORD).await,
        Err(AdminError::BadInput { .. })
    ));

    assert_eq!(
        send(&router, token_request("wrong password"))
            .await
            .0
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let token = bearer(&router).await;

    let (response, row) = api(
        &router,
        &token,
        "POST",
        "/api/note",
        Some(r#"{"text":"buy milk","done":false}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED, "{row}");
    assert_eq!(row["text"], "buy milk");
    // The entity declares `text` unique; migrate must have created that constraint.
    let (response, error) = api(
        &router,
        &token,
        "POST",
        "/api/note",
        Some(r#"{"text":"buy milk","done":true}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "conflict");

    // The model is in the admin too.
    let body = format!("username=root&password={}", PASSWORD.replace(' ', "+"));
    let (response, _) = send(&router, post_form("/admin/login", None, &body)).await;
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let (response, html) = send(&router, get("/admin/note", Some(&cookie))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(html.contains("buy milk") && html.contains("Notes"));

    change_password(&db, "root", "a brand new password")
        .await
        .unwrap();
    assert_eq!(
        send(&router, token_request(PASSWORD)).await.0.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&router, token_request("a brand new password"))
            .await
            .0
            .status(),
        StatusCode::OK
    );
    assert!(matches!(
        change_password(&db, "nobody", "a brand new password").await,
        Err(AdminError::NotFound(_))
    ));
}

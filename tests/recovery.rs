//! Two-factor recovery codes: each stands in for an authenticator code once.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use heraapi::{App, AuthConfig, create_superuser, disable_totp, enable_totp, new_recovery_codes};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

async fn site() -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().cookie_secure(false),
    );
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    (app.router().unwrap(), db)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix(&format!("{name}=")))
            .map(|rest| rest.split(';').next().unwrap_or_default().to_owned())
            .filter(|value| !value.is_empty())
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn form(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body.to_owned())).unwrap()).await
}

async fn get(app: &Router, path: &str, cookie: &str) -> Reply {
    let request = Request::get(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    send(app, request).await
}

/// A password login with `code` in the authenticator field; the session cookie if it worked.
async fn log_in(app: &Router, code: &str) -> Option<String> {
    let body = format!(
        "username=root&password={}&totp={}",
        PASSWORD.replace(' ', "+"),
        url::form_urlencoded::byte_serialize(code.as_bytes()).collect::<String>()
    );
    form(app, "/admin/login", &body, None)
        .await
        .cookie("admin_token")
        .map(|token| format!("admin_token={token}"))
}

fn totp(secret: &str) -> String {
    let key = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).unwrap();
    mac.update(&(now / 30).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let chunk: [u8; 4] = digest[offset..offset + 4].try_into().unwrap();
    format!(
        "{:06}",
        (u32::from_be_bytes(chunk) & 0x7fff_ffff) % 1_000_000
    )
}

fn csrf(page: &str) -> String {
    page.split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn each_recovery_code_logs_in_once() {
    let (app, db) = site().await;
    enable_totp(&db, "root").await.unwrap();
    let codes = new_recovery_codes(&db, "root").await.unwrap();
    assert_eq!(codes.len(), 10);

    assert!(
        log_in(&app, "").await.is_none(),
        "the second factor is needed"
    );
    let session = log_in(&app, &codes[0])
        .await
        .expect("a recovery code works");
    assert!(log_in(&app, &codes[0]).await.is_none(), "but only once");
    // However it is typed.
    assert!(
        log_in(&app, &codes[1].to_uppercase().replace('-', " "))
            .await
            .is_some()
    );
    assert!(log_in(&app, "aaaa-bbbb-cccc-dddd").await.is_none());

    let page = get(&app, "/admin/auth/two-factor", &session).await;
    assert!(page.body.contains("8 of 10 left"), "{}", page.body);

    // The API's token endpoint takes them in the same field.
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"username": "root", "password": PASSWORD, "totp": codes[2]}).to_string(),
        ))
        .unwrap();
    let reply = send(&app, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let token: Value = serde_json::from_str(&reply.body).unwrap();
    assert!(token["access_token"].is_string());
}

#[tokio::test]
async fn new_codes_replace_the_old_and_two_factor_changes_retire_them() {
    let (app, db) = site().await;
    let secret = enable_totp(&db, "root").await.unwrap();
    let old = new_recovery_codes(&db, "root").await.unwrap();
    let session = log_in(&app, &old[0]).await.unwrap();
    let page = get(&app, "/admin/auth/two-factor", &session).await;
    let token = csrf(&page.body);

    let wrong = form(
        &app,
        "/admin/auth/two-factor/recovery",
        &format!("_csrf={token}&code=000000"),
        Some(&session),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);

    let made = form(
        &app,
        "/admin/auth/two-factor/recovery",
        &format!("_csrf={token}&code={}", totp(&secret)),
        Some(&session),
    )
    .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert!(made.body.contains("Save your recovery codes"));
    let fresh: Vec<String> = made
        .body
        .split("<li><code>")
        .skip(1)
        .map(|rest| rest.split('<').next().unwrap().to_owned())
        .collect();
    assert_eq!(fresh.len(), 10);
    assert!(
        log_in(&app, &old[1]).await.is_none(),
        "old codes stop working"
    );
    assert!(log_in(&app, &fresh[0]).await.is_some());

    // Turning two-factor off, or setting it up again, retires every code.
    disable_totp(&db, "root").await.unwrap();
    enable_totp(&db, "root").await.unwrap();
    assert!(log_in(&app, &fresh[1]).await.is_none());
}

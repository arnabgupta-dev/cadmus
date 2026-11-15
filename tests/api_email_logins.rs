//! E-mailed links for apps: login links, sign-up confirmations and password resets that
//! point to the app, which sends the token back to the API.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use cadmus::settings::*;
use cadmus::{AdminError, App, Email, Mailer, create_superuser, enable_totp, set_email};
use http_body_util::BodyExt;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery";
const APP: &str = "https://app.example.com";

/// Keeps every message instead of sending it.
#[derive(Clone, Default)]
struct Outbox(Arc<Mutex<Vec<Email>>>);

#[async_trait]
impl Mailer for Outbox {
    async fn send(&self, email: Email) -> Result<(), AdminError> {
        self.0.lock().unwrap().push(email);
        Ok(())
    }
}

impl Outbox {
    /// Waits for the `n`th message (messages are sent in the background).
    async fn nth(&self, n: usize) -> Email {
        for _ in 0..100 {
            if let Some(email) = self.0.lock().unwrap().get(n) {
                return email.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no message number {n} was sent");
    }

    async fn count_after_a_moment(&self) -> usize {
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.0.lock().unwrap().len()
    }
}

/// The token in the app link of a message, checking the link points to the app.
fn token_in(email: &Email, path: &str) -> String {
    let start = email
        .text
        .find(&format!("{APP}{path}?token="))
        .unwrap_or_else(|| panic!("no {path} link in: {}", email.text));
    let rest = &email.text[start + APP.len() + path.len() + "?token=".len()..];
    rest.split_whitespace().next().unwrap().to_owned()
}

struct Site {
    app: Router,
    db: DatabaseConnection,
    outbox: Outbox,
}

async fn site(configure: impl FnOnce(&mut Settings)) -> Site {
    let mut settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            methods: vec![AuthMethod::Password, AuthMethod::MagicLink],
            signup: Signup::Domains(vec!["acme.com".into()]),
            app: AppSettings {
                url: Some(format!("{APP}/")),
                ..Default::default()
            },
            cookie_secure: false,
            ..Default::default()
        },
        ..Default::default()
    };
    configure(&mut settings);
    let db = common::database().await;
    let outbox = Outbox::default();
    let app = App::with_settings(db.clone(), settings)
        .unwrap()
        .mailer(outbox.clone());
    app.migrate().await.unwrap();
    create_superuser(&db, "ada", PASSWORD).await.unwrap();
    set_email(&db, "ada", "ada@acme.com").await.unwrap();
    Site {
        app: app.router().unwrap(),
        db,
        outbox,
    }
}

impl Site {
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn me(&self, reply: &Value) -> Value {
        let token = reply["access_token"].as_str().expect("an access token");
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get("/api/auth/me")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

#[tokio::test]
async fn a_login_link_points_to_the_app_and_works_once() {
    let site = site(|_| {}).await;
    let (status, _) = site
        .post("/api/auth/link", json!({"email": "Ada@Acme.com"}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let email = site.outbox.nth(0).await;
    assert_eq!(email.to, "ada@acme.com");
    let token = token_in(&email, "/login/link");

    let (status, reply) = site
        .post("/api/auth/link/verify", json!({"token": token}))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(site.me(&reply).await["username"], "ada");
    let (status, reply) = site
        .post("/api/auth/link/verify", json!({"token": token}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(reply["error"]["code"], "invalid_link");

    // The same answer for an address without an account, and no message.
    let (status, _) = site
        .post("/api/auth/link", json!({"email": "nobody@elsewhere.com"}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(site.outbox.count_after_a_moment().await, 1);

    // The admin's links and the app's are not interchangeable.
    let (status, _) = site
        .post("/api/auth/link/verify", json!({"token": "nonsense"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_login_link_signs_up_allowed_addresses_and_honours_two_factor() {
    let site = site(|_| {}).await;
    site.post("/api/auth/link", json!({"email": "bea@acme.com"}))
        .await;
    let token = token_in(&site.outbox.nth(0).await, "/login/link");
    let (status, reply) = site
        .post("/api/auth/link/verify", json!({"token": token}))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let me = site.me(&reply).await;
    assert_eq!(me["username"], "bea@acme.com");
    assert_eq!(me["staff"], false);

    enable_totp(&site.db, "ada").await.unwrap();
    site.post("/api/auth/link", json!({"email": "ada@acme.com"}))
        .await;
    let token = token_in(&site.outbox.nth(1).await, "/login/link");
    let (status, reply) = site
        .post("/api/auth/link/verify", json!({"token": token}))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply["error"]["code"], "totp_required");
}

#[tokio::test]
async fn signing_up_confirms_the_address_before_the_account_exists() {
    let site = site(|_| {}).await;
    let (status, reply) = site
        .post("/api/auth/register", json!({"email": "eve@elsewhere.com"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(reply["error"]["code"], "signup_closed");

    let (status, _) = site
        .post("/api/auth/register", json!({"email": "cal@acme.com"}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let token = token_in(&site.outbox.nth(0).await, "/register/verify");
    assert!(account(&site.db, "cal@acme.com").await.is_none());

    let (status, reply) = site
        .post(
            "/api/auth/register/verify",
            json!({"token": token, "password": "short"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert!(account(&site.db, "cal@acme.com").await.is_none());

    let (status, reply) = site
        .post(
            "/api/auth/register/verify",
            json!({"token": token, "password": "a long enough password"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let created = account(&site.db, "cal@acme.com").await.unwrap();
    assert!(!created.is_staff && created.is_active);
    // The password works, and the link does not work twice.
    let (status, _) = site
        .post(
            "/api/auth/token",
            json!({"username": "cal@acme.com", "password": "a long enough password"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = site
        .post(
            "/api/auth/register/verify",
            json!({"token": token, "password": "another long password"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An address that already has an account gets the same answer, and no message.
    let (status, _) = site
        .post("/api/auth/register", json!({"email": "ada@acme.com"}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(site.outbox.count_after_a_moment().await, 1);
}

#[tokio::test]
async fn a_password_reset_link_works_until_the_password_changes() {
    let site = site(|_| {}).await;
    site.post(
        "/api/auth/password/forgot",
        json!({"email": "ada@acme.com"}),
    )
    .await;
    let token = token_in(&site.outbox.nth(0).await, "/password/reset");
    let (status, _) = site
        .post(
            "/api/auth/password/reset",
            json!({"token": token, "password": "brand new password"}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = site
        .post(
            "/api/auth/token",
            json!({"username": "ada", "password": PASSWORD}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = site
        .post(
            "/api/auth/token",
            json!({"username": "ada", "password": "brand new password"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, reply) = site
        .post(
            "/api/auth/password/reset",
            json!({"token": token, "password": "third password here"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(reply["error"]["code"], "invalid_link");
}

#[tokio::test]
async fn the_links_are_off_until_the_app_has_an_address() {
    let site = site(|settings| settings.auth.app.url = None).await;
    for path in [
        "/api/auth/link",
        "/api/auth/register",
        "/api/auth/password/forgot",
    ] {
        let (status, _) = site.post(path, json!({"email": "ada@acme.com"})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(site.outbox.count_after_a_moment().await, 0);
}

#[tokio::test]
async fn link_templates_must_carry_the_token() {
    let db = common::database().await;
    let settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            app: AppSettings {
                url: Some(APP.into()),
                links: AppLinks {
                    login: "/login".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let error = App::with_settings(db, settings).err().unwrap().to_string();
    assert!(error.contains("auth.app.links.login"), "{error}");
}

async fn account(db: &DatabaseConnection, username: &str) -> Option<cadmus::user::Model> {
    cadmus::user::Entity::find()
        .filter(cadmus::user::Column::Username.eq(username))
        .one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_api_documentation_lists_the_links_for_apps() {
    let site = site(|_| {}).await;
    let response = site
        .app
        .clone()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    for path in [
        "link",
        "link/verify",
        "register",
        "register/verify",
        "password/forgot",
        "password/reset",
    ] {
        assert!(
            doc["paths"][format!("/api/auth/{path}")]["post"].is_object(),
            "{path}"
        );
    }
}

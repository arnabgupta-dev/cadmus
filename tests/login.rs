//! Login hardening: attempt limits, and setting up, using and resetting two-factor
//! authentication.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use heraapi::{
    App, AuthConfig, ModelOptions, create_superuser, disable_totp, enable_totp, totp_uri,
    verify_totp,
};
use hmac::{Hmac, KeyInit, Mac};
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

async fn app_with(auth: AuthConfig) -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = App::new(db.clone(), auth).model::<note::Entity>(ModelOptions::new());
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    create_superuser(&db, "other", PASSWORD).await.unwrap();
    (app.router().unwrap(), db)
}

fn auth() -> AuthConfig {
    AuthConfig::new(SECRET).unwrap()
}

struct Reply {
    status: StatusCode,
    retry_after: Option<u64>,
    cookie: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let text = |name| {
        response
            .headers()
            .get(name)
            .map(|v: &header::HeaderValue| v.to_str().unwrap().to_owned())
    };
    let retry_after = text(header::RETRY_AFTER).map(|v| v.parse().unwrap());
    let cookie = text(header::SET_COOKIE).map(|v| v.split(';').next().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        retry_after,
        cookie,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// Asks the API for a token, optionally with an authenticator code and a client address
/// header.
async fn token(
    app: &Router,
    username: &str,
    password: &str,
    code: Option<&str>,
    forwarded_for: Option<&str>,
) -> Reply {
    let mut request =
        Request::post("/api/auth/token").header(header::CONTENT_TYPE, "application/json");
    if let Some(address) = forwarded_for {
        request = request.header("x-forwarded-for", address);
    }
    let body = json!({ "username": username, "password": password, "totp": code });
    send(app, request.body(Body::from(body.to_string())).unwrap()).await
}

/// Logs in through the admin form.
async fn form_login(app: &Router, username: &str, password: &str, code: Option<&str>) -> Reply {
    let request = Request::post("/admin/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "username={username}&password={}&totp={}",
            password.replace(' ', "+"),
            code.unwrap_or_default()
        )))
        .unwrap();
    send(app, request).await
}

/// A logged-in admin session.
struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn login(app: &Router, username: &str, code: Option<&str>) -> Self {
        let reply = form_login(app, username, PASSWORD, code).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let mut browser = Self {
            cookie: reply.cookie.unwrap(),
            csrf: String::new(),
        };
        let home = browser.get(app, "/admin/").await;
        browser.csrf = field(&home.body, "_csrf");
        browser
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        let request = Request::get(path)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        send(app, request).await
    }

    async fn post(&self, app: &Router, path: &str, fields: &str) -> Reply {
        let request = Request::post(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("_csrf={}&{fields}", self.csrf)))
            .unwrap();
        send(app, request).await
    }
}

/// The value of the hidden form field `name` in a page.
fn field(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no field {name}"))
        + marker.len();
    page[start..].split('"').next().unwrap().to_owned()
}

/// What an authenticator app shows for `secret`, `steps_ahead` 30-second steps from now.
fn code(secret: &str, steps_ahead: u64) -> String {
    let key = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).unwrap();
    mac.update(&(now / 30 + steps_ahead).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let chunk: [u8; 4] = digest[offset..offset + 4].try_into().unwrap();
    format!(
        "{:06}",
        (u32::from_be_bytes(chunk) & 0x7fff_ffff) % 1_000_000
    )
}

fn password_field() -> String {
    format!("password={}", PASSWORD.replace(' ', "+"))
}

#[tokio::test]
async fn an_account_is_locked_after_too_many_attempts() {
    let (app, _db) = app_with(auth().login_limit(3, 120)).await;

    for _ in 0..3 {
        let reply = token(&app, "root", "wrong", None, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // Locked: even the right password is refused, by the API and by the form.
    let reply = token(&app, "root", PASSWORD, None, None).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.json()["error"]["code"], "rate_limited");
    let wait = reply.retry_after.expect("Retry-After");
    assert!((1..=120).contains(&wait), "{wait}");

    let reply = form_login(&app, "root", PASSWORD, None).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.retry_after.is_some());
    assert!(reply.body.contains("Too many attempts") && reply.body.contains("name=\"password\""));
    assert!(reply.cookie.is_none());

    // Another account is not affected, and neither is a name that does not exist until it
    // has used up its own attempts: the answer never says which names are real.
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::OK
    );
    for expected in [401, 401, 401, 429] {
        assert_eq!(token(&app, "ghost", "x", None, None).await.status, expected);
    }
}

#[tokio::test]
async fn a_successful_login_clears_the_count_and_the_limit_can_be_turned_off() {
    let (app, _db) = app_with(auth().login_limit(3, 120)).await;
    for _ in 0..4 {
        // Two typos and then the right password, over and over, never locks.
        for _ in 0..2 {
            assert_eq!(
                token(&app, "root", "typo", None, None).await.status,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            token(&app, "root", PASSWORD, None, None).await.status,
            StatusCode::OK
        );
    }

    let (app, _db) = app_with(auth().login_limit(0, 120)).await;
    for _ in 0..12 {
        assert_eq!(
            token(&app, "root", "wrong", None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_address_is_limited_across_accounts() {
    let config = auth()
        .login_limit(2, 120)
        .client_ip_header("X-Forwarded-For")
        .unwrap();
    let (app, _db) = app_with(config).await;

    // Eight guesses (2 x 4) on eight names from one address; the proxy's entry is the last.
    for i in 0..8 {
        let reply = token(
            &app,
            &format!("user{i}"),
            "x",
            None,
            Some("203.0.113.9, 198.51.100.7"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let blocked = token(&app, "root", PASSWORD, None, Some("198.51.100.7")).await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    // A different client is fine, and the refused attempt did not count against `root`.
    let reply = token(&app, "root", PASSWORD, None, Some("198.51.100.8")).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Putting another address first does not help: only the proxy's entry counts.
    let spoofed = token(
        &app,
        "root",
        PASSWORD,
        None,
        Some("198.51.100.8, 198.51.100.7"),
    )
    .await;
    assert_eq!(spoofed.status, StatusCode::TOO_MANY_REQUESTS);

    assert!(auth().client_ip_header("not a header").is_err());
}

#[tokio::test]
async fn two_factor_can_be_set_up_used_once_per_code_and_turned_off() {
    let (app, _db) = app_with(auth().login_limit(50, 120)).await;
    let root = Browser::login(&app, "root", None).await;

    // The header links to the page; it offers a key that is not stored yet.
    assert!(
        root.get(&app, "/admin/")
            .await
            .body
            .contains("admin/auth/two-factor")
    );
    let page = root.get(&app, "/admin/auth/two-factor").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<svg") && page.body.contains("Turn on"));
    let secret = field(&page.body, "secret");
    assert!(
        page.body
            .contains(&format!("<code class=\"key\">{secret}</code>"))
    );
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::OK
    );

    // Wrong password, wrong code, tampered key: nothing is turned on, and the same key is
    // offered again so the code already scanned stays valid.
    let path = "/admin/auth/two-factor";
    for (fields, message) in [
        (
            format!("secret={secret}&password=wrong&code={}", code(&secret, 0)),
            "password is not correct",
        ),
        (
            format!("secret={secret}&{}&code=000000", password_field()),
            "code is not correct",
        ),
        (
            format!("secret={secret}&{}&code=", password_field()),
            "code is not correct",
        ),
    ] {
        let reply = root.post(&app, path, &fields).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{fields}");
        assert!(reply.body.contains(message), "{}", reply.body);
        assert_eq!(field(&reply.body, "secret"), secret);
    }
    let weak = root
        .post(
            &app,
            path,
            &format!("secret=AAAA&{}&code=123456", password_field()),
        )
        .await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_ne!(field(&weak.body, "secret"), "AAAA");
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::OK
    );

    // Without the CSRF token nothing happens either.
    let request = Request::post(path)
        .header(header::COOKIE, &root.cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "secret={secret}&{}&code={}",
            password_field(),
            code(&secret, 0)
        )))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::FORBIDDEN);

    // Turn it on.
    let confirm = code(&secret, 0);
    let reply = root
        .post(
            &app,
            path,
            &format!("secret={secret}&{}&code={confirm}", password_field()),
        )
        .await;
    // The answer shows the recovery codes, once.
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("Save your recovery codes"));
    assert_eq!(reply.body.matches("<li><code>").count(), 10);
    let page = root.get(&app, path).await;
    assert!(page.body.contains("<strong>On.</strong>") && !page.body.contains(&secret));
    assert!(!page.body.contains("Save your recovery codes"));
    assert!(page.body.contains("10 of 10 left"));
    assert!(
        root.get(&app, "/admin/auth/users")
            .await
            .body
            .contains("data-label=\"Two-factor\"><span class=\"badge on\">")
    );
    // Turning it on twice is refused.
    let again = root
        .post(
            &app,
            path,
            &format!(
                "secret={secret}&{}&code={}",
                password_field(),
                code(&secret, 1)
            ),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // Logging in now needs a code...
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        token(&app, "root", PASSWORD, Some("000000"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // ...and not the one that was just used to turn it on...
    assert_eq!(
        token(&app, "root", PASSWORD, Some(&confirm), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // ...but the next one works, exactly once, for the API and the form alike.
    let next = code(&secret, 1);
    assert_eq!(
        token(&app, "root", PASSWORD, Some(&next), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        token(&app, "root", PASSWORD, Some(&next), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        form_login(&app, "root", PASSWORD, Some(&next)).await.status,
        StatusCode::UNAUTHORIZED
    );
    // A wrong password does not use a code up.
    // (The account without two-factor is untouched throughout.)
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::OK
    );

    // Turning it off needs the password.
    let reply = root
        .post(&app, "/admin/auth/two-factor/disable", "password=wrong")
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let reply = root
        .post(&app, "/admin/auth/two-factor/disable", &password_field())
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::OK
    );
    assert!(
        root.get(&app, path)
            .await
            .body
            .contains("<strong>Off.</strong>")
    );
}

#[tokio::test]
async fn two_factor_can_be_set_from_the_command_line_and_reset_by_a_superuser() {
    let (app, db) = app_with(auth().login_limit(50, 120)).await;

    // What the `totp` command does.
    let secret = enable_totp(&db, "other").await.unwrap();
    assert!(verify_totp(&secret, &code(&secret, 0), unix_now()));
    let uri = totp_uri("My Site", "other", &secret);
    assert!(
        uri.starts_with("otpauth://totp/My%20Site:other?secret="),
        "{uri}"
    );
    assert!(
        uri.contains(&secret) && uri.ends_with("&issuer=My+Site"),
        "{uri}"
    );
    assert!(enable_totp(&db, "nobody").await.is_err());

    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let first = code(&secret, 0);
    assert_eq!(
        token(&app, "other", PASSWORD, Some(&first), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        token(&app, "other", PASSWORD, Some(&first), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // A code for an earlier step than the last one used is refused too.
    let later = code(&secret, 1);
    assert_eq!(
        token(&app, "other", PASSWORD, Some(&later), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        token(&app, "other", PASSWORD, Some(&first), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    // A superuser resets it from the user form (the device was lost).
    let root = Browser::login(&app, "root", None).await;
    let users = root.get(&app, "/admin/auth/users").await;
    let row = users
        .body
        .split("<tr>")
        .find(|r| r.contains("<strong>other</strong>"))
        .unwrap();
    let marker = "admin/auth/users/";
    let id = row[row.find(marker).unwrap() + marker.len()..]
        .split('"')
        .next()
        .unwrap();
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(form.body.contains("name=\"reset_totp\""));
    // Saving without ticking the box keeps it on.
    let keep = "username=other&is_active=true&is_staff=true&is_superuser=true";
    let reply = root
        .post(&app, &format!("/admin/auth/users/{id}"), keep)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let reply = root
        .post(
            &app,
            &format!("/admin/auth/users/{id}"),
            &format!("{keep}&reset_totp=true"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::OK
    );
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(!form.body.contains("name=\"reset_totp\""));

    // And from the command line.
    let secret = enable_totp(&db, "other").await.unwrap();
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Setting it up again starts afresh: the first code of the new key works.
    assert_eq!(
        token(&app, "other", PASSWORD, Some(&code(&secret, 0)), None)
            .await
            .status,
        StatusCode::OK
    );
    disable_totp(&db, "other").await.unwrap();
    assert_eq!(
        token(&app, "other", PASSWORD, None, None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_site_that_requires_two_factor_does_not_let_it_be_turned_off() {
    let (app, db) = app_with(auth().totp_required(true)).await;
    // Nobody can log in to set it up, so the first key comes from the command line.
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let secret = enable_totp(&db, "root").await.unwrap();
    let root = Browser::login(&app, "root", Some(&code(&secret, 0))).await;

    let page = root.get(&app, "/admin/auth/two-factor").await;
    assert!(page.body.contains("cannot be turned off here") && !page.body.contains("Turn off"));
    let reply = root
        .post(&app, "/admin/auth/two-factor/disable", &password_field())
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(
        token(&app, "root", PASSWORD, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn guessing_the_password_on_the_two_factor_page_is_limited_too() {
    let (app, _db) = app_with(auth().login_limit(3, 120)).await;
    let root = Browser::login(&app, "root", None).await;
    let page = root.get(&app, "/admin/auth/two-factor").await;
    let secret = field(&page.body, "secret");
    let guess = format!("secret={secret}&password=guess&code={}", code(&secret, 0));
    for _ in 0..3 {
        let reply = root.post(&app, "/admin/auth/two-factor", &guess).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let reply = root.post(&app, "/admin/auth/two-factor", &guess).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(reply.retry_after.is_some() && reply.body.contains("<main id=\"main\""));
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

//! Logging in through the REST API: accounts that are not staff, refresh tokens, two-factor
//! challenges, `me`, and handing out logins from your own routes.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router};
use cadmus::{
    ApiResult, App, AuthConfig, LoginOutcome, Logins, ModelOptions, change_password,
    create_superuser, enable_totp, new_recovery_codes, user,
};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
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

/// A login of your own: whoever names an account gets it. Only for the test.
async fn trusting_login(logins: Logins, Json(body): Json<Value>) -> ApiResult<LoginOutcome> {
    Ok(logins.log_in(body["username"].as_str().unwrap()).await?)
}

async fn site(auth: AuthConfig) -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = App::new(db.clone(), auth)
        .model::<note::Entity>(ModelOptions::new())
        .routes(Router::new().route("/custom/login", post(trusting_login)));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    // An account for an app's user: not staff, allowed to read notes.
    create_superuser(&db, "member", PASSWORD).await.unwrap();
    user::Entity::update_many()
        .col_expr(user::Column::IsStaff, Expr::value(false))
        .col_expr(user::Column::IsSuperuser, Expr::value(false))
        .filter(user::Column::Username.eq("member"))
        .exec(&db)
        .await
        .unwrap();
    let app = app.router().unwrap();
    // Give `member` the note:view permission through the admin, as a superuser would.
    let root = admin_login(&app, "root").await.unwrap();
    let users = send(
        &app,
        get("/admin/auth/users").header(header::COOKIE, &root.0),
    )
    .await;
    let id = users
        .text
        .split("<tr>")
        .find(|row| row.contains("<strong>member</strong>"))
        .and_then(|row| row.split("auth/users/").nth(1))
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let saved = send(
        &app,
        Request::post(format!("/admin/auth/users/{id}"))
            .header(header::COOKIE, &root.0)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "_csrf={}&username=member&is_active=true&perm=note:view",
                root.1
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text);
    (app, db)
}

fn auth() -> AuthConfig {
    AuthConfig::new(SECRET).unwrap()
}

struct Reply {
    status: StatusCode,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, request: impl Into<RequestOrBuilder>) -> Reply {
    let request = match request.into() {
        RequestOrBuilder::Request(request) => request,
        RequestOrBuilder::Builder(builder) => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

enum RequestOrBuilder {
    Request(Request<Body>),
    Builder(axum::http::request::Builder),
}

impl From<Request<Body>> for RequestOrBuilder {
    fn from(request: Request<Body>) -> Self {
        Self::Request(request)
    }
}

impl From<axum::http::request::Builder> for RequestOrBuilder {
    fn from(builder: axum::http::request::Builder) -> Self {
        Self::Builder(builder)
    }
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path)
}

async fn post_json(app: &Router, path: &str, body: Value) -> Reply {
    send(
        app,
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn bearer(app: &Router, token: &str, path: &str) -> StatusCode {
    send(
        app,
        get(path).header(header::AUTHORIZATION, format!("Bearer {token}")),
    )
    .await
    .status
}

async fn password_login(app: &Router, username: &str, password: &str) -> Reply {
    post_json(
        app,
        "/api/auth/token",
        json!({ "username": username, "password": password }),
    )
    .await
}

/// The admin's login form: the cookie and the page's CSRF token, if it let the account in.
async fn admin_login(app: &Router, username: &str) -> Option<(String, String)> {
    let reply = app
        .clone()
        .oneshot(
            Request::post("/admin/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username={username}&password={}",
                    PASSWORD.replace(' ', "+")
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    if reply.status() != StatusCode::SEE_OTHER {
        return None;
    }
    let cookie = reply.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let home = send(app, get("/admin/").header(header::COOKIE, &cookie)).await;
    let csrf = home
        .text
        .split("name=\"_csrf\" value=\"")
        .nth(1)?
        .split('"')
        .next()?
        .to_owned();
    Some((cookie, csrf))
}

/// The current authenticator code for `secret`, `steps_ahead` 30-second steps later.
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

#[tokio::test]
async fn accounts_that_are_not_staff_use_the_api_but_not_the_admin() {
    let (app, _db) = site(auth()).await;
    let reply = password_login(&app, "member", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let token = reply.json()["access_token"].as_str().unwrap().to_owned();

    assert_eq!(bearer(&app, &token, "/api/note").await, StatusCode::OK);
    assert_eq!(
        bearer(&app, &token, "/admin/note").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(bearer(&app, &token, "/admin/").await, StatusCode::FORBIDDEN);
    // Its permissions still apply.
    let added = send(
        &app,
        Request::post("/api/note")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"text":"hi"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(added.status, StatusCode::FORBIDDEN);
    // The admin's login form refuses it, saying why.
    assert!(admin_login(&app, "member").await.is_none());

    let me = send(
        &app,
        get("/api/auth/me").header(header::AUTHORIZATION, format!("Bearer {token}")),
    )
    .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text);
    assert_eq!(me.json()["username"], "member");
    assert_eq!(me.json()["staff"], false);
    assert_eq!(me.json()["superuser"], false);
    assert_eq!(me.json()["two_factor"], false);
    assert_eq!(
        send(&app, get("/api/auth/me")).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn refresh_tokens_rotate_and_a_reused_one_ends_the_login() {
    let (app, db) = site(auth().refresh_ttl_secs(3600)).await;
    let first = password_login(&app, "member", PASSWORD).await.json();
    assert_eq!(first["token_type"], "Bearer");
    assert_eq!(first["refresh_expires_in"], 3600);
    let r1 = first["refresh_token"].as_str().unwrap().to_owned();
    assert!(r1.starts_with("cadmus_rt_"));

    let second = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r1 })).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.text);
    let r2 = second.json()["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(r1, r2);
    let access = second.json()["access_token"].as_str().unwrap().to_owned();
    assert_eq!(bearer(&app, &access, "/api/note").await, StatusCode::OK);

    // r1 was spent: using it again means it was copied, so the whole login ends, r2 too.
    let replay = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r1 })).await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    assert_eq!(replay.json()["error"]["code"], "invalid_refresh_token");
    let after = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r2 })).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);

    // Logging out ends a login.
    let r3 = password_login(&app, "member", PASSWORD).await.json()["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = post_json(&app, "/api/auth/logout", json!({ "refresh_token": r3 })).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let dead = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r3 })).await;
    assert_eq!(dead.status, StatusCode::UNAUTHORIZED);
    // Unknown and malformed tokens are answered the same way.
    for junk in ["nonsense", "cadmus_rt_0000"] {
        let reply = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": junk })).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    // A password change ends logins made with the old password.
    let r4 = password_login(&app, "member", PASSWORD).await.json()["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned();
    change_password(&db, "member", "a different long password")
        .await
        .unwrap();
    let reply = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r4 })).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // So does deactivating the account.
    let r5 = password_login(&app, "member", "a different long password")
        .await
        .json()["refresh_token"]
        .as_str()
        .unwrap()
        .to_owned();
    user::Entity::update_many()
        .col_expr(user::Column::IsActive, Expr::value(false))
        .filter(user::Column::Username.eq("member"))
        .exec(&db)
        .await
        .unwrap();
    let reply = post_json(&app, "/api/auth/refresh", json!({ "refresh_token": r5 })).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn refresh_tokens_can_be_turned_off() {
    let (app, _db) = site(auth().refresh_ttl_secs(0)).await;
    let reply = password_login(&app, "member", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.json().get("refresh_token").is_none(),
        "{}",
        reply.text
    );
    assert!(reply.json()["access_token"].is_string());
}

#[tokio::test]
async fn two_factor_logins_answer_with_a_challenge() {
    let (app, db) = site(auth()).await;
    let secret = enable_totp(&db, "member").await.unwrap();

    let first = password_login(&app, "member", PASSWORD).await;
    assert_eq!(first.status, StatusCode::UNAUTHORIZED);
    assert_eq!(first.json()["error"]["code"], "totp_required");
    let challenge = first.json()["challenge"].as_str().unwrap().to_owned();
    // A wrong password gets no challenge.
    let wrong = password_login(&app, "member", "not the password").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.json().get("challenge").is_none());

    let bad = post_json(
        &app,
        "/api/auth/totp",
        json!({ "challenge": challenge, "code": "000000" }),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    let forged = post_json(
        &app,
        "/api/auth/totp",
        json!({ "challenge": format!("{challenge}x"), "code": code(&secret, 0) }),
    )
    .await;
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
    assert_eq!(forged.json()["error"]["code"], "challenge_expired");

    let good = post_json(
        &app,
        "/api/auth/totp",
        json!({ "challenge": challenge, "code": code(&secret, 0) }),
    )
    .await;
    assert_eq!(good.status, StatusCode::OK, "{}", good.text);
    assert!(good.json()["refresh_token"].is_string());

    // A recovery code works in place of the authenticator code, once.
    let recovery = new_recovery_codes(&db, "member").await.unwrap();
    let challenge = password_login(&app, "member", PASSWORD).await.json()["challenge"]
        .as_str()
        .unwrap()
        .to_owned();
    let with_recovery = post_json(
        &app,
        "/api/auth/totp",
        json!({ "challenge": challenge, "code": recovery[0] }),
    )
    .await;
    assert_eq!(
        with_recovery.status,
        StatusCode::OK,
        "{}",
        with_recovery.text
    );
    let again = post_json(
        &app,
        "/api/auth/totp",
        json!({ "challenge": challenge, "code": recovery[0] }),
    )
    .await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);

    // The code can still come with the password, as in 0.1.
    let inline = post_json(
        &app,
        "/api/auth/token",
        json!({ "username": "member", "password": PASSWORD, "totp": code(&secret, 1) }),
    )
    .await;
    assert_eq!(inline.status, StatusCode::OK, "{}", inline.text);
    assert_eq!(inline.json()["token"], inline.json()["access_token"]);
}

#[tokio::test]
async fn your_own_routes_can_hand_out_logins() {
    let (app, db) = site(auth()).await;
    let reply = post_json(&app, "/custom/login", json!({ "username": "member" })).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let token = reply.json()["access_token"].as_str().unwrap().to_owned();
    assert_eq!(bearer(&app, &token, "/api/note").await, StatusCode::OK);

    // Two-factor login still applies.
    enable_totp(&db, "member").await.unwrap();
    let reply = post_json(&app, "/custom/login", json!({ "username": "member" })).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.json()["challenge"].is_string());
    // Unknown accounts get nothing.
    let reply = post_json(&app, "/custom/login", json!({ "username": "nobody" })).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

async fn with_token(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    send(
        app,
        request
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap(),
    )
    .await
}

fn access(reply: &Reply) -> String {
    reply.json()["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("no access token in {}", reply.text))
        .to_owned()
}

#[tokio::test]
async fn people_manage_their_own_account_through_the_api() {
    let (app, _db) = site(auth()).await;
    let token = access(&password_login(&app, "member", PASSWORD).await);

    // Profile.
    let reply = with_token(
        &app,
        &token,
        "PATCH",
        "/api/auth/me",
        Some(json!({"first_name": " Mia ", "last_name": "Wong"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    let me = with_token(&app, &token, "GET", "/api/auth/me", None)
        .await
        .json();
    assert_eq!(me["first_name"], "Mia");
    assert_eq!(me["last_name"], "Wong");

    // Password: the old one must be given, and the change ends old logins.
    let wrong = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/password",
        Some(json!({"current": "nope", "password": "a brand new password"})),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    let changed = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/password",
        Some(json!({"current": PASSWORD, "password": "a brand new password"})),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    assert_eq!(
        bearer(&app, &token, "/api/note").await,
        StatusCode::UNAUTHORIZED
    );
    let token = access(&changed);
    assert_eq!(bearer(&app, &token, "/api/note").await, StatusCode::OK);

    // Two-factor: a key to scan, confirmed with a code from it, then recovery codes.
    let setup = with_token(&app, &token, "POST", "/api/auth/me/totp", None).await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
    let secret = setup.json()["secret"].as_str().unwrap().to_owned();
    assert!(
        setup.json()["uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );
    let bad = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/totp/confirm",
        Some(json!({"secret": secret, "code": "000000"})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let on = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/totp/confirm",
        Some(json!({"secret": secret, "code": code(&secret, 0)})),
    )
    .await;
    assert_eq!(on.status, StatusCode::OK, "{}", on.text);
    assert_eq!(on.json()["recovery_codes"].as_array().unwrap().len(), 10);
    let me = with_token(&app, &token, "GET", "/api/auth/me", None)
        .await
        .json();
    assert_eq!(me["two_factor"], true);
    let login = password_login(&app, "member", "a brand new password").await;
    assert_eq!(login.json()["error"]["code"], "totp_required");

    let renewed = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/recovery-codes",
        Some(json!({"code": code(&secret, 1)})),
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.text);
    let recovery = renewed.json()["recovery_codes"][0]
        .as_str()
        .unwrap()
        .to_owned();
    let off = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/totp/disable",
        Some(json!({"code": recovery})),
    )
    .await;
    assert_eq!(off.status, StatusCode::NO_CONTENT, "{}", off.text);
    assert_eq!(
        password_login(&app, "member", "a brand new password")
            .await
            .status,
        StatusCode::OK
    );

    // No linked sites yet.
    let links = with_token(&app, &token, "GET", "/api/auth/me/identities", None).await;
    assert_eq!(links.json()["data"], json!([]));
}

#[tokio::test]
async fn api_keys_are_made_and_revoked_through_the_api_but_cannot_manage_the_account() {
    let (app, _db) = site(auth()).await;
    let token = access(&password_login(&app, "member", PASSWORD).await);

    let refused = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/api-keys",
        Some(json!({"name": "x", "expires_in_days": 7})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    let made = with_token(
        &app,
        &token,
        "POST",
        "/api/auth/me/api-keys",
        Some(json!({"name": "sync job", "read_only": true})),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text);
    let key = made.json()["key"].as_str().unwrap().to_owned();
    assert_eq!(bearer(&app, &key, "/api/note").await, StatusCode::OK);

    let listed = with_token(&app, &token, "GET", "/api/auth/me/api-keys", None).await;
    let keys = listed.json()["data"].as_array().unwrap().clone();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "sync job");
    assert!(keys[0].get("key").is_none());

    // A key cannot change its own account.
    for (method, path, body) in [
        (
            "POST",
            "/api/auth/me/password",
            json!({"current": PASSWORD, "password": "x long password"}),
        ),
        ("POST", "/api/auth/me/api-keys", json!({"name": "another"})),
        ("POST", "/api/auth/me/totp", json!({})),
    ] {
        let reply = with_token(&app, &key, method, path, Some(body)).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{path}: {}",
            reply.text
        );
    }

    let id = keys[0]["id"].as_i64().unwrap();
    let revoked = with_token(
        &app,
        &token,
        "DELETE",
        &format!("/api/auth/me/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(
        bearer(&app, &key, "/api/note").await,
        StatusCode::UNAUTHORIZED
    );
    let again = with_token(
        &app,
        &token,
        "DELETE",
        &format!("/api/auth/me/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

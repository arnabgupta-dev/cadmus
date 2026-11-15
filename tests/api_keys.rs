//! API keys: made on the account's page (or with `new_api_key`), used by programs calling the
//! REST API, and revoked.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use cadmus::{App, AuthConfig, ModelOptions, create_superuser, new_api_key};
use http_body_util::BodyExt;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter};
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

async fn site_with(keys: bool) -> (Router, DatabaseConnection) {
    let db = common::database().await;
    let app = App::new(
        db.clone(),
        AuthConfig::new(SECRET)
            .unwrap()
            .cookie_secure(false)
            .api_keys(keys),
    )
    .model::<note::Entity>(ModelOptions::new().label("Notes"));
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    (app.router().unwrap(), db)
}

async fn site() -> (Router, DatabaseConnection) {
    site_with(true).await
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
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

/// A request to the API with `key` as a bearer token.
async fn api(app: &Router, method: &str, path: &str, key: &str, body: Option<Value>) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    send(app, request.body(body).unwrap()).await
}

/// A browser session for `username`, logged in just now or (with `stale`) an hour ago.
struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn new(db: &DatabaseConnection, username: &str, stale: bool) -> Self {
        #[derive(serde::Serialize)]
        struct Claims<'a> {
            sub: &'a str,
            iat: u64,
            exp: u64,
            csrf: &'a str,
            pw: String,
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &Claims {
                sub: username,
                iat: if stale { now - 3600 } else { now },
                exp: now + 3600,
                csrf: "test-csrf",
                pw: common::password_print(db, username).await,
            },
            &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();
        Self {
            cookie: format!("admin_token={token}"),
            csrf: "test-csrf".into(),
        }
    }

    async fn get(&self, app: &Router, path: &str) -> Reply {
        let request = Request::get(path)
            .header(header::COOKIE, &self.cookie)
            .body(Body::empty())
            .unwrap();
        send(app, request).await
    }

    async fn post(&self, app: &Router, path: &str, fields: &str) -> Reply {
        let body = if fields.is_empty() {
            format!("_csrf={}", self.csrf)
        } else {
            format!("_csrf={}&{fields}", self.csrf)
        };
        let request = Request::post(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        send(app, request).await
    }
}

/// The key shown once on the page after making it.
fn shown_key(page: &str) -> String {
    page.split("data-secret>")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .expect("the new key is shown")
        .to_owned()
}

#[tokio::test]
async fn a_key_made_on_the_page_works_until_it_is_revoked() {
    let (app, db) = site().await;
    let me = Browser::new(&db, "root", false).await;
    let page = me.get(&app, "/admin/auth/api-keys").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("You have no keys yet"));
    // A fresh login needs no password.
    assert!(!page.body.contains("name=\"password\""));

    let made = me
        .post(
            &app,
            "/admin/auth/api-keys",
            "name=Deploy+script&expires=30",
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(made.headers[header::CACHE_CONTROL], "no-store");
    let key = shown_key(&made.body);
    assert!(key.starts_with("cadmus_") && key.len() == 47, "{key}");
    assert!(made.body.contains("Deploy script"));
    assert!(made.body.contains(&format!("{}…", &key[..15])));
    assert!(made.body.contains("expires "));

    // It reads and writes through the API, as a bearer token or in X-API-Key.
    let created = api(&app, "POST", "/api/note", &key, Some(json!({"text": "hi"}))).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let listed = api(&app, "GET", "/api/note", &key, None).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(listed.body.contains("\"hi\""));
    let header_key = send(
        &app,
        Request::get("/api/note")
            .header("x-api-key", &key)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(header_key.status, StatusCode::OK);
    // Its use is recorded.
    let page = me.get(&app, "/admin/auth/api-keys").await;
    assert!(page.body.contains("last used"), "{}", page.body);
    // The key is never shown again.
    assert!(!page.body.contains(&key));

    // Keys are for the API only, not the admin pages.
    let admin = api(&app, "GET", "/admin/note", &key, None).await;
    assert_eq!(admin.status, StatusCode::UNAUTHORIZED);

    let revoked = me.post(&app, "/admin/auth/api-keys/1/delete", "").await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn read_only_keys_cannot_change_anything() {
    let (app, db) = site().await;
    let key = new_api_key(&db, "root", "dashboard", None, true)
        .await
        .unwrap();
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::OK
    );
    let write = api(&app, "POST", "/api/note", &key, Some(json!({"text": "no"}))).await;
    assert_eq!(write.status, StatusCode::FORBIDDEN, "{}", write.body);
}

#[tokio::test]
async fn keys_stop_working_when_they_expire_or_the_account_is_locked() {
    let (app, db) = site().await;
    let expiring = new_api_key(&db, "root", "short", Some(1), false)
        .await
        .unwrap();
    let lasting = new_api_key(&db, "root", "long", None, false).await.unwrap();
    assert_eq!(
        api(&app, "GET", "/api/note", &expiring, None).await.status,
        StatusCode::OK
    );
    // MySQL and MariaDB take no offset in a date; the others store one.
    let past = match db.get_database_backend() {
        sea_orm::DbBackend::MySql => "2001-01-01 00:00:00",
        _ => "2001-01-01 00:00:00+00:00",
    };
    db.execute_unprepared(&format!(
        "UPDATE auth_api_key SET expires = '{past}' WHERE name = 'short'"
    ))
    .await
    .unwrap();
    assert_eq!(
        api(&app, "GET", "/api/note", &expiring, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api(&app, "GET", "/api/note", &lasting, None).await.status,
        StatusCode::OK
    );
    db.execute_unprepared("UPDATE auth_user SET is_active = FALSE")
        .await
        .unwrap();
    assert_eq!(
        api(&app, "GET", "/api/note", &lasting, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Made-up and malformed keys are simply refused.
    for key in [
        "cadmus_0000000000000000000000000000000000000000",
        "cadmus_short",
    ] {
        assert_eq!(
            api(&app, "GET", "/api/note", key, None).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn an_old_login_needs_the_password_to_make_a_key() {
    let (app, db) = site().await;
    let me = Browser::new(&db, "root", true).await;
    let page = me.get(&app, "/admin/auth/api-keys").await;
    assert!(page.body.contains("name=\"password\""));
    let refused = me
        .post(
            &app,
            "/admin/auth/api-keys",
            "name=ci&expires=90&password=wrong",
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("The password is not correct"));
    // What was typed is kept.
    assert!(refused.body.contains("value=\"ci\""));
    let made = me
        .post(
            &app,
            "/admin/auth/api-keys",
            &format!(
                "name=ci&expires=never&read_only=true&password={}",
                PASSWORD.replace(' ', "+")
            ),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert!(made.body.contains("Read only"));

    let unnamed = me
        .post(
            &app,
            "/admin/auth/api-keys",
            &format!("name=+&expires=90&password={}", PASSWORD.replace(' ', "+")),
        )
        .await;
    assert_eq!(unnamed.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(unnamed.body.contains("Give the key a name"));
}

#[tokio::test]
async fn superusers_see_and_revoke_other_accounts_keys() {
    let (app, db) = site().await;
    create_superuser(&db, "bob", PASSWORD).await.unwrap();
    let key = new_api_key(&db, "bob", "Bob's script", None, false)
        .await
        .unwrap();
    let root = Browser::new(&db, "root", false).await;
    let form = root.get(&app, "/admin/auth/users/2").await;
    assert!(form.body.contains("Bob&#x27;s script"), "{}", form.body);
    let revoked = root
        .post(&app, "/admin/auth/users/2/api-keys/1/delete", "")
        .await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // A key that belongs to someone else cannot be revoked from your own page.
    let other = new_api_key(&db, "bob", "another", None, false)
        .await
        .unwrap();
    let mine = root.post(&app, "/admin/auth/api-keys/2/delete", "").await;
    assert_eq!(mine.status, StatusCode::NOT_FOUND);
    assert_eq!(
        api(&app, "GET", "/api/note", &other, None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn keys_can_be_turned_off() {
    let (app, db) = site_with(false).await;
    let key = new_api_key(&db, "root", "unused", None, false)
        .await
        .unwrap();
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let me = Browser::new(&db, "root", false).await;
    assert_eq!(
        me.get(&app, "/admin/auth/api-keys").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!me.get(&app, "/admin/").await.body.contains("api-keys"));
    let docs = send(
        &app,
        Request::get("/api/openapi.json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(!docs.body.contains("X-API-Key"));
}

#[tokio::test]
async fn the_api_documentation_describes_keys() {
    let (app, _db) = site().await;
    let docs = send(
        &app,
        Request::get("/api/openapi.json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let document: Value = serde_json::from_str(&docs.body).unwrap();
    assert_eq!(
        document["components"]["securitySchemes"]["apiKey"]["name"],
        "X-API-Key"
    );
    assert!(
        document["security"]
            .as_array()
            .unwrap()
            .contains(&json!({"apiKey": []}))
    );
}

#[tokio::test]
async fn deactivating_an_account_ends_its_keys_for_good() {
    let (app, db) = site().await;
    create_superuser(&db, "bot", PASSWORD).await.unwrap();
    let bot = Browser::new(&db, "bot", false).await;
    let key = shown_key(
        &bot.post(&app, "/admin/auth/api-keys", "name=Sync&expires=30")
            .await
            .body,
    );
    let read_only = shown_key(
        &bot.post(
            &app,
            "/admin/auth/api-keys",
            "name=Reader&expires=30&read_only=true",
        )
        .await
        .body,
    );
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::OK
    );
    // A read-only key says so when it tries to change something.
    let refused = api(
        &app,
        "POST",
        "/api/note",
        &read_only,
        Some(json!({"text": "x"})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(refused.body.contains("read-only"), "{}", refused.body);

    let id = cadmus::user::Entity::find()
        .filter(cadmus::user::Column::Username.eq("bot"))
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .id;
    let root = Browser::new(&db, "root", false).await;
    let path = format!("/admin/auth/users/{id}");
    let off = root
        .post(&app, &path, "username=bot&is_staff=true&is_superuser=true")
        .await;
    assert_eq!(off.status, StatusCode::SEE_OTHER, "{}", off.body);
    let on = root
        .post(
            &app,
            &path,
            "username=bot&is_active=true&is_staff=true&is_superuser=true",
        )
        .await;
    assert_eq!(on.status, StatusCode::SEE_OTHER, "{}", on.body);
    // Back on, but the keys made before are gone.
    assert_eq!(
        api(&app, "GET", "/api/note", &key, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

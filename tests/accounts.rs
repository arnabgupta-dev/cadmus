//! Account details: e-mail, names and phone; logging in with the e-mail address; when an
//! account joined and last logged in.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use herapi::{App, AuthConfig, create_superuser, hash_password, set_email};
use http_body_util::BodyExt;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend};
use serde_json::json;
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";

fn new_app(db: &DatabaseConnection) -> App {
    App::new(
        db.clone(),
        AuthConfig::new(SECRET).unwrap().login_limit(4, 120),
    )
}

async fn setup() -> (Router, DatabaseConnection, Browser) {
    let db = common::database().await;
    let app = new_app(&db);
    app.migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    let app = app.router().unwrap();
    let root = Browser::login(&app, "root").await;
    (app, db, root)
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

async fn form_login(app: &Router, login: &str, password: &str) -> Reply {
    let request = Request::post("/admin/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "username={}&password={}",
            login.replace('@', "%40").replace('+', "%2B"),
            password.replace(' ', "+")
        )))
        .unwrap();
    send(app, request).await
}

async fn token(app: &Router, login: &str, password: &str) -> Reply {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": login, "password": password }).to_string(),
        ))
        .unwrap();
    send(app, request).await
}

struct Browser {
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn login(app: &Router, login: &str) -> Self {
        let reply = form_login(app, login, PASSWORD).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let cookie = reply.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut browser = Self {
            cookie,
            csrf: String::new(),
        };
        let home = browser.get(app, "/admin/").await;
        let marker = "name=\"_csrf\" value=\"";
        let start = home.body.find(marker).unwrap() + marker.len();
        browser.csrf = home.body[start..].split('"').next().unwrap().to_owned();
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

/// The form body for a new active staff account.
fn account(username: &str, extra: &str) -> String {
    format!(
        "username={username}&password={}&is_active=true&is_staff=true&{extra}",
        PASSWORD.replace(' ', "+")
    )
}

/// The id of an account, from the Edit link of its row on the Users page.
async fn id_of(app: &Router, root: &Browser, username: &str) -> String {
    let page = root.get(app, "/admin/auth/users").await;
    let row = page
        .body
        .split("<tr>")
        .find(|r| r.contains(&format!("<strong>{username}</strong>")))
        .unwrap_or_else(|| panic!("no row for {username}"));
    let marker = "admin/auth/users/";
    row[row.find(marker).unwrap() + marker.len()..]
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn details_are_saved_shown_and_tidied() {
    let (app, _db, root) = setup().await;

    let extra = "email=+Ada.Lovelace%40Example.COM+&first_name=+Ada+&last_name=Lovelace&phone=%2B44+20+7946+0958";
    let reply = root
        .post(&app, "/admin/auth/users/new", &account("ada", extra))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);

    // The list shows the full name and the address, in lower case.
    let list = root.get(&app, "/admin/auth/users").await;
    assert!(
        list.body.contains("data-label=\"Name\">Ada Lovelace</td>"),
        "{}",
        list.body
    );
    assert!(
        list.body
            .contains("data-label=\"Email\">ada.lovelace@example.com</td>")
    );
    // An account without a name shows nothing there, not its username again.
    assert!(list.body.contains("data-label=\"Name\"></td>"));

    let id = id_of(&app, &root, "ada").await;
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(
        form.body
            .contains("name=\"email\" type=\"email\" value=\"ada.lovelace@example.com\"")
    );
    assert!(
        form.body.contains("name=\"first_name\" value=\"Ada\""),
        "trimmed"
    );
    assert!(
        form.body
            .contains("name=\"phone\" type=\"tel\" value=\"+44 20 7946 0958\"")
    );
    assert!(form.body.contains("Joined 20") && form.body.contains("Has not logged in yet."));

    // Clearing a field stores nothing, and the rest is kept.
    let reply = root
        .post(
            &app,
            &format!("/admin/auth/users/{id}"),
            "username=ada&is_active=true&is_staff=true&email=&first_name=Ada&last_name=&phone=",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(
        form.body
            .contains("name=\"email\" type=\"email\" value=\"\"")
    );
    assert!(form.body.contains("name=\"last_name\" value=\"\""));
    let list = root.get(&app, "/admin/auth/users").await;
    assert!(list.body.contains("data-label=\"Name\">Ada</td>"));

    // The dashboard greets an account by name.
    let ada = Browser::login(&app, "ada").await;
    assert!(ada.get(&app, "/admin/").await.body.contains("Hello, Ada"));
}

#[tokio::test]
async fn bad_or_taken_details_are_refused_with_the_field_named() {
    let (app, _db, root) = setup().await;
    let reply = root
        .post(
            &app,
            "/admin/auth/users/new",
            &account("ada", "email=ada%40example.com"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    // Several accounts may have no address at all.
    for name in ["bob", "cy"] {
        let reply = root
            .post(&app, "/admin/auth/users/new", &account(name, "email="))
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    }

    for (extra, message) in [
        (
            "email=ADA%40example.com",
            "another account already uses this address",
        ),
        ("email=not-an-address", "email: enter an address"),
        ("email=a%40b", "email: enter an address"),
        ("email=a+b%40example.com", "email: enter an address"),
        ("phone=call+me", "phone: use digits"),
        ("phone=123", "phone: use digits"),
    ] {
        let reply = root
            .post(&app, "/admin/auth/users/new", &account("dave", extra))
            .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{extra}");
        assert!(reply.body.contains(message), "{extra}: {}", reply.body);
    }
    assert!(
        !root
            .get(&app, "/admin/auth/users")
            .await
            .body
            .contains("<strong>dave</strong>")
    );

    // An account may keep its own address when saved again.
    let id = id_of(&app, &root, "ada").await;
    let reply = root
        .post(
            &app,
            &format!("/admin/auth/users/{id}"),
            "username=ada&is_active=true&is_staff=true&email=ada%40example.com",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
}

#[tokio::test]
async fn an_account_can_log_in_with_its_email_address() {
    let (app, db, root) = setup().await;
    let reply = root
        .post(
            &app,
            "/admin/auth/users/new",
            &account("ada", "email=ada%40example.com"),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    // Through the API, ignoring case and surrounding spaces; the token names the username.
    for login in ["ada@example.com", "ADA@Example.com", " ada@example.com "] {
        let reply = token(&app, login, PASSWORD).await;
        assert_eq!(reply.status, StatusCode::OK, "{login}: {}", reply.body);
    }
    assert_eq!(token(&app, "ada", PASSWORD).await.status, StatusCode::OK);
    assert_eq!(
        token(&app, "ada@example.com", "wrong").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        token(&app, "nobody@example.com", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Through the form, and the session is the account's own.
    let ada = Browser::login(&app, "Ada@Example.com").await;
    let home = ada.get(&app, "/admin/").await;
    assert!(
        home.body.contains("<div class=\"me-name\">ada</div>"),
        "{}",
        home.body
    );

    // The username and the address share one attempt count (4 here).
    for login in ["ada", "ada@example.com", "ada", "ADA@example.com"] {
        assert_eq!(
            token(&app, login, "wrong").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        token(&app, "ada", PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        token(&app, "ada@example.com", PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );

    // A username that looks like an address wins over another account's address.
    create_superuser(&db, "ada@example.com", PASSWORD)
        .await
        .unwrap();
    let reply = form_login(&app, "ada@example.com", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let cookie = reply.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let request = Request::get("/admin/")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    assert!(
        send(&app, request)
            .await
            .body
            .contains("<div class=\"me-name\">ada@example.com</div>")
    );
}

#[tokio::test]
async fn logging_in_is_recorded() {
    let (app, _db, root) = setup().await;
    let id = id_of(&app, &root, "root").await;
    // `root` logged in during setup.
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(
        form.body.contains("Last logged in 20") && form.body.contains(" UTC."),
        "{}",
        form.body
    );
    // A failed attempt is not a login.
    let reply = root
        .post(&app, "/admin/auth/users/new", &account("ada", ""))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        token(&app, "ada", "wrong").await.status,
        StatusCode::UNAUTHORIZED
    );
    let id = id_of(&app, &root, "ada").await;
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(form.body.contains("Has not logged in yet."));
    assert_eq!(token(&app, "ada", PASSWORD).await.status, StatusCode::OK);
    let form = root.get(&app, &format!("/admin/auth/users/{id}")).await;
    assert!(form.body.contains("Last logged in 20"));
}

#[tokio::test]
async fn older_account_tables_gain_the_new_columns_and_a_unique_address() {
    let db = common::database().await;
    let id = match db.get_database_backend() {
        DbBackend::Postgres => "SERIAL PRIMARY KEY",
        DbBackend::MySql => "INTEGER PRIMARY KEY AUTO_INCREMENT",
        _ => "INTEGER PRIMARY KEY AUTOINCREMENT",
    };
    // The table as the release before this one created it, with two accounts.
    // One statement at a time: MySQL takes no more in one call.
    let sql = format!(
        "CREATE TABLE auth_user (
            id {id},
            username VARCHAR(255) NOT NULL UNIQUE,
            password_hash VARCHAR(255) NOT NULL,
            totp_secret VARCHAR(255),
            is_active BOOLEAN NOT NULL,
            is_staff BOOLEAN NOT NULL,
            is_superuser BOOLEAN NOT NULL DEFAULT FALSE,
            totp_last_step BIGINT
        );
        INSERT INTO auth_user (username, password_hash, is_active, is_staff, is_superuser)
        VALUES ('old', '{hash}', TRUE, TRUE, TRUE), ('other', '{hash}', TRUE, TRUE, FALSE)",
        hash = hash_password(PASSWORD).unwrap()
    );
    for statement in sql.split(";\n") {
        db.execute_unprepared(statement).await.unwrap();
    }

    let app = new_app(&db);
    app.migrate().await.unwrap();
    app.migrate().await.unwrap();
    let app = app.router().unwrap();

    // The accounts still log in, and get addresses.
    assert_eq!(token(&app, "old", PASSWORD).await.status, StatusCode::OK);
    set_email(&db, "old", "Old@Example.com").await.unwrap();
    assert_eq!(
        token(&app, "old@example.com", PASSWORD).await.status,
        StatusCode::OK
    );
    assert!(set_email(&db, "other", "old@example.com").await.is_err());
    assert!(set_email(&db, "other", "nonsense").await.is_err());
    assert!(set_email(&db, "nobody", "x@example.com").await.is_err());

    // The database itself refuses a second account with the same address.
    let duplicate = db
        .execute_unprepared(
            "UPDATE auth_user SET email = 'old@example.com' WHERE username = 'other'",
        )
        .await;
    assert!(duplicate.is_err(), "the unique index is missing");
    // ...while any number may have none.
    db.execute_unprepared("UPDATE auth_user SET email = NULL WHERE username = 'other'")
        .await
        .unwrap();
}

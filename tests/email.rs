//! Logging in by e-mail: login links, password resets, and changing one's own password.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use heraapi::settings::*;
use heraapi::{AdminError, App, Email, Mailer, create_superuser, enable_totp, set_email};
use http_body_util::BodyExt;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery";

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
            cookie_secure: false,
            ..Default::default()
        },
        admin: AdminSettings {
            site_title: "Acme".into(),
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

    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
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

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn post(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body.to_owned())).unwrap()).await
}

/// The path of the first link to this site in a message.
fn link_path(email: &Email) -> String {
    let start = email
        .text
        .find("http://localhost:3000/")
        .expect("the message has a link");
    let link: String = email.text[start..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    link.trim_start_matches("http://localhost:3000").to_owned()
}

async fn log_in(app: &Router, username: &str, password: &str) -> Reply {
    post(
        app,
        "/admin/login",
        &format!(
            "username={username}&password={}",
            password.replace(' ', "+")
        ),
        None,
    )
    .await
}

async fn account(db: &DatabaseConnection, username: &str) -> Option<heraapi::user::Model> {
    heraapi::user::Entity::find()
        .filter(heraapi::user::Column::Username.eq(username))
        .one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_login_link_logs_in_once() {
    let site = site(|_| {}).await;
    let page = get(&site.app, "/admin/login", None).await;
    assert!(page.body.contains("Email me a login link"), "{}", page.body);
    assert!(page.body.contains("Forgot your password?"));

    let reply = post(
        &site.app,
        "/admin/login/link",
        "email=+ADA%40Acme.com+",
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("Check your inbox"));
    let email = site.outbox.nth(0).await;
    assert_eq!(email.to, "ada@acme.com");
    assert_eq!(email.subject, "Your login link for Acme");
    assert!(email.html.as_deref().unwrap().contains("Log in"));
    let path = link_path(&email);
    assert!(path.starts_with("/admin/login/link/"), "{path}");

    // Opening the link (as a mail scanner would) does not log in or use it up.
    let opened = get(&site.app, &path, None).await;
    assert_eq!(opened.status, StatusCode::OK);
    assert!(opened.body.contains("Log in to Acme"));
    assert!(opened.cookie("admin_token").is_none());

    let used = post(&site.app, &path, "", None).await;
    assert_eq!(used.status, StatusCode::SEE_OTHER, "{}", used.body);
    assert_eq!(used.location(), "/admin/");
    let session = format!("admin_token={}", used.cookie("admin_token").unwrap());
    assert_eq!(
        get(&site.app, "/admin/", Some(&session)).await.status,
        StatusCode::OK
    );

    // Once is all.
    let again = post(&site.app, &path, "", None).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert!(again.body.contains("expired or was already used"));
    assert!(again.cookie("admin_token").is_none());
}

#[tokio::test]
async fn unknown_addresses_get_the_same_answer_and_no_message() {
    let site = site(|_| {}).await;
    let known = post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
    let unknown = post(&site.app, "/admin/login/link", "email=eve%40acme.com", None).await;
    assert_eq!(known.status, unknown.status);
    assert_eq!(known.body, unknown.body);
    assert_eq!(site.outbox.count_after_a_moment().await, 1);
    assert_eq!(site.outbox.nth(0).await.to, "ada@acme.com");

    let bad = post(&site.app, "/admin/login/link", "email=not-an-address", None).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn asking_too_often_is_limited() {
    let site = site(|_| {}).await;
    for _ in 0..5 {
        let reply = post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    let reply = post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(site.outbox.count_after_a_moment().await, 5);
}

#[tokio::test]
async fn forged_or_tampered_links_are_refused() {
    let site = site(|_| {}).await;
    post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
    let path = link_path(&site.outbox.nth(0).await);
    let mut tampered = path.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(
        post(&site.app, &tampered, "", None).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(&site.app, "/admin/login/link/not-a-token", "", None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // A login some other way retires links made before it.
    log_in(&site.app, "ada", PASSWORD).await;
    assert_eq!(
        post(&site.app, &path, "", None).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn login_links_ask_for_the_authenticator_code() {
    let site = site(|_| {}).await;
    enable_totp(&site.db, "ada").await.unwrap();
    post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
    let path = link_path(&site.outbox.nth(0).await);
    let used = post(&site.app, &path, "", None).await;
    assert_eq!(used.status, StatusCode::SEE_OTHER);
    assert_eq!(used.location(), "/admin/login/code");
    assert!(used.cookie("admin_token").is_none());
    assert!(used.cookie("hera_pending").is_some());
}

#[tokio::test]
async fn signup_by_login_link_creates_the_account() {
    let site = site(|s| {
        s.auth.methods = vec![AuthMethod::MagicLink];
        s.auth.signup = Signup::Domains(vec!["acme.com".into()]);
    })
    .await;
    let page = get(&site.app, "/admin/login", None).await;
    assert!(!page.body.contains("name=\"password\""));
    assert!(!page.body.contains("Forgot your password?"));

    post(
        &site.app,
        "/admin/login/link",
        "email=grace%40acme.com",
        None,
    )
    .await;
    post(
        &site.app,
        "/admin/login/link",
        "email=eve%40elsewhere.org",
        None,
    )
    .await;
    assert_eq!(site.outbox.count_after_a_moment().await, 1);
    let path = link_path(&site.outbox.nth(0).await);
    assert!(
        account(&site.db, "grace@acme.com").await.is_none(),
        "not before the link is used"
    );
    let used = post(&site.app, &path, "", None).await;
    assert_eq!(used.status, StatusCode::SEE_OTHER, "{}", used.body);
    let grace = account(&site.db, "grace@acme.com").await.expect("created");
    assert!(grace.is_staff && !grace.is_superuser);
    assert_eq!(grace.email.as_deref(), Some("grace@acme.com"));
}

#[tokio::test]
async fn a_forgotten_password_is_reset_by_e_mail() {
    let site = site(|_| {}).await;
    let form = get(&site.app, "/admin/password/reset", None).await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains("Forgot your password?"));

    let unknown = post(
        &site.app,
        "/admin/password/reset",
        "email=eve%40acme.com",
        None,
    )
    .await;
    let known = post(
        &site.app,
        "/admin/password/reset",
        "email=ada%40acme.com",
        None,
    )
    .await;
    assert_eq!(known.status, StatusCode::OK);
    assert_eq!(known.body, unknown.body);
    assert_eq!(site.outbox.count_after_a_moment().await, 1);
    let email = site.outbox.nth(0).await;
    assert_eq!(email.subject, "Choose a new password for Acme");
    let path = link_path(&email);
    assert!(path.starts_with("/admin/password/reset/"));

    let page = get(&site.app, &path, None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Choose a new password"));
    let mismatch = post(
        &site.app,
        &path,
        "password=new-secret-1&password2=new-secret-2",
        None,
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(mismatch.body.contains("different"));
    let short = post(&site.app, &path, "password=short&password2=short", None).await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);

    let done = post(
        &site.app,
        &path,
        "password=new-secret-1&password2=new-secret-1",
        None,
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert!(done.body.contains("Your password is changed"));
    assert!(
        log_in(&site.app, "ada", PASSWORD)
            .await
            .cookie("admin_token")
            .is_none()
    );
    assert!(
        log_in(&site.app, "ada", "new-secret-1")
            .await
            .cookie("admin_token")
            .is_some()
    );

    // The link retires with the old password.
    let reused = post(
        &site.app,
        &path,
        "password=another-one-2&password2=another-one-2",
        None,
    )
    .await;
    assert_eq!(reused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        get(&site.app, &path, None).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn resets_need_the_sites_address() {
    let site = site(|s| {
        s.server.public_url = None;
        s.auth.methods = vec![AuthMethod::Password];
    })
    .await;
    let page = get(&site.app, "/admin/login", None).await;
    assert!(!page.body.contains("Forgot your password?"));
    // Nothing is mounted there: the admin asks for a login, as for any unknown page.
    let reset = get(&site.app, "/admin/password/reset", None).await;
    assert_ne!(reset.status, StatusCode::OK);
    assert!(!reset.body.contains("Forgot your password?"));
}

#[tokio::test]
async fn accounts_change_their_own_password() {
    let site = site(|_| {}).await;
    let session = format!(
        "admin_token={}",
        log_in(&site.app, "ada", PASSWORD)
            .await
            .cookie("admin_token")
            .unwrap()
    );
    let page = get(&site.app, "/admin/auth/password", Some(&session)).await;
    assert_eq!(page.status, StatusCode::OK);
    let csrf = page
        .body
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let change = |current: &str, new: &str, again: &str| {
        format!(
            "_csrf={csrf}&current={}&password={new}&password2={again}",
            current.replace(' ', "+")
        )
    };

    let wrong = post(
        &site.app,
        "/admin/auth/password",
        &change("nope", "brand-new-1", "brand-new-1"),
        Some(&session),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wrong.body.contains("current password is not correct"));
    let mismatch = post(
        &site.app,
        "/admin/auth/password",
        &change(PASSWORD, "brand-new-1", "brand-new-2"),
        Some(&session),
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    let no_csrf = post(
        &site.app,
        "/admin/auth/password",
        "current=x&password=y&password2=y",
        Some(&session),
    )
    .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);

    let done = post(
        &site.app,
        "/admin/auth/password",
        &change(PASSWORD, "brand-new-1", "brand-new-1"),
        Some(&session),
    )
    .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    assert_eq!(
        done.location(),
        "/admin/auth/password?notice=password_changed"
    );
    assert!(
        log_in(&site.app, "ada", "brand-new-1")
            .await
            .cookie("admin_token")
            .is_some()
    );
}

#[tokio::test]
async fn mail_settings_are_checked() {
    let db = common::database().await;
    let with = |mail: MailSettings| {
        let settings = Settings {
            secret_key: SECRET.into(),
            mail,
            ..Default::default()
        };
        match App::with_settings(db.clone(), settings) {
            Err(AdminError::Config(message)) => message,
            Err(other) => panic!("{other}"),
            Ok(_) => String::new(),
        }
    };
    assert!(
        with(MailSettings {
            url: Some("smtps://u:p@smtp.example.com".into()),
            from: None,
        })
        .starts_with("mail.from")
    );
    assert!(
        with(MailSettings {
            url: Some("smtp://u:p@smtp.example.com".into()),
            from: Some("Acme <noreply@acme.com>".into()),
        })
        .contains("unencrypted")
    );
    assert!(
        with(MailSettings {
            url: Some("smtps://u:p@smtp.example.com".into()),
            from: Some("Acme <noreply@acme.com>".into()),
        })
        .is_empty()
    );

    // Login links need the site's address to link to.
    let settings = Settings {
        secret_key: SECRET.into(),
        auth: AuthSettings {
            methods: vec![AuthMethod::MagicLink],
            ..Default::default()
        },
        ..Default::default()
    };
    match App::with_settings(db.clone(), settings) {
        Err(AdminError::Config(message)) => assert!(message.contains("public_url"), "{message}"),
        _ => panic!("login links without public_url must be refused"),
    }
}

/// Logs in as `ada` (a superuser) and returns the session cookie and a CSRF token.
async fn superuser_session(site: &Site) -> (String, String) {
    let session = format!(
        "admin_token={}",
        log_in(&site.app, "ada", PASSWORD)
            .await
            .cookie("admin_token")
            .unwrap()
    );
    let page = get(&site.app, "/admin/auth/users/new", Some(&session)).await;
    let csrf = page
        .body
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    (session, csrf)
}

#[tokio::test]
async fn invited_people_choose_their_own_password() {
    let site = site(|_| {}).await;
    let (session, csrf) = superuser_session(&site).await;
    let form = get(&site.app, "/admin/auth/users/new", Some(&session)).await;
    assert!(form.body.contains("Send an invitation instead"));

    let without_email = post(
        &site.app,
        "/admin/auth/users/new",
        &format!("_csrf={csrf}&username=grace&email=&is_active=true&is_staff=true&invite=true"),
        Some(&session),
    )
    .await;
    assert_eq!(without_email.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(account(&site.db, "grace").await.is_none());

    let created = post(
        &site.app,
        "/admin/auth/users/new",
        &format!(
            "_csrf={csrf}&username=grace&email=grace%40acme.com&is_active=true&is_staff=true&invite=true"
        ),
        Some(&session),
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    assert_eq!(created.location(), "/admin/auth/users?notice=user_invited");
    let email = site.outbox.nth(0).await;
    assert_eq!(email.to, "grace@acme.com");
    assert_eq!(email.subject, "You are invited to Acme");
    assert!(
        email
            .text
            .contains("ada invited you to Acme, with the username grace")
    );
    let path = link_path(&email);
    assert!(path.starts_with("/admin/invite/"), "{path}");
    // Nobody knows the password in the meantime.
    assert!(
        log_in(&site.app, "grace", "")
            .await
            .cookie("admin_token")
            .is_none()
    );

    let welcome = get(&site.app, &path, None).await;
    assert_eq!(welcome.status, StatusCode::OK);
    assert!(welcome.body.contains("Welcome to Acme"));
    let mismatch = post(
        &site.app,
        &path,
        "password=grace-secret-1&password2=other",
        None,
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    let accepted = post(
        &site.app,
        &path,
        "password=grace-secret-1&password2=grace-secret-1",
        None,
    )
    .await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER, "{}", accepted.body);
    assert_eq!(accepted.location(), "/admin/");
    assert!(
        accepted.cookie("admin_token").is_some(),
        "accepting logs in"
    );
    assert!(
        log_in(&site.app, "grace", "grace-secret-1")
            .await
            .cookie("admin_token")
            .is_some()
    );

    // An invitation works once.
    let again = post(
        &site.app,
        &path,
        "password=x-secret-22&password2=x-secret-22",
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invitations_can_be_sent_again_and_work_without_passwords() {
    let site = site(|s| s.auth.methods = vec![AuthMethod::MagicLink, AuthMethod::Password]).await;
    let (session, csrf) = superuser_session(&site).await;
    post(
        &site.app,
        "/admin/auth/users/new",
        &format!("_csrf={csrf}&username=grace&password=grace-secret-1&email=grace%40acme.com&is_active=true&is_staff=true"),
        Some(&session),
    )
    .await;
    assert_eq!(
        site.outbox.count_after_a_moment().await,
        0,
        "no invitation unless asked"
    );
    let grace = account(&site.db, "grace").await.unwrap();
    let page = get(
        &site.app,
        &format!("/admin/auth/users/{}", grace.id),
        Some(&session),
    )
    .await;
    assert!(page.body.contains("Send invitation"));
    let sent = post(
        &site.app,
        &format!("/admin/auth/users/{}/invite", grace.id),
        &format!("_csrf={csrf}"),
        Some(&session),
    )
    .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER);
    assert!(sent.location().ends_with("?notice=invitation_sent"));
    let email = site.outbox.nth(0).await;
    assert!(email.text.contains("login link at any time"));

    // Without passwords, the invitation is a login.
    let site = self::site(|s| s.auth.methods = vec![AuthMethod::MagicLink]).await;
    let ada = account(&site.db, "ada").await.unwrap();
    let session = format!("admin_token={}", {
        post(&site.app, "/admin/login/link", "email=ada%40acme.com", None).await;
        let path = link_path(&site.outbox.nth(0).await);
        post(&site.app, &path, "", None)
            .await
            .cookie("admin_token")
            .unwrap()
    });
    let page = get(
        &site.app,
        &format!("/admin/auth/users/{}", ada.id),
        Some(&session),
    )
    .await;
    let csrf = page
        .body
        .split("name=\"_csrf\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let created = post(
        &site.app,
        "/admin/auth/users/new",
        &format!("_csrf={csrf}&username=lin&email=lin%40acme.com&is_active=true&is_staff=true&invite=true"),
        Some(&session),
    )
    .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let path = link_path(&site.outbox.nth(1).await);
    let welcome = get(&site.app, &path, None).await;
    assert!(welcome.body.contains("Your account is ready"));
    assert!(!welcome.body.contains("name=\"password\""));
    let accepted = post(&site.app, &path, "", None).await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);
    assert!(accepted.cookie("admin_token").is_some());
}

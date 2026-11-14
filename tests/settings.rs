//! Project settings: one `Settings` value configures the whole app, and mistakes are reported
//! with the name of the setting.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use heraapi::settings::*;
use heraapi::{AdminError, App};
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

fn settings() -> Settings {
    Settings {
        secret_key: SECRET.into(),
        database: DatabaseSettings {
            url: "sqlite::memory:".into(),
            max_connections: Some(1),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn get(app: &Router, path: &str, accept: &str) -> (StatusCode, String) {
    let request = Request::get(path)
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn config_error(result: Result<App, AdminError>) -> String {
    match result {
        Err(AdminError::Config(message)) => message,
        Err(other) => panic!("expected a configuration error, got {other}"),
        Ok(_) => panic!("expected a configuration error, got an app"),
    }
}

#[tokio::test]
async fn settings_configure_the_whole_app() {
    let mut custom = settings();
    custom.admin = AdminSettings {
        path: "/backoffice".into(),
        site_title: "Acme back office".into(),
        accent_color: "#0a7f5a".into(),
        ..Default::default()
    };
    custom.api.path = "/v1".into();
    custom.home = HomeSettings {
        title: Some("Acme status".into()),
        tagline: Some("All systems go".into()),
        ..Default::default()
    };
    custom.docs.enabled = false;

    let app = App::from_settings(custom).await.unwrap();
    app.migrate().await.unwrap();
    assert!(app.openapi().is_err(), "the documentation is off");
    let router = app.router().unwrap();

    let (status, body) = get(&router, "/backoffice/login", "text/html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Acme back office"), "{body}");
    assert!(body.contains("#0a7f5a"));
    let (status, _) = get(&router, "/admin/login", "text/html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The API moved too.
    let (status, _) = get(&router, "/v1/auth/me", "application/json").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get(&router, "/api/auth/me", "application/json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(&router, "/", "text/html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Acme status"));
    assert!(body.contains("All systems go"));
}

#[tokio::test]
async fn protected_docs_ask_for_the_configured_password() {
    let mut custom = settings();
    custom.docs.username = Some("docs".into());
    custom.docs.password = Some("open sesame".into());
    let app = App::from_settings(custom).await.unwrap();
    let router = app.router().unwrap();
    let (status, _) = get(&router, "/api/openapi.json", "application/json").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_home_page_can_be_turned_off() {
    let mut custom = settings();
    custom.home.enabled = false;
    let router = App::from_settings(custom).await.unwrap().router().unwrap();
    let (status, _) = get(&router, "/", "text/html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mistakes_name_the_setting() {
    let db = common::database().await;
    let check = |change: fn(&mut Settings)| {
        let mut custom = settings();
        change(&mut custom);
        config_error(App::with_settings(db.clone(), custom))
    };

    assert!(check(|s| s.secret_key = String::new()).starts_with("secret_key is empty"));
    assert!(check(|s| s.secret_key = "short".into()).starts_with("secret_key: "));
    assert!(check(|s| s.admin.accent_color = "red".into()).starts_with("admin.accent_color: "));
    assert!(check(|s| s.admin.page_size = 0).starts_with("admin.page_size"));
    assert!(
        check(|s| s.server.cors_origins = vec!["not an origin".into()])
            .starts_with("server.cors_origins: ")
    );
    assert!(
        check(|s| s.server.client_ip_header = Some("bad header".into()))
            .starts_with("server.client_ip_header: ")
    );
    assert!(check(|s| s.docs.username = Some("docs".into())).starts_with("docs.username"));
    assert!(
        check(|s| s.uploads.storage_url = Some("nonsense".into()))
            .starts_with("uploads.storage_url: ")
    );

    // A bad path is caught when the routes are built.
    let mut custom = settings();
    custom.admin.path = "no-slash".into();
    let app = App::with_settings(db.clone(), custom).unwrap();
    assert!(app.router().is_err());
}

#[tokio::test]
async fn from_settings_checks_before_connecting() {
    // An unreachable database must not hide the real mistake, nor make the check wait.
    let mut custom = settings();
    custom.database.url = "postgres://nobody@127.0.0.1:1/none".into();
    custom.admin.accent_color = "blue".into();
    let message = config_error(App::from_settings(custom).await);
    assert!(message.starts_with("admin.accent_color: "), "{message}");

    let mut custom = settings();
    custom.database.url = String::new();
    let message = config_error(App::from_settings(custom).await);
    assert!(message.starts_with("database.url is empty"), "{message}");

    let mut custom = settings();
    custom.admin.path = "no-slash".into();
    let message = config_error(App::from_settings(custom).await);
    assert!(message.starts_with("admin.path: "), "{message}");
}

#[test]
fn debug_output_hides_secrets() {
    let mut custom = settings();
    custom.database.url = "postgres://app:hunter2@db/app".into();
    custom.docs.password = Some("open sesame".into());
    let text = format!("{custom:?}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("open sesame"), "{text}");
    assert!(text.contains("postgres:<hidden>"), "{text}");
}

#[test]
fn environment_helpers_read_parse_and_name_the_variable() {
    // Names no other test uses, set once before anything reads them.
    // SAFETY: this is the only test in this file that touches the environment, and the other
    // tests here only read variables that are never written.
    unsafe {
        std::env::set_var("HERA_TEST_FLAG_YES", "yes");
        std::env::set_var("HERA_TEST_FLAG_OFF", "off");
        std::env::set_var("HERA_TEST_NUMBER", " 42 ");
        std::env::set_var("HERA_TEST_BAD_NUMBER", "forty-two");
        std::env::set_var("HERA_TEST_LEVEL", "debug");
        std::env::set_var("HERA_TEST_EMPTY", "");
    }
    assert!(env_or("HERA_TEST_FLAG_YES", false).unwrap());
    assert!(!env_or("HERA_TEST_FLAG_OFF", true).unwrap());
    assert!(env_or("HERA_TEST_UNSET", true).unwrap());
    assert_eq!(env_or("HERA_TEST_NUMBER", 0_u32).unwrap(), 42);
    assert_eq!(
        env_or("HERA_TEST_LEVEL", LogLevel::INFO).unwrap(),
        LogLevel::DEBUG
    );
    match env_or("HERA_TEST_BAD_NUMBER", 0_u32) {
        Err(AdminError::Config(message)) => assert!(message.contains("HERA_TEST_BAD_NUMBER")),
        other => panic!("{other:?}"),
    }
    match env("HERA_TEST_UNSET") {
        Err(AdminError::Config(message)) => assert!(message.starts_with("HERA_TEST_UNSET")),
        other => panic!("{other:?}"),
    }
    // Empty counts as unset.
    assert!(env("HERA_TEST_EMPTY").is_err());
    assert_eq!(env_opt("HERA_TEST_EMPTY"), None);
    assert_eq!(env("HERA_TEST_NUMBER").unwrap(), " 42 ");
}

//! The layers around the routes: time limit, request log, cross-origin access and protective
//! headers.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::get;
use cadmus::{App, AuthConfig, ModelOptions, Storage, create_superuser};
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
const BOUNDARY: &str = "----test-boundary";

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(400)).await;
    "finally"
}

fn new_app(db: &DatabaseConnection) -> App {
    let routes = Router::new()
        .route("/hello", get(|| async { "hello" }))
        .route("/slow", get(slow))
        .route("/api/slow", get(slow))
        .route("/admin/extra/slow", get(slow));
    App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .model::<note::Entity>(ModelOptions::new())
        .storage(Storage::memory())
        .routes(routes)
}

async fn database() -> DatabaseConnection {
    let db = common::database().await;
    new_app(&db).migrate().await.unwrap();
    create_superuser(&db, "root", PASSWORD).await.unwrap();
    db
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn header(&self, name: impl header::AsHeaderName) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
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

async fn fetch(app: &Router, method: Method, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn token(app: &Router) -> String {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "username": "root", "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    let reply = send(app, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.json()["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn slow_requests_are_cut_off_but_file_transfers_are_not() {
    let db = database().await;
    // Logging in hashes a password, which is slower than the limit used here.
    let patient = new_app(&db).request_timeout(None).router().unwrap();
    let token = token(&patient).await;
    let bearer = format!("Bearer {token}");
    let app = new_app(&db)
        .request_timeout(Some(Duration::from_millis(150)))
        .router()
        .unwrap();

    // Each area reports it in its own format.
    let reply = fetch(&app, Method::GET, "/slow", &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body, "the request took too long");
    let reply = fetch(&app, Method::GET, "/api/slow", &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.json()["error"]["code"], "timeout");
    let reply = fetch(&app, Method::GET, "/admin/extra/slow", &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(reply.body.contains("<main id=\"main\""));

    // Ordinary requests are unaffected.
    assert_eq!(fetch(&app, Method::GET, "/hello", &[]).await.body, "hello");
    let reply = fetch(
        &app,
        Method::GET,
        "/api/note",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    // An upload that arrives slower than the limit still goes through.
    let whole = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello slow world\r\n--{BOUNDARY}--\r\n"
    );
    let (first, second) = whole.split_at(whole.len() / 2);
    let parts = vec![first.to_owned(), second.to_owned()];
    let trickle = futures_util::stream::unfold(parts.into_iter(), |mut rest| async move {
        let part = rest.next()?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        Some((Ok::<_, std::io::Error>(part), rest))
    });
    let request = Request::post("/api/files")
        .header(header::AUTHORIZATION, &bearer)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from_stream(trickle))
        .unwrap();
    let reply = send(&app, request).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(reply.json()["size"], 16);

    // Without a limit, the slow handler finishes.
    assert_eq!(
        fetch(&patient, Method::GET, "/slow", &[]).await.body,
        "finally"
    );
}

#[tokio::test]
async fn cross_origin_access_is_off_unless_origins_are_named() {
    let db = database().await;
    let origin = ("origin", "https://app.example.com");
    let preflight = [
        origin,
        ("access-control-request-method", "PATCH"),
        (
            "access-control-request-headers",
            "authorization,content-type",
        ),
    ];

    // By default a browser on another origin gets nothing it can use.
    let closed = new_app(&db).router().unwrap();
    let reply = fetch(&closed, Method::OPTIONS, "/api/note/1", &preflight).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(reply.header("access-control-allow-origin").is_none());
    let reply = fetch(&closed, Method::GET, "/api/note", &[origin]).await;
    assert!(reply.header("access-control-allow-origin").is_none());

    let app = new_app(&db)
        .cors_origins(["https://app.example.com, http://localhost:5173"])
        .unwrap()
        .router()
        .unwrap();

    // The preflight is answered without a login.
    let reply = fetch(&app, Method::OPTIONS, "/api/note/1", &preflight).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    let methods = reply.header("access-control-allow-methods").unwrap();
    assert!(
        methods.contains("PATCH") && methods.contains("DELETE"),
        "{methods}"
    );
    let headers = reply
        .header("access-control-allow-headers")
        .unwrap()
        .to_lowercase();
    assert!(
        headers.contains("authorization") && headers.contains("upload-offset"),
        "{headers}"
    );
    // Cookies are never allowed across origins.
    assert!(reply.header("access-control-allow-credentials").is_none());

    // Real requests carry the header, also when they fail, so the client can read the error.
    let bearer = format!("Bearer {}", token(&app).await);
    let reply = fetch(
        &app,
        Method::GET,
        "/api/note",
        &[origin, ("authorization", &bearer)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    let reply = fetch(&app, Method::GET, "/api/note", &[origin]).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    let reply = fetch(
        &app,
        Method::GET,
        "/api/note",
        &[("origin", "http://localhost:5173")],
    )
    .await;
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("http://localhost:5173")
    );

    // An origin that was not named gets no permission.
    let stranger = [
        ("origin", "https://evil.example"),
        preflight[1],
        preflight[2],
    ];
    let reply = fetch(&app, Method::OPTIONS, "/api/note/1", &stranger).await;
    assert!(reply.header("access-control-allow-origin").is_none());
    let reply = fetch(
        &app,
        Method::GET,
        "/api/note",
        &[("origin", "https://evil.example")],
    )
    .await;
    assert!(reply.header("access-control-allow-origin").is_none());

    // `*` allows everyone.
    let open = new_app(&db).cors_origins(["*"]).unwrap().router().unwrap();
    let reply = fetch(
        &open,
        Method::GET,
        "/api/note",
        &[("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(reply.header("access-control-allow-origin"), Some("*"));

    // Anything that is not exactly an origin is a configuration error, not a silent no-op.
    for bad in [
        "app.example.com",
        "https://app.example.com/",
        "https://app.example.com/path",
        "ftp://x.example",
    ] {
        assert!(new_app(&db).cors_origins([bad]).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn admin_and_api_responses_carry_protective_headers() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();

    for path in [
        "/admin/login",
        "/admin/",
        "/api/note",
        "/api/nothing/at/all",
        "/admin/nothing/at/all",
    ] {
        let reply = fetch(&app, Method::GET, path, &[]).await;
        assert_eq!(
            reply.header(header::X_CONTENT_TYPE_OPTIONS),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(
            reply.header(header::X_FRAME_OPTIONS),
            Some("DENY"),
            "{path}"
        );
        assert!(
            reply
                .header(header::CONTENT_SECURITY_POLICY)
                .is_some_and(|policy| policy.contains("frame-ancestors 'none'")),
            "{path}"
        );
        assert_eq!(
            reply.header("cross-origin-opener-policy"),
            Some("same-origin"),
            "{path}"
        );
        assert_eq!(
            reply.header(header::REFERRER_POLICY),
            Some("same-origin"),
            "{path}"
        );
        assert_eq!(
            reply.header(header::CACHE_CONTROL),
            Some("no-store"),
            "{path}"
        );
    }
    // The application's own routes are its own business.
    let reply = fetch(&app, Method::GET, "/hello", &[]).await;
    assert!(reply.header(header::X_FRAME_OPTIONS).is_none());
    assert!(reply.header(header::CACHE_CONTROL).is_none());
}

/// Collects what the log would print.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn every_request_is_logged_without_its_query_string() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();

    // A process-wide subscriber: one that is set for this thread only can miss events while
    // other tests in this binary register the same log call sites at the same moment.
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("only this test sets one");

    // Paths no other test uses, since every test's requests now reach this log.
    fetch(&app, Method::GET, "/log-probe?secret=hunter2", &[]).await;
    fetch(&app, Method::GET, "/api/log-probe?q=private+search", &[]).await;
    fetch(&app, Method::POST, "/log-probe-post", &[]).await;

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let ours: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(" request ") && l.contains("log-probe"))
        .collect();
    assert_eq!(ours.len(), 3, "{log}");
    assert!(
        ours[0].contains("method=GET path=/log-probe status=404 ms="),
        "{log}"
    );
    assert!(
        ours[1].contains("method=GET path=/api/log-probe status=401 ms="),
        "{log}"
    );
    assert!(
        ours[2].contains("method=POST path=/log-probe-post status=404 ms="),
        "{log}"
    );
    assert!(
        !log.contains("hunter2") && !log.contains("private"),
        "{log}"
    );
}

#[tokio::test]
async fn admin_pages_run_only_their_own_scripts() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();

    for path in ["/admin/login", "/admin/nothing/at/all", "/"] {
        let reply = fetch(&app, Method::GET, path, &[]).await;
        let policy = reply.header(header::CONTENT_SECURITY_POLICY).unwrap();
        let nonce = policy
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap_or_else(|| panic!("{path}: no nonce in {policy}"));
        assert!(policy.contains("script-src 'self' 'nonce-"), "{policy}");
        assert!(policy.contains("object-src 'none'"), "{policy}");
        let scripts = reply.body.matches("<script").count();
        assert!(scripts > 0, "{path}");
        assert_eq!(
            reply
                .body
                .matches(&format!("<script nonce=\"{nonce}\""))
                .count(),
            scripts,
            "{path}: every script carries this response's nonce"
        );
        // A new one for every response.
        let again = fetch(&app, Method::GET, path, &[]).await;
        assert_ne!(again.header(header::CONTENT_SECURITY_POLICY), Some(policy));
    }
    // JSON keeps the short policy.
    let reply = fetch(&app, Method::GET, "/api/note", &[]).await;
    assert_eq!(
        reply.header(header::CONTENT_SECURITY_POLICY),
        Some("frame-ancestors 'none'")
    );
}

#[tokio::test]
async fn browsers_are_told_to_keep_to_https_unless_turned_off() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();
    for path in ["/admin/login", "/api/note", "/hello"] {
        let reply = fetch(&app, Method::GET, path, &[]).await;
        assert_eq!(
            reply.header(header::STRICT_TRANSPORT_SECURITY),
            Some("max-age=31536000"),
            "{path}"
        );
    }
    let app = new_app(&db).hsts(None).router().unwrap();
    let reply = fetch(&app, Method::GET, "/admin/login", &[]).await;
    assert!(reply.header(header::STRICT_TRANSPORT_SECURITY).is_none());
}

async fn fetch_bytes(app: &Router, path: &str, headers: &[(&str, &str)]) -> (HeaderMap, Vec<u8>) {
    let mut request = Request::get(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.headers,
        body.collect().await.unwrap().to_bytes().to_vec(),
    )
}

#[tokio::test]
async fn api_responses_are_compressed_and_admin_pages_are_not() {
    let db = database().await;
    let app = new_app(&db).router().unwrap();
    let bearer = format!("Bearer {}", token(&app).await);
    for i in 0..20 {
        let request = Request::post("/api/note")
            .header(header::AUTHORIZATION, &bearer)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "text": format!("note number {i}") }).to_string(),
            ))
            .unwrap();
        assert_eq!(send(&app, request).await.status, StatusCode::CREATED);
    }
    let gzip = [
        ("authorization", bearer.as_str()),
        ("accept-encoding", "gzip"),
    ];
    let (headers, body) = fetch_bytes(&app, "/api/note", &gzip).await;
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
    assert!(
        headers[header::VARY]
            .to_str()
            .unwrap()
            .contains("accept-encoding")
    );
    assert_eq!(&body[..2], &[0x1f, 0x8b], "a gzip stream");
    let br = [
        ("authorization", bearer.as_str()),
        ("accept-encoding", "br"),
    ];
    let (headers, _) = fetch_bytes(&app, "/api/note", &br).await;
    assert_eq!(headers[header::CONTENT_ENCODING], "br");
    // Without asking, plain JSON.
    let (headers, body) = fetch_bytes(&app, "/api/note", &gzip[..1]).await;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    assert!(serde_json::from_slice::<Value>(&body).is_ok());

    // Pages with a CSRF token next to the visitor's own text never are.
    let (headers, _) = fetch_bytes(&app, "/admin/login", &[("accept-encoding", "gzip, br")]).await;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
    let (headers, _) = fetch_bytes(&app, "/hello", &[("accept-encoding", "gzip")]).await;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());

    let app = new_app(&db).compress_api(false).router().unwrap();
    let (headers, _) = fetch_bytes(&app, "/api/note", &gzip).await;
    assert!(headers.get(header::CONTENT_ENCODING).is_none());
}

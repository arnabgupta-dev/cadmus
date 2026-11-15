//! Request rate limits: per address, per account and per API key, rules, model limits, the
//! layer for your own routes, a store of your own, and what is never counted.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::{get, post};
use cadmus::settings::RateLimitSettings;
use cadmus::{
    AdminError, App, AuthConfig, Decision, ModelOptions, Rate, RateLimit, RateLimitStore, RateRule,
    create_superuser,
};
use http_body_util::BodyExt;
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

fn limits() -> RateLimitSettings {
    RateLimitSettings {
        anonymous: Rate::per_minute(3),
        logged_in: Rate::per_minute(5),
        ..Default::default()
    }
}

async fn site(limits: RateLimitSettings, store: Option<Arc<dyn RateLimitStore>>) -> Router {
    site_with(limits, store, ModelOptions::new()).await
}

async fn site_with(
    limits: RateLimitSettings,
    store: Option<Arc<dyn RateLimitStore>>,
    note_options: ModelOptions,
) -> Router {
    let db = common::database().await;
    let auth = AuthConfig::new(SECRET)
        .unwrap()
        .client_ip_header("X-Forwarded-For")
        .unwrap();
    let routes = Router::new()
        .route(
            "/open",
            get(|| async { "hello" }).post(|| async { "posted" }),
        )
        .route(
            "/sms",
            post(|| async { "sent" }).layer(RateLimit::new("sms", Rate::per_minute(1))),
        );
    let mut app = App::new(db.clone(), auth)
        .model::<note::Entity>(note_options)
        .routes(routes)
        .rate_limit(limits);
    if let Some(store) = store {
        app = app.rate_limit_store(Shared(store));
    }
    app.migrate().await.unwrap();
    create_superuser(&db, "ann", PASSWORD).await.unwrap();
    create_superuser(&db, "bob", PASSWORD).await.unwrap();
    app.router().unwrap()
}

/// Passes a store the test keeps a handle on.
struct Shared(Arc<dyn RateLimitStore>);

#[async_trait]
impl RateLimitStore for Shared {
    async fn hit(&self, key: &str, rate: Rate) -> Result<Decision, AdminError> {
        self.0.hit(key, rate).await
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn from(method: Method, path: &str, address: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header("X-Forwarded-For", address)
}

async fn call(app: &Router, method: Method, path: &str, address: &str) -> Reply {
    send(
        app,
        from(method, path, address).body(Body::empty()).unwrap(),
    )
    .await
}

async fn call_as(app: &Router, path: &str, address: &str, token: &str) -> Reply {
    send(
        app,
        from(Method::GET, path, address)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn login(app: &Router, username: &str, address: &str) -> String {
    let reply = send(
        app,
        from(Method::POST, "/api/auth/token", address)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "username": username, "password": PASSWORD }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    reply.json()["access_token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn visitors_are_limited_by_address() {
    let app = site(limits(), None).await;
    for left in ["2", "1", "0"] {
        let reply = call(&app, Method::GET, "/open", "10.0.0.1").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.header("ratelimit-limit"), Some("3"));
        assert_eq!(reply.header("ratelimit-remaining"), Some(left));
        assert!(reply.header("ratelimit-reset").is_some());
    }
    let refused = call(&app, Method::GET, "/open", "10.0.0.1").await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("retry-after"), Some("20"));
    assert_eq!(refused.header("ratelimit-remaining"), Some("0"));

    // Another address has its own allowance.
    assert_eq!(
        call(&app, Method::GET, "/open", "10.0.0.2").await.status,
        StatusCode::OK
    );

    // Under the API the refusal is JSON; the allowance is shared with the rest of the site.
    let api = call(&app, Method::GET, "/api/note", "10.0.0.1").await;
    assert_eq!(api.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(api.json()["error"]["code"], "rate_limited", "{}", api.text);
}

#[tokio::test]
async fn accounts_have_their_own_allowance() {
    let app = site(limits(), None).await;
    // Logging in counts as a visitor's request; use other addresses for it.
    let ann = login(&app, "ann", "10.0.1.1").await;
    let bob = login(&app, "bob", "10.0.1.2").await;
    // Both behind one address, each with five requests: more than a visitor's three.
    for _ in 0..5 {
        assert_eq!(
            call_as(&app, "/api/note", "10.0.0.9", &ann).await.status,
            StatusCode::OK
        );
        assert_eq!(
            call_as(&app, "/api/note", "10.0.0.9", &bob).await.status,
            StatusCode::OK
        );
    }
    let refused = call_as(&app, "/api/note", "10.0.0.9", &ann).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("ratelimit-limit"), Some("5"));
    // The address itself has not been used.
    assert_eq!(
        call(&app, Method::GET, "/open", "10.0.0.9").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_forged_token_counts_as_a_visitor() {
    let app = site(limits(), None).await;
    for _ in 0..3 {
        let reply = call_as(&app, "/api/note", "10.0.0.3", "made.up.token").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.header("ratelimit-limit"), Some("3"));
    }
    let refused = call_as(&app, "/api/note", "10.0.0.3", "another.made.up").await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    // So does a key that does not exist.
    let key = format!("cadmus_{}", "A".repeat(40));
    let app = site(limits(), None).await;
    for _ in 0..3 {
        call_as(&app, "/api/note", "10.0.0.4", &key).await;
    }
    assert_eq!(
        call_as(&app, "/api/note", "10.0.0.4", &key).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn an_api_key_has_its_own_allowance() {
    let app = site(limits(), None).await;
    let ann = login(&app, "ann", "10.0.2.1").await;
    let made = send(
        &app,
        from(Method::POST, "/api/auth/me/api-keys", "10.0.2.1")
            .header(header::AUTHORIZATION, format!("Bearer {ann}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "name": "script" }).to_string()))
            .unwrap(),
    )
    .await;
    assert!(made.status.is_success(), "{}", made.text);
    let key = made.json()["key"].as_str().unwrap().to_owned();
    // The key's five, separate from ann's logins (two used above).
    for _ in 0..5 {
        let reply = call_as(&app, "/api/note", "10.0.2.2", &key).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(
        call_as(&app, "/api/note", "10.0.2.2", &key).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call_as(&app, "/api/note", "10.0.2.2", &ann).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn rules_and_model_limits_replace_the_default() {
    let settings = RateLimitSettings {
        rules: vec![RateRule::new("/open", Rate::per_minute(1)).methods(["POST"])],
        ..limits()
    };
    let app = site_with(
        settings,
        None,
        ModelOptions::new().rate_limit(Rate::per_minute(10)),
    )
    .await;
    // The rule: one POST, with its own counter.
    assert_eq!(
        call(&app, Method::POST, "/open", "10.0.3.1").await.status,
        StatusCode::OK
    );
    let refused = call(&app, Method::POST, "/open", "10.0.3.1").await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("ratelimit-limit"), Some("1"));
    // GET is not covered by the rule; the default three still apply.
    for _ in 0..3 {
        assert_eq!(
            call(&app, Method::GET, "/open", "10.0.3.1").await.status,
            StatusCode::OK
        );
    }
    // The model's own ten, more than a visitor's three.
    for _ in 0..10 {
        let reply = call(&app, Method::GET, "/api/note", "10.0.3.2").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.header("ratelimit-limit"), Some("10"));
    }
    assert_eq!(
        call(&app, Method::GET, "/api/note", "10.0.3.2")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn the_layer_limits_a_route_of_your_own() {
    let app = site(limits(), None).await;
    let first = call(&app, Method::POST, "/sms", "10.0.4.1").await;
    assert_eq!(first.status, StatusCode::OK);
    // The route's own allowance is the one described.
    assert_eq!(first.header("ratelimit-limit"), Some("1"));
    let refused = call(&app, Method::POST, "/sms", "10.0.4.1").await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.header("retry-after"), Some("60"));
}

#[tokio::test]
async fn turned_off_nothing_is_limited() {
    let app = site(
        RateLimitSettings {
            enabled: false,
            ..limits()
        },
        None,
    )
    .await;
    for _ in 0..10 {
        let reply = call(&app, Method::GET, "/open", "10.0.5.1").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.header("ratelimit-limit").is_none());
    }
}

#[tokio::test]
async fn preflights_admin_pages_and_unknown_callers_are_not_counted() {
    let app = site(limits(), None).await;
    for _ in 0..10 {
        assert_ne!(
            call(&app, Method::OPTIONS, "/api/note", "10.0.6.1")
                .await
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );
        let page = call(&app, Method::GET, "/admin/login", "10.0.6.1").await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.header("ratelimit-limit").is_none());
    }
    // The address is still untouched.
    assert_eq!(
        call(&app, Method::GET, "/open", "10.0.6.1")
            .await
            .header("ratelimit-remaining"),
        Some("2")
    );
    // Without an address (no proxy header, no connection info) there is nothing to count by.
    for _ in 0..10 {
        let reply = send(&app, Request::get("/open").body(Body::empty()).unwrap()).await;
        assert_eq!(reply.status, StatusCode::OK);
    }

    // Admin pages count once a limit is set for them.
    let app = site(
        RateLimitSettings {
            admin: Some(Rate::per_minute(2)),
            ..limits()
        },
        None,
    )
    .await;
    for _ in 0..2 {
        call(&app, Method::GET, "/admin/login", "10.0.6.2").await;
    }
    assert_eq!(
        call(&app, Method::GET, "/admin/login", "10.0.6.2")
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

/// Refuses every third request, and counts what it was asked.
#[derive(Default)]
struct Strict {
    calls: AtomicU32,
}

#[async_trait]
impl RateLimitStore for Strict {
    async fn hit(&self, key: &str, rate: Rate) -> Result<Decision, AdminError> {
        assert!(key.starts_with("default|ip:"), "{key}");
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Decision {
            allowed: !n.is_multiple_of(3),
            limit: rate.count(),
            remaining: 7,
            reset_secs: 9,
            retry_after_secs: 4,
        })
    }
}

/// Always fails.
struct Broken;

#[async_trait]
impl RateLimitStore for Broken {
    async fn hit(&self, _key: &str, _rate: Rate) -> Result<Decision, AdminError> {
        Err(AdminError::Config("the store is down".into()))
    }
}

#[tokio::test]
async fn a_store_of_your_own_decides() {
    let strict = Arc::new(Strict::default());
    let app = site(limits(), Some(strict.clone())).await;
    let replies = [
        call(&app, Method::GET, "/open", "10.0.7.1").await,
        call(&app, Method::GET, "/open", "10.0.7.1").await,
        call(&app, Method::GET, "/open", "10.0.7.1").await,
    ];
    assert_eq!(replies[0].header("ratelimit-remaining"), Some("7"));
    assert_eq!(replies[1].status, StatusCode::OK);
    assert_eq!(replies[2].status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(replies[2].header("retry-after"), Some("4"));
    assert_eq!(strict.calls.load(Ordering::SeqCst), 3);

    // A store that fails lets requests through rather than taking the site down.
    let app = site(limits(), Some(Arc::new(Broken))).await;
    for _ in 0..5 {
        let reply = call(&app, Method::GET, "/open", "10.0.7.2").await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.header("ratelimit-limit").is_none());
    }
}

#[test]
fn rates_come_from_text() {
    assert_eq!("90/min".parse::<Rate>().unwrap(), Rate::per_minute(90));
    let error = "lots".parse::<Rate>().unwrap_err().to_string();
    assert!(error.contains("120/min"), "{error}");
}

#[tokio::test]
async fn the_api_documentation_mentions_the_limits() {
    let app = site(limits(), None).await;
    let doc = call(&app, Method::GET, "/api/openapi.json", "10.0.8.1")
        .await
        .json();
    assert_eq!(
        doc["paths"]["/api/note"]["get"]["responses"]["429"]["$ref"],
        "#/components/responses/RateLimited"
    );
    let headers = &doc["components"]["responses"]["RateLimited"]["headers"];
    for name in [
        "Retry-After",
        "RateLimit-Limit",
        "RateLimit-Remaining",
        "RateLimit-Reset",
    ] {
        assert!(headers[name].is_object(), "{name}");
    }

    let app = site(
        RateLimitSettings {
            enabled: false,
            ..limits()
        },
        None,
    )
    .await;
    let doc = call(&app, Method::GET, "/api/openapi.json", "10.0.8.1")
        .await
        .json();
    assert!(doc["paths"]["/api/note"]["get"]["responses"]["429"].is_null());
}

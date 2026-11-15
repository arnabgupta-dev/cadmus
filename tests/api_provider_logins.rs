//! Logging in through another site from an app: `POST /api/auth/<provider>` with an ID token
//! or a code, against a fake OpenID Connect provider on this machine that signs its tokens.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cadmus::settings::*;
use cadmus::{App, create_superuser, enable_totp, set_email};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const CLIENT_ID: &str = "web-app";
const CLIENT_SECRET: &str = "web app secret";
const IOS_CLIENT_ID: &str = "ios-app";
const REDIRECT: &str = "https://app.example.com/auth/callback";
/// The provider's signing key; only its public half is published.
const KEY: &str = include_str!("fixtures/test_signing_key.pem");
const KEY_N: &str = include_str!("fixtures/test_signing_key.n");
/// A key the provider never published, as a forger would have.
const OTHER_KEY: &str = include_str!("fixtures/test_other_key.pem");

// ---- the fake provider ---------------------------------------------------------------------

struct Grant {
    claims: Value,
    challenge: Option<String>,
    redirect_uri: String,
}

#[derive(Default)]
struct Provider {
    issuer: String,
    grants: HashMap<String, Grant>,
    key_reads: u32,
}

type Shared = Arc<Mutex<Provider>>;

async fn discovery(State(provider): State<Shared>) -> Json<Value> {
    let issuer = provider.lock().unwrap().issuer.clone();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/keys"),
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
    }))
}

async fn keys(State(provider): State<Shared>) -> Json<Value> {
    provider.lock().unwrap().key_reads += 1;
    Json(json!({
        "keys": [
            {"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": KEY_N.trim(), "e": "AQAB"},
            // Keys that are not for signatures, or not understood, are skipped.
            {"kty": "RSA", "kid": "e1", "use": "enc", "n": KEY_N.trim(), "e": "AQAB"},
            {"kty": "weird", "kid": "w1"},
        ]
    }))
}

fn sign(claims: &Value, key: &str, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_owned);
    jsonwebtoken::encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(key.as_bytes()).unwrap(),
    )
    .unwrap()
}

async fn token(
    State(provider): State<Shared>,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let refuse = || {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response()
    };
    if form.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let code = form.get("code").cloned().unwrap_or_default();
    let Some(grant) = provider.lock().unwrap().grants.remove(&code) else {
        return refuse();
    };
    if let Some(challenge) = &grant.challenge {
        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
        if &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != challenge {
            return refuse();
        }
    }
    if form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return refuse();
    }
    Json(json!({
        "access_token": "access",
        "token_type": "Bearer",
        "id_token": sign(&grant.claims, KEY, Some("k1")),
    }))
    .into_response()
}

async fn fake_provider() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let provider: Shared = Arc::new(Mutex::new(Provider {
        issuer,
        ..Default::default()
    }));
    let routes = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/keys", get(keys))
        .route("/token", post(token))
        .with_state(Arc::clone(&provider));
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    provider
}

// ---- the site --------------------------------------------------------------------------------

struct Site {
    app: Router,
    db: DatabaseConnection,
    provider: Shared,
}

async fn site() -> Site {
    let provider = fake_provider().await;
    let issuer = provider.lock().unwrap().issuer.clone();
    let settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            methods: vec![
                AuthMethod::Password,
                AuthMethod::Oidc(OidcProvider::new(
                    "acme",
                    "Acme ID",
                    issuer,
                    OAuthClient::new(CLIENT_ID, CLIENT_SECRET).audiences([IOS_CLIENT_ID]),
                )),
            ],
            signup: Signup::Domains(vec!["acme.com".into()]),
            signup_groups: vec!["Members".into()],
            app: AppSettings {
                redirect_uris: vec![REDIRECT.into(), "myapp://auth".into()],
                ..Default::default()
            },
            cookie_secure: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let db = common::database().await;
    let app = App::with_settings(db.clone(), settings).unwrap();
    app.migrate().await.unwrap();
    create_superuser(&db, "ada", "correct horse battery")
        .await
        .unwrap();
    set_email(&db, "ada", "ada@acme.com").await.unwrap();
    Site {
        app: app.router().unwrap(),
        db,
        provider,
    }
}

impl Site {
    fn issuer(&self) -> String {
        self.provider.lock().unwrap().issuer.clone()
    }

    /// Standard claims for `email`, with `extra` on top.
    fn claims(&self, email: &str, extra: Value) -> Value {
        let mut claims = json!({
            "iss": self.issuer(),
            "aud": CLIENT_ID,
            "sub": format!("sub-{email}"),
            "email": email,
            "email_verified": true,
            "exp": now() + 300,
            "iat": now(),
        });
        for (key, value) in extra.as_object().unwrap() {
            claims[key] = value.clone();
        }
        claims
    }

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

    async fn with_id_token(&self, id_token: String) -> (StatusCode, Value) {
        self.post("/api/auth/acme", json!({ "id_token": id_token }))
            .await
    }

    async fn me(&self, access_token: &str) -> Value {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get("/api/auth/me")
                    .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn next() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn refused(reply: &(StatusCode, Value)) {
    assert_eq!(reply.0, StatusCode::UNAUTHORIZED, "{}", reply.1);
    assert_eq!(
        reply.1["error"]["code"], "provider_login_failed",
        "{}",
        reply.1
    );
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test]
async fn an_id_token_from_the_app_logs_in_when_the_provider_signed_it() {
    let site = site().await;
    let good = site
        .with_id_token(sign(
            &site.claims("ada@acme.com", json!({})),
            KEY,
            Some("k1"),
        ))
        .await;
    assert_eq!(good.0, StatusCode::OK, "{}", good.1);
    assert!(good.1["refresh_token"].is_string());
    let me = site.me(good.1["access_token"].as_str().unwrap()).await;
    assert_eq!(me["username"], "ada");

    // Issued to the iOS app, also ours.
    let ios = site
        .with_id_token(sign(
            &site.claims("ada@acme.com", json!({"aud": IOS_CLIENT_ID})),
            KEY,
            Some("k1"),
        ))
        .await;
    assert_eq!(ios.0, StatusCode::OK, "{}", ios.1);

    // Everything else is refused.
    let cases = [
        (
            "someone else's app",
            site.claims("ada@acme.com", json!({"aud": "other-app"})),
            KEY,
            Some("k1"),
        ),
        (
            "expired",
            site.claims("ada@acme.com", json!({"exp": now() - 600})),
            KEY,
            Some("k1"),
        ),
        (
            "another issuer",
            site.claims("ada@acme.com", json!({"iss": "https://evil.example"})),
            KEY,
            Some("k1"),
        ),
        (
            "forged",
            site.claims("ada@acme.com", json!({})),
            OTHER_KEY,
            Some("k1"),
        ),
        (
            "an encryption key",
            site.claims("ada@acme.com", json!({})),
            KEY,
            Some("e1"),
        ),
    ];
    for (what, claims, key, kid) in cases {
        let reply = site.with_id_token(sign(&claims, key, kid)).await;
        assert_eq!(reply.0, StatusCode::UNAUTHORIZED, "{what}: {}", reply.1);
        refused(&reply);
    }
    // Signed with the client secret: a front end must not have it, so HMAC is refused.
    let hmac = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &site.claims("ada@acme.com", json!({})),
        &EncodingKey::from_secret(CLIENT_SECRET.as_bytes()),
    )
    .unwrap();
    refused(&site.with_id_token(hmac).await);
    refused(&site.with_id_token("not.a.token".into()).await);
    // Unsigned.
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(site.claims("ada@acme.com", json!({})).to_string());
    refused(&site.with_id_token(format!("{header}.{payload}.")).await);
}

#[tokio::test]
async fn unknown_signing_keys_are_looked_up_again_once() {
    let site = site().await;
    let good = sign(&site.claims("ada@acme.com", json!({})), KEY, Some("k1"));
    assert_eq!(site.with_id_token(good.clone()).await.0, StatusCode::OK);
    assert_eq!(site.with_id_token(good).await.0, StatusCode::OK);
    // Read once, then cached.
    assert_eq!(site.provider.lock().unwrap().key_reads, 1);
    // A key the provider may have just added: read again, still unknown, refused.
    refused(
        &site
            .with_id_token(sign(
                &site.claims("ada@acme.com", json!({})),
                KEY,
                Some("k9"),
            ))
            .await,
    );
    assert_eq!(site.provider.lock().unwrap().key_reads, 2);
}

#[tokio::test]
async fn a_nonce_must_match_when_the_app_sends_one() {
    let site = site().await;
    let token = sign(
        &site.claims("ada@acme.com", json!({"nonce": "n-1"})),
        KEY,
        Some("k1"),
    );
    let wrong = site
        .post("/api/auth/acme", json!({"id_token": token, "nonce": "n-2"}))
        .await;
    refused(&wrong);
    let right = site
        .post("/api/auth/acme", json!({"id_token": token, "nonce": "n-1"}))
        .await;
    assert_eq!(right.0, StatusCode::OK, "{}", right.1);
}

#[tokio::test]
async fn new_people_get_api_only_accounts_when_signup_allows_them() {
    let site = site().await;
    let new = site
        .with_id_token(sign(
            &site.claims(
                "bea@acme.com",
                json!({"given_name": "Bea", "family_name": "Smith"}),
            ),
            KEY,
            Some("k1"),
        ))
        .await;
    assert_eq!(new.0, StatusCode::OK, "{}", new.1);
    let me = site.me(new.1["access_token"].as_str().unwrap()).await;
    assert_eq!(me["username"], "bea@acme.com");
    assert_eq!(me["staff"], false);
    assert_eq!(me["first_name"], "Bea");
    let account = cadmus::user::Entity::find()
        .filter(cadmus::user::Column::Username.eq("bea@acme.com"))
        .one(&site.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!account.is_staff && account.is_active && !account.is_superuser);

    // Other domains, and addresses the provider did not verify, get no account.
    for claims in [
        site.claims("eve@elsewhere.com", json!({})),
        site.claims("cal@acme.com", json!({"email_verified": false})),
    ] {
        let reply = site.with_id_token(sign(&claims, KEY, Some("k1"))).await;
        assert_eq!(reply.0, StatusCode::FORBIDDEN, "{}", reply.1);
        assert_eq!(reply.1["error"]["code"], "no_account");
    }
}

#[tokio::test]
async fn a_code_from_the_app_is_traded_with_the_client_secret() {
    let site = site().await;
    let verifier = "a-verifier-long-enough-for-pkce-0123456789";
    let grant = |redirect_uri: &str| {
        let code = format!("code-{}", next());
        // Before taking the lock: `claims` takes it too.
        let claims = site.claims("ada@acme.com", json!({}));
        site.provider.lock().unwrap().grants.insert(
            code.clone(),
            Grant {
                claims,
                challenge: Some(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))),
                redirect_uri: redirect_uri.to_owned(),
            },
        );
        code
    };

    let code = grant(REDIRECT);
    let good = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": verifier, "redirect_uri": REDIRECT}),
        )
        .await;
    assert_eq!(good.0, StatusCode::OK, "{}", good.1);
    // Each code works once.
    let again = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": verifier, "redirect_uri": REDIRECT}),
        )
        .await;
    refused(&again);

    let code = grant(REDIRECT);
    let wrong_verifier = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": "something else entirely", "redirect_uri": REDIRECT}),
        )
        .await;
    refused(&wrong_verifier);

    // Only the app's own addresses.
    let code = grant("https://evil.example/steal");
    let (status, body) = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": verifier, "redirect_uri": "https://evil.example/steal"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["field"], "redirect_uri");
    let (status, _) = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": verifier}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A mobile app's own scheme works when listed.
    let code = grant("myapp://auth");
    let mobile = site
        .post(
            "/api/auth/acme",
            json!({"code": code, "code_verifier": verifier, "redirect_uri": "myapp://auth"}),
        )
        .await;
    assert_eq!(mobile.0, StatusCode::OK, "{}", mobile.1);
}

#[tokio::test]
async fn malformed_requests_and_unknown_providers_are_refused() {
    let site = site().await;
    let token = sign(&site.claims("ada@acme.com", json!({})), KEY, Some("k1"));
    let (status, _) = site
        .post("/api/auth/acme", json!({"id_token": token, "code": "c"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = site.post("/api/auth/acme", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = site
        .post("/api/auth/nowhere", json!({"id_token": token}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn accounts_with_two_factor_get_a_challenge() {
    let site = site().await;
    enable_totp(&site.db, "ada").await.unwrap();
    let reply = site
        .with_id_token(sign(
            &site.claims("ada@acme.com", json!({})),
            KEY,
            Some("k1"),
        ))
        .await;
    assert_eq!(reply.0, StatusCode::UNAUTHORIZED, "{}", reply.1);
    assert_eq!(reply.1["error"]["code"], "totp_required");
    assert!(reply.1["challenge"].is_string());
}

#[tokio::test]
async fn the_api_documentation_lists_the_providers() {
    let site = site().await;
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
    let operation = &doc["paths"]["/api/auth/{provider}"]["post"];
    assert_eq!(
        operation["parameters"][0]["schema"]["enum"],
        json!(["acme"])
    );
}

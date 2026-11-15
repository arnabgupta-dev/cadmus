//! Microsoft and Facebook, against stand-ins on this machine (`CADMUS_PROVIDER_BASE_URLS`):
//! through the admin's login page and through `POST /api/auth/<provider>`, with ID tokens
//! and codes.
//!
//! The stand-in's address is in an environment variable, which belongs to the whole process,
//! so everything runs in one test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cadmus::settings::*;
use cadmus::{App, create_superuser, set_email};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const CLIENT_ID: &str = "stand-in-app";
const CLIENT_SECRET: &str = "stand-in secret";
const REDIRECT: &str = "https://app.example.com/auth/callback";
const KEY: &str = include_str!("fixtures/test_signing_key.pem");
const KEY_N: &str = include_str!("fixtures/test_signing_key.n");
const SHARED: [&str; 3] = ["common", "organizations", "consumers"];

// ---- the stand-in ----------------------------------------------------------------------------

struct Grant {
    /// Microsoft: the ID token's claims. Facebook: the profile.
    data: Value,
    challenge: Option<String>,
    redirect_uri: String,
}

#[derive(Default)]
struct StandIn {
    base: String,
    microsoft: HashMap<String, Grant>,
    facebook: HashMap<String, Grant>,
    /// Facebook access tokens and the profiles they open.
    facebook_tokens: HashMap<String, Value>,
}

type Shared = Arc<Mutex<StandIn>>;

fn pkce_ok(challenge: Option<&String>, verifier: Option<&String>) -> bool {
    match challenge {
        None => true,
        Some(challenge) => verifier.is_some_and(|verifier| {
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
        }),
    }
}

fn bad(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

async fn ms_discovery(State(s): State<Shared>, Path(tenant): Path<String>) -> Json<Value> {
    let base = s.lock().unwrap().base.clone();
    let issuer = if SHARED.contains(&tenant.as_str()) {
        format!("{base}/{{tenantid}}/v2.0")
    } else {
        format!("{base}/{tenant}/v2.0")
    };
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{base}/{tenant}/oauth2/v2.0/authorize"),
        "token_endpoint": format!("{base}/{tenant}/oauth2/v2.0/token"),
        "jwks_uri": format!("{base}/{tenant}/discovery/v2.0/keys"),
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

async fn ms_keys() -> Json<Value> {
    Json(json!({ "keys": [
        {"kty": "RSA", "kid": "ms1", "use": "sig", "n": KEY_N.trim(), "e": "AQAB"},
    ] }))
}

fn sign(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("ms1".into());
    jsonwebtoken::encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

async fn ms_token(
    State(s): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    // The secret may come in the form or as HTTP Basic.
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok());
    let form_secret = form.get("client_id").map(String::as_str) == Some(CLIENT_ID)
        && form.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET);
    if !form_secret
        && basic.as_deref() != Some(&format!("{CLIENT_ID}:{}", urlencode(CLIENT_SECRET)))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let code = form.get("code").cloned().unwrap_or_default();
    let Some(grant) = s.lock().unwrap().microsoft.remove(&code) else {
        return bad("invalid_grant");
    };
    if !pkce_ok(grant.challenge.as_ref(), form.get("code_verifier"))
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return bad("invalid_grant");
    }
    Json(json!({ "access_token": "ms-access", "token_type": "Bearer", "id_token": sign(&grant.data) }))
        .into_response()
}

fn urlencode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

async fn fb_token(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || q.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
    {
        return bad("invalid client");
    }
    let code = q.get("code").cloned().unwrap_or_default();
    let mut s = s.lock().unwrap();
    let Some(grant) = s.facebook.remove(&code) else {
        return bad("invalid code");
    };
    if !pkce_ok(grant.challenge.as_ref(), q.get("code_verifier"))
        || q.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return bad("invalid code");
    }
    let token = format!("fb-token-{code}");
    s.facebook_tokens.insert(token.clone(), grant.data);
    Json(json!({ "access_token": token, "token_type": "bearer", "expires_in": 5000 }))
        .into_response()
}

async fn fb_me(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let token = q.get("access_token").cloned().unwrap_or_default();
    let mut mac = Hmac::<Sha256>::new_from_slice(CLIENT_SECRET.as_bytes()).unwrap();
    mac.update(token.as_bytes());
    let proof: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if q.get("appsecret_proof") != Some(&proof) {
        return bad("invalid appsecret_proof");
    }
    match s.lock().unwrap().facebook_tokens.get(&token) {
        Some(profile) => Json(profile.clone()).into_response(),
        None => bad("invalid token"),
    }
}

async fn stand_in() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let shared: Shared = Arc::new(Mutex::new(StandIn {
        base,
        ..Default::default()
    }));
    let routes = Router::new()
        .route(
            "/{tenant}/v2.0/.well-known/openid-configuration",
            get(ms_discovery),
        )
        .route("/{tenant}/discovery/v2.0/keys", get(ms_keys))
        .route("/{tenant}/oauth2/v2.0/token", post(ms_token))
        .route("/v21.0/oauth/access_token", get(fb_token))
        .route("/v21.0/me", get(fb_me))
        .with_state(Arc::clone(&shared));
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    shared
}

// ---- the site --------------------------------------------------------------------------------

async fn site(tenant: &str) -> Router {
    let settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            methods: vec![
                AuthMethod::Password,
                AuthMethod::Microsoft {
                    client: OAuthClient::new(CLIENT_ID, CLIENT_SECRET),
                    tenant: tenant.into(),
                },
                AuthMethod::Facebook(OAuthClient::new(CLIENT_ID, CLIENT_SECRET)),
            ],
            signup: Signup::Domains(vec!["acme.com".into()]),
            app: AppSettings {
                redirect_uris: vec![REDIRECT.into()],
                ..Default::default()
            },
            cookie_secure: false,
            ..Default::default()
        },
        rate_limit: RateLimitSettings {
            enabled: false,
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
    app.router().unwrap()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix(&format!("{name}=")))
            .map(|rest| rest.split(';').next().unwrap_or_default().to_owned())
            .filter(|value| !value.is_empty())
    }
}

async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A Microsoft ID token's claims for an account in directory `tid`.
fn ms_claims(base: &str, tid: &str, email: &str, extra: Value) -> Value {
    let mut claims = json!({
        "iss": format!("{base}/{tid}/v2.0"),
        "aud": CLIENT_ID,
        "tid": tid,
        "sub": format!("ms-{email}"),
        "oid": format!("oid-{email}"),
        "email": email,
        "exp": now() + 300,
        "iat": now(),
    });
    for (key, value) in extra.as_object().unwrap() {
        claims[key] = value.clone();
    }
    claims
}

/// Starts a login on the admin's page and comes back from `provider` with a code the
/// stand-in will trade for `data`.
async fn admin_round_trip(app: &Router, s: &Shared, provider: &str, data: Value) -> Reply {
    let start = send(
        app,
        Request::get(format!("/admin/auth/{provider}/start"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(start.status, StatusCode::SEE_OTHER, "{}", start.body);
    let flow = start
        .cookie("cadmus_oauth")
        .expect("the round trip is remembered");
    let target = url::Url::parse(
        start
            .headers
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let base = s.lock().unwrap().base.clone();
    assert!(target.as_str().starts_with(&base), "{target}");
    let query: HashMap<String, String> = target.query_pairs().into_owned().collect();
    let mut data = data;
    if provider == "microsoft" {
        data["nonce"] = json!(query["nonce"]);
    }
    let code = format!("code-{provider}-{}", now());
    let grant = Grant {
        data,
        challenge: Some(query["code_challenge"].clone()),
        redirect_uri: query["redirect_uri"].clone(),
    };
    {
        let mut s = s.lock().unwrap();
        if provider == "microsoft" {
            s.microsoft.insert(code.clone(), grant);
        } else {
            s.facebook.insert(code.clone(), grant);
        }
    }
    send(
        app,
        Request::get(format!(
            "/admin/auth/{provider}/callback?code={code}&state={}",
            query["state"]
        ))
        .header(header::COOKIE, format!("cadmus_oauth={flow}"))
        .body(Body::empty())
        .unwrap(),
    )
    .await
}

const VERIFIER: &str = "a-verifier-of-sufficient-length-for-pkce-0123456789";

fn challenge() -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()))
}

fn grant_for_app(s: &Shared, provider: &str, code: &str, data: Value) {
    let grant = Grant {
        data,
        challenge: Some(challenge()),
        redirect_uri: REDIRECT.into(),
    };
    let mut s = s.lock().unwrap();
    if provider == "microsoft" {
        s.microsoft.insert(code.into(), grant);
    } else {
        s.facebook.insert(code.into(), grant);
    }
}

async fn code_from_app(app: &Router, provider: &str, code: &str) -> Reply {
    post_json(
        app,
        &format!("/api/auth/{provider}"),
        json!({ "code": code, "code_verifier": VERIFIER, "redirect_uri": REDIRECT }),
    )
    .await
}

async fn me(app: &Router, token: &str) -> Value {
    send(
        app,
        Request::get("/api/auth/me")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .json()
}

fn logged_in(reply: &Reply) -> String {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.json()["access_token"].as_str().unwrap().to_owned()
}

fn refused(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.status, status, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], code, "{}", reply.body);
}

// ---- the test ----------------------------------------------------------------------------------

#[tokio::test]
async fn microsoft_and_facebook_through_stand_ins() {
    let s = stand_in().await;
    let base = s.lock().unwrap().base.clone();
    // SAFETY: the only test in this binary, and no other thread reads the environment yet.
    unsafe {
        std::env::set_var(
            "CADMUS_PROVIDER_BASE_URLS",
            format!("microsoft={base},facebook={base}"),
        );
    }

    // A stand-in elsewhere than this machine is refused when the site starts.
    unsafe {
        std::env::set_var(
            "CADMUS_PROVIDER_BASE_URLS",
            "microsoft=https://evil.example",
        );
    }
    let db = common::database().await;
    let refused_settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            methods: vec![AuthMethod::Microsoft {
                client: OAuthClient::new(CLIENT_ID, CLIENT_SECRET),
                tenant: "common".into(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let error = App::with_settings(db, refused_settings)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("must point at this machine"), "{error}");
    unsafe {
        std::env::set_var(
            "CADMUS_PROVIDER_BASE_URLS",
            format!("microsoft={base},facebook={base}"),
        );
    }

    // ---- Microsoft, shared directories (`common`) ----
    let app = site("common").await;
    let tid = "9188040d-6c67-4c5b-b112-36a304b66dad";

    // An ID token from an app: the issuer names the account's own directory, and the
    // address counts only with `xms_edov`.
    let vouched = sign(&ms_claims(
        &base,
        tid,
        "ada@acme.com",
        json!({"xms_edov": true}),
    ));
    let token =
        logged_in(&post_json(&app, "/api/auth/microsoft", json!({ "id_token": vouched })).await);
    assert_eq!(me(&app, &token).await["username"], "ada");

    // Someone else claiming ada's address, unvouched for: no account. (Ada's own identity is
    // linked now, so her later logins no longer depend on the address.)
    let unvouched = sign(&ms_claims(
        &base,
        tid,
        "ada@acme.com",
        json!({"sub": "someone-else", "oid": "someone-else"}),
    ));
    refused(
        &post_json(
            &app,
            "/api/auth/microsoft",
            json!({ "id_token": unvouched }),
        )
        .await,
        StatusCode::FORBIDDEN,
        "no_account",
    );

    // The issuer must be the directory the token names.
    let mut wrong_issuer = ms_claims(&base, tid, "ada@acme.com", json!({"xms_edov": true}));
    wrong_issuer["iss"] = json!(format!("{base}/another-directory/v2.0"));
    refused(
        &post_json(
            &app,
            "/api/auth/microsoft",
            json!({ "id_token": sign(&wrong_issuer) }),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "provider_login_failed",
    );

    // A new person at an allowed domain signs up; the app's code works too.
    grant_for_app(
        &s,
        "microsoft",
        "ms-code-1",
        ms_claims(
            &base,
            tid,
            "bo@acme.com",
            json!({"xms_edov": true, "given_name": "Bo"}),
        ),
    );
    let token = logged_in(&code_from_app(&app, "microsoft", "ms-code-1").await);
    let bo = me(&app, &token).await;
    assert_eq!(bo["email"], "bo@acme.com");
    assert_eq!(bo["staff"], false);
    // A wrong verifier is refused by the stand-in, so the login fails.
    grant_for_app(
        &s,
        "microsoft",
        "ms-code-2",
        ms_claims(&base, tid, "ada@acme.com", json!({"xms_edov": true})),
    );
    refused(
        &post_json(
            &app,
            "/api/auth/microsoft",
            json!({ "code": "ms-code-2", "code_verifier": "not-the-one-that-was-hashed-0123456789", "redirect_uri": REDIRECT }),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "provider_login_failed",
    );

    // Through the admin's login page.
    let reply = admin_round_trip(
        &app,
        &s,
        "microsoft",
        ms_claims(&base, tid, "ada@acme.com", json!({"xms_edov": true})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.cookie("admin_token").is_some(), "{}", reply.body);

    // ---- Microsoft, one directory: its addresses are trusted without `xms_edov` ----
    let contoso = "contoso-0001";
    let app = site(contoso).await;
    let token = logged_in(
        &post_json(
            &app,
            "/api/auth/microsoft",
            json!({ "id_token": sign(&ms_claims(&base, contoso, "ada@acme.com", json!({}))) }),
        )
        .await,
    );
    assert_eq!(me(&app, &token).await["username"], "ada");
    // A token from another directory is not accepted.
    refused(
        &post_json(
            &app,
            "/api/auth/microsoft",
            json!({ "id_token": sign(&ms_claims(&base, tid, "ada@acme.com", json!({"xms_edov": true}))) }),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "provider_login_failed",
    );

    // ---- Facebook ----
    let app = site("common").await;
    grant_for_app(
        &s,
        "facebook",
        "fb-code-1",
        json!({"id": "fb-1", "email": "Ada@Acme.com", "first_name": "Ada"}),
    );
    let token = logged_in(&code_from_app(&app, "facebook", "fb-code-1").await);
    assert_eq!(me(&app, &token).await["username"], "ada");

    // A profile without an address (the person declined to share it) finds no account.
    grant_for_app(&s, "facebook", "fb-code-2", json!({"id": "fb-2"}));
    refused(
        &code_from_app(&app, "facebook", "fb-code-2").await,
        StatusCode::FORBIDDEN,
        "no_account",
    );

    // A wrong verifier, and a code used twice, are refused.
    grant_for_app(
        &s,
        "facebook",
        "fb-code-3",
        json!({"id": "fb-1", "email": "ada@acme.com"}),
    );
    refused(
        &post_json(
            &app,
            "/api/auth/facebook",
            json!({ "code": "fb-code-3", "code_verifier": "not-the-one-that-was-hashed-0123456789", "redirect_uri": REDIRECT }),
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "provider_login_failed",
    );
    refused(
        &code_from_app(&app, "facebook", "fb-code-1").await,
        StatusCode::UNAUTHORIZED,
        "provider_login_failed",
    );

    // Facebook issues no ID tokens.
    let reply = post_json(&app, "/api/auth/facebook", json!({ "id_token": "eyJ.x.y" })).await;
    assert!(reply.status.is_client_error(), "{}", reply.body);
    assert_ne!(reply.status, StatusCode::OK);

    // Through the admin's login page.
    let reply = admin_round_trip(
        &app,
        &s,
        "facebook",
        json!({"id": "fb-1", "email": "ada@acme.com"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.cookie("admin_token").is_some(), "{}", reply.body);
}

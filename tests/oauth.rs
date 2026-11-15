//! Logging in through another site, against a fake OpenID Connect provider running on this
//! machine: the round trip, matching accounts, sign-up, the second factor, and tampering.

// Needs these features: cargo test --all-features
#![cfg(feature = "oidc")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use cadmus::settings::*;
use cadmus::{AdminError, App, EnvUserStore, create_superuser, enable_totp, set_email};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

mod common;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const CLIENT_ID: &str = "app-id";
const CLIENT_SECRET: &str = "app secret&more";

// ---- the fake provider ---------------------------------------------------------------------

/// A code the provider handed out: who it stands for, and what the token request must prove.
struct Grant {
    claims: Value,
    challenge: String,
    redirect_uri: String,
    userinfo: Option<Value>,
}

#[derive(Default)]
struct ProviderState {
    issuer: String,
    grants: HashMap<String, Grant>,
    userinfo: HashMap<String, Value>,
    token_requests: u32,
}

type Shared = Arc<Mutex<ProviderState>>;

async fn discovery(State(provider): State<Shared>) -> Json<Value> {
    let issuer = provider.lock().unwrap().issuer.clone();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

fn fake_jwt(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.c2lnbmF0dXJl")
}

async fn token(
    State(provider): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> axum::response::Response {
    let mut provider = provider.lock().unwrap();
    provider.token_requests += 1;
    // Client authentication: HTTP Basic with both parts form-encoded first.
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{}", "app+secret%26more"))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(&expected)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let code = form.get("code").cloned().unwrap_or_default();
    let Some(grant) = provider.grants.remove(&code) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let proof = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if proof != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
        || form.get("grant_type").map(String::as_str) != Some("authorization_code")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    let access = format!("access-{code}");
    if let Some(info) = grant.userinfo {
        provider.userinfo.insert(access.clone(), info);
    }
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "id_token": fake_jwt(&grant.claims),
    }))
    .into_response()
}

async fn userinfo(State(provider): State<Shared>, headers: HeaderMap) -> axum::response::Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    match provider.lock().unwrap().userinfo.get(token) {
        Some(info) => Json(info.clone()).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn fake_provider() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let provider: Shared = Arc::new(Mutex::new(ProviderState {
        issuer,
        ..Default::default()
    }));
    let routes = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/token", post(token))
        .route("/userinfo", get(userinfo))
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

fn settings(provider: &Shared, configure: impl FnOnce(&mut Settings)) -> Settings {
    let issuer = provider.lock().unwrap().issuer.clone();
    let mut settings = Settings {
        secret_key: SECRET.into(),
        server: ServerSettings {
            public_url: Some("http://localhost:3000/".into()),
            ..Default::default()
        },
        auth: AuthSettings {
            methods: vec![
                AuthMethod::Password,
                AuthMethod::Oidc(OidcProvider::new(
                    "acme",
                    "Acme ID",
                    issuer,
                    OAuthClient::new(CLIENT_ID, CLIENT_SECRET),
                )),
            ],
            cookie_secure: false,
            ..Default::default()
        },
        ..Default::default()
    };
    configure(&mut settings);
    settings
}

async fn site(configure: impl FnOnce(&mut Settings)) -> Site {
    let provider = fake_provider().await;
    let db = common::database().await;
    let app = App::with_settings(db.clone(), settings(&provider, configure)).unwrap();
    app.migrate().await.unwrap();
    create_superuser(&db, "root", "correct horse battery")
        .await
        .unwrap();
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

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    /// The value of the cookie `name` this response sets, if any (empty when it clears it).
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix(&format!("{name}=")))
            .map(|rest| rest.split(';').next().unwrap_or_default().to_owned())
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

async fn get_with(app: &Router, path: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::empty()).unwrap()).await
}

async fn post_form(app: &Router, path: &str, body: &str, cookie: Option<&str>) -> Reply {
    let mut request =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    send(app, request.body(Body::from(body.to_owned())).unwrap()).await
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Starts a login, lets the provider vouch for `claims` (filled in with the standard claims
/// unless they set their own), and returns the response to the callback.
async fn login_as(site: &Site, claims: Value) -> Reply {
    login_with(site, claims, None).await
}

async fn login_with(site: &Site, claims: Value, userinfo: Option<Value>) -> Reply {
    let start = get_with(&site.app, "/admin/auth/acme/start", None).await;
    assert_eq!(start.status, StatusCode::SEE_OTHER, "{}", start.body);
    let flow = start
        .cookie("cadmus_oauth")
        .expect("the round trip is remembered");
    let target = url::Url::parse(start.location()).unwrap();
    let query: HashMap<String, String> = target.query_pairs().into_owned().collect();
    let issuer = site.provider.lock().unwrap().issuer.clone();
    assert_eq!(target.path(), "/authorize");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["scope"], "openid email profile");
    assert_eq!(
        query["redirect_uri"],
        "http://localhost:3000/admin/auth/acme/callback"
    );

    let mut full = json!({
        "iss": issuer,
        "aud": CLIENT_ID,
        "exp": now() + 300,
        "iat": now(),
        "nonce": query["nonce"],
    });
    for (key, value) in claims.as_object().unwrap() {
        full[key] = value.clone();
    }
    let code = format!("code-{}", rand_suffix());
    site.provider.lock().unwrap().grants.insert(
        code.clone(),
        Grant {
            claims: full,
            challenge: query["code_challenge"].clone(),
            redirect_uri: query["redirect_uri"].clone(),
            userinfo,
        },
    );
    let path = format!(
        "/admin/auth/acme/callback?code={code}&state={}",
        query["state"]
    );
    get_with(&site.app, &path, Some(&format!("cadmus_oauth={flow}"))).await
}

fn rand_suffix() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn totp_code(secret: &str) -> String {
    let key = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret).unwrap();
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&key).unwrap();
    mac.update(&(now() / 30).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let chunk: [u8; 4] = digest[offset..offset + 4].try_into().unwrap();
    format!(
        "{:06}",
        (u32::from_be_bytes(chunk) & 0x7fff_ffff) % 1_000_000
    )
}

async fn identities(db: &DatabaseConnection) -> Vec<cadmus::identity::Model> {
    cadmus::identity::Entity::find().all(db).await.unwrap()
}

async fn account(db: &DatabaseConnection, username: &str) -> Option<cadmus::user::Model> {
    cadmus::user::Entity::find()
        .filter(cadmus::user::Column::Username.eq(username))
        .one(db)
        .await
        .unwrap()
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_login_page_offers_the_configured_methods() {
    let both = site(|_| {}).await;
    let page = get_with(&both.app, "/admin/login", None).await;
    assert!(page.body.contains("Continue with Acme ID"), "{}", page.body);
    assert!(page.body.contains("href=\"&#x2f;admin/auth/acme/start\""));
    assert!(page.body.contains("name=\"password\""));

    let providers_only = site(|s| {
        s.auth.methods.remove(0);
    })
    .await;
    let page = get_with(&providers_only.app, "/admin/login", None).await;
    assert!(page.body.contains("Continue with Acme ID"));
    assert!(!page.body.contains("name=\"password\""));
    // The password form is not just hidden: password logins are refused.
    let refused = post_form(
        &providers_only.app,
        "/admin/login",
        "username=root&password=correct+horse+battery",
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.cookie("admin_token").is_none());
    let api = send(
        &providers_only.app,
        Request::post("/api/auth/token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"username": "root", "password": "correct horse battery"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(api.status, StatusCode::FORBIDDEN, "{}", api.body);

    let unknown = get_with(&both.app, "/admin/auth/nope/start", None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_account_is_matched_by_its_verified_address_then_by_the_link() {
    let site = site(|_| {}).await;
    let reply = login_as(
        &site,
        json!({"sub": "person-1", "email": "Ada@Acme.com", "email_verified": true}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    // A page of our own moves on, so the strict session cookie is sent with the next request.
    assert!(
        reply
            .body
            .contains("http-equiv=\"refresh\" content=\"0;url=/admin/\"")
    );
    let session = reply.cookie("admin_token").expect("logged in");
    assert_eq!(
        reply.cookie("cadmus_oauth").as_deref(),
        Some(""),
        "the round trip is used up"
    );
    let dashboard = get_with(
        &site.app,
        "/admin/",
        Some(&format!("admin_token={session}")),
    )
    .await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(account(&site.db, "ada").await.unwrap().last_login.is_some());

    let links = identities(&site.db).await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].provider, "acme");
    assert_eq!(links[0].subject, "person-1");

    // Later logins follow the link, even after the address changed at the provider.
    let again = login_as(
        &site,
        json!({"sub": "person-1", "email": "ada@elsewhere.org", "email_verified": true}),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert!(again.cookie("admin_token").is_some());
    let links = identities(&site.db).await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].email.as_deref(), Some("ada@elsewhere.org"));

    // The user page lists the link.
    let root = post_form(
        &site.app,
        "/admin/login",
        "username=root&password=correct+horse+battery",
        None,
    )
    .await;
    let root = format!("admin_token={}", root.cookie("admin_token").unwrap());
    let ada = account(&site.db, "ada").await.unwrap();
    let page = get_with(
        &site.app,
        &format!("/admin/auth/users/{}", ada.id),
        Some(&root),
    )
    .await;
    assert!(page.body.contains("Logs in with"), "{}", page.body);
    assert!(page.body.contains("Acme ID"));
}

#[tokio::test]
async fn addresses_the_provider_does_not_vouch_for_are_not_trusted() {
    let site = site(|_| {}).await;
    let unverified = login_as(
        &site,
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": false}),
    )
    .await;
    assert_eq!(unverified.status, StatusCode::FORBIDDEN);
    assert!(
        unverified
            .body
            .contains("did not confirm an e-mail address")
    );
    assert!(unverified.cookie("admin_token").is_none());

    let silent = login_as(&site, json!({"sub": "p", "email": "ada@acme.com"})).await;
    assert_eq!(silent.status, StatusCode::FORBIDDEN);

    let stranger = login_as(
        &site,
        json!({"sub": "q", "email": "eve@acme.com", "email_verified": true}),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::FORBIDDEN);
    assert!(stranger.body.contains("No account here uses eve@acme.com"));
    assert!(identities(&site.db).await.is_empty());
}

#[tokio::test]
async fn the_address_can_come_from_the_userinfo_endpoint() {
    let site = site(|_| {}).await;
    let reply = login_with(
        &site,
        json!({"sub": "person-2"}),
        Some(json!({"sub": "person-2", "email": "ada@acme.com", "email_verified": true})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.cookie("admin_token").is_some());
}

#[tokio::test]
async fn signup_admits_allowed_domains_into_the_signup_groups() {
    let site = site(|s| {
        s.auth.signup = Signup::Domains(vec!["acme.com".into()]);
        s.auth.signup_groups = vec!["Editors".into(), "Missing".into()];
    })
    .await;
    cadmus::group::ActiveModel {
        name: Set("Editors".into()),
        ..Default::default()
    }
    .insert(&site.db)
    .await
    .unwrap();

    let reply = login_as(
        &site,
        json!({"sub": "new-1", "email": "grace@acme.com", "email_verified": true,
               "given_name": "Grace", "family_name": "Hopper"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.cookie("admin_token").is_some());
    let grace = account(&site.db, "grace@acme.com").await.expect("created");
    assert!(grace.is_active && grace.is_staff && !grace.is_superuser);
    assert_eq!(grace.email.as_deref(), Some("grace@acme.com"));
    assert_eq!(grace.display_name(), "Grace Hopper");
    assert!(grace.date_joined.is_some());

    // Signed up accounts can see what their groups allow and nothing else.
    let session = format!("admin_token={}", reply.cookie("admin_token").unwrap());
    let users_page = get_with(&site.app, "/admin/auth/users", Some(&session)).await;
    assert_eq!(users_page.status, StatusCode::FORBIDDEN);

    let outsider = login_as(
        &site,
        json!({"sub": "new-2", "email": "mallory@acme.com.evil.io", "email_verified": true}),
    )
    .await;
    assert_eq!(outsider.status, StatusCode::FORBIDDEN);
    assert!(
        account(&site.db, "mallory@acme.com.evil.io")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn accounts_with_two_factor_enter_their_code_afterwards() {
    let site = site(|_| {}).await;
    let secret = enable_totp(&site.db, "ada").await.unwrap();
    let reply = login_as(
        &site,
        json!({"sub": "person-1", "email": "ada@acme.com", "email_verified": true}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("url=/admin/login/code"));
    assert!(reply.cookie("admin_token").is_none(), "not logged in yet");
    let pending = format!("cadmus_pending={}", reply.cookie("cadmus_pending").unwrap());

    let form = get_with(&site.app, "/admin/login/code", Some(&pending)).await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains("One more step"));
    assert!(form.body.contains("action=\"&#x2f;admin/login/code\""));
    let without = get_with(&site.app, "/admin/login/code", None).await;
    assert_eq!(without.status, StatusCode::SEE_OTHER);

    let wrong = post_form(
        &site.app,
        "/admin/login/code",
        "totp=000000",
        Some(&pending),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.cookie("admin_token").is_none());

    let code = totp_code(&secret);
    let right = post_form(
        &site.app,
        "/admin/login/code",
        &format!("totp={code}"),
        Some(&pending),
    )
    .await;
    assert_eq!(right.status, StatusCode::SEE_OTHER, "{}", right.body);
    assert_eq!(right.location(), "/admin/");
    assert!(right.cookie("admin_token").is_some());
    // The code works once.
    let replay = post_form(
        &site.app,
        "/admin/login/code",
        &format!("totp={code}"),
        Some(&pending),
    )
    .await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn required_two_factor_and_inactive_accounts_are_enforced() {
    let site = site(|s| s.auth.totp_required = true).await;
    let reply = login_as(
        &site,
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.contains("requires two-factor"));

    let site = self::site(|_| {}).await;
    let ada = account(&site.db, "ada").await.unwrap();
    let mut ada: cadmus::user::ActiveModel = ada.into();
    ada.is_active = Set(false);
    ada.update(&site.db).await.unwrap();
    let reply = login_as(
        &site,
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.cookie("admin_token").is_none());
}

#[tokio::test]
async fn tampered_round_trips_are_refused() {
    let site = site(|_| {}).await;
    let start = get_with(&site.app, "/admin/auth/acme/start", None).await;
    let flow = format!("cadmus_oauth={}", start.cookie("cadmus_oauth").unwrap());
    let query: HashMap<String, String> = url::Url::parse(start.location())
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();

    // No remembered round trip (another browser), a wrong state, or a cancelled login.
    let path = format!("/admin/auth/acme/callback?code=x&state={}", query["state"]);
    assert_eq!(
        get_with(&site.app, &path, None).await.status,
        StatusCode::BAD_REQUEST
    );
    let forged = get_with(
        &site.app,
        "/admin/auth/acme/callback?code=x&state=guess",
        Some(&flow),
    )
    .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let cancelled = get_with(
        &site.app,
        "/admin/auth/acme/callback?error=access_denied",
        Some(&flow),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::BAD_REQUEST);
    assert!(cancelled.body.contains("was cancelled"));
    // A round trip for another provider name does not count.
    let other = get_with(
        &site.app,
        "/admin/auth/acme/callback?code=x&state=y",
        Some("cadmus_oauth=garbage"),
    )
    .await;
    assert_eq!(other.status, StatusCode::BAD_REQUEST);
    assert_eq!(site.provider.lock().unwrap().token_requests, 0);

    // A token for another login, another app or from another issuer.
    for claims in [
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true, "nonce": "stale"}),
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true, "aud": "other-app"}),
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true, "iss": "http://evil.test"}),
        json!({"sub": "p", "email": "ada@acme.com", "email_verified": true, "exp": 1000}),
    ] {
        let reply = login_as(&site, claims.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{claims}");
        assert!(reply.cookie("admin_token").is_none());
    }
    assert!(identities(&site.db).await.is_empty());
}

#[tokio::test]
async fn an_unreachable_provider_is_reported_on_the_login_page() {
    let mut settings = Settings {
        secret_key: SECRET.into(),
        ..Default::default()
    };
    settings.server.public_url = Some("http://localhost:3000".into());
    settings.auth.methods = vec![AuthMethod::Oidc(OidcProvider::new(
        "down",
        "Down",
        "http://127.0.0.1:1",
        OAuthClient::new("id", "secret"),
    ))];
    let db = common::database().await;
    let app = App::with_settings(db, settings).unwrap().router().unwrap();
    let reply = get_with(&app, "/admin/auth/down/start", None).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert!(reply.body.contains("Down is not answering"));
}

#[tokio::test]
async fn providers_need_the_built_in_accounts() {
    let provider = fake_provider().await;
    let db = common::database().await;
    let store = EnvUserStore::from_lookup(|key| match key {
        "ADMIN_USERNAME" => Some("root".into()),
        "ADMIN_PASSWORD_HASH" => Some(cadmus::hash_password("whatever-long").unwrap()),
        _ => None,
    })
    .unwrap();
    let app = App::with_settings(db, settings(&provider, |_| {}))
        .unwrap()
        .users(store);
    match app.router() {
        Err(AdminError::Config(message)) => assert!(message.contains("built-in"), "{message}"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("a custom store cannot hold provider links"),
    }
}

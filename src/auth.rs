//! Authentication: password + optional TOTP second factor, issuing a signed JWT that is
//! accepted either as an `HttpOnly` cookie (browser) or a `Bearer` token (API clients).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use async_trait::async_trait;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

use crate::AdminState;
use crate::access::{Access, Action};
use crate::error::{AdminError, ApiError};

pub(crate) const COOKIE: &str = "admin_token";
const MIN_SECRET_BYTES: usize = 32;
const TOTP_STEP_SECS: u64 = 30;

/// Authentication settings. Build in code, or load with [`AuthConfig::from_env`].
#[derive(Clone)]
pub struct AuthConfig {
    jwt_secret: Vec<u8>,
    pub(crate) token_ttl_secs: u64,
    pub(crate) totp_required: bool,
    pub(crate) cookie_secure: bool,
    pub(crate) login_max_attempts: u32,
    pub(crate) login_lockout_secs: u64,
    pub(crate) client_ip_header: Option<HeaderName>,
}

// Hand-written so the signing secret can never reach a log line.
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("jwt_secret", &"<redacted>")
            .field("token_ttl_secs", &self.token_ttl_secs)
            .field("totp_required", &self.totp_required)
            .field("cookie_secure", &self.cookie_secure)
            .field("login_max_attempts", &self.login_max_attempts)
            .field("login_lockout_secs", &self.login_lockout_secs)
            .field("client_ip_header", &self.client_ip_header)
            .finish()
    }
}

impl AuthConfig {
    /// Creates a config that signs tokens with `jwt_secret` (HS256).
    ///
    /// Defaults: 1-hour tokens, TOTP optional per user, `Secure` cookies, and at most 5 login
    /// attempts per account in 5 minutes.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the secret is shorter than 32 bytes.
    pub fn new(jwt_secret: impl Into<Vec<u8>>) -> Result<Self, AdminError> {
        let jwt_secret = jwt_secret.into();
        if jwt_secret.len() < MIN_SECRET_BYTES {
            return Err(AdminError::Config(format!(
                "the JWT secret must be at least {MIN_SECRET_BYTES} bytes"
            )));
        }
        Ok(Self {
            jwt_secret,
            token_ttl_secs: 3600,
            totp_required: false,
            cookie_secure: true,
            login_max_attempts: 5,
            login_lockout_secs: 300,
            client_ip_header: None,
        })
    }

    /// Reads the config from the environment.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `ADMIN_JWT_SECRET` | **Required.** Token signing secret, at least 32 bytes |
    /// | `ADMIN_JWT_TTL_SECS` | Token lifetime in seconds (default 3600) |
    /// | `ADMIN_TOTP_REQUIRED` | `true` refuses users without a TOTP secret (default `false`) |
    /// | `ADMIN_COOKIE_SECURE` | `false` allows the cookie over plain HTTP, for local development only (default `true`) |
    /// | `ADMIN_LOGIN_MAX_ATTEMPTS` | Login attempts allowed per account in one lockout period; `0` disables the limit (default 5) |
    /// | `ADMIN_LOGIN_LOCKOUT_SECS` | Length of the lockout period in seconds (default 300) |
    /// | `ADMIN_CLIENT_IP_HEADER` | Header your reverse proxy sets to the client's address, e.g. `X-Forwarded-For` (default: use the connection's address) |
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a variable is missing or invalid.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`AuthConfig::from_env`] but reads from `lookup`.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is missing or invalid.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let secret = lookup("ADMIN_JWT_SECRET")
            .ok_or_else(|| AdminError::Config("ADMIN_JWT_SECRET is not set".into()))?;
        let mut config = Self::new(secret)?;
        if let Some(v) = lookup("ADMIN_JWT_TTL_SECS") {
            config.token_ttl_secs = v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                AdminError::Config("ADMIN_JWT_TTL_SECS must be a positive integer".into())
            })?;
        }
        if let Some(v) = lookup("ADMIN_TOTP_REQUIRED") {
            config.totp_required = parse_bool("ADMIN_TOTP_REQUIRED", &v)?;
        }
        if let Some(v) = lookup("ADMIN_COOKIE_SECURE") {
            config.cookie_secure = parse_bool("ADMIN_COOKIE_SECURE", &v)?;
        }
        if let Some(v) = lookup("ADMIN_LOGIN_MAX_ATTEMPTS") {
            config.login_max_attempts = v.parse().map_err(|_| {
                AdminError::Config("ADMIN_LOGIN_MAX_ATTEMPTS must be a non-negative integer".into())
            })?;
        }
        if let Some(v) = lookup("ADMIN_LOGIN_LOCKOUT_SECS") {
            config.login_lockout_secs =
                v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    AdminError::Config("ADMIN_LOGIN_LOCKOUT_SECS must be a positive integer".into())
                })?;
        }
        if let Some(v) = lookup("ADMIN_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
            config = config.client_ip_header(&v)?;
        }
        Ok(config)
    }

    /// Sets the token lifetime in seconds.
    #[must_use]
    pub fn token_ttl_secs(mut self, secs: u64) -> Self {
        self.token_ttl_secs = secs.max(1);
        self
    }

    /// Refuses logins from users that have no TOTP secret.
    #[must_use]
    pub fn totp_required(mut self, required: bool) -> Self {
        self.totp_required = required;
        self
    }

    /// Limits login attempts: at most `max_attempts` per account, and four times that per
    /// client address, in any `lockout_secs` seconds. `max_attempts == 0` removes the limit.
    #[must_use]
    pub fn login_limit(mut self, max_attempts: u32, lockout_secs: u64) -> Self {
        self.login_max_attempts = max_attempts;
        self.login_lockout_secs = lockout_secs.max(1);
        self
    }

    /// Names the header that carries the client's address when the app runs behind a reverse
    /// proxy, e.g. `X-Forwarded-For`. Its last entry is used, which is the one your own proxy
    /// added. Without this, the address of the connection is used, and behind a proxy that
    /// is the proxy's.
    ///
    /// Only set this if the proxy always sets the header: otherwise clients can send it
    /// themselves and pick the address they are counted under.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if `name` is not a valid header name.
    pub fn client_ip_header(mut self, name: &str) -> Result<Self, AdminError> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AdminError::Config(format!("`{name}` is not a valid header name")))?;
        self.client_ip_header = Some(name);
        Ok(self)
    }

    /// The address a request came from, for rate limiting.
    pub(crate) fn client_ip(&self, parts: &Parts) -> Option<IpAddr> {
        let forwarded = self.client_ip_header.as_ref().and_then(|name| {
            let value = parts.headers.get(name)?.to_str().ok()?;
            value.rsplit(',').next()?.trim().parse().ok()
        });
        forwarded.or_else(|| {
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0.ip())
        })
    }

    /// Controls the cookie's `Secure` flag; turn off only for local HTTP development.
    #[must_use]
    pub fn cookie_secure(mut self, secure: bool) -> Self {
        self.cookie_secure = secure;
        self
    }

    pub(crate) fn issue(&self, username: &str) -> Result<String, AdminError> {
        let now = unix_now();
        let claims = Claims {
            sub: username.to_owned(),
            iat: now,
            exp: now + self.token_ttl_secs,
            csrf: rand::random::<[u8; 16]>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        };
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&self.jwt_secret),
        )
        .map_err(|e| AdminError::Config(format!("could not sign token: {e}")))
    }

    fn verify(&self, token: &str) -> Option<Claims> {
        // `Validation::new` pins the algorithm and checks `exp`.
        jsonwebtoken::decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.jwt_secret),
            &Validation::new(Algorithm::HS256),
        )
        .ok()
        .map(|data| data.claims)
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool, AdminError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(AdminError::Config(format!("{key} must be true or false"))),
    }
}

/// Login request body (form or JSON).
#[derive(Deserialize)]
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) totp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    iat: u64,
    exp: u64,
    /// Random per-token value echoed by forms; proves a POST came from our own page.
    csrf: String,
}

/// An account allowed into the admin.
#[derive(Clone)]
pub struct AdminUser {
    /// Login name.
    pub username: String,
    /// Argon2 hash in PHC string format (see [`hash_password`]).
    pub password_hash: String,
    /// Base32 TOTP secret; `None` means no second factor for this user.
    pub totp_secret: Option<String>,
}

/// Where admin accounts come from. Implement this over your own users table;
/// [`EnvUserStore`] covers the single-operator case.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Looks up an account by login name.
    async fn find(&self, username: &str) -> Result<Option<AdminUser>, AdminError>;

    /// What the account may do, or `None` if it may no longer use the site at all.
    ///
    /// Called on every authenticated request, so the answer is always current. The default
    /// treats every account [`UserStore::find`] returns as a superuser; override it to give
    /// accounts in your own store narrower permissions.
    ///
    /// # Errors
    /// Whatever the lookup fails with; the request is then answered with a `500`.
    async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
        Ok(self.find(username).await?.map(|_| Access::superuser()))
    }

    /// Records that the account logged in with the authenticator code of time step `step`,
    /// and says whether that is allowed: `false` if this step or a later one was already
    /// used, which means the code is being replayed.
    ///
    /// The default allows everything, so a code stays usable until it expires (up to 90
    /// seconds). Override it, storing the last step per account, to make codes single-use.
    ///
    /// # Errors
    /// Whatever the store fails with; the login is then refused with a `500`.
    async fn accept_totp_step(&self, _username: &str, _step: u64) -> Result<bool, AdminError> {
        Ok(true)
    }
}

/// A single admin account defined by environment variables.
#[derive(Clone)]
pub struct EnvUserStore {
    user: AdminUser,
    /// The time step of the last authenticator code that was accepted.
    last_totp_step: Arc<AtomicU64>,
}

impl EnvUserStore {
    /// Reads `ADMIN_USERNAME`, `ADMIN_PASSWORD_HASH` (Argon2 PHC string) and, optionally,
    /// `ADMIN_TOTP_SECRET` (base32).
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a variable is missing or malformed.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`EnvUserStore::from_env`] but reads from `lookup`.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is missing or malformed.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let required =
            |key: &str| lookup(key).ok_or_else(|| AdminError::Config(format!("{key} is not set")));
        Self::new(AdminUser {
            username: required("ADMIN_USERNAME")?,
            password_hash: required("ADMIN_PASSWORD_HASH")?,
            totp_secret: lookup("ADMIN_TOTP_SECRET"),
        })
    }

    /// Wraps one account, validating its hash and TOTP secret up front.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the hash is not a PHC string or the secret is not base32.
    pub fn new(user: AdminUser) -> Result<Self, AdminError> {
        PasswordHash::new(&user.password_hash).map_err(|_| {
            AdminError::Config("the password hash is not an Argon2 PHC string".into())
        })?;
        if let Some(secret) = &user.totp_secret {
            decode_secret(secret)
                .ok_or_else(|| AdminError::Config("the TOTP secret is not valid base32".into()))?;
        }
        Ok(Self {
            user,
            last_totp_step: Arc::new(AtomicU64::new(0)),
        })
    }
}

#[async_trait]
impl UserStore for EnvUserStore {
    async fn find(&self, username: &str) -> Result<Option<AdminUser>, AdminError> {
        Ok((username == self.user.username).then(|| self.user.clone()))
    }

    async fn accept_totp_step(&self, _username: &str, step: u64) -> Result<bool, AdminError> {
        Ok(self.last_totp_step.fetch_max(step, Ordering::SeqCst) < step)
    }
}

/// [`hash_password`] on the blocking thread pool, for use from async code.
///
/// # Errors
/// Returns [`AdminError::Config`] if hashing fails.
pub async fn hash_password_async(password: &str) -> Result<String, AdminError> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AdminError::Config(format!("password hash task failed: {e}")))?
}

/// Hashes a password with Argon2id for storage in `ADMIN_PASSWORD_HASH` or your users table.
///
/// # Errors
/// Returns [`AdminError::Config`] if hashing fails.
pub fn hash_password(password: &str) -> Result<String, AdminError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| AdminError::Config(format!("could not hash password: {e}")))
}

fn password_matches(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

/// A valid hash of nothing in particular, to spend the same time on a login for an unknown
/// account as for a real one. Otherwise the response time would reveal which names exist.
fn decoy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("no account has this password").unwrap_or_default())
}

/// Checks credentials and returns a signed token.
///
/// Every credential failure maps to the same [`AdminError::Unauthorized`] so the response
/// does not reveal which part was wrong. Too many attempts for one account or from one
/// address get [`AdminError::RateLimited`] instead, whether or not the password was right.
pub(crate) async fn login(
    state: &AdminState,
    credentials: &Credentials,
    address: Option<IpAddr>,
) -> Result<String, AdminError> {
    let username = credentials.username.as_str();
    state
        .limiter
        .attempt(username, address)
        .map_err(|retry_after_secs| {
            tracing::warn!(%username, ?address, "login attempts exhausted");
            AdminError::RateLimited { retry_after_secs }
        })?;

    let user = state.users.find(username).await?;
    // Argon2 is deliberately slow; run it on the blocking pool so it cannot stall other requests.
    let candidate = credentials.password.clone();
    let hash = user
        .as_ref()
        .map_or_else(|| decoy_hash().to_owned(), |u| u.password_hash.clone());
    let matches = tokio::task::spawn_blocking(move || password_matches(&candidate, &hash))
        .await
        .map_err(|e| AdminError::Config(format!("password check task failed: {e}")))?;
    let Some(user) = user.filter(|_| matches) else {
        return Err(AdminError::Unauthorized);
    };
    match &user.totp_secret {
        Some(secret) => {
            let code = credentials.totp.as_deref().unwrap_or_default();
            let step = totp_step(secret, code, unix_now()).ok_or(AdminError::Unauthorized)?;
            // Each code works once, so one read over a shoulder or off the wire is useless.
            if !state.users.accept_totp_step(username, step).await? {
                tracing::warn!(%username, "authenticator code reused");
                return Err(AdminError::Unauthorized);
            }
        }
        None if state.auth.totp_required => return Err(AdminError::Unauthorized),
        None => {}
    }
    state.limiter.succeeded(username, address);
    state.auth.issue(&user.username)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn decode_secret(base32_secret: &str) -> Option<Vec<u8>> {
    let cleaned: String = base32_secret
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .collect();
    base32::decode(
        base32::Alphabet::Rfc4648 { padding: false },
        &cleaned.to_ascii_uppercase(),
    )
    .filter(|bytes| !bytes.is_empty())
}

/// RFC 4226 HOTP value (6 digits) for `counter`.
fn hotp(secret: &[u8], counter: u64) -> Option<u32> {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).ok()?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest.last()? & 0x0f);
    let chunk: [u8; 4] = digest.get(offset..offset + 4)?.try_into().ok()?;
    Some((u32::from_be_bytes(chunk) & 0x7fff_ffff) % 1_000_000)
}

/// Verifies an RFC 6238 code (SHA-1, 6 digits, 30 s step), accepting one step of clock drift
/// either way.
///
/// `now_unix` is a parameter so the check is deterministic under test.
#[must_use]
pub fn verify_totp(base32_secret: &str, code: &str, now_unix: u64) -> bool {
    totp_step(base32_secret, code, now_unix).is_some()
}

/// Whether `password` is the account's current password. Counts as a login attempt, so a
/// stolen session cannot be used to guess the password without limit.
///
/// # Errors
/// [`AdminError::RateLimited`] after too many wrong guesses.
pub(crate) async fn confirm_password(
    state: &AdminState,
    username: &str,
    password: &str,
) -> Result<bool, AdminError> {
    state
        .limiter
        .attempt(username, None)
        .map_err(|retry_after_secs| AdminError::RateLimited { retry_after_secs })?;
    let Some(user) = state.users.find(username).await? else {
        return Ok(false);
    };
    let (candidate, hash) = (password.to_owned(), user.password_hash);
    let matches = tokio::task::spawn_blocking(move || password_matches(&candidate, &hash))
        .await
        .map_err(|e| AdminError::Config(format!("password check task failed: {e}")))?;
    if matches {
        state.limiter.succeeded(username, None);
    }
    Ok(matches)
}

/// The current time step, for code that needs to compare against [`totp_step`].
pub(crate) fn totp_step_now(base32_secret: &str, code: &str) -> Option<u64> {
    totp_step(base32_secret, code, unix_now())
}

/// The time step `code` is valid for, if it is valid now.
fn totp_step(base32_secret: &str, code: &str, now_unix: u64) -> Option<u64> {
    let secret = decode_secret(base32_secret)?;
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let step = now_unix / TOTP_STEP_SECS;
    let mut matched = None;
    for counter in [step.saturating_sub(1), step, step.saturating_add(1)] {
        if let Some(expected) = hotp(&secret, counter) {
            // No early exit: every candidate is compared so timing does not depend on which matched.
            if constant_time_eq(format!("{expected:06}").as_bytes(), code.as_bytes()) {
                matched = Some(counter);
            }
        }
    }
    matched
}

/// A new random TOTP secret, base32-encoded as authenticator apps expect.
pub(crate) fn new_totp_secret() -> String {
    base32::encode(
        base32::Alphabet::Rfc4648 { padding: false },
        &rand::random::<[u8; 20]>(),
    )
}

/// Whether `base32_secret` is long enough to be used as a TOTP secret (at least 80 bits).
pub(crate) fn totp_secret_is_strong(base32_secret: &str) -> bool {
    decode_secret(base32_secret).is_some_and(|bytes| bytes.len() >= 10)
}

/// The `otpauth://` address an authenticator app reads from a QR code. `issuer` is the name
/// the app shows next to the account, usually the site's title.
#[must_use]
pub fn totp_uri(issuer: &str, username: &str, base32_secret: &str) -> String {
    let mut uri = url::Url::parse("otpauth://totp/").expect("a constant, valid URL");
    uri.set_path(&format!("{issuer}:{username}"));
    uri.query_pairs_mut()
        .append_pair("secret", base32_secret)
        .append_pair("issuer", issuer);
    uri.into()
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An authenticated request. Extracting it is what protects a handler.
pub(crate) struct Session {
    pub(crate) username: String,
    pub(crate) access: Access,
    csrf: String,
    via_cookie: bool,
}

impl Session {
    /// Refuses cookie sessions: the JSON API has no CSRF token, so it only accepts credentials
    /// a browser cannot attach on its own.
    pub(crate) fn require_bearer(&self) -> Result<(), AdminError> {
        if self.via_cookie {
            return Err(AdminError::Forbidden(
                "the API requires an Authorization: Bearer token".into(),
            ));
        }
        Ok(())
    }

    /// Builds the session for verified claims, reading the account's current permissions.
    /// `None` means the account no longer exists or may no longer log in.
    async fn load(
        state: &AdminState,
        claims: Claims,
        via_cookie: bool,
    ) -> Result<Option<Self>, AdminError> {
        Ok(state.users.access(&claims.sub).await?.map(|access| Self {
            username: claims.sub,
            access,
            csrf: claims.csrf,
            via_cookie,
        }))
    }

    /// The session carried by the admin cookie, if there is a valid one.
    pub(crate) async fn from_cookie(
        headers: &HeaderMap,
        state: &AdminState,
    ) -> Result<Option<Self>, AdminError> {
        let jar = CookieJar::from_headers(headers);
        let claims = jar
            .get(COOKIE)
            .and_then(|cookie| state.auth.verify(cookie.value()));
        match claims {
            Some(claims) => Self::load(state, claims, true).await,
            None => Ok(None),
        }
    }

    /// Whether this account may do `action` on `model`.
    pub(crate) fn can(&self, model: &str, action: Action) -> bool {
        self.access.allows(model, action)
    }

    /// Refuses the request unless this account may do `action` on `model`.
    pub(crate) fn require(&self, model: &str, action: Action) -> Result<(), AdminError> {
        if self.can(model, action) {
            return Ok(());
        }
        Err(AdminError::Forbidden(format!(
            "you do not have permission to {} `{model}`",
            action.as_str()
        )))
    }

    /// Refuses the request unless this account is a superuser.
    pub(crate) fn require_superuser(&self) -> Result<(), AdminError> {
        if self.access.is_superuser() {
            return Ok(());
        }
        Err(AdminError::Forbidden(
            "only superusers can manage accounts".into(),
        ))
    }

    /// Token to embed in forms.
    pub(crate) fn csrf(&self) -> &str {
        &self.csrf
    }

    /// Rejects a state-changing request that does not carry this session's CSRF token.
    ///
    /// Bearer-token requests are exempt: a browser never attaches that header on its own, so
    /// they cannot be forged cross-site.
    pub(crate) fn check_csrf(&self, submitted: Option<&str>) -> Result<(), AdminError> {
        if !self.via_cookie {
            return Ok(());
        }
        match submitted {
            Some(token) if constant_time_eq(token.as_bytes(), self.csrf.as_bytes()) => Ok(()),
            _ => Err(AdminError::Forbidden(
                "missing or invalid CSRF token".into(),
            )),
        }
    }
}

impl FromRequestParts<Arc<AdminState>> for Session {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AdminState>,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let api = parts
            .uri
            .path()
            .starts_with(&format!("{}/", state.api_base));
        let session = match bearer {
            Some(token) => match state.auth.verify(token) {
                Some(claims) => Self::load(state, claims, false).await,
                None => Ok(None),
            },
            None => Self::from_cookie(&parts.headers, state).await,
        };
        match session {
            Ok(Some(session)) => Ok(session),
            // API clients get a status code, not a login page.
            Ok(None) if bearer.is_some() || api => {
                Err(ApiError::from(AdminError::Unauthorized).into_response())
            }
            Ok(None) => Err(Redirect::to(&format!("{}/login", state.base)).into_response()),
            Err(error) if bearer.is_some() || api => Err(ApiError::from(error).into_response()),
            Err(error) => Err(error.into_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B test secret ("12345678901234567890") in base32.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn totp_matches_rfc_6238_vectors() {
        // The RFC lists 8-digit codes; ours are the last 6 digits of each.
        assert!(verify_totp(RFC_SECRET, "287082", 59));
        assert!(verify_totp(RFC_SECRET, "081804", 1_111_111_109));
        assert!(verify_totp(RFC_SECRET, "005924", 1_234_567_890));
    }

    #[test]
    fn totp_rejects_wrong_stale_and_malformed_codes() {
        assert!(!verify_totp(RFC_SECRET, "287083", 59));
        // Correct for t=59, but more than one step in the past.
        assert!(!verify_totp(RFC_SECRET, "287082", 59 + 90));
        assert!(!verify_totp(RFC_SECRET, "28708", 59));
        assert!(!verify_totp(RFC_SECRET, "28708a", 59));
        assert!(!verify_totp("not base32 !!", "287082", 59));
    }

    #[test]
    fn token_round_trips_and_rejects_other_secrets() {
        let config = AuthConfig::new("0123456789abcdef0123456789abcdef").unwrap();
        let token = config.issue("root").unwrap();
        assert_eq!(config.verify(&token).unwrap().sub, "root");

        let other = AuthConfig::new("ffffffffffffffffffffffffffffffff").unwrap();
        assert!(other.verify(&token).is_none());
        assert!(config.verify("not.a.token").is_none());
    }

    #[test]
    fn short_secret_is_refused() {
        assert!(AuthConfig::new("too short").is_err());
    }

    #[test]
    fn password_hash_verifies_only_the_right_password() {
        let hash = hash_password("correct horse").unwrap();
        assert!(password_matches("correct horse", &hash));
        assert!(!password_matches("wrong", &hash));
        assert!(!password_matches("correct horse", "not a hash"));
    }
}

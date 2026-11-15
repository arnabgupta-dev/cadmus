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
use axum::extract::{ConnectInfo, FromRequestParts, OptionalFromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::Digest;

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
    /// How people may log in: password, other sites, or both.
    pub(crate) methods: Arc<crate::oauth::Methods>,
    /// Whether the REST API accepts API keys.
    pub(crate) api_keys: bool,
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
            .field("password_login", &self.methods.password)
            .field("api_keys", &self.api_keys)
            .field(
                "providers",
                &self
                    .methods
                    .providers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>(),
            )
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
            methods: Arc::new(crate::oauth::Methods::password_only()),
            api_keys: true,
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

    /// Whether the REST API accepts API keys, which accounts make on their API keys page
    /// (default `true`). Turned off, existing keys stop working and the page is hidden.
    #[must_use]
    pub fn api_keys(mut self, enabled: bool) -> Self {
        self.api_keys = enabled;
        self
    }

    /// Controls the cookie's `Secure` flag; turn off only for local HTTP development.
    #[must_use]
    pub fn cookie_secure(mut self, secure: bool) -> Self {
        self.cookie_secure = secure;
        self
    }

    /// Signs a login token for `username`, whose password hash is `password_hash`.
    pub(crate) fn issue(&self, username: &str, password_hash: &str) -> Result<String, AdminError> {
        let now = unix_now();
        let claims = Claims {
            pw: fingerprint(password_hash),
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

    /// Sets the login methods (see [`crate::settings::AuthSettings::methods`]).
    pub(crate) fn with_methods(mut self, methods: crate::oauth::Methods) -> Self {
        self.methods = Arc::new(methods);
        self
    }

    /// A key for one purpose, derived from the secret, so a token made for one purpose (such
    /// as a login in progress) can never pass for another (such as a session).
    fn purpose_key(&self, purpose: &str) -> Vec<u8> {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&self.jwt_secret)
            .expect("HMAC accepts keys of any length");
        // An earlier name of the framework. Changing it would sign everything differently.
        mac.update(b"heraapi:");
        mac.update(purpose.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Signs `claims` (which must include `exp`) for `purpose`.
    pub(crate) fn seal<T: Serialize>(
        &self,
        purpose: &str,
        claims: &T,
    ) -> Result<String, AdminError> {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(&self.purpose_key(purpose)),
        )
        .map_err(|e| AdminError::Config(format!("could not sign token: {e}")))
    }

    /// Reads claims signed by [`AuthConfig::seal`] for `purpose`, if they are unexpired.
    pub(crate) fn open<T: serde::de::DeserializeOwned>(
        &self,
        purpose: &str,
        token: &str,
    ) -> Option<T> {
        jsonwebtoken::decode::<T>(
            token,
            &DecodingKey::from_secret(&self.purpose_key(purpose)),
            &Validation::new(Algorithm::HS256),
        )
        .ok()
        .map(|data| data.claims)
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
    /// Fingerprint of the password hash the token was issued under: changing the password
    /// ends every login made with the old one. Missing in tokens from before 0.1.5, which
    /// are then refused.
    #[serde(default)]
    pw: String,
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
    ///
    /// Also asked on every authenticated request: a login ends when the `password_hash`
    /// returned here is no longer the one it was made with.
    async fn find(&self, username: &str) -> Result<Option<AdminUser>, AdminError>;

    /// What the account may do, or `None` if it may no longer use the site at all.
    ///
    /// Called on every authenticated request, so the answer is always current.
    ///
    /// The default lets every account [`UserStore::find`] returns log in but do nothing
    /// ([`Access::none`]), and logs a warning once: a store that forgets this method fails
    /// closed. Override it to say what each account may do; return [`Access::superuser`] for
    /// accounts that may do everything.
    ///
    /// # Errors
    /// Whatever the lookup fails with; the request is then answered with a `500`.
    async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "the account store does not implement UserStore::access, so accounts may log \
                 in but do nothing; implement it to grant permissions"
            );
        });
        Ok(self.find(username).await?.map(|_| Access::none()))
    }

    /// Called after each successful login with the account's username, e.g. to record the
    /// time. The default does nothing.
    ///
    /// # Errors
    /// Whatever the store fails with; the login is then refused with a `500`.
    async fn logged_in(&self, _username: &str) -> Result<(), AdminError> {
        Ok(())
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

    /// Uses up the account's recovery code with this digest, and says whether there was one.
    /// Recovery codes stand in for an authenticator code once each, after a lost phone. The
    /// default has none.
    ///
    /// # Errors
    /// Whatever the store fails with; the login is then refused with a `500`.
    async fn use_recovery_code(&self, _username: &str, _digest: &str) -> Result<bool, AdminError> {
        Ok(false)
    }

    /// The account owning the API key with this digest, if the key exists and has not
    /// expired. The account's permissions still come from [`UserStore::access`], so a
    /// deactivated account's keys stop working. The default has no keys.
    ///
    /// # Errors
    /// Whatever the store fails with; the request is then answered with a `500`.
    async fn api_key(&self, _digest: &str) -> Result<Option<ApiKeyOwner>, AdminError> {
        Ok(None)
    }
}

/// The account an API key belongs to, as [`UserStore::api_key`] finds it.
#[derive(Clone, Debug)]
pub struct ApiKeyOwner {
    /// Login name of the account the key acts as.
    pub username: String,
    /// The key may only read: adding, changing and deleting are refused.
    pub read_only: bool,
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

    /// The one operator may do everything.
    async fn access(&self, username: &str) -> Result<Option<Access>, AdminError> {
        Ok((username == self.user.username).then(Access::superuser))
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
    if !state.auth.methods.password {
        return Err(AdminError::Forbidden(
            "password login is turned off on this site".into(),
        ));
    }
    let typed = credentials.username.trim();
    // Look the account up first, so that its username and its e-mail address share one
    // attempt count; a name that matches nothing is counted as typed.
    let user = state.users.find(typed).await?;
    let key = user
        .as_ref()
        .map_or_else(|| typed.to_lowercase(), |u| u.username.clone());
    state
        .limiter
        .attempt(&key, address)
        .map_err(|retry_after_secs| {
            tracing::warn!(login = %typed, ?address, "login attempts exhausted");
            AdminError::RateLimited { retry_after_secs }
        })?;

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
            if !second_factor(state, &user.username, secret, code).await? {
                return Err(AdminError::Unauthorized);
            }
        }
        None if state.auth.totp_required => return Err(AdminError::Unauthorized),
        None => {}
    }
    state.limiter.succeeded(&key, address);
    state.users.logged_in(&user.username).await?;
    state.auth.issue(&user.username, &user.password_hash)
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

/// Characters of recovery codes: no `0`/`o`, `1`/`l`/`i`, which are easy to misread.
const RECOVERY_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const RECOVERY_CHARS: usize = 16;
/// How many recovery codes an account gets at a time.
pub(crate) const RECOVERY_CODES: usize = 10;

const API_KEY_PREFIX: &str = "cadmus_";
const API_KEY_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// Random characters in a key: 40 of 62 is about 238 bits.
const API_KEY_CHARS: usize = 40;

/// A new API key, and the start of it that is shown to tell keys apart.
pub(crate) fn new_api_key() -> (String, String) {
    let random: String = (0..API_KEY_CHARS)
        .map(|_| char::from(API_KEY_ALPHABET[rand::random_range(0..API_KEY_ALPHABET.len())]))
        .collect();
    let key = format!("{API_KEY_PREFIX}{random}");
    let prefix = key[..API_KEY_PREFIX.len() + 8].to_owned();
    (key, prefix)
}

/// What is stored for an API key, or `None` if it is not shaped like one. The key is long and
/// random, so a plain hash is enough; no slow password hash is needed.
pub(crate) fn api_key_digest(key: &str) -> Option<String> {
    let random = key.strip_prefix(API_KEY_PREFIX)?;
    if random.len() != API_KEY_CHARS || !random.bytes().all(|b| API_KEY_ALPHABET.contains(&b)) {
        return None;
    }
    // "heraapi" is an earlier name of the framework; changing it would invalidate every stored key.
    let digest = sha2::Sha256::digest(format!("heraapi-api-key:{key}").as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// A fresh set of recovery codes, as shown to the person: `xxxx-xxxx-xxxx-xxxx`.
pub(crate) fn new_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODES)
        .map(|_| {
            let chars: Vec<char> = (0..RECOVERY_CHARS)
                .map(|_| {
                    char::from(RECOVERY_ALPHABET[rand::random_range(0..RECOVERY_ALPHABET.len())])
                })
                .collect();
            chars
                .chunks(4)
                .map(|group| group.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect()
}

/// What is stored of a recovery code; `None` if `typed` cannot be one. Codes carry about 79
/// random bits, so a plain SHA-256 cannot be reversed by guessing.
pub(crate) fn recovery_digest(typed: &str) -> Option<String> {
    let normalised: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if normalised.len() != RECOVERY_CHARS
        || !normalised.bytes().all(|b| RECOVERY_ALPHABET.contains(&b))
    {
        return None;
    }
    // "heraapi" is an earlier name of the framework; changing it would invalidate every stored
    // recovery code.
    let digest = sha2::Sha256::digest(format!("heraapi-recovery:{normalised}").as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks the second factor of a login: a current authenticator code, used once, or one of
/// the account's recovery codes, which is used up.
pub(crate) async fn second_factor(
    state: &AdminState,
    username: &str,
    secret: &str,
    typed: &str,
) -> Result<bool, AdminError> {
    if let Some(step) = totp_step(secret, typed, unix_now()) {
        // Each code works once, so one read over a shoulder or off the wire is useless.
        let fresh = state.users.accept_totp_step(username, step).await?;
        if !fresh {
            tracing::warn!(%username, "authenticator code reused");
        }
        return Ok(fresh);
    }
    if let Some(digest) = recovery_digest(typed)
        && state.users.use_recovery_code(username, &digest).await?
    {
        tracing::warn!(%username, "logged in with a recovery code");
        return Ok(true);
    }
    Ok(false)
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

/// A short digest of `value`, for telling whether a secret (such as a password hash) changed
/// without carrying the secret itself.
pub(crate) fn fingerprint(value: &str) -> String {
    sha2::Sha256::digest(value.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The login token and the API key a request carries, if any. An API key comes as a Bearer
/// token or in `X-API-Key`; a Bearer token that is not an API key is a login token.
fn credentials(headers: &HeaderMap) -> (Option<&str>, Option<&str>) {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let api_key = bearer
        .filter(|token| token.starts_with(API_KEY_PREFIX))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    let token = bearer.filter(|_| api_key.is_none());
    (token, api_key)
}

/// An authenticated request. Extracting it is what protects a handler.
pub(crate) struct Session {
    pub(crate) username: String,
    pub(crate) access: Access,
    csrf: String,
    via_cookie: bool,
    /// When the login happened (seconds since 1970); `0` for API keys.
    issued_at: u64,
    /// Authenticated by a read-only API key: only viewing is allowed.
    read_only: bool,
}

/// How long after logging in an account counts as having just proved who it is, for
/// changes that would otherwise ask for the password again.
const FRESH_LOGIN_SECS: u64 = 15 * 60;

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

    /// Who sent the request: a login token, an API key (if `api_keys` are accepted here) or
    /// the session cookie. `None` if none of them is valid.
    async fn authenticate(
        headers: &HeaderMap,
        state: &AdminState,
        api_keys: bool,
    ) -> Result<Option<Self>, AdminError> {
        match credentials(headers) {
            (_, Some(key)) if api_keys => Self::from_api_key(state, key).await,
            (_, Some(_)) => Ok(None),
            (Some(token), None) => match state.auth.verify(token) {
                Some(claims) => Self::load(state, claims, false).await,
                None => Ok(None),
            },
            (None, None) => Self::from_cookie(headers, state).await,
        }
    }

    /// Builds the session for verified claims, reading the account's current permissions.
    /// `None` means the account no longer exists, may no longer log in, or has changed its
    /// password since the token was issued.
    async fn load(
        state: &AdminState,
        claims: Claims,
        via_cookie: bool,
    ) -> Result<Option<Self>, AdminError> {
        let Some(user) = state.users.find(&claims.sub).await? else {
            return Ok(None);
        };
        // Compare usernames too: `find` may also accept an e-mail address.
        if user.username != claims.sub
            || !constant_time_eq(
                fingerprint(&user.password_hash).as_bytes(),
                claims.pw.as_bytes(),
            )
        {
            return Ok(None);
        }
        Ok(state.users.access(&claims.sub).await?.map(|access| Self {
            username: claims.sub,
            access,
            csrf: claims.csrf,
            via_cookie,
            issued_at: claims.iat,
            read_only: false,
        }))
    }

    /// The session for an API key, if it is a valid key of an account that may still log in.
    async fn from_api_key(state: &AdminState, key: &str) -> Result<Option<Self>, AdminError> {
        if !state.auth.api_keys {
            return Ok(None);
        }
        let Some(digest) = api_key_digest(key) else {
            return Ok(None);
        };
        let Some(owner) = state.users.api_key(&digest).await? else {
            return Ok(None);
        };
        Ok(state
            .users
            .access(&owner.username)
            .await?
            .map(|access| Self {
                username: owner.username,
                access,
                csrf: String::new(),
                via_cookie: false,
                issued_at: 0,
                read_only: owner.read_only,
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

    /// Whether the login is recent enough to stand in for typing the password again.
    pub(crate) fn is_fresh(&self) -> bool {
        unix_now().saturating_sub(self.issued_at) <= FRESH_LOGIN_SECS
    }

    /// Whether this account may do `action` on `model`.
    pub(crate) fn can(&self, model: &str, action: Action) -> bool {
        (!self.read_only || action == Action::View) && self.access.allows(model, action)
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
        if self.access.is_superuser() && !self.read_only {
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
        let api = parts
            .uri
            .path()
            .starts_with(&format!("{}/", state.api_base));
        // API keys only work for the REST API (and your own routes): the admin pages, and
        // with them account management, need a login.
        let session = Self::authenticate(&parts.headers, state, api).await;
        let (bearer, api_key) = credentials(&parts.headers);
        let bearer = bearer.or(api_key);
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

/// Put on the requests of your own routes by [`crate::Admin::identify`], so that
/// [`CurrentUser`] can check who sent them.
#[derive(Clone)]
pub(crate) struct Identify(pub(crate) Arc<AdminState>);

/// The header a cookie-authenticated request that changes something must carry the
/// session's CSRF token in, for [`CurrentUser`].
const CSRF_HEADER: &str = "x-csrf-token";

/// The account that sent a request to one of your own routes.
///
/// Take it as an extractor in a handler given to [`crate::App::routes`]: the handler then
/// answers `401` to anyone who is not logged in. Take `Option<CurrentUser>` for a route that
/// serves everyone but shows more to accounts. Routes that take neither are public.
///
/// A request is logged in by any of: a login token (`Authorization: Bearer`, from
/// `POST /api/auth/token`), an API key (`Authorization: Bearer` or `X-API-Key`), or the
/// admin's session cookie. The cookie is only sent to paths under the admin (`/admin/...`).
/// The account's permissions are read again on every request, like everywhere else.
///
/// Requests that change something (any method but `GET`, `HEAD`, `OPTIONS` and `TRACE`) are
/// refused with `403` when they come with a read-only API key, or with the session cookie but
/// without the session's CSRF token in an `X-CSRF-Token` header (see
/// [`CurrentUser::csrf_token`]).
///
/// Being logged in says nothing about what the account may do: check that with
/// [`CurrentUser::require`] or [`CurrentUser::can`].
///
/// ```
/// use axum::{Json, Router, routing::post};
/// use cadmus::{Action, ApiResult, CurrentUser};
///
/// async fn publish(user: CurrentUser) -> ApiResult<Json<String>> {
///     user.require("post", Action::Change)?;
///     Ok(Json(format!("published by {}", user.username())))
/// }
///
/// let routes: Router = Router::new().route("/posts/publish", post(publish));
/// ```
#[derive(Debug, Clone)]
pub struct CurrentUser {
    username: String,
    access: Access,
    read_only: bool,
    /// The session's CSRF token, for requests that came with the session cookie.
    csrf: Option<String>,
}

impl CurrentUser {
    /// The account's login name.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Everything the account may do.
    #[must_use]
    pub fn access(&self) -> &Access {
        &self.access
    }

    /// Whether the account is a superuser (and did not log in with a read-only API key).
    #[must_use]
    pub fn is_superuser(&self) -> bool {
        self.access.is_superuser() && !self.read_only
    }

    /// Whether the request came with a read-only API key, which may only view.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the account may do `action` on `model` (the model's URL name; `files` for file
    /// uploads). A read-only API key may only view.
    #[must_use]
    pub fn can(&self, model: &str, action: Action) -> bool {
        (!self.read_only || action == Action::View) && self.access.allows(model, action)
    }

    /// Refuses with `403` unless the account may do `action` on `model`.
    ///
    /// # Errors
    /// [`AdminError::Forbidden`] if it may not.
    pub fn require(&self, model: &str, action: Action) -> Result<(), AdminError> {
        if self.can(model, action) {
            return Ok(());
        }
        Err(AdminError::Forbidden(format!(
            "you do not have permission to {} `{model}`",
            action.as_str()
        )))
    }

    /// Refuses with `403` unless the account is a superuser.
    ///
    /// # Errors
    /// [`AdminError::Forbidden`] if it is not.
    pub fn require_superuser(&self) -> Result<(), AdminError> {
        if self.is_superuser() {
            return Ok(());
        }
        Err(AdminError::Forbidden("only superusers may do this".into()))
    }

    /// The token a page of yours must send back in an `X-CSRF-Token` header with requests
    /// that change something, when it is logged in with the session cookie. `None` for
    /// requests that came with a login token or an API key, which need no such token.
    #[must_use]
    pub fn csrf_token(&self) -> Option<&str> {
        self.csrf.as_deref()
    }

    /// Who sent the request, if anyone. Errors are refusals (a missing CSRF token, a
    /// read-only key trying to change something) and failures of the account store.
    async fn identify(parts: &Parts) -> Result<Option<Self>, AdminError> {
        let Some(Identify(state)) = parts.extensions.get::<Identify>().cloned() else {
            return Err(AdminError::Config(
                "CurrentUser only works in routes given to App::routes (or passed through \
                 Admin::identify)"
                    .into(),
            ));
        };
        let Some(session) = Session::authenticate(&parts.headers, &state, true).await? else {
            return Ok(None);
        };
        if !parts.method.is_safe() {
            if session.read_only {
                return Err(AdminError::Forbidden(
                    "a read-only API key may only view".into(),
                ));
            }
            let submitted = parts.headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
            session.check_csrf(submitted)?;
        }
        Ok(Some(Self {
            csrf: session.via_cookie.then_some(session.csrf),
            username: session.username,
            access: session.access,
            read_only: session.read_only,
        }))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::identify(parts)
            .await?
            .ok_or(AdminError::Unauthorized)?)
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(Self::identify(parts).await?)
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
        let token = config.issue("root", "some-hash").unwrap();
        assert_eq!(config.verify(&token).unwrap().sub, "root");

        let other = AuthConfig::new("ffffffffffffffffffffffffffffffff").unwrap();
        assert!(other.verify(&token).is_none());
        assert!(config.verify("not.a.token").is_none());
    }

    #[test]
    fn recovery_codes_are_random_readable_and_normalised() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODES);
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), codes.len());
        for code in &codes {
            assert_eq!(code.len(), 19);
            assert_eq!(code.matches('-').count(), 3);
            assert!(!code.contains(['0', 'o', '1', 'l', 'i']));
            let digest = recovery_digest(code).unwrap();
            // Typed in capitals, with spaces, or without dashes: the same code.
            assert_eq!(recovery_digest(&code.to_uppercase()), Some(digest.clone()));
            assert_eq!(
                recovery_digest(&code.replace('-', " ")),
                Some(digest.clone())
            );
            assert_eq!(recovery_digest(&code.replace('-', "")), Some(digest));
        }
        assert_eq!(recovery_digest("123456"), None);
        assert_eq!(recovery_digest("aaaa-aaaa-aaaa-aaa0"), None);
        assert_eq!(recovery_digest("aaaa-aaaa-aaaa-aaaa-a"), None);
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

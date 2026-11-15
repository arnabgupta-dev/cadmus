//! Logging in through the REST API, for apps: every login method ends in a pair of tokens, a
//! short-lived access token for requests and a refresh token that keeps the login going.
//!
//! Any active account may log in here; whether it is staff only decides whether it may also
//! use the admin. Accounts with two-factor login first get a `challenge`, which they trade for
//! the tokens with an authenticator code at `POST /api/auth/totp`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AdminState;
use crate::auth::{AdminUser, Session, fingerprint, second_factor};
use crate::error::{AdminError, ApiError, ApiResult};

type Shared = State<Arc<AdminState>>;

/// What a challenge stands for: a login that passed its first factor.
const PENDING_PURPOSE: &str = "api-pending-login";
/// How long someone may take to type their authenticator code.
const PENDING_SECS: u64 = 300;

/// The tokens of a login through the API.
///
/// Send `access_token` as `Authorization: Bearer <token>` until it expires (`expires_in`
/// seconds), then trade `refresh_token` for a new pair at `POST /api/auth/refresh`.
#[derive(Debug, Clone, Serialize)]
pub struct TokenPair {
    /// Signed login token for API requests.
    pub access_token: String,
    /// Always `"Bearer"`.
    pub token_type: &'static str,
    /// Seconds until `access_token` expires.
    pub expires_in: u64,
    /// Trades for a new pair once; `None` when refresh tokens are off or the accounts are
    /// not the built-in ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Seconds until `refresh_token` expires if it is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_expires_in: Option<u64>,
}

/// How a login through the API ends: the tokens, or a request for the authenticator code.
#[derive(Debug, Clone)]
pub enum LoginOutcome {
    /// Logged in.
    Tokens(TokenPair),
    /// The account uses two-factor login: send this `challenge` with a code from the
    /// authenticator app (or a recovery code) to `POST /api/auth/totp` within 5 minutes.
    TwoFactor {
        /// Proves the first factor passed.
        challenge: String,
    },
}

impl IntoResponse for LoginOutcome {
    fn into_response(self) -> Response {
        match self {
            Self::Tokens(pair) => Json(pair).into_response(),
            Self::TwoFactor { challenge } => (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "code": "totp_required",
                        "message": "enter the code from your authenticator app",
                        "field": null,
                    },
                    "challenge": challenge,
                })),
            )
                .into_response(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Pending {
    sub: String,
    exp: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn totp_setup_required() -> AdminError {
    AdminError::custom(
        StatusCode::FORBIDDEN,
        "totp_setup_required",
        "this site requires two-factor login; ask an administrator to set it up for your account",
    )
}

/// Signs the tokens for an account whose login is complete, and records the login.
pub(crate) async fn issue(state: &AdminState, user: &AdminUser) -> Result<TokenPair, AdminError> {
    let access_token = state.auth.issue(&user.username, &user.password_hash)?;
    let ttl = state.auth.refresh_ttl_secs;
    let refresh_token = match &state.accounts {
        Some(db) if ttl > 0 => match crate::users::find_account(db, &user.username).await? {
            Some(account) => Some(
                crate::sessions::start(db, account.id, &fingerprint(&user.password_hash), ttl)
                    .await?,
            ),
            None => None,
        },
        _ => None,
    };
    state.users.logged_in(&user.username).await?;
    Ok(TokenPair {
        access_token,
        token_type: "Bearer",
        expires_in: state.auth.token_ttl_secs,
        refresh_expires_in: refresh_token.as_ref().map(|_| ttl),
        refresh_token,
    })
}

/// Finishes a login whose first factor is proven (another site vouched for the person, or
/// they used a login link): a challenge if the account has two-factor login, otherwise the
/// tokens. Inactive accounts get [`AdminError::Unauthorized`].
pub(crate) async fn finish(state: &AdminState, username: &str) -> Result<LoginOutcome, AdminError> {
    let Some(user) = state.users.find(username).await? else {
        return Err(AdminError::Unauthorized);
    };
    if user.totp_secret.is_some() {
        let pending = Pending {
            sub: user.username,
            exp: unix_now() + PENDING_SECS,
        };
        return Ok(LoginOutcome::TwoFactor {
            challenge: state.auth.seal(PENDING_PURPOSE, &pending)?,
        });
    }
    if state.auth.totp_required {
        return Err(totp_setup_required());
    }
    Ok(LoginOutcome::Tokens(issue(state, &user).await?))
}

/// `POST /api/auth/token`: username (or e-mail address) and password, and the authenticator
/// code if the account has one. Without the code, an account with two-factor login gets a
/// challenge instead.
pub(crate) async fn password(
    state: &AdminState,
    credentials: &crate::auth::Credentials,
    address: Option<IpAddr>,
) -> Result<LoginOutcome, AdminError> {
    let (user, key) = crate::auth::check_password(state, credentials, address).await?;
    let code = credentials
        .totp
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty());
    match (&user.totp_secret, code) {
        (Some(secret), Some(code)) => {
            if !second_factor(state, &user.username, secret, code).await? {
                return Err(AdminError::Unauthorized);
            }
        }
        (Some(_), None) => return finish(state, &user.username).await,
        (None, _) if state.auth.totp_required => return Err(totp_setup_required()),
        (None, _) => {}
    }
    state.limiter.succeeded(&key, address);
    Ok(LoginOutcome::Tokens(issue(state, &user).await?))
}

#[derive(Deserialize)]
pub(crate) struct CodeBody {
    challenge: String,
    #[serde(alias = "totp")]
    code: String,
}

/// `POST /api/auth/totp`: the second step of any login with two-factor.
pub(crate) async fn totp(
    State(state): Shared,
    parts: Parts,
    Json(body): Json<CodeBody>,
) -> ApiResult<LoginOutcome> {
    let expired = || {
        AdminError::custom(
            StatusCode::UNAUTHORIZED,
            "challenge_expired",
            "that login took too long or is not valid; log in again",
        )
    };
    let pending = state
        .auth
        .open::<Pending>(PENDING_PURPOSE, body.challenge.trim())
        .ok_or_else(expired)?;
    let address = state.auth.client_ip(&parts);
    state
        .limiter
        .attempt(&pending.sub, address)
        .map_err(|retry_after_secs| AdminError::RateLimited { retry_after_secs })?;
    let user = state.users.find(&pending.sub).await?.ok_or_else(expired)?;
    let accepted = match &user.totp_secret {
        Some(secret) => second_factor(&state, &user.username, secret, body.code.trim()).await?,
        // Turned off meanwhile.
        None => !state.auth.totp_required,
    };
    if !accepted {
        tracing::warn!(username = %user.username, "wrong authenticator code for an API login");
        return Err(AdminError::Unauthorized.into());
    }
    state.limiter.succeeded(&user.username, address);
    Ok(LoginOutcome::Tokens(issue(&state, &user).await?))
}

#[derive(Deserialize)]
pub(crate) struct RefreshBody {
    refresh_token: String,
}

fn refresh_refused() -> AdminError {
    AdminError::custom(
        StatusCode::UNAUTHORIZED,
        "invalid_refresh_token",
        "the refresh token is not valid; log in again",
    )
}

/// `POST /api/auth/refresh`: a new pair for a refresh token, which is then spent.
pub(crate) async fn refresh(
    State(state): Shared,
    Json(body): Json<RefreshBody>,
) -> ApiResult<Json<TokenPair>> {
    let db = state.accounts.as_ref().ok_or_else(refresh_refused)?;
    let ttl = state.auth.refresh_ttl_secs;
    if ttl == 0 {
        return Err(refresh_refused().into());
    }
    let current = crate::sessions::find(db, &body.refresh_token)
        .await?
        .ok_or_else(refresh_refused)?;
    // The account must still be able to log in, with the password it logged in with.
    let account = crate::users::user::Entity::find_by_id(current.user_id)
        .one(db)
        .await?;
    let user = match account {
        Some(account) => state.users.find(&account.username).await?,
        None => None,
    };
    let Some(user) = user.filter(|user| {
        crate::auth::constant_time_eq(
            fingerprint(&user.password_hash).as_bytes(),
            current.password_print.as_bytes(),
        )
    }) else {
        crate::sessions::end_family(db, &current.family).await?;
        return Err(refresh_refused().into());
    };
    let next = crate::sessions::rotate(db, &current, ttl)
        .await?
        .ok_or_else(refresh_refused)?;
    Ok(Json(TokenPair {
        access_token: state.auth.issue(&user.username, &user.password_hash)?,
        token_type: "Bearer",
        expires_in: state.auth.token_ttl_secs,
        refresh_token: Some(next),
        refresh_expires_in: Some(ttl),
    }))
}

/// `POST /api/auth/logout`: ends the login the refresh token belongs to. The access token
/// keeps working until it expires, which is why it is short-lived.
pub(crate) async fn logout(
    State(state): Shared,
    Json(body): Json<RefreshBody>,
) -> ApiResult<StatusCode> {
    if let Some(db) = &state.accounts {
        crate::sessions::end(db, &body.refresh_token).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/auth/me`: the account the request is logged in as.
pub(crate) async fn me(
    State(state): Shared,
    session: Session,
) -> ApiResult<Json<serde_json::Value>> {
    session.require_bearer()?;
    let mut me = json!({
        "username": session.username,
        "staff": session.access.is_staff(),
        "superuser": session.access.is_superuser(),
    });
    if let Some(db) = &state.accounts
        && let Some(account) = crate::users::find_account(db, &session.username).await?
    {
        me["id"] = json!(account.id);
        me["email"] = json!(account.email);
        me["first_name"] = json!(account.first_name);
        me["last_name"] = json!(account.last_name);
        me["two_factor"] = json!(account.totp_secret.is_some());
        me["last_login"] = json!(account.last_login);
    }
    Ok(Json(me))
}

/// Hands out logins from your own routes, after you have proven who someone is in a way of
/// your own (a code sent by text message, a partner's single sign-on, ...).
///
/// The account's two-factor login still applies: [`Logins::log_in`] answers with a
/// challenge for accounts that have it, which the app completes at `POST /api/auth/totp`.
///
/// ```ignore
/// async fn sms_login(logins: Logins, Json(body): Json<SmsCode>) -> ApiResult<LoginOutcome> {
///     let username = check_sms_code(&body).await?;
///     Ok(logins.log_in(&username).await?)
/// }
/// ```
#[derive(Clone)]
pub struct Logins {
    state: Arc<AdminState>,
}

impl Logins {
    /// Logs the account in as if it had passed its first factor: the tokens, or a challenge
    /// for an account with two-factor login.
    ///
    /// # Errors
    /// [`AdminError::Unauthorized`] if there is no such active account, and
    /// [`AdminError::Custom`] with code `totp_setup_required` when two-factor login is
    /// required but the account has none.
    pub async fn log_in(&self, username: &str) -> Result<LoginOutcome, AdminError> {
        finish(&self.state, username).await
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Logins {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<crate::auth::Identify>() {
            Some(crate::auth::Identify(state)) => Ok(Self {
                state: Arc::clone(state),
            }),
            None => Err(AdminError::Config(
                "Logins only works in routes given to App::routes (or passed through \
                 Admin::identify)"
                    .into(),
            )
            .into()),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ProviderBody {
    /// An ID token the app got from the provider (OpenID Connect providers).
    id_token: Option<String>,
    /// Or the code the provider sent back to the app, with what the app used to get it.
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    /// The nonce the app sent to the provider, if any; the ID token must carry it.
    nonce: Option<String>,
}

/// `POST /api/auth/{provider}`: a login the app did with another site. The app sends either
/// the ID token it received (Google, Microsoft, any OpenID Connect provider), or the code the
/// provider sent back to it with its `redirect_uri` and PKCE `code_verifier` (every provider,
/// including GitHub and Facebook). Then the same rules as the admin: the account linked to
/// that identity, or the one with the address the provider vouches for, or a new account
/// (not staff) if `signup` allows the address.
pub(crate) async fn provider(
    State(state): Shared,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(body): Json<ProviderBody>,
) -> ApiResult<LoginOutcome> {
    let methods = &state.auth.methods;
    let provider = methods
        .provider(&name)
        .ok_or_else(|| AdminError::NotFound(format!("login method `{name}`")))?;
    let http = methods.http().map_err(|problem| {
        tracing::warn!(provider = %provider.name, %problem, "login provider unavailable");
        AdminError::custom(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            format!(
                "{} cannot be reached; try again in a moment",
                provider.label
            ),
        )
    })?;
    let filled =
        |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let nonce = filled(body.nonce);
    let identity = match (filled(body.id_token), filled(body.code)) {
        (Some(id_token), None) => {
            if !provider.issues_id_tokens() {
                return Err(AdminError::BadInput {
                    field: "id_token".into(),
                    message: format!(
                        "{} issues no ID tokens; send the `code` it gave the app instead",
                        provider.label
                    ),
                }
                .into());
            }
            provider
                .identify_id_token(http, &id_token, nonce.as_deref())
                .await
        }
        (None, Some(code)) => {
            let redirect_uri = filled(body.redirect_uri).ok_or_else(|| AdminError::BadInput {
                field: "redirect_uri".into(),
                message: "send the redirect_uri the app used to get the code".into(),
            })?;
            if !methods.app.redirect_uris.contains(&redirect_uri) {
                return Err(AdminError::BadInput {
                    field: "redirect_uri".into(),
                    message: "not one of the addresses in auth.app.redirect_uris".into(),
                }
                .into());
            }
            provider
                .identify(
                    http,
                    &code,
                    filled(body.code_verifier).as_deref(),
                    nonce.as_deref(),
                    &redirect_uri,
                )
                .await
        }
        _ => {
            return Err(AdminError::Unsupported(
                "send either `id_token` or `code` (with `redirect_uri`)".into(),
            )
            .into());
        }
    };
    let identity = identity.map_err(|crate::oauth::Failure::Broken(problem)| {
        tracing::warn!(provider = %provider.name, %problem, "API login through provider failed");
        AdminError::custom(
            StatusCode::UNAUTHORIZED,
            "provider_login_failed",
            format!("{} did not confirm this login", provider.label),
        )
    })?;
    let db = state.accounts.as_ref().ok_or_else(|| {
        AdminError::Config("logging in through providers needs the built-in accounts".into())
    })?;
    match crate::users::resolve_identity(db, methods, &provider.label, &identity, false).await? {
        Ok(account) => Ok(finish(&state, &account.username).await?),
        Err(message) => {
            tracing::warn!(provider = %provider.name, email = ?identity.email, "API login through provider refused");
            Err(AdminError::custom(StatusCode::FORBIDDEN, "no_account", message).into())
        }
    }
}

// ---- e-mailed links, for apps -------------------------------------------------------------------

/// Separate from the admin's, so a link made for one cannot be used at the other.
const LINK_PURPOSE: &str = "api-login-link";
const REGISTER_PURPOSE: &str = "api-register";
const RESET_PURPOSE: &str = "api-password-reset";
const LINK_SECS: u64 = 15 * 60;
const REGISTER_SECS: u64 = 24 * 60 * 60;
const RESET_SECS: u64 = 60 * 60;

/// What a login link stands for.
#[derive(Serialize, Deserialize)]
struct LinkClaims {
    /// The account, or `None` for someone `signup` lets create one.
    sub: Option<String>,
    email: String,
    /// The account's last login when the link was made: any login since retires it.
    seen: String,
    exp: u64,
}

/// What a sign-up confirmation stands for: someone who can read mail at this address.
#[derive(Serialize, Deserialize)]
struct RegisterClaims {
    email: String,
    exp: u64,
}

/// What a password reset link stands for.
#[derive(Serialize, Deserialize)]
struct ResetClaims {
    sub: String,
    /// Fingerprint of the password hash when the link was made: a new password retires it.
    pw: String,
    exp: u64,
}

#[derive(Deserialize)]
pub(crate) struct EmailBody {
    email: String,
}

#[derive(Deserialize)]
pub(crate) struct TokenBody {
    token: String,
    #[serde(default)]
    password: Option<String>,
}

/// The same answer whether or not the address has an account, so nobody can find out who
/// is registered; the mail itself is sent in the background.
fn sent(what: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::ACCEPTED,
        Json(json!({ "detail": format!("if the address may {what}, a message is on its way") })),
    )
}

fn invalid_link() -> AdminError {
    AdminError::custom(
        StatusCode::BAD_REQUEST,
        "invalid_link",
        "that link has expired or was already used; ask for a new one",
    )
}

/// The accounts database, and the app's address that e-mailed links point to.
fn mail_ready(state: &AdminState, what: &str) -> Result<sea_orm::DatabaseConnection, AdminError> {
    let off = || AdminError::NotFound(format!("{what} for apps are not enabled on this site"));
    if state.auth.methods.app.url.is_none() {
        return Err(off());
    }
    state.accounts.clone().ok_or_else(off)
}

fn email_address(typed: &str) -> Result<String, AdminError> {
    crate::login::address(typed).ok_or_else(|| AdminError::BadInput {
        field: "email".into(),
        message: "enter an e-mail address like name@example.com".into(),
    })
}

fn limited(state: &AdminState, key: &str, parts: &Parts) -> Result<(), AdminError> {
    state
        .limiter
        .attempt(key, state.auth.client_ip(parts))
        .map_err(|retry_after_secs| AdminError::RateLimited { retry_after_secs })
}

fn app_link(state: &AdminState, template: &str, token: &str) -> Result<String, AdminError> {
    state
        .auth
        .methods
        .app_link(template, token)
        .ok_or_else(|| AdminError::Config("auth.app.url is not set".into()))
}

/// `POST /api/auth/link`: e-mails a login link to the app, for `AuthMethod::MagicLink`.
pub(crate) async fn link_request(
    State(state): Shared,
    parts: Parts,
    Json(body): Json<EmailBody>,
) -> ApiResult<impl IntoResponse> {
    if !state.auth.methods.magic_link {
        return Err(AdminError::NotFound("login links are not enabled on this site".into()).into());
    }
    let db = mail_ready(&state, "login links")?;
    let email = email_address(&body.email)?;
    limited(&state, &email, &parts)?;
    let account = crate::users::find_by_email(&db, &email)
        .await?
        .filter(|account| account.is_active);
    let claims = match &account {
        Some(account) => Some(LinkClaims {
            sub: Some(account.username.clone()),
            email: email.clone(),
            seen: crate::login::last_login_print(account),
            exp: unix_now() + LINK_SECS,
        }),
        None if state.auth.methods.signup_allows(&email) => Some(LinkClaims {
            sub: None,
            email: email.clone(),
            seen: String::new(),
            exp: unix_now() + LINK_SECS,
        }),
        None => None,
    };
    match claims {
        Some(claims) => {
            let token = state.auth.seal(LINK_PURPOSE, &claims)?;
            let link = app_link(&state, &state.auth.methods.app.links.login, &token)?;
            let site = &state.settings.site_title;
            let message = crate::login::link_email(
                &state,
                email,
                format!("Your login link for {site}"),
                &[&format!("Use this link to log in to {site}:")],
                "Log in",
                &link,
                "It works once, within 15 minutes. If you did not ask for it, ignore this \
                 message: nobody can log in without the link.",
            );
            crate::mail::send_later(Arc::clone(&state.mailer), message);
        }
        None => tracing::info!(%email, "API login link asked for an address without an account"),
    }
    Ok(sent("log in"))
}

/// `POST /api/auth/link/verify`: the token from a login link, for the tokens (or a challenge).
pub(crate) async fn link_verify(
    State(state): Shared,
    Json(body): Json<TokenBody>,
) -> ApiResult<LoginOutcome> {
    if !state.auth.methods.magic_link {
        return Err(AdminError::NotFound("login links are not enabled on this site".into()).into());
    }
    let db = mail_ready(&state, "login links")?;
    let claims = state
        .auth
        .open::<LinkClaims>(LINK_PURPOSE, body.token.trim())
        .ok_or_else(invalid_link)?;
    let username = match &claims.sub {
        Some(username) => {
            let account = crate::users::find_account(&db, username).await?;
            // Any login since the link was made, including through this link, retires it.
            account
                .filter(|account| crate::login::last_login_print(account) == claims.seen)
                .ok_or_else(invalid_link)?
                .username
        }
        // Someone allowed to sign up, who proved the address is theirs.
        None => match crate::users::find_by_email(&db, &claims.email).await? {
            Some(account) => account.username,
            None if state.auth.methods.signup_allows(&claims.email) => {
                crate::users::signup(
                    &db,
                    &claims.email,
                    None,
                    None,
                    &state.auth.methods.signup_groups,
                    false,
                    "API login link",
                )
                .await?
                .username
            }
            None => return Err(invalid_link().into()),
        },
    };
    Ok(finish(&state, &username).await?)
}

/// `POST /api/auth/register`: someone without an account, whose address `signup` allows,
/// asks for one. A confirmation link goes to the address; the password is chosen when it is
/// used, at `POST /api/auth/register/verify`.
pub(crate) async fn register(
    State(state): Shared,
    parts: Parts,
    Json(body): Json<EmailBody>,
) -> ApiResult<impl IntoResponse> {
    if !state.auth.methods.password {
        return Err(
            AdminError::NotFound("password accounts are not enabled on this site".into()).into(),
        );
    }
    let db = mail_ready(&state, "sign-ups")?;
    let email = email_address(&body.email)?;
    if !state.auth.methods.signup_allows(&email) {
        return Err(AdminError::custom(
            StatusCode::FORBIDDEN,
            "signup_closed",
            "this site does not take sign-ups from that address",
        )
        .into());
    }
    limited(&state, &email, &parts)?;
    if crate::users::find_by_email(&db, &email).await?.is_some() {
        // Already registered: they should log in or reset their password. Say nothing more.
        tracing::info!(%email, "sign-up asked for an address that has an account");
        return Ok(sent("sign up"));
    }
    let claims = RegisterClaims {
        email: email.clone(),
        exp: unix_now() + REGISTER_SECS,
    };
    let token = state.auth.seal(REGISTER_PURPOSE, &claims)?;
    let link = app_link(&state, &state.auth.methods.app.links.register, &token)?;
    let site = &state.settings.site_title;
    let message = crate::login::link_email(
        &state,
        email,
        format!("Confirm your address for {site}"),
        &[&format!(
            "Someone asked for an account at {site} with this address. Confirm it and choose \
             your password here:"
        )],
        "Confirm my address",
        &link,
        "The link works for a day. If you did not ask for it, ignore this message: no \
         account is made without it.",
    );
    crate::mail::send_later(Arc::clone(&state.mailer), message);
    Ok(sent("sign up"))
}

/// `POST /api/auth/register/verify`: the token from a confirmation link and the password to
/// use. Creates the account (not staff, in `signup_groups`) and logs it in.
pub(crate) async fn register_verify(
    State(state): Shared,
    Json(body): Json<TokenBody>,
) -> ApiResult<LoginOutcome> {
    if !state.auth.methods.password {
        return Err(
            AdminError::NotFound("password accounts are not enabled on this site".into()).into(),
        );
    }
    let db = mail_ready(&state, "sign-ups")?;
    let claims = state
        .auth
        .open::<RegisterClaims>(REGISTER_PURPOSE, body.token.trim())
        .ok_or_else(invalid_link)?;
    let password = body.password.unwrap_or_default();
    crate::users::check_password(&password)?;
    // The rules may have changed since the link was sent, and the link works only once.
    if !state.auth.methods.signup_allows(&claims.email)
        || crate::users::find_by_email(&db, &claims.email)
            .await?
            .is_some()
    {
        return Err(invalid_link().into());
    }
    let account = crate::users::signup(
        &db,
        &claims.email,
        None,
        None,
        &state.auth.methods.signup_groups,
        false,
        "API sign-up",
    )
    .await?;
    crate::users::change_password(&db, &account.username, &password).await?;
    Ok(finish(&state, &account.username).await?)
}

/// `POST /api/auth/password/forgot`: e-mails a link to the app for choosing a new password.
pub(crate) async fn password_forgot(
    State(state): Shared,
    parts: Parts,
    Json(body): Json<EmailBody>,
) -> ApiResult<impl IntoResponse> {
    if !state.auth.methods.password {
        return Err(
            AdminError::NotFound("password logins are not enabled on this site".into()).into(),
        );
    }
    let db = mail_ready(&state, "password resets")?;
    let email = email_address(&body.email)?;
    limited(&state, &email, &parts)?;
    let account = crate::users::find_by_email(&db, &email)
        .await?
        .filter(|account| account.is_active);
    match account {
        Some(account) => {
            let claims = ResetClaims {
                sub: account.username.clone(),
                pw: fingerprint(&account.password_hash),
                exp: unix_now() + RESET_SECS,
            };
            let token = state.auth.seal(RESET_PURPOSE, &claims)?;
            let link = app_link(&state, &state.auth.methods.app.links.reset, &token)?;
            let site = &state.settings.site_title;
            let message = crate::login::link_email(
                &state,
                email,
                format!("Choose a new password for {site}"),
                &[&format!(
                    "Someone asked to reset the password of your account at {site}. Choose a \
                     new one here:"
                )],
                "Choose a new password",
                &link,
                "The link works for an hour, until the password is changed. If you did not \
                 ask for it, ignore this message: your password stays as it is.",
            );
            crate::mail::send_later(Arc::clone(&state.mailer), message);
        }
        None => {
            tracing::info!(%email, "API password reset asked for an address without an account")
        }
    }
    Ok(sent("reset its password"))
}

/// `POST /api/auth/password/reset`: the token from a reset link and the new password. Every
/// login made with the old password ends; the app then logs in with the new one.
pub(crate) async fn password_reset(
    State(state): Shared,
    Json(body): Json<TokenBody>,
) -> ApiResult<StatusCode> {
    if !state.auth.methods.password {
        return Err(
            AdminError::NotFound("password logins are not enabled on this site".into()).into(),
        );
    }
    let db = mail_ready(&state, "password resets")?;
    let claims = state
        .auth
        .open::<ResetClaims>(RESET_PURPOSE, body.token.trim())
        .ok_or_else(invalid_link)?;
    let password = body.password.unwrap_or_default();
    crate::users::check_password(&password)?;
    let account = crate::users::find_account(&db, &claims.sub)
        .await?
        .filter(|account| account.is_active && fingerprint(&account.password_hash) == claims.pw)
        .ok_or_else(invalid_link)?;
    crate::users::change_password(&db, &account.username, &password).await?;
    tracing::info!(username = %account.username, "password reset by e-mail, through the API");
    Ok(StatusCode::NO_CONTENT)
}

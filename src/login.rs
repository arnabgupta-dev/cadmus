//! What every way of logging in has in common once the first factor is proven (the
//! authenticator code, then the session), and the logins that go through e-mail: login
//! links and password resets.
//!
//! Requests that send e-mail answer the same whether or not the address has an account, and
//! send in the background, so neither the reply nor its timing tells anyone which addresses
//! are registered. Links are signed, expire, and stop working once used: a login link names
//! the account's last login, a reset link its password hash, and both change when used.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::AdminState;
use crate::auth::totp_step_now;
use crate::error::AdminError;
use crate::handlers::{Mode, login_page_as, session_cookie};
use crate::mail::{Email, send_later};
use crate::users;

const PENDING_COOKIE: &str = "hera_pending";
const PENDING_PURPOSE: &str = "pending-login";
const LINK_PURPOSE: &str = "login-link";
const RESET_PURPOSE: &str = "password-reset";
const INVITE_PURPOSE: &str = "invitation";
/// How long someone may take to type their authenticator code.
const PENDING_SECS: u64 = 300;
const LINK_SECS: u64 = 15 * 60;
const RESET_SECS: u64 = 60 * 60;
const INVITE_SECS: u64 = 7 * 24 * 60 * 60;

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.base.clone();
    let methods = &state.auth.methods;
    let mut router = Router::new();
    if !methods.providers.is_empty() || methods.magic_link {
        router = router.route(&format!("{b}/login/code"), get(code_page).post(code_submit));
    }
    if methods.magic_link && state.accounts.is_some() {
        router = router
            .route(
                &format!("{b}/login/link"),
                axum::routing::post(link_request),
            )
            .route(
                &format!("{b}/login/link/{{token}}"),
                get(link_page).post(link_use),
            );
    }
    if can_invite(&state) {
        router = router.route(
            &format!("{b}/invite/{{token}}"),
            get(invite_page).post(invite_accept),
        );
    }
    if can_reset(&state) {
        router = router
            .route(
                &format!("{b}/password/reset"),
                get(reset_request_page).post(reset_request),
            )
            .route(
                &format!("{b}/password/reset/{{token}}"),
                get(reset_page).post(reset_submit),
            );
    }
    router.with_state(state)
}

/// Whether "Forgot your password?" is offered: passwords are on, accounts are the built-in
/// ones, and links in e-mails can point back to the site.
pub(crate) fn can_reset(state: &AdminState) -> bool {
    let methods = &state.auth.methods;
    methods.password && state.accounts.is_some() && methods.has_public_url()
}

/// Whether superusers can invite people by e-mail: accounts are the built-in ones, and links
/// in e-mails can point back to the site.
pub(crate) fn can_invite(state: &AdminState) -> bool {
    state.accounts.is_some() && state.auth.methods.has_public_url()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A short digest of what must stay the same for a link to keep working.
fn fingerprint(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

type Shared = State<Arc<AdminState>>;

// ---- after the first factor ------------------------------------------------------------------

/// A login that passed its first factor and waits for the authenticator code.
#[derive(Serialize, Deserialize)]
struct Pending {
    sub: String,
    exp: u64,
}

/// Finishes a login whose first factor is proven: asks for the authenticator code if the
/// account has one, otherwise logs in. `cross_site` is for requests that arrive from another
/// site's redirect, where the next step is reached through a page of our own (see
/// [`continue_page`]).
pub(crate) async fn complete(
    state: &AdminState,
    jar: CookieJar,
    username: &str,
    cross_site: bool,
) -> Result<Response, AdminError> {
    // The same rules as a password login: active staff only, and the second factor.
    let Some(user) = state.users.find(username).await? else {
        let page = login_page_as(
            state,
            StatusCode::FORBIDDEN,
            Mode::Login,
            Some("This account cannot log in. Ask an administrator.".into()),
            None,
        )?;
        return Ok((jar, page).into_response());
    };
    let go = |next: String| -> Response {
        if cross_site {
            continue_page(&next)
        } else {
            Redirect::to(&next).into_response()
        }
    };
    if user.totp_secret.is_some() {
        let pending = Pending {
            sub: user.username,
            exp: unix_now() + PENDING_SECS,
        };
        let cookie = Cookie::build((PENDING_COOKIE, state.auth.seal(PENDING_PURPOSE, &pending)?))
            .path(state.base.clone())
            .http_only(true)
            .same_site(SameSite::Lax)
            .secure(state.auth.cookie_secure)
            .build();
        return Ok((jar.add(cookie), go(format!("{}/login/code", state.base))).into_response());
    }
    if state.auth.totp_required {
        let page = login_page_as(
            state,
            StatusCode::FORBIDDEN,
            Mode::Login,
            Some(
                "This site requires two-factor authentication. Ask an administrator to set it \
                 up for your account."
                    .into(),
            ),
            None,
        )?;
        return Ok((jar, page).into_response());
    }
    let session = finish(state, &user.username).await?;
    Ok((jar.add(session), go(format!("{}/", state.base))).into_response())
}

/// Logs the account in: records it and returns the session cookie.
async fn finish(state: &AdminState, username: &str) -> Result<Cookie<'static>, AdminError> {
    state.users.logged_in(username).await?;
    let token = state.auth.issue(username)?;
    Ok(session_cookie(state, token))
}

/// A page that moves on to `next` by itself.
///
/// The provider's redirect makes the request to the callback a cross-site one, and a
/// redirect from there would still count as cross-site, so the `SameSite=Strict` session
/// cookie would not be sent with it. Moving on from a page of our own makes it same-site.
pub(crate) fn continue_page(next: &str) -> Response {
    let next = html_escape(next);
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta http-equiv=\"refresh\" content=\"0;url={next}\"><title>Logging in…</title>\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head>\
             <body style=\"font-family:system-ui,sans-serif;display:grid;place-items:center;\
             min-height:90vh\"><p>Logging in… <a href=\"{next}\">Continue</a></p></body></html>"
        )),
    )
        .into_response()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn pending(state: &AdminState, jar: &CookieJar) -> Option<Pending> {
    jar.get(PENDING_COOKIE)
        .and_then(|cookie| state.auth.open::<Pending>(PENDING_PURPOSE, cookie.value()))
}

async fn code_page(State(state): Shared, jar: CookieJar) -> Result<Response, AdminError> {
    if pending(&state, &jar).is_none() {
        return Ok(Redirect::to(&format!("{}/login", state.base)).into_response());
    }
    login_page_as(&state, StatusCode::OK, Mode::Code, None, None)
}

#[derive(Deserialize)]
struct CodeForm {
    #[serde(default)]
    totp: String,
}

fn too_many(state: &AdminState, mode: Mode, retry_after_secs: u64) -> Result<Response, AdminError> {
    let minutes = retry_after_secs.div_ceil(60);
    let page = login_page_as(
        state,
        StatusCode::TOO_MANY_REQUESTS,
        mode,
        Some(format!(
            "Too many attempts. Try again in {minutes} minute{}.",
            if minutes == 1 { "" } else { "s" }
        )),
        None,
    )?;
    let retry = [(header::RETRY_AFTER, retry_after_secs.to_string())];
    Ok((retry, page).into_response())
}

async fn code_submit(
    State(state): Shared,
    jar: CookieJar,
    parts: Parts,
    Form(form): Form<CodeForm>,
) -> Result<Response, AdminError> {
    let Some(pending) = pending(&state, &jar) else {
        return login_page_as(
            &state,
            StatusCode::UNAUTHORIZED,
            Mode::Login,
            Some("That took too long. Log in again.".into()),
            None,
        );
    };
    let address = state.auth.client_ip(&parts);
    if let Err(retry_after_secs) = state.limiter.attempt(&pending.sub, address) {
        return too_many(&state, Mode::Code, retry_after_secs);
    }
    let Some(user) = state.users.find(&pending.sub).await? else {
        return login_page_as(
            &state,
            StatusCode::FORBIDDEN,
            Mode::Login,
            Some("This account cannot log in. Ask an administrator.".into()),
            None,
        );
    };
    let accepted = match &user.totp_secret {
        Some(secret) => match totp_step_now(secret, &form.totp) {
            Some(step) => state.users.accept_totp_step(&user.username, step).await?,
            None => false,
        },
        // Turned off meanwhile.
        None => !state.auth.totp_required,
    };
    if !accepted {
        tracing::warn!(username = %user.username, "wrong authenticator code at the second step");
        return login_page_as(
            &state,
            StatusCode::UNAUTHORIZED,
            Mode::Code,
            Some("That code is not right. Codes change every 30 seconds.".into()),
            None,
        );
    }
    state.limiter.succeeded(&user.username, address);
    let session = finish(&state, &user.username).await?;
    let jar = jar
        .remove(Cookie::build((PENDING_COOKIE, "")).path(state.base.clone()))
        .add(session);
    Ok((jar, Redirect::to(&format!("{}/", state.base))).into_response())
}

// ---- e-mail ------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct AddressForm {
    #[serde(default)]
    email: String,
}

/// The address as typed, normalised; `None` if it cannot be one.
fn address(typed: &str) -> Option<String> {
    let address = typed.trim().to_lowercase();
    let (local, domain) = address.split_once('@')?;
    (!local.is_empty() && domain.contains('.') && !address.contains(char::is_whitespace))
        .then_some(address)
}

/// A message with one button: plain text, and HTML with the same words.
fn link_email(
    state: &AdminState,
    to: String,
    subject: String,
    lines: &[&str],
    button: &str,
    link: &str,
    after: &str,
) -> Email {
    let site = &state.settings.site_title;
    let text = format!(
        "Hello,\n\n{}\n\n{link}\n\n{after}\n\n— {site}\n",
        lines.join("\n")
    );
    let escape = |s: &str| html_escape(s);
    let html = format!(
        "<!doctype html><html><body style=\"font-family:system-ui,-apple-system,Segoe UI,sans-serif;\
         color:#0f172a;line-height:1.5;padding:24px\"><p>Hello,</p><p>{}</p>\
         <p><a href=\"{href}\" style=\"display:inline-block;background:{accent};color:#fff;\
         padding:12px 20px;border-radius:8px;text-decoration:none;font-weight:600\">{button}</a></p>\
         <p style=\"color:#475569;font-size:14px\">Or copy this address into your browser:<br>{href}</p>\
         <p style=\"color:#475569;font-size:14px\">{after}</p><p>— {site}</p></body></html>",
        lines
            .iter()
            .map(|line| escape(line))
            .collect::<Vec<_>>()
            .join("<br>"),
        href = escape(link),
        accent = escape(&state.settings.accent_color),
        button = escape(button),
        after = escape(after),
        site = escape(site),
    );
    Email {
        to,
        subject,
        text,
        html: Some(html),
    }
}

/// What a login link stands for.
#[derive(Serialize, Deserialize)]
struct LinkClaims {
    /// The account, or `None` for someone [`crate::settings::Signup`] lets create one.
    sub: Option<String>,
    email: String,
    /// The account's last login when the link was made.
    seen: String,
    exp: u64,
}

fn last_login_print(account: &users::user::Model) -> String {
    fingerprint(&format!("{:?}", account.last_login))
}

async fn link_request(
    State(state): Shared,
    parts: Parts,
    Form(form): Form<AddressForm>,
) -> Result<Response, AdminError> {
    let Some(email) = address(&form.email) else {
        return login_page_as(
            &state,
            StatusCode::UNPROCESSABLE_ENTITY,
            Mode::Login,
            Some("Enter an e-mail address like name@example.com.".into()),
            None,
        );
    };
    if let Err(retry_after_secs) = state.limiter.attempt(&email, state.auth.client_ip(&parts)) {
        return too_many(&state, Mode::Login, retry_after_secs);
    }
    let db = state
        .accounts
        .as_ref()
        .ok_or_else(|| AdminError::Config("login links need the built-in accounts".into()))?;
    let account = users::find_by_email(db, &email).await?;
    let claims = match &account {
        Some(account) => Some(LinkClaims {
            sub: Some(account.username.clone()),
            email: email.clone(),
            seen: last_login_print(account),
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
            let link = state
                .auth
                .methods
                .absolute(&format!("{}/login/link/{token}", state.base));
            let site = &state.settings.site_title;
            let message = link_email(
                &state,
                email,
                format!("Your login link for {site}"),
                &[&format!("Use this link to log in to {site}:")],
                "Log in",
                &link,
                "It works once, within 15 minutes. If you did not ask for it, ignore this \
                 message: nobody can log in without the link.",
            );
            send_later(Arc::clone(&state.mailer), message);
        }
        None => tracing::info!(%email, "login link asked for an address without an account"),
    }
    login_page_as(
        &state,
        StatusCode::OK,
        Mode::Sent,
        None,
        Some(
            "If an account uses that address, a login link is on its way. It works once, \
             within 15 minutes."
                .into(),
        ),
    )
}

fn expired_link(state: &AdminState) -> Result<Response, AdminError> {
    login_page_as(
        state,
        StatusCode::BAD_REQUEST,
        Mode::Login,
        Some("That link has expired or was already used. Ask for a new one.".into()),
        None,
    )
}

/// Opening the link only shows a button: mail scanners that open links to check them must
/// not use up the login.
async fn link_page(
    State(state): Shared,
    Path(token): Path<String>,
) -> Result<Response, AdminError> {
    if state
        .auth
        .open::<LinkClaims>(LINK_PURPOSE, &token)
        .is_none()
    {
        return expired_link(&state);
    }
    login_page_as(&state, StatusCode::OK, Mode::Link, None, None)
}

async fn link_use(
    State(state): Shared,
    jar: CookieJar,
    Path(token): Path<String>,
) -> Result<Response, AdminError> {
    let Some(claims) = state.auth.open::<LinkClaims>(LINK_PURPOSE, &token) else {
        return expired_link(&state);
    };
    let db = state
        .accounts
        .as_ref()
        .ok_or_else(|| AdminError::Config("login links need the built-in accounts".into()))?;
    let username = match &claims.sub {
        Some(username) => {
            let account = users::find_account(db, username).await?;
            // Any login since the link was made, including through this link, retires it.
            match account.filter(|a| last_login_print(a) == claims.seen) {
                Some(account) => account.username,
                None => return expired_link(&state),
            }
        }
        // Someone allowed to sign up; they proved the address is theirs.
        None => match users::find_by_email(db, &claims.email).await? {
            Some(account) => account.username,
            None if state.auth.methods.signup_allows(&claims.email) => {
                users::signup(
                    db,
                    &claims.email,
                    None,
                    None,
                    &state.auth.methods.signup_groups,
                    "login link",
                )
                .await?
                .username
            }
            None => return expired_link(&state),
        },
    };
    complete(&state, jar, &username, false).await
}

/// What a password reset link stands for.
#[derive(Serialize, Deserialize)]
struct ResetClaims {
    sub: String,
    /// The password hash when the link was made: changing the password retires the link.
    pw: String,
    exp: u64,
}

async fn reset_request_page(State(state): Shared) -> Result<Response, AdminError> {
    login_page_as(&state, StatusCode::OK, Mode::Forgot, None, None)
}

async fn reset_request(
    State(state): Shared,
    parts: Parts,
    Form(form): Form<AddressForm>,
) -> Result<Response, AdminError> {
    let Some(email) = address(&form.email) else {
        return login_page_as(
            &state,
            StatusCode::UNPROCESSABLE_ENTITY,
            Mode::Forgot,
            Some("Enter an e-mail address like name@example.com.".into()),
            None,
        );
    };
    if let Err(retry_after_secs) = state.limiter.attempt(&email, state.auth.client_ip(&parts)) {
        return too_many(&state, Mode::Forgot, retry_after_secs);
    }
    let db = state
        .accounts
        .as_ref()
        .ok_or_else(|| AdminError::Config("password resets need the built-in accounts".into()))?;
    let account = users::find_by_email(db, &email)
        .await?
        .filter(|a| a.is_active && a.is_staff);
    if let Some(account) = account {
        let claims = ResetClaims {
            sub: account.username.clone(),
            pw: fingerprint(&account.password_hash),
            exp: unix_now() + RESET_SECS,
        };
        let token = state.auth.seal(RESET_PURPOSE, &claims)?;
        let link = state
            .auth
            .methods
            .absolute(&format!("{}/password/reset/{token}", state.base));
        let site = &state.settings.site_title;
        let message = link_email(
            &state,
            email,
            format!("Choose a new password for {site}"),
            &[&format!(
                "Someone asked to reset the password of your account ({}) at {site}. \
                 Choose a new one here:",
                account.username
            )],
            "Choose a new password",
            &link,
            "The link works for an hour, until the password is changed. If you did not ask \
             for it, ignore this message: your password stays as it is.",
        );
        send_later(Arc::clone(&state.mailer), message);
    } else {
        tracing::info!(%email, "password reset asked for an address without an account");
    }
    login_page_as(
        &state,
        StatusCode::OK,
        Mode::Sent,
        None,
        Some(
            "If an account uses that address, a link to choose a new password is on its way. \
             It works for an hour."
                .into(),
        ),
    )
}

/// The account a reset link is for, if the link is still good.
async fn reset_account(
    state: &AdminState,
    token: &str,
) -> Result<Option<users::user::Model>, AdminError> {
    let Some(claims) = state.auth.open::<ResetClaims>(RESET_PURPOSE, token) else {
        return Ok(None);
    };
    let Some(db) = state.accounts.as_ref() else {
        return Ok(None);
    };
    Ok(users::find_account(db, &claims.sub)
        .await?
        .filter(|a| a.is_active && a.is_staff && fingerprint(&a.password_hash) == claims.pw))
}

fn expired_reset(state: &AdminState) -> Result<Response, AdminError> {
    login_page_as(
        state,
        StatusCode::BAD_REQUEST,
        Mode::Forgot,
        Some("That link has expired or was already used. Ask for a new one.".into()),
        None,
    )
}

async fn reset_page(
    State(state): Shared,
    Path(token): Path<String>,
) -> Result<Response, AdminError> {
    if reset_account(&state, &token).await?.is_none() {
        return expired_reset(&state);
    }
    login_page_as(&state, StatusCode::OK, Mode::Reset, None, None)
}

#[derive(Deserialize)]
struct NewPassword {
    #[serde(default)]
    password: String,
    #[serde(default)]
    password2: String,
}

async fn reset_submit(
    State(state): Shared,
    Path(token): Path<String>,
    Form(form): Form<NewPassword>,
) -> Result<Response, AdminError> {
    let Some(account) = reset_account(&state, &token).await? else {
        return expired_reset(&state);
    };
    let again = |message: String| {
        login_page_as(
            &state,
            StatusCode::UNPROCESSABLE_ENTITY,
            Mode::Reset,
            Some(message),
            None,
        )
    };
    if form.password != form.password2 {
        return again("The two passwords are different.".into());
    }
    let db = state
        .accounts
        .as_ref()
        .ok_or_else(|| AdminError::Config("password resets need the built-in accounts".into()))?;
    match users::change_password(db, &account.username, &form.password).await {
        Ok(()) => {}
        Err(AdminError::BadInput { message, .. }) => return again(capitalised(&message)),
        Err(other) => return Err(other),
    }
    tracing::info!(username = %account.username, "password reset by e-mail");
    login_page_as(
        &state,
        StatusCode::OK,
        Mode::Login,
        None,
        Some("Your password is changed. Log in with it now.".into()),
    )
}

// ---- invitations -------------------------------------------------------------------------------

/// What an invitation stands for. It retires once the account sets a password or logs in.
#[derive(Serialize, Deserialize)]
struct InviteClaims {
    sub: String,
    pw: String,
    seen: String,
    exp: u64,
}

/// E-mails `account` an invitation from `inviter`: a link to choose a password, or to log in
/// when passwords are off.
pub(crate) fn invite(
    state: &AdminState,
    account: &users::user::Model,
    inviter: &str,
) -> Result<(), AdminError> {
    let email = account.email.clone().ok_or_else(|| AdminError::BadInput {
        field: "email".into(),
        message: "an invitation needs the account's e-mail address".into(),
    })?;
    let claims = InviteClaims {
        sub: account.username.clone(),
        pw: fingerprint(&account.password_hash),
        seen: last_login_print(account),
        exp: unix_now() + INVITE_SECS,
    };
    let token = state.auth.seal(INVITE_PURPOSE, &claims)?;
    let link = state
        .auth
        .methods
        .absolute(&format!("{}/invite/{token}", state.base));
    let methods = &state.auth.methods;
    let site = &state.settings.site_title;
    let (first, button) = if methods.password {
        (
            "Choose your password here to get started:",
            "Choose a password",
        )
    } else {
        ("Log in here to get started:", "Log in")
    };
    let mut after = String::from("The link works for 7 days, once.");
    let others: Vec<&str> = methods.providers.iter().map(|p| p.label.as_str()).collect();
    if !others.is_empty() {
        after.push_str(&format!(
            " You can also log in with {} using this address.",
            others.join(" or ")
        ));
    }
    if methods.magic_link {
        after.push_str(" The login page can e-mail you a login link at any time, too.");
    }
    let message = link_email(
        state,
        email,
        format!("You are invited to {site}"),
        &[
            &format!(
                "{inviter} invited you to {site}, with the username {}.",
                account.username
            ),
            first,
        ],
        button,
        &link,
        &after,
    );
    send_later(Arc::clone(&state.mailer), message);
    tracing::info!(username = %account.username, %inviter, "invitation sent");
    Ok(())
}

/// The account an invitation is for, if it is still good.
async fn invited_account(
    state: &AdminState,
    token: &str,
) -> Result<Option<users::user::Model>, AdminError> {
    let Some(claims) = state.auth.open::<InviteClaims>(INVITE_PURPOSE, token) else {
        return Ok(None);
    };
    let Some(db) = state.accounts.as_ref() else {
        return Ok(None);
    };
    Ok(users::find_account(db, &claims.sub).await?.filter(|a| {
        fingerprint(&a.password_hash) == claims.pw && last_login_print(a) == claims.seen
    }))
}

fn expired_invite(state: &AdminState) -> Result<Response, AdminError> {
    login_page_as(
        state,
        StatusCode::BAD_REQUEST,
        Mode::Login,
        Some(
            "That invitation has expired or was already used. Log in, or ask for a new one.".into(),
        ),
        None,
    )
}

async fn invite_page(
    State(state): Shared,
    Path(token): Path<String>,
) -> Result<Response, AdminError> {
    if invited_account(&state, &token).await?.is_none() {
        return expired_invite(&state);
    }
    login_page_as(&state, StatusCode::OK, Mode::Invite, None, None)
}

async fn invite_accept(
    State(state): Shared,
    jar: CookieJar,
    Path(token): Path<String>,
    Form(form): Form<NewPassword>,
) -> Result<Response, AdminError> {
    let Some(account) = invited_account(&state, &token).await? else {
        return expired_invite(&state);
    };
    if state.auth.methods.password {
        let again = |message: String| {
            login_page_as(
                &state,
                StatusCode::UNPROCESSABLE_ENTITY,
                Mode::Invite,
                Some(message),
                None,
            )
        };
        if form.password != form.password2 {
            return again("The two passwords are different.".into());
        }
        let db = state
            .accounts
            .as_ref()
            .ok_or_else(|| AdminError::Config("invitations need the built-in accounts".into()))?;
        match users::change_password(db, &account.username, &form.password).await {
            Ok(()) => {}
            Err(AdminError::BadInput { message, .. }) => return again(capitalised(&message)),
            Err(other) => return Err(other),
        }
    }
    tracing::info!(username = %account.username, "invitation accepted");
    complete(&state, jar, &account.username, false).await
}

/// `"the password is too short"` -> `"The password is too short."`
pub(crate) fn capitalised(message: &str) -> String {
    let mut chars = message.chars();
    let mut out: String = chars
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(chars.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_normalised_or_refused() {
        assert_eq!(
            address(" Ada@Example.COM ").as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(address("ada"), None);
        assert_eq!(address("@example.com"), None);
        assert_eq!(address("ada@localhost"), None);
        assert_eq!(address("a da@example.com"), None);
    }

    #[test]
    fn continue_page_escapes_the_target() {
        assert_eq!(html_escape("/a?b=1&c=\"x\""), "/a?b=1&amp;c=&quot;x&quot;");
    }

    #[test]
    fn messages_are_capitalised() {
        assert_eq!(capitalised("too short"), "Too short.");
        assert_eq!(capitalised("Done."), "Done.");
    }
}

//! The Users and Groups pages of the admin, where superusers manage the built-in accounts
//! and decide what each one may do.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use minijinja::value::Serde;
use minijinja::{Value, context};
use sea_orm::{DatabaseConnection, EntityTrait, QueryOrder};
use serde::Serialize;

use crate::AdminState;
use crate::access::{Access, Action};
use crate::auth::{self, Session};
use crate::error::AdminError;
use crate::handlers::{notice_text, render};
use crate::uploads::FILES;
use crate::users::{self, AccountInput, group, user};

type Shared = State<Arc<AdminState>>;

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = format!("{}/auth", state.base);
    Router::new()
        .route(&format!("{b}/users"), get(user_list))
        .route(&format!("{b}/users/new"), get(user_new).post(user_create))
        .route(
            &format!("{b}/users/{{id}}"),
            get(user_edit).post(user_update),
        )
        .route(&format!("{b}/users/{{id}}/delete"), post(user_delete))
        .route(
            &format!("{b}/users/{{id}}/identities/{{identity}}/delete"),
            post(identity_delete),
        )
        .route(&format!("{b}/groups"), get(group_list))
        .route(
            &format!("{b}/groups/new"),
            get(group_new).post(group_create),
        )
        .route(
            &format!("{b}/groups/{{id}}"),
            get(group_edit).post(group_update),
        )
        .route(&format!("{b}/groups/{{id}}/delete"), post(group_delete))
        .route(
            &format!("{b}/two-factor"),
            get(two_factor_page).post(two_factor_enable),
        )
        .route(&format!("{b}/two-factor/disable"), post(two_factor_disable))
        .route(
            &format!("{b}/password"),
            get(password_page).post(password_change),
        )
        .with_state(state)
}

/// The database the accounts are in. These routes are only mounted when there is one.
fn database(state: &AdminState) -> Result<&DatabaseConnection, AdminError> {
    state
        .accounts
        .as_ref()
        .ok_or_else(|| AdminError::NotFound("account management is not enabled".into()))
}

fn parse_id(kind: &str, id: &str) -> Result<i32, AdminError> {
    id.parse()
        .map_err(|_| AdminError::NotFound(format!("{kind} `{id}`")))
}

/// A submitted form. Unlike the model forms, these repeat field names (one per ticked box).
struct Submitted(Vec<(String, String)>);

impl Submitted {
    fn parse(body: &Bytes) -> Self {
        Self(url::form_urlencoded::parse(body).into_owned().collect())
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn checked(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The ticked permission boxes, dropping anything that is not a known model and action.
    fn grants(&self, state: &AdminState) -> Vec<(String, Action)> {
        let known: Vec<&str> = subjects(state).into_iter().map(|(slug, _)| slug).collect();
        self.all("perm")
            .filter_map(|value| value.rsplit_once(':'))
            .filter(|(model, _)| known.contains(model))
            .filter_map(|(model, action)| Some((model.to_owned(), Action::parse(action)?)))
            .collect()
    }
}

/// Everything a permission can be about: each model, and file uploads if they are enabled.
fn subjects(state: &AdminState) -> Vec<(&str, &str)> {
    let mut all: Vec<(&str, &str)> = state
        .resources
        .iter()
        .map(|r| (r.slug(), r.label()))
        .collect();
    if state.uploads.is_some() {
        all.push((FILES, "Files"));
    }
    all
}

#[derive(Serialize)]
struct PermissionCell {
    /// Form value: `model:action`.
    value: String,
    checked: bool,
}

#[derive(Serialize)]
struct PermissionRow<'a> {
    label: &'a str,
    cells: Vec<PermissionCell>,
}

/// The grid of tick boxes, one row per model and one column per action.
fn permission_grid<'a>(state: &'a AdminState, granted: &Access) -> Vec<PermissionRow<'a>> {
    subjects(state)
        .into_iter()
        .map(|(slug, label)| PermissionRow {
            label,
            cells: Action::ALL
                .into_iter()
                .map(|action| PermissionCell {
                    value: format!("{slug}:{}", action.as_str()),
                    checked: granted.granted(slug, action),
                })
                .collect(),
        })
        .collect()
}

fn as_access(grants: &[(String, Action)]) -> Access {
    grants
        .iter()
        .fold(Access::none(), |access, (model, action)| {
            access.grant(model.clone(), *action)
        })
}

fn action_names() -> Vec<&'static str> {
    Action::ALL.into_iter().map(Action::as_str).collect()
}

fn yes_no(flag: bool) -> String {
    if flag { "Yes" } else { "No" }.to_owned()
}

#[derive(Serialize)]
struct ListRow {
    id: i32,
    cells: Vec<String>,
}

/// `?notice=…` after a change; only known codes produce text (see [`notice_text`]).
#[derive(serde::Deserialize)]
struct NoticeQuery {
    notice: Option<String>,
}

fn render_list(
    state: &AdminState,
    session: &Session,
    kind: &str,
    title: &str,
    columns: &[&str],
    rows: &[ListRow],
    notice: Option<&str>,
) -> Result<Html<String>, AdminError> {
    render(
        state,
        "accounts_list.html",
        Some(session),
        context! {
            section => kind,
            kind,
            title,
            columns => Value::from(Serde(columns)),
            rows => Value::from(Serde(rows)),
            notice => notice_text(notice),
        },
    )
}

/// Re-renders a form for a mistake the user can fix; passes everything else up.
fn fixable(error: AdminError) -> Result<String, AdminError> {
    match error {
        AdminError::BadInput { field, message } => Ok(format!("{field}: {message}")),
        other => Err(other),
    }
}

// ---- users -------------------------------------------------------------------------------

async fn user_list(
    State(state): Shared,
    session: Session,
    Query(query): Query<NoticeQuery>,
) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    let accounts = user::Entity::find()
        .order_by_asc(user::Column::Username)
        .all(database(&state)?)
        .await?;
    let rows: Vec<ListRow> = accounts
        .into_iter()
        .map(|a| ListRow {
            id: a.id,
            cells: vec![
                a.username.clone(),
                // A blank name is shown as nothing rather than repeating the username.
                Some(a.display_name())
                    .filter(|n| *n != a.username)
                    .unwrap_or_default(),
                a.email.clone().unwrap_or_default(),
                yes_no(a.is_active),
                yes_no(a.is_staff),
                yes_no(a.is_superuser),
                yes_no(a.totp_secret.is_some()),
            ],
        })
        .collect();
    render_list(
        &state,
        &session,
        "users",
        "Users",
        &[
            "Username",
            "Name",
            "Email",
            "Active",
            "Staff",
            "Superuser",
            "Two-factor",
        ],
        &rows,
        query.notice.as_deref(),
    )
}

#[derive(Serialize)]
struct GroupChoice {
    id: i32,
    name: String,
    checked: bool,
}

/// What the user form shows: either a stored account or a rejected submission, as typed.
struct UserView {
    username: String,
    email: String,
    first_name: String,
    last_name: String,
    phone: String,
    /// Shown, never edited.
    last_login: Option<String>,
    date_joined: Option<String>,
    is_active: bool,
    is_staff: bool,
    is_superuser: bool,
    has_totp: bool,
    groups: Vec<i32>,
    grants: Access,
}

impl UserView {
    fn blank() -> Self {
        Self {
            username: String::new(),
            email: String::new(),
            first_name: String::new(),
            last_name: String::new(),
            phone: String::new(),
            last_login: None,
            date_joined: None,
            is_active: true,
            is_staff: true,
            is_superuser: false,
            has_totp: false,
            groups: Vec::new(),
            grants: Access::none(),
        }
    }

    fn from_input(input: &AccountInput, has_totp: bool) -> Self {
        Self {
            username: input.username.clone(),
            email: input.email.clone().unwrap_or_default(),
            first_name: input.first_name.clone().unwrap_or_default(),
            last_name: input.last_name.clone().unwrap_or_default(),
            phone: input.phone.clone().unwrap_or_default(),
            last_login: None,
            date_joined: None,
            is_active: input.is_active,
            is_staff: input.is_staff,
            is_superuser: input.is_superuser,
            has_totp,
            groups: input.groups.clone(),
            grants: as_access(&input.grants),
        }
    }
}

async fn render_user(
    state: &AdminState,
    session: &Session,
    id: Option<i32>,
    view: &UserView,
    error: Option<String>,
) -> Result<Html<String>, AdminError> {
    let groups: Vec<GroupChoice> = users::all_groups(database(state)?)
        .await?
        .into_iter()
        .map(|g| GroupChoice {
            checked: view.groups.contains(&g.id),
            id: g.id,
            name: g.name,
        })
        .collect();
    let identities: Vec<LinkedView> = match id {
        Some(id) => users::user_identities(database(state)?, id)
            .await?
            .into_iter()
            .map(|link| LinkedView {
                id: link.id,
                label: state
                    .auth
                    .methods
                    .provider(&link.provider)
                    .map_or_else(|| link.provider.clone(), |p| p.label.clone()),
                email: link.email,
                linked: when(Some(link.date_linked)).unwrap_or_default(),
                last_used: when(link.last_used),
            })
            .collect(),
        None => Vec::new(),
    };
    render(
        state,
        "user_form.html",
        Some(session),
        context! {
            section => "users",
            id,
            username => &view.username,
            email => &view.email,
            first_name => &view.first_name,
            last_name => &view.last_name,
            phone => &view.phone,
            last_login => &view.last_login,
            date_joined => &view.date_joined,
            is_active => view.is_active,
            is_staff => view.is_staff,
            is_superuser => view.is_superuser,
            has_totp => view.has_totp,
            groups => Value::from(Serde(&groups)),
            actions => action_names(),
            permissions => Value::from(Serde(permission_grid(state, &view.grants))),
            identities => Value::from(Serde(&identities)),
            error,
        },
    )
}

/// A link to another site, as the user form lists it.
#[derive(Serialize)]
struct LinkedView {
    id: i32,
    label: String,
    email: Option<String>,
    linked: String,
    last_used: Option<String>,
}

async fn identity_delete(
    State(state): Shared,
    session: Session,
    Path((id, identity)): Path<(String, String)>,
    body: Bytes,
) -> Result<Redirect, AdminError> {
    session.check_csrf(Submitted::parse(&body).get("_csrf"))?;
    session.require_superuser()?;
    let id = parse_id("user", &id)?;
    let identity = parse_id("link", &identity)?;
    users::unlink_identity(database(&state)?, id, identity).await?;
    Ok(Redirect::to(&format!(
        "{}/auth/users/{id}?notice=user_saved",
        state.base
    )))
}

fn user_input(state: &AdminState, form: &Submitted) -> AccountInput {
    AccountInput {
        username: form.get("username").unwrap_or_default().trim().to_owned(),
        // An empty password box means "leave it as it is".
        password: form
            .get("password")
            .filter(|p| !p.is_empty())
            .map(str::to_owned),
        is_active: form.checked("is_active"),
        is_staff: form.checked("is_staff"),
        is_superuser: form.checked("is_superuser"),
        groups: form.all("group").filter_map(|id| id.parse().ok()).collect(),
        grants: form.grants(state),
        reset_totp: form.checked("reset_totp"),
        email: form.get("email").map(str::to_owned),
        first_name: form.get("first_name").map(str::to_owned),
        last_name: form.get("last_name").map(str::to_owned),
        phone: form.get("phone").map(str::to_owned),
    }
}

/// A timestamp as the admin shows it.
fn when(at: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    at.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

async fn user_new(State(state): Shared, session: Session) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    render_user(&state, &session, None, &UserView::blank(), None).await
}

async fn user_create(
    State(state): Shared,
    session: Session,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    session.require_superuser()?;
    save_user(&state, &session, None, user_input(&state, &form)).await
}

async fn user_edit(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    let id = parse_id("user", &id)?;
    let db = database(&state)?;
    let account = user::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("user `{id}`")))?;
    let view = UserView {
        email: account.email.clone().unwrap_or_default(),
        first_name: account.first_name.clone().unwrap_or_default(),
        last_name: account.last_name.clone().unwrap_or_default(),
        phone: account.phone.clone().unwrap_or_default(),
        last_login: when(account.last_login),
        date_joined: when(account.date_joined),
        username: account.username,
        is_active: account.is_active,
        is_staff: account.is_staff,
        is_superuser: account.is_superuser,
        has_totp: account.totp_secret.is_some(),
        groups: users::user_groups(db, id).await?,
        grants: users::user_grants(db, id).await?,
    };
    render_user(&state, &session, Some(id), &view, None).await
}

async fn user_update(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    session.require_superuser()?;
    let id = parse_id("user", &id)?;
    save_user(&state, &session, Some(id), user_input(&state, &form)).await
}

/// Whether `id` is the account that is making the request.
async fn is_self(state: &AdminState, session: &Session, id: i32) -> Result<bool, AdminError> {
    let account = user::Entity::find_by_id(id).one(database(state)?).await?;
    Ok(account.is_some_and(|a| a.username == session.username))
}

/// Whether the stored account `id` has two-factor authentication on.
async fn has_totp(state: &AdminState, id: Option<i32>) -> Result<bool, AdminError> {
    let Some(id) = id else { return Ok(false) };
    let account = user::Entity::find_by_id(id).one(database(state)?).await?;
    Ok(account.is_some_and(|a| a.totp_secret.is_some()))
}

async fn save_user(
    state: &AdminState,
    session: &Session,
    id: Option<i32>,
    input: AccountInput,
) -> Result<Response, AdminError> {
    let view = UserView::from_input(&input, has_totp(state, id).await?);
    // Nobody can lock themselves out, so there is always a superuser left to undo a mistake.
    let keeps_access = input.is_active && input.is_staff && input.is_superuser;
    let outcome = match id {
        Some(id) if !keeps_access && is_self(state, session, id).await? => {
            Err(AdminError::BadInput {
                field: "account".into(),
                message: "you cannot remove your own access; ask another superuser".into(),
            })
        }
        _ => users::save_user(database(state)?, id, input).await,
    };
    match outcome {
        Ok(_) => {
            let notice = if id.is_some() {
                "user_saved"
            } else {
                "user_created"
            };
            Ok(Redirect::to(&format!("{}/auth/users?notice={notice}", state.base)).into_response())
        }
        Err(error) => {
            let message = fixable(error)?;
            let page = render_user(state, session, id, &view, Some(message)).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn user_delete(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Redirect, AdminError> {
    session.check_csrf(Submitted::parse(&body).get("_csrf"))?;
    session.require_superuser()?;
    let id = parse_id("user", &id)?;
    if is_self(&state, &session, id).await? {
        return Err(AdminError::Forbidden(
            "you cannot delete your own account".into(),
        ));
    }
    users::delete_user(database(&state)?, id).await?;
    Ok(Redirect::to(&format!(
        "{}/auth/users?notice=user_deleted",
        state.base
    )))
}

// ---- groups ------------------------------------------------------------------------------

async fn group_list(
    State(state): Shared,
    session: Session,
    Query(query): Query<NoticeQuery>,
) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    let rows: Vec<ListRow> = users::all_groups(database(&state)?)
        .await?
        .into_iter()
        .map(|g| ListRow {
            id: g.id,
            cells: vec![g.name],
        })
        .collect();
    render_list(
        &state,
        &session,
        "groups",
        "Groups",
        &["Name"],
        &rows,
        query.notice.as_deref(),
    )
}

fn render_group(
    state: &AdminState,
    session: &Session,
    id: Option<i32>,
    name: &str,
    grants: &Access,
    error: Option<String>,
) -> Result<Html<String>, AdminError> {
    render(
        state,
        "group_form.html",
        Some(session),
        context! {
            section => "groups",
            id,
            name,
            actions => action_names(),
            permissions => Value::from(Serde(permission_grid(state, grants))),
            error,
        },
    )
}

async fn group_new(State(state): Shared, session: Session) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    render_group(&state, &session, None, "", &Access::none(), None)
}

async fn group_create(
    State(state): Shared,
    session: Session,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    session.require_superuser()?;
    save_group(&state, &session, None, &form).await
}

async fn group_edit(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
) -> Result<Html<String>, AdminError> {
    session.require_superuser()?;
    let id = parse_id("group", &id)?;
    let db = database(&state)?;
    let row = group::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("group `{id}`")))?;
    let grants = users::group_grants(db, id).await?;
    render_group(&state, &session, Some(id), &row.name, &grants, None)
}

async fn group_update(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    session.require_superuser()?;
    let id = parse_id("group", &id)?;
    save_group(&state, &session, Some(id), &form).await
}

async fn save_group(
    state: &AdminState,
    session: &Session,
    id: Option<i32>,
    form: &Submitted,
) -> Result<Response, AdminError> {
    let name = form.get("name").unwrap_or_default();
    let grants = form.grants(state);
    let typed = as_access(&grants);
    match users::save_group(database(state)?, id, name, grants).await {
        Ok(_) => {
            let notice = if id.is_some() {
                "group_saved"
            } else {
                "group_created"
            };
            Ok(
                Redirect::to(&format!("{}/auth/groups?notice={notice}", state.base))
                    .into_response(),
            )
        }
        Err(error) => {
            let message = fixable(error)?;
            let page = render_group(state, session, id, name, &typed, Some(message))?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
    }
}

async fn group_delete(
    State(state): Shared,
    session: Session,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Redirect, AdminError> {
    session.check_csrf(Submitted::parse(&body).get("_csrf"))?;
    session.require_superuser()?;
    users::delete_group(database(&state)?, parse_id("group", &id)?).await?;
    Ok(Redirect::to(&format!(
        "{}/auth/groups?notice=group_deleted",
        state.base
    )))
}

// ---- two-factor authentication -----------------------------------------------------------

/// The logged-in account's own row.
async fn own_account(state: &AdminState, session: &Session) -> Result<user::Model, AdminError> {
    users::find_account(database(state)?, &session.username)
        .await?
        .ok_or(AdminError::Unauthorized)
}

/// The setup page: a QR code for a secret that is only stored once a code proves it works.
fn render_two_factor(
    state: &AdminState,
    session: &Session,
    enabled: bool,
    secret: Option<&str>,
    error: Option<&str>,
    notice: Option<&str>,
) -> Result<Html<String>, AdminError> {
    let fresh;
    let secret = match (enabled, secret) {
        (true, _) => None,
        (false, Some(submitted)) => Some(submitted),
        (false, None) => {
            fresh = auth::new_totp_secret();
            Some(fresh.as_str())
        }
    };
    let uri = secret.map(|s| auth::totp_uri(&state.settings.site_title, &session.username, s));
    let qr = uri.as_deref().and_then(|uri| {
        let code = qrcode::QrCode::new(uri).ok()?;
        Some(
            code.render::<qrcode::render::svg::Color<'_>>()
                .min_dimensions(220, 220)
                .build(),
        )
    });
    render(
        state,
        "two_factor.html",
        Some(session),
        context! {
            section => "two_factor",
            notice => notice_text(notice),
            enabled,
            required => state.auth.totp_required,
            secret,
            uri,
            // Generated here from our own data, so it is safe to insert as markup.
            qr => qr.map(Value::from_safe_string),
            error,
        },
    )
}

async fn two_factor_page(
    State(state): Shared,
    session: Session,
    Query(query): Query<NoticeQuery>,
) -> Result<Html<String>, AdminError> {
    let account = own_account(&state, &session).await?;
    let enabled = account.totp_secret.is_some();
    render_two_factor(
        &state,
        &session,
        enabled,
        None,
        None,
        query.notice.as_deref(),
    )
}

async fn two_factor_enable(
    State(state): Shared,
    session: Session,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    let account = own_account(&state, &session).await?;
    if account.totp_secret.is_some() {
        return Err(AdminError::Conflict(
            "two-factor authentication is already on".into(),
        ));
    }
    let secret = form.get("secret").unwrap_or_default();
    let again = |message: &str| -> Result<Response, AdminError> {
        // Keep the same secret, so the code already scanned stays valid.
        let kept = auth::totp_secret_is_strong(secret).then_some(secret);
        let page = render_two_factor(&state, &session, false, kept, Some(message), None)?;
        Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
    };
    if !auth::totp_secret_is_strong(secret) {
        return again("That setup page was incomplete; scan the new code below.");
    }
    let password = form.get("password").unwrap_or_default();
    if !auth::confirm_password(&state, &session.username, password).await? {
        return again("The password is not correct.");
    }
    let code = form.get("code").unwrap_or_default();
    let Some(step) = auth::totp_step_now(secret, code) else {
        return again("That code is not correct. Check the time on your device and try again.");
    };
    users::store_totp(
        database(&state)?,
        &session.username,
        Some(secret.to_owned()),
        Some(step),
    )
    .await?;
    tracing::info!(username = %session.username, "two-factor authentication enabled");
    Ok(Redirect::to(&format!(
        "{}/auth/two-factor?notice=two_factor_on",
        state.base
    ))
    .into_response())
}

async fn two_factor_disable(
    State(state): Shared,
    session: Session,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    if state.auth.totp_required {
        return Err(AdminError::Forbidden(
            "this site requires two-factor authentication".into(),
        ));
    }
    let password = form.get("password").unwrap_or_default();
    if !auth::confirm_password(&state, &session.username, password).await? {
        let page = render_two_factor(
            &state,
            &session,
            true,
            None,
            Some("The password is not correct."),
            None,
        )?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }
    users::store_totp(database(&state)?, &session.username, None, None).await?;
    tracing::info!(username = %session.username, "two-factor authentication disabled");
    Ok(Redirect::to(&format!(
        "{}/auth/two-factor?notice=two_factor_off",
        state.base
    ))
    .into_response())
}

// ---- own password ------------------------------------------------------------------------

fn render_password(
    state: &AdminState,
    session: &Session,
    status: StatusCode,
    error: Option<String>,
    notice: Option<&str>,
) -> Result<Response, AdminError> {
    if !state.auth.methods.password {
        return Err(AdminError::NotFound("password login is turned off".into()));
    }
    let page = render(
        state,
        "password.html",
        Some(session),
        context! {
            section => "password",
            notice => notice_text(notice),
            can_reset => crate::login::can_reset(state),
            error,
        },
    )?;
    Ok((status, page).into_response())
}

async fn password_page(
    State(state): Shared,
    session: Session,
    Query(query): Query<NoticeQuery>,
) -> Result<Response, AdminError> {
    render_password(
        &state,
        &session,
        StatusCode::OK,
        None,
        query.notice.as_deref(),
    )
}

async fn password_change(
    State(state): Shared,
    session: Session,
    body: Bytes,
) -> Result<Response, AdminError> {
    let form = Submitted::parse(&body);
    session.check_csrf(form.get("_csrf"))?;
    let again = |message: String| {
        render_password(
            &state,
            &session,
            StatusCode::UNPROCESSABLE_ENTITY,
            Some(message),
            None,
        )
    };
    let current = form.get("current").unwrap_or_default();
    if !auth::confirm_password(&state, &session.username, current).await? {
        return again("The current password is not correct.".into());
    }
    let password = form.get("password").unwrap_or_default();
    if form.get("password2").unwrap_or_default() != password {
        return again("The two new passwords are different.".into());
    }
    match users::change_password(database(&state)?, &session.username, password).await {
        Ok(()) => {}
        Err(AdminError::BadInput { message, .. }) => {
            return again(crate::login::capitalised(&message));
        }
        Err(other) => return Err(other),
    }
    tracing::info!(username = %session.username, "password changed by its owner");
    Ok(Redirect::to(&format!(
        "{}/auth/password?notice=password_changed",
        state.base
    ))
    .into_response())
}

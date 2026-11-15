//! HTTP handlers. They only translate between HTTP and the `Resource`/auth seams.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{self, RETRY_AFTER};
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use minijinja::value::Serde;
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use crate::AdminState;
use crate::access::Action;
use crate::auth::{self, COOKIE, Session};
use crate::error::AdminError;
use crate::resource::{
    Field, FieldKind, Filter, FilterOp, FormData, Input, LinkField, ListQuery, Reference, Resource,
    Row, WriteMode,
};

type Shared = State<Arc<AdminState>>;

/// Longest cell text shown on a list page.
const CELL_CHARS: usize = 80;
const CSRF_FIELD: &str = "_csrf";

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.base.clone();
    // Forms may carry files, up to the upload limit.
    let form_limit = state.uploads.as_ref().map_or(2 * 1024 * 1024, |uploads| {
        usize::try_from(uploads.file_limit())
            .unwrap_or(usize::MAX)
            .saturating_add(1024 * 1024)
    });
    let limit = axum::extract::DefaultBodyLimit::max(form_limit);
    Router::new()
        .route(&b, get(index))
        .route(&format!("{b}/"), get(index))
        .route(&format!("{b}/login"), get(login_page).post(login_submit))
        .route(&format!("{b}/logout"), post(logout))
        .route(&format!("{b}/{{model}}"), get(list))
        .route(
            &format!("{b}/{{model}}/new"),
            get(new_form).post(create).layer(limit),
        )
        .route(&format!("{b}/{{model}}/bulk"), post(bulk))
        .route(
            &format!("{b}/{{model}}/{{id}}"),
            get(edit_form).post(update).layer(limit),
        )
        .route(
            &format!("{b}/{{model}}/{{id}}/file/{{field}}"),
            get(file_content),
        )
        .route(&format!("{b}/{{model}}/{{id}}/delete"), post(delete))
        .route(&format!("{b}/{QUILL_PATH}/quill.js"), get(quill_js))
        .route(&format!("{b}/{QUILL_PATH}/quill.snow.css"), get(quill_css))
        .with_state(state)
}

/// Where the rich-text editor's files are served, under the admin. The version is in the
/// path, so browsers may keep them for good.
const QUILL_PATH: &str = "static/quill-2.0.3";

/// One of the admin's own files, cached for a year and compressed like API responses (it
/// holds no secrets).
fn static_file(content_type: &'static str, body: &'static str) -> Response {
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response();
    response
        .extensions_mut()
        .insert(crate::server::Compressible);
    response
}

async fn quill_js() -> Response {
    static_file(
        "text/javascript; charset=utf-8",
        include_str!("../assets/quill/quill.js"),
    )
}

async fn quill_css() -> Response {
    static_file(
        "text/css; charset=utf-8",
        include_str!("../assets/quill/quill.snow.css"),
    )
}

/// A model as the logged-in account sees it: what it is called and what may be done to it.
#[derive(Serialize)]
struct NavModel<'a> {
    slug: &'a str,
    label: &'a str,
    can_view: bool,
    can_add: bool,
    can_change: bool,
    can_delete: bool,
}

impl<'a> NavModel<'a> {
    fn new(resource: &'a dyn Resource, session: &Session) -> Self {
        let slug = resource.slug();
        Self {
            slug,
            label: resource.label(),
            can_view: session.can(slug, Action::View),
            can_add: session.can(slug, Action::Add),
            can_change: session.can(slug, Action::Change),
            can_delete: session.can(slug, Action::Delete),
        }
    }
}

pub(crate) fn render(
    state: &AdminState,
    template: &str,
    session: Option<&Session>,
    page: Value,
) -> Result<Html<String>, AdminError> {
    // Only the models this account may see; nothing for a visitor.
    let models: Vec<NavModel<'_>> = session
        .map(|session| {
            state
                .resources
                .iter()
                .filter(|r| session.can(r.slug(), Action::View))
                .map(|r| NavModel::new(r.as_ref(), session))
                .collect()
        })
        .unwrap_or_default();
    let shared = context! {
        site => Value::from(Serde(&state.settings)),
        base => &state.base,
        models => Value::from(Serde(&models)),
        user => session.map(|s| s.username.as_str()),
        csrf => session.map(Session::csrf),
        is_superuser => session.is_some_and(|s| s.access.is_superuser()),
        // Whether accounts are the built-in ones, which have a page for the user's own settings.
        self_service => state.accounts.is_some() && session.is_some(),
        password_login => state.auth.methods.password,
        api_keys_enabled => state.auth.api_keys,
        // Whether to link to the Users and Groups pages.
        manages_accounts => state.accounts.is_some()
            && session.is_some_and(|s| s.access.is_superuser()),
        // Every `<script>` carries it; see `server::with_csp`.
        csp_nonce => crate::server::csp_nonce(),
    };
    let html = state
        .templates
        .get_template(template)?
        .render(context! { ..page, ..shared })?;
    Ok(Html(html))
}

fn visible_fields(resource: &dyn Resource) -> impl Iterator<Item = &Field> {
    let hidden = &resource.options().hidden_fields;
    resource
        .fields()
        .iter()
        .filter(move |f| !hidden.contains(&f.name))
}

/// A stored boolean, which some databases return as `0`/`1`.
fn boolean_value(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

fn display(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn truncate(text: String) -> String {
    if text.chars().count() <= CELL_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(CELL_CHARS).collect();
    short.push('…');
    short
}

fn pk_name(resource: &dyn Resource) -> Option<&str> {
    let mut keys = resource.fields().iter().filter(|f| f.primary_key);
    match (keys.next(), keys.next()) {
        (Some(only), None) => Some(only.name.as_str()),
        _ => None,
    }
}

// ---- ownership ---------------------------------------------------------------------------

/// The condition that limits `resource` to this account's own rows: for a model with
/// `owned_by`, when the account is not a superuser.
pub(crate) fn owner_filter(resource: &dyn Resource, session: &Session) -> Option<Filter> {
    let field = resource.options().owner_field.as_ref()?;
    (!session.access.is_superuser()).then(|| Filter {
        field: field.clone(),
        op: FilterOp::Equal,
        value: session.username.clone(),
    })
}

/// The row with this id if this account may reach it. Other accounts' rows are "not found",
/// so their ids are not confirmed.
pub(crate) async fn owned_row(
    resource: &dyn Resource,
    session: &Session,
    id: &str,
) -> Result<Row, AdminError> {
    let missing = || AdminError::NotFound(format!("{} `{id}`", resource.slug()));
    let row = resource.get(id).await?.ok_or_else(missing)?;
    if let Some(filter) = owner_filter(resource, session)
        && display(row.get(&filter.field)) != filter.value
    {
        return Err(missing());
    }
    Ok(row)
}

/// Writes the owner of a row being saved: always this account for one that is not a
/// superuser; for a superuser, this account when adding a row without choosing one.
pub(crate) fn stamp_owner(
    resource: &dyn Resource,
    session: &Session,
    input: &mut Input,
    creating: bool,
) {
    let Some(field) = &resource.options().owner_field else {
        return;
    };
    let chosen = input
        .get(field)
        .and_then(|v| v.as_deref())
        .is_some_and(|v| !v.trim().is_empty());
    if !session.access.is_superuser() || (creating && !chosen) {
        input.insert(field.clone(), Some(session.username.clone()));
    }
}

/// Refuses file ids this account may not put in a file field. Anyone who may view a row can
/// download the file its field names, so a field may only name a file the account uploaded
/// itself, any file if it may view files, or the file the row already has. Otherwise an
/// account without the `files` permissions could read any upload by naming its id.
pub(crate) async fn check_file_ids(
    state: &AdminState,
    resource: &dyn Resource,
    session: &Session,
    input: &Input,
    current: Option<&Row>,
) -> Result<(), AdminError> {
    let Some(uploads) = &state.uploads else {
        return Ok(());
    };
    let mut named = Vec::new();
    for field in &resource.options().file_fields {
        let Some(value) = input.get(field).and_then(|v| v.as_deref()).map(str::trim) else {
            continue;
        };
        if value.is_empty() || current.is_some_and(|row| display(row.get(field)) == value) {
            continue;
        }
        // Not a number: the model's own validation reports it.
        if let Ok(id) = value.parse::<i32>() {
            named.push((field, id));
        }
    }
    if named.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = named.iter().map(|(_, id)| *id).collect();
    let found = uploads.files_by_id(&ids).await?;
    let sees_all = session.can(crate::uploads::FILES, Action::View);
    for (field, id) in named {
        // A missing file and someone else's get the same answer, so ids are not confirmed.
        if !found
            .get(&id)
            .is_some_and(|file| sees_all || file.created_by == session.username)
        {
            return Err(AdminError::BadInput {
                field: field.clone(),
                message: format!("no file `{id}` that you uploaded or may view"),
            });
        }
    }
    Ok(())
}

/// A list query with this account's ownership condition added.
fn scoped(mut query: ListQuery, resource: &dyn Resource, session: &Session) -> ListQuery {
    query.filters.extend(owner_filter(resource, session));
    query
}

// ---- login -------------------------------------------------------------------------------

/// A provider's button on the login page.
#[derive(Serialize)]
struct LoginButton<'a> {
    name: &'a str,
    label: &'a str,
}

/// Which step of logging in the login page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The ways to log in.
    Login,
    /// The authenticator code, after another site or a login link.
    Code,
    /// "Check your inbox."
    Sent,
    /// The button that uses a login link.
    Link,
    /// The address to send a password reset link to.
    Forgot,
    /// Choosing the new password.
    Reset,
    /// Accepting an invitation.
    Invite,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Code => "code",
            Self::Sent => "sent",
            Self::Link => "link",
            Self::Forgot => "forgot",
            Self::Reset => "reset",
            Self::Invite => "invite",
        }
    }
}

/// The login page in `mode`, with an error or a notice.
pub(crate) fn login_page_as(
    state: &AdminState,
    status: StatusCode,
    mode: Mode,
    error: Option<String>,
    notice: Option<String>,
) -> Result<Response, AdminError> {
    let methods = &state.auth.methods;
    let buttons: Vec<LoginButton<'_>> = methods
        .providers
        .iter()
        .map(|p| LoginButton {
            name: &p.name,
            label: &p.label,
        })
        .collect();
    let page = render(
        state,
        "login.html",
        None,
        context! {
            mode => mode.name(),
            totp_required => state.auth.totp_required,
            password_login => methods.password,
            magic_link => methods.magic_link && state.accounts.is_some(),
            can_reset => crate::login::can_reset(state),
            providers => Serde(buttons),
            error => error,
            notice => notice,
        },
    )?;
    Ok((
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        page,
    )
        .into_response())
}

/// The login page with a message, e.g. after a refused login through a provider.
pub(crate) fn login_error(
    state: &AdminState,
    status: StatusCode,
    message: String,
) -> Result<Response, AdminError> {
    login_page_as(state, status, Mode::Login, Some(message), None)
}

/// The admin session cookie.
pub(crate) fn session_cookie(state: &AdminState, token: String) -> Cookie<'static> {
    Cookie::build((COOKIE, token))
        .path(state.base.clone())
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(state.auth.cookie_secure)
        .build()
}

async fn login_page(State(state): Shared) -> Result<Response, AdminError> {
    login_page_as(&state, StatusCode::OK, Mode::Login, None, None)
}

async fn login_submit(
    State(state): Shared,
    jar: CookieJar,
    parts: Parts,
    Form(form): Form<auth::Credentials>,
) -> Result<Response, AdminError> {
    if !state.auth.methods.password {
        return login_error(
            &state,
            StatusCode::FORBIDDEN,
            "Logging in with a password is turned off on this site.".into(),
        );
    }
    let attempt = auth::login(&state, &form, state.auth.client_ip(&parts)).await;
    match attempt {
        Ok(token) => {
            let cookie = session_cookie(&state, token);
            Ok((jar.add(cookie), Redirect::to(&format!("{}/", state.base))).into_response())
        }
        Err(AdminError::Unauthorized) => {
            tracing::warn!(username = %form.username, "admin login failed");
            login_error(
                &state,
                StatusCode::UNAUTHORIZED,
                "Invalid credentials.".into(),
            )
        }
        Err(AdminError::RateLimited { retry_after_secs }) => {
            let minutes = retry_after_secs.div_ceil(60);
            let page = login_error(
                &state,
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Too many attempts. Try again in {minutes} minute{}.",
                    if minutes == 1 { "" } else { "s" }
                ),
            )?;
            let retry = [(RETRY_AFTER, retry_after_secs.to_string())];
            Ok((retry, page).into_response())
        }
        Err(AdminError::Forbidden(_)) => login_error(
            &state,
            StatusCode::FORBIDDEN,
            "This account cannot use the admin.".into(),
        ),
        Err(other) => Err(other),
    }
}

async fn logout(
    State(state): Shared,
    session: Session,
    jar: CookieJar,
    Form(form): Form<FormData>,
) -> Result<Response, AdminError> {
    session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
    let removal = Cookie::build((COOKIE, "")).path(state.base.clone()).build();
    Ok((
        jar.remove(removal),
        Redirect::to(&format!("{}/login", state.base)),
    )
        .into_response())
}

// ---- pages -------------------------------------------------------------------------------

/// One model on the dashboard.
#[derive(Serialize)]
struct Tile<'a> {
    #[serde(flatten)]
    model: NavModel<'a>,
    /// `None` if the count could not be read.
    count: Option<u64>,
}

async fn index(State(state): Shared, session: Session) -> Result<Html<String>, AdminError> {
    let mut tiles = Vec::new();
    for resource in &state.resources {
        if !session.can(resource.slug(), Action::View) {
            continue;
        }
        // One broken model should not take the whole dashboard down with it.
        let count = resource
            .list(&scoped(ListQuery::first(1), resource.as_ref(), &session))
            .await
            .ok()
            .map(|p| p.total);
        tiles.push(Tile {
            model: NavModel::new(resource.as_ref(), &session),
            count,
        });
    }
    let mut accounts = None;
    let mut two_factor = None;
    let mut greeting = None;
    if let Some(db) = &state.accounts {
        use sea_orm::{EntityTrait, PaginatorTrait};
        if session.access.is_superuser() {
            let users = crate::users::user::Entity::find().count(db).await?;
            let groups = crate::users::group::Entity::find().count(db).await?;
            accounts = Some(context! { users, groups });
        }
        if let Some(account) = crate::users::find_account(db, &session.username).await? {
            two_factor = Some(account.totp_secret.is_some());
            greeting = Some(account.display_name());
        }
    }
    render(
        &state,
        "index.html",
        Some(&session),
        context! {
            section => "home",
            tiles => Value::from(Serde(&tiles)),
            accounts,
            two_factor,
            greeting,
        },
    )
}

#[derive(Deserialize)]
pub(crate) struct ListParams {
    #[serde(default)]
    pub(crate) page: Option<u64>,
    #[serde(default)]
    pub(crate) per_page: Option<u64>,
    #[serde(default)]
    pub(crate) q: Option<String>,
    /// A column name to order by, prefixed with `-` for highest first.
    #[serde(default)]
    pub(crate) sort: Option<String>,
    /// What just happened, from a redirect after a change; see [`notice_text`].
    #[serde(default)]
    pub(crate) notice: Option<String>,
    /// The bulk action just done, how many rows it did, and how many failed.
    #[serde(default)]
    pub(crate) done: Option<String>,
    #[serde(default)]
    pub(crate) n: Option<u64>,
    #[serde(default)]
    pub(crate) failed: Option<u64>,
}

/// The columns a list may be ordered by: the visible ones.
pub(crate) fn sortable(resource: &dyn Resource) -> impl Iterator<Item = &Field> {
    visible_fields(resource)
}

/// Splits `-title` into `("title", true)`.
pub(crate) fn parse_sort(sort: &str) -> (&str, bool) {
    match sort.strip_prefix('-') {
        Some(name) => (name, true),
        None => (sort, false),
    }
}

/// The message for a notice code. Only these fixed texts are ever shown, so a crafted link
/// cannot put words on the page.
pub(crate) fn notice_text(code: Option<&str>) -> Option<&'static str> {
    Some(match code? {
        "created" => "Created.",
        "saved" => "Changes saved.",
        "deleted" => "Deleted.",
        "user_created" => "Account created.",
        "user_saved" => "Account saved.",
        "user_deleted" => "Account deleted.",
        "group_created" => "Group created.",
        "group_saved" => "Group saved.",
        "group_deleted" => "Group deleted.",
        "two_factor_on" => "Two-factor authentication is on.",
        "two_factor_off" => "Two-factor authentication is off.",
        "api_key_revoked" => "The key is revoked; it no longer works.",
        "password_changed" => "Your password is changed.",
        "user_invited" => "Account created and invitation sent.",
        "invitation_sent" => "Invitation sent.",
        "recovery_codes_new" => "New recovery codes made; the old ones no longer work.",
        _ => return None,
    })
}

/// Query parameters of a list that are not filters.
const LIST_PARAMS: [&str; 8] = [
    "page", "per_page", "q", "sort", "notice", "done", "n", "failed",
];
/// The choice in a filter's dropdown that means "empty".
const NULL_CHOICE: &str = "__null";
/// Filter suffixes: `?views__gte=10`, `?deleted_at__isnull=true`, `?created_at__range=7d`.
const FILTER_SUFFIXES: [&str; 6] = ["gt", "gte", "lt", "lte", "isnull", "range"];

/// Whether a list can be filtered by `field`.
fn filterable(field: &Field) -> bool {
    !matches!(field.kind, FieldKind::Json | FieldKind::Other)
}

/// The start of a period and the start of the one after it, for date filters:
/// `today`, `7d`, `30d`, `month` and `year`, counted in UTC.
fn period(kind: FieldKind, name: &str) -> Option<(String, String)> {
    use chrono::{Datelike, Duration, NaiveDate, Utc};
    let today = Utc::now().date_naive();
    let tomorrow = today + Duration::days(1);
    let (from, to) = match name {
        "today" => (today, tomorrow),
        "7d" => (today - Duration::days(6), tomorrow),
        "30d" => (today - Duration::days(29), tomorrow),
        "month" => {
            let first = today.with_day(1)?;
            let next = if first.month() == 12 {
                NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)?
            };
            (first, next)
        }
        "year" => (
            NaiveDate::from_ymd_opt(today.year(), 1, 1)?,
            NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)?,
        ),
        _ => return None,
    };
    let format = match kind {
        FieldKind::Date => "%Y-%m-%d",
        FieldKind::DateTime => "%Y-%m-%dT00:00:00",
        _ => return None,
    };
    Some((
        from.format(format).to_string(),
        to.format(format).to_string(),
    ))
}

/// The filters in a list's query string: `field=value` for equality, `field__gte=value` and
/// the like for comparisons. With `strict`, a parameter that names no filterable column is
/// an error (the API); otherwise it is ignored (the admin, where links may be stale).
pub(crate) fn parse_filters(
    resource: &dyn Resource,
    raw: &HashMap<String, String>,
    strict: bool,
) -> Result<Vec<Filter>, AdminError> {
    let mut keys: Vec<&String> = raw.keys().collect();
    keys.sort();
    let mut filters = Vec::new();
    for key in keys {
        let value = raw[key].trim();
        if LIST_PARAMS.contains(&key.as_str()) || value.is_empty() {
            continue;
        }
        let (name, suffix) = match key.rsplit_once("__") {
            Some((name, suffix)) if FILTER_SUFFIXES.contains(&suffix) => (name, suffix),
            _ => (key.as_str(), ""),
        };
        let wrong = |message: String| AdminError::BadInput {
            field: key.clone(),
            message,
        };
        if suffix.is_empty() && resource.links().iter().any(|l| l.name == name) {
            filters.push(Filter {
                field: name.to_owned(),
                op: FilterOp::Equal,
                value: value.to_owned(),
            });
            continue;
        }
        let Some(field) = visible_fields(resource).find(|f| f.name == name && filterable(f)) else {
            if strict {
                return Err(wrong(format!(
                    "`{name}` is not a column this list can be filtered by"
                )));
            }
            continue;
        };
        let filter = |op, value: &str| Filter {
            field: field.name.clone(),
            op,
            value: value.to_owned(),
        };
        match suffix {
            "" if value == NULL_CHOICE => filters.push(filter(FilterOp::IsNull, "")),
            "" => filters.push(filter(FilterOp::Equal, value)),
            "gt" => filters.push(filter(FilterOp::Greater, value)),
            "gte" => filters.push(filter(FilterOp::GreaterOrEqual, value)),
            "lt" => filters.push(filter(FilterOp::Less, value)),
            "lte" => filters.push(filter(FilterOp::LessOrEqual, value)),
            "isnull" => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => filters.push(filter(FilterOp::IsNull, "")),
                "false" | "0" | "no" => filters.push(filter(FilterOp::IsNotNull, "")),
                _ if strict => return Err(wrong("must be true or false".into())),
                _ => {}
            },
            _ => match period(field.kind, value) {
                Some((from, to)) => {
                    filters.push(filter(FilterOp::GreaterOrEqual, &from));
                    filters.push(filter(FilterOp::Less, &to));
                }
                None if strict => {
                    return Err(wrong(
                        "must be today, 7d, 30d, month or year, on a date column".into(),
                    ));
                }
                None => {}
            },
        }
    }
    Ok(filters)
}

/// The filter parameters of a list, ready to append to its links: `a=1&b=2&`.
fn filter_query(raw: &HashMap<String, String>) -> (String, Vec<(String, String)>) {
    let mut pairs: Vec<(String, String)> = raw
        .iter()
        .filter(|(key, value)| !LIST_PARAMS.contains(&key.as_str()) && !value.trim().is_empty())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    pairs.sort();
    let text = pairs
        .iter()
        .map(|(key, value)| {
            let encode =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            format!("{}={}&", encode(key), encode(value))
        })
        .collect();
    (text, pairs)
}

pub(crate) fn list_query(
    state: &AdminState,
    resource: &dyn Resource,
    params: &ListParams,
) -> ListQuery {
    ListQuery {
        // The UI counts pages from 1.
        page: params.page.unwrap_or(1).saturating_sub(1),
        per_page: resource
            .options()
            .per_page
            .unwrap_or(state.settings.page_size),
        search: params.q.clone().filter(|q| !q.trim().is_empty()),
        sort: None,
        descending: false,
        filters: Vec::new(),
    }
    .sorted(resource, params.sort.as_deref())
}

impl ListQuery {
    /// Orders by `sort` if it names a column that may be sorted on; otherwise leaves the
    /// default order.
    fn sorted(mut self, resource: &dyn Resource, sort: Option<&str>) -> Self {
        if let Some((name, descending)) = sort.map(parse_sort)
            && sortable(resource).any(|f| f.name == name)
        {
            self.sort = Some(name.to_owned());
            self.descending = descending;
        }
        self
    }
}

/// One cell of a list page. A foreign key shows the name of the row it points to, linked.
#[derive(Serialize)]
struct ListCell {
    text: String,
    /// Model and id of the related row to link to.
    link: Option<(String, String)>,
    /// For a boolean column: the value, shown as Yes or No.
    flag: Option<bool>,
    /// A link of its own, such as an uploaded file's download.
    href: Option<String>,
}

/// A column heading of a list, with the sort order that clicking it asks for.
#[derive(Serialize)]
struct ColumnHead {
    label: String,
    /// `asc` or `desc` if the list is ordered by this column.
    active: Option<&'static str>,
    /// The `sort` parameter of the heading's link.
    next: String,
    /// Whether the list can be ordered by this column.
    sortable: bool,
}

/// The ids in a cell: one for a foreign key, several for a many-to-many field.
fn cell_ids(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::Array(items)) => items.iter().map(|v| display(Some(v))).collect(),
        other => vec![display(other)],
    }
}

#[derive(Serialize)]
struct ListRow {
    id: Option<String>,
    cells: Vec<ListCell>,
}

/// The most related rows a form offers in a dropdown. Beyond this the id is typed instead.
const MAX_CHOICES: u64 = 200;

/// The model a foreign-key field points to, if it is registered, the key is its primary key,
/// and this account may view it. Otherwise the field is shown as the plain value it holds,
/// so nothing about the other model leaks.
fn related<'a>(
    state: &'a AdminState,
    session: &Session,
    field: &Field,
) -> Option<&'a dyn Resource> {
    let reference = field.reference.as_ref()?;
    let target = state.resource(&reference.model).ok()?.as_ref();
    let usable = pk_name(target) == Some(reference.column.as_str())
        && session.can(target.slug(), Action::View);
    usable.then_some(target)
}

/// The model a many-to-many field links to, if it is registered and this account may view it.
fn link_target<'a>(
    state: &'a AdminState,
    session: &Session,
    link: &LinkField,
) -> Option<&'a dyn Resource> {
    let target = state.resource(&link.model).ok()?.as_ref();
    (pk_name(target).is_some() && session.can(target.slug(), Action::View)).then_some(target)
}

/// The name of a row where another model points to it: its display field (by default the
/// first text column, or else the start of the first long text), or `#id` if that is empty
/// or there is none.
fn row_label(resource: &dyn Resource, row: &Row) -> String {
    let chosen = resource.options().display_field.as_deref();
    let field = visible_fields(resource)
        .find(|f| match chosen {
            Some(name) => f.name == name,
            None => f.kind == FieldKind::Text && f.reference.is_none(),
        })
        .or_else(|| {
            chosen
                .is_none()
                .then(|| {
                    visible_fields(resource)
                        .find(|f| matches!(f.kind, FieldKind::LongText | FieldKind::Html))
                })
                .flatten()
        });
    field
        .map(|f| {
            let text = display(row.get(&f.name));
            let text = if f.kind == FieldKind::Html {
                crate::html::plain_text(&text)
            } else {
                text
            };
            let line = text.lines().next().unwrap_or_default().trim().to_owned();
            if line.chars().count() > 60 || text.trim() != line {
                format!("{}…", line.chars().take(60).collect::<String>())
            } else {
                line
            }
        })
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            let id = pk_name(resource).map(|pk| display(row.get(pk)));
            format!("#{}", id.unwrap_or_default())
        })
}

/// A related model with the names of some of its rows, keyed by id.
type RelatedNames<'a> = (&'a dyn Resource, HashMap<String, String>);

/// Names of the rows of `target` with these ids, keyed by id.
async fn related_labels(
    target: &dyn Resource,
    mut ids: Vec<String>,
) -> Result<HashMap<String, String>, AdminError> {
    ids.sort();
    ids.dedup();
    let Some(pk) = pk_name(target) else {
        return Ok(HashMap::new());
    };
    Ok(target
        .get_many(&ids)
        .await?
        .iter()
        .map(|row| (display(row.get(pk)), row_label(target, row)))
        .collect())
}

/// One dropdown above a list.
#[derive(Serialize)]
struct FilterView {
    /// The query parameter it sets.
    param: String,
    label: String,
    options: Vec<(String, String)>,
    selected: String,
}

/// The dropdowns for a model's `list_filter` columns.
async fn filter_views(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    raw: &HashMap<String, String>,
) -> Result<Vec<FilterView>, AdminError> {
    let mut views = Vec::new();
    let owner = resource.options().owner_field.as_ref();
    for name in &resource.options().list_filter {
        if let Some(link) = resource.links().into_iter().find(|l| &l.name == name) {
            let Some(target) = link_target(state, session, &link) else {
                continue;
            };
            let Some(choices) = reference_input(target, session, "").await?.0 else {
                continue;
            };
            let mut options = vec![(String::new(), "All".to_owned())];
            options.extend(choices.into_iter().map(|c| (c.value, c.label)));
            views.push(FilterView {
                selected: raw.get(name).cloned().unwrap_or_default(),
                label: link.label.clone(),
                param: name.clone(),
                options,
            });
            continue;
        }
        // Listing the owners would show other accounts' names to those limited to their own.
        if owner == Some(name) && !session.access.is_superuser() {
            continue;
        }
        let Some(field) = visible_fields(resource).find(|f| &f.name == name && filterable(f))
        else {
            continue;
        };
        let mut param = field.name.clone();
        let mut options = vec![(String::new(), "All".to_owned())];
        if let Some(target) = related(state, session, field) {
            let (choices, _) = reference_input(target, session, "").await?;
            match choices {
                Some(choices) => options.extend(choices.into_iter().map(|c| (c.value, c.label))),
                // Too many rows to list.
                None => continue,
            }
        } else {
            match field.kind {
                FieldKind::Boolean => {
                    options.push(("true".into(), "Yes".into()));
                    options.push(("false".into(), "No".into()));
                }
                FieldKind::Date | FieldKind::DateTime => {
                    param = format!("{}__range", field.name);
                    options[0].1 = "Any date".into();
                    for (value, label) in [
                        ("today", "Today"),
                        ("7d", "Past 7 days"),
                        ("30d", "Past 30 days"),
                        ("month", "This month"),
                        ("year", "This year"),
                    ] {
                        options.push((value.into(), label.into()));
                    }
                }
                _ => {
                    let values = resource.distinct(&field.name, MAX_CHOICES).await?;
                    if values.is_empty() {
                        continue;
                    }
                    options.extend(values.into_iter().map(|v| (v.clone(), truncate(v))));
                }
            }
        }
        if field.nullable && !param.ends_with("__range") {
            options.push((NULL_CHOICE.into(), "(empty)".into()));
        }
        views.push(FilterView {
            selected: raw.get(&param).cloned().unwrap_or_default(),
            label: field.label.clone(),
            param,
            options,
        });
    }
    Ok(views)
}

async fn list(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    Query(params): Query<ListParams>,
    Query(raw): Query<HashMap<String, String>>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    let mut query = list_query(&state, resource, &params);
    query.filters = parse_filters(resource, &raw, false)?;
    query.filters.extend(owner_filter(resource, &session));
    let filters = filter_views(&state, &session, resource, &raw).await?;
    let actions = bulk_choices(resource, &session);
    let (filter_qs, filter_pairs) = filter_query(&raw);
    let page = match resource.list(&query).await {
        Ok(page) => page,
        // A hand-edited link with a value the column cannot hold: show nothing, not an error.
        Err(AdminError::BadInput { .. }) => crate::resource::Page {
            rows: Vec::new(),
            total: 0,
        },
        Err(other) => return Err(other),
    };

    let chosen = &resource.options().list_display;
    // Many-to-many fields as columns: like foreign keys, but holding several ids.
    let links = resource.links();
    let link_columns: Vec<Field> = links
        .iter()
        .filter(|l| chosen.contains(&l.name))
        .filter_map(|l| {
            let target = link_target(&state, &session, l)?;
            Some(Field {
                name: l.name.clone(),
                label: l.label.clone(),
                kind: FieldKind::Other,
                nullable: true,
                primary_key: false,
                auto: false,
                editable: false,
                has_default: false,
                reference: Some(Reference {
                    model: target.slug().to_owned(),
                    column: pk_name(target)?.to_owned(),
                }),
                choices: Vec::new(),
            })
        })
        .collect();
    let columns: Vec<&Field> = if chosen.is_empty() {
        visible_fields(resource).collect()
    } else {
        // Keep the configured order; silently skip names that are hidden or unknown.
        chosen
            .iter()
            .filter_map(|name| {
                visible_fields(resource)
                    .find(|f| &f.name == name)
                    .or_else(|| link_columns.iter().find(|f| &f.name == name))
            })
            .collect()
    };
    let is_link = |f: &Field| links.iter().any(|l| l.name == f.name);
    // For each foreign-key column, the names of the rows this page points to: one query per
    // such column, however many rows are shown.
    let mut names: Vec<Option<RelatedNames<'_>>> = Vec::new();
    for field in &columns {
        names.push(match related(&state, &session, field) {
            Some(target) => {
                let ids = page
                    .rows
                    .iter()
                    .flat_map(|row| cell_ids(row.get(&field.name)))
                    .filter(|id| !id.is_empty())
                    .collect();
                Some((target, related_labels(target, ids).await?))
            }
            None => None,
        });
    }
    // For each file column, the files this page points to: one query per column.
    let file_columns = &resource.options().file_fields;
    let mut files: Vec<Option<HashMap<i32, crate::uploads::file::Model>>> = Vec::new();
    for field in &columns {
        files.push(match &state.uploads {
            Some(uploads) if file_columns.contains(&field.name) => {
                let ids: Vec<i32> = page
                    .rows
                    .iter()
                    .filter_map(|row| display(row.get(&field.name)).parse().ok())
                    .collect();
                Some(uploads.files_by_id(&ids).await?)
            }
            _ => None,
        });
    }
    let pk = pk_name(resource);
    let rows: Vec<ListRow> = page
        .rows
        .iter()
        .map(|row| ListRow {
            id: pk.map(|name| display(row.get(name))),
            cells: columns
                .iter()
                .zip(&names)
                .zip(&files)
                .map(|((f, names), files)| {
                    let value = display(row.get(&f.name));
                    let value = if f.kind == FieldKind::Html {
                        crate::html::plain_text(&value)
                    } else {
                        value
                    };
                    if let Some(files) = files {
                        let found = value.parse::<i32>().ok().and_then(|id| files.get(&id));
                        let row_id = pk.map(|name| display(row.get(name)));
                        return ListCell {
                            text: found.map_or_else(String::new, |f| truncate(f.filename.clone())),
                            link: None,
                            flag: None,
                            href: found.zip(row_id).map(|(_, row_id)| {
                                format!(
                                    "{}/{}/{}/file/{}",
                                    state.base,
                                    resource.slug(),
                                    url::form_urlencoded::byte_serialize(row_id.as_bytes())
                                        .collect::<String>(),
                                    f.name
                                )
                            }),
                        };
                    }
                    if is_link(f) {
                        let labels: Vec<String> = cell_ids(row.get(&f.name))
                            .iter()
                            .map(|id| {
                                names
                                    .as_ref()
                                    .and_then(|(_, labels)| labels.get(id).cloned())
                                    .unwrap_or_else(|| format!("#{id}"))
                            })
                            .collect();
                        return ListCell {
                            text: truncate(labels.join(", ")),
                            link: None,
                            flag: None,
                            href: None,
                        };
                    }
                    let named = names
                        .as_ref()
                        .and_then(|(target, labels)| Some((target, labels.get(&value)?)));
                    match named {
                        Some((target, label)) => ListCell {
                            text: truncate(label.clone()),
                            link: Some((target.slug().to_owned(), value)),
                            flag: None,
                            href: None,
                        },
                        // Not a foreign key, or one that points at nothing.
                        None => ListCell {
                            flag: (f.kind == FieldKind::Boolean)
                                .then(|| row.get(&f.name).and_then(boolean_value))
                                .flatten(),
                            text: truncate(value),
                            link: None,
                            href: None,
                        },
                    }
                })
                .collect(),
        })
        .collect();
    let pages = page.total.div_ceil(query.per_page).max(1);
    let heads: Vec<ColumnHead> = columns
        .iter()
        .map(|f| {
            let active = (query.sort.as_deref() == Some(f.name.as_str()))
                .then_some(if query.descending { "desc" } else { "asc" });
            // Lowest first, then highest first, then back to the default order.
            let next = match active {
                None => f.name.clone(),
                Some("asc") => format!("-{}", f.name),
                Some(_) => String::new(),
            };
            ColumnHead {
                label: f.label.clone(),
                active,
                next,
                sortable: !is_link(f),
            }
        })
        .collect();
    let sort = query
        .sort
        .as_ref()
        .map(|name| format!("{}{name}", if query.descending { "-" } else { "" }));

    render(
        &state,
        "list.html",
        Some(&session),
        context! {
            section => &model,
            model => Value::from(Serde(NavModel::new(resource, &session))),
            can_change => session.can(&model, Action::Change),
            columns => Value::from(Serde(&heads)),
            sort,
            notice => bulk_message(resource, &params)
                .or_else(|| notice_text(params.notice.as_deref()).map(str::to_owned)),
            rows => Value::from(Serde(&rows)),
            total => page.total,
            page => query.page + 1,
            pages => pages,
            q => query.search.unwrap_or_default(),
            filters => Value::from(Serde(&filters)),
            actions => Value::from(Serde(&actions)),
            back => back_query(&raw),
            filter_qs,
            filter_pairs => Value::from(Serde(&filter_pairs)),
            filtered => !query.filters.is_empty(),
        },
    )
}

#[derive(Serialize)]
struct FormField<'a> {
    name: &'a str,
    label: &'a str,
    kind: FieldKind,
    value: String,
    required: bool,
    readonly: bool,
    /// For a foreign key: the rows it can point to, as a dropdown.
    choices: Option<Vec<Choice>>,
    /// Extra line under the input.
    hint: Option<String>,
    /// For a file field: a file picker, and the current file if there is one.
    file_field: bool,
    file: Option<FileView>,
}

/// An uploaded file as a form shows it.
#[derive(Serialize)]
struct FileView {
    filename: String,
    size: String,
    url: String,
    image: bool,
}

/// `1536` -> `1.5 KB`.
fn human_size(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    match bytes {
        b if b < 1024.0 => format!("{b} bytes"),
        b if b < 1024.0 * 1024.0 => format!("{:.1} KB", b / 1024.0),
        b if b < 1024.0 * 1024.0 * 1024.0 => format!("{:.1} MB", b / (1024.0 * 1024.0)),
        b => format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0)),
    }
}

#[derive(Serialize)]
struct Choice {
    value: String,
    label: String,
    selected: bool,
}

/// How a foreign-key field is offered: a dropdown of the related rows, or, if there are too
/// many to list, the plain input with a hint naming the row it currently points to.
async fn reference_input(
    target: &dyn Resource,
    session: &Session,
    current: &str,
) -> Result<(Option<Vec<Choice>>, Option<String>), AdminError> {
    let Some(pk) = pk_name(target) else {
        return Ok((None, None));
    };
    let page = target
        .list(&scoped(ListQuery::first(MAX_CHOICES), target, session))
        .await?;
    if page.total > MAX_CHOICES {
        let labels = related_labels(target, vec![current.to_owned()]).await?;
        let pointing = labels
            .get(current)
            .map(|label| format!("Currently: {label}. "))
            .unwrap_or_default();
        let hint = format!(
            "{pointing}There are too many {} to list here; enter an id.",
            target.label()
        );
        return Ok((None, Some(hint)));
    }
    let mut choices: Vec<Choice> = page
        .rows
        .iter()
        .map(|row| {
            let value = display(row.get(pk));
            Choice {
                selected: value == current,
                label: row_label(target, row),
                value,
            }
        })
        .collect();
    choices.sort_by_cached_key(|c| c.label.to_lowercase());
    // Keep a value that points at nothing visible, so saving does not silently change it.
    if !current.is_empty() && !choices.iter().any(|c| c.selected) {
        choices.push(Choice {
            value: current.to_owned(),
            label: format!("#{current} (not found)"),
            selected: true,
        });
    }
    Ok((Some(choices), None))
}

fn input_value(field: &Field, value: Option<&serde_json::Value>) -> String {
    let text = display(value);
    match field.kind {
        // One item per line.
        FieldKind::List => match value {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| display(Some(item)))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => text,
        },
        // `datetime-local` wants `YYYY-MM-DDTHH:MM:SS`; databases usually print a space.
        FieldKind::DateTime => text.replacen(' ', "T", 1).chars().take(19).collect(),
        FieldKind::Json => value
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_default(),
        // Cleaned again on the way out, in case it reached the database some other way.
        FieldKind::Html => crate::html::clean_html(&text),
        _ => text,
    }
}

/// Builds the form description for a new (`row == None`) or existing record.
async fn form_fields<'a>(
    state: &AdminState,
    session: &Session,
    resource: &'a dyn Resource,
    row: Option<&Row>,
    creating: bool,
) -> Result<Vec<FormField<'a>>, AdminError> {
    let readonly_names = &resource.options().readonly_fields;
    // Only superusers may choose whose a row is.
    let owner_locked = resource
        .options()
        .owner_field
        .as_ref()
        .filter(|_| !session.access.is_superuser());
    let mut fields = Vec::new();
    // A database-generated key has nothing to show before the row exists.
    for f in visible_fields(resource).filter(|f| !(creating && f.auto)) {
        let owned = owner_locked == Some(&f.name);
        let readonly = !f.editable
            || f.auto
            || (f.primary_key && !creating)
            || readonly_names.contains(&f.name)
            || owned;
        let mut value = row
            .map(|r| input_value(f, r.get(&f.name)))
            .unwrap_or_default();
        if owned && creating {
            value.clone_from(&session.username);
        }
        let file_field = resource.options().file_fields.contains(&f.name);
        let made_up = creating && f.primary_key && f.kind == FieldKind::Uuid;
        let (choices, mut hint) = match related(state, session, f) {
            Some(target) if !file_field => reference_input(target, session, &value).await?,
            _ if f.kind == FieldKind::Choice => {
                let mut choices: Vec<Choice> = f
                    .choices
                    .iter()
                    .map(|c| Choice {
                        selected: *c == value,
                        label: c.clone(),
                        value: c.clone(),
                    })
                    .collect();
                if !value.is_empty() && !choices.iter().any(|c| c.selected) {
                    choices.push(Choice {
                        label: format!("{value} (not a choice)"),
                        value: value.clone(),
                        selected: true,
                    });
                }
                (Some(choices), None)
            }
            _ => (None, None),
        };
        if made_up {
            hint = Some("Leave empty to have one made up.".into());
        } else if f.kind == FieldKind::List && !readonly {
            hint = Some("One item per line.".into());
        } else if owned {
            hint = Some("Rows belong to the account that adds them.".into());
        }
        let file = match (file_field, value.parse::<i32>(), &state.uploads, row) {
            (true, Ok(file_id), Some(uploads), Some(row)) => {
                let row_id = pk_name(resource).map(|pk| display(row.get(pk)));
                uploads
                    .files_by_id(&[file_id])
                    .await?
                    .remove(&file_id)
                    .zip(row_id)
                    .map(|(found, row_id)| FileView {
                        url: format!(
                            "{}/{}/{}/file/{}",
                            state.base,
                            resource.slug(),
                            url::form_urlencoded::byte_serialize(row_id.as_bytes())
                                .collect::<String>(),
                            f.name
                        ),
                        image: crate::uploads::is_raster_image(&found.content_type),
                        size: human_size(found.size),
                        filename: found.filename,
                    })
            }
            _ => None,
        };
        fields.push(FormField {
            name: &f.name,
            label: &f.label,
            kind: f.kind,
            value,
            required: !f.nullable
                && !readonly
                && !made_up
                && !matches!(
                    f.kind,
                    FieldKind::Boolean
                        | FieldKind::Text
                        | FieldKind::LongText
                        | FieldKind::Html
                        | FieldKind::List
                ),
            readonly,
            choices,
            hint,
            file_field,
            file,
        });
    }
    Ok(fields)
}

/// A many-to-many field on a form: checkboxes of the rows it can link to, or, if there are
/// too many to list, their ids separated by commas.
#[derive(Serialize)]
struct LinkInput {
    name: String,
    label: String,
    choices: Option<Vec<Choice>>,
    /// The linked ids, for the plain input.
    value: String,
    hint: Option<String>,
}

async fn link_inputs(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    row: Option<&Row>,
) -> Result<Vec<LinkInput>, AdminError> {
    let mut inputs = Vec::new();
    for link in resource.links() {
        let selected: Vec<String> = match row.and_then(|r| r.get(&link.name)) {
            Some(serde_json::Value::Array(items)) => {
                items.iter().map(|v| display(Some(v))).collect()
            }
            Some(serde_json::Value::String(text)) => text
                .split(',')
                .map(|id| id.trim().to_owned())
                .filter(|id| !id.is_empty())
                .collect(),
            _ => Vec::new(),
        };
        let value = selected.join(", ");
        let (choices, hint) = match link_target(state, session, &link) {
            Some(target) => match reference_input(target, session, "").await? {
                (Some(mut choices), _) => {
                    for choice in &mut choices {
                        choice.selected = selected.contains(&choice.value);
                    }
                    (Some(choices), None)
                }
                (None, _) => (
                    None,
                    Some(format!(
                        "There are too many {} to list here; enter their ids, separated by commas.",
                        target.label()
                    )),
                ),
            },
            None => (None, Some("Ids, separated by commas.".to_owned())),
        };
        inputs.push(LinkInput {
            name: link.name,
            label: link.label,
            choices,
            value,
            hint,
        });
    }
    Ok(inputs)
}

/// Rows of another model that point to a record, shown under its form.
#[derive(Serialize)]
struct RelatedList {
    /// URL name of the model the rows belong to.
    model: String,
    label: String,
    total: u64,
    /// `(id, name)` of the first few.
    rows: Vec<(String, String)>,
    /// The query string that picks these rows, and starts a new one pointing here.
    query: String,
    /// Whether this account may add rows to the other model.
    can_add: bool,
}

/// How many pointing rows each related list shows.
const RELATED_ROWS: u64 = 5;

async fn related_lists(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    id: &str,
) -> Result<Vec<RelatedList>, AdminError> {
    let mut lists = Vec::new();
    if pk_name(resource).is_none() {
        return Ok(lists);
    }
    for other in &state.resources {
        let other = other.as_ref();
        if !session.can(other.slug(), Action::View) || pk_name(other).is_none() {
            continue;
        }
        for field in visible_fields(other) {
            let points_here = field.reference.as_ref().is_some_and(|r| {
                r.model == resource.slug() && Some(r.column.as_str()) == pk_name(resource)
            });
            if !points_here {
                continue;
            }
            let mut query = ListQuery::first(RELATED_ROWS);
            query.filters.push(Filter {
                field: field.name.clone(),
                op: FilterOp::Equal,
                value: id.to_owned(),
            });
            let page = other.list(&scoped(query, other, session)).await?;
            let pk = pk_name(other).unwrap_or_default();
            let encode =
                |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
            // "Books" for one key; "Posts (editor)" when another key points here too.
            let several = visible_fields(other)
                .filter(|f| {
                    f.reference
                        .as_ref()
                        .is_some_and(|r| r.model == resource.slug())
                })
                .count()
                > 1;
            let label = if several {
                format!("{} ({})", other.label(), field.label.to_lowercase())
            } else {
                other.label().to_owned()
            };
            lists.push(RelatedList {
                model: other.slug().to_owned(),
                label,
                total: page.total,
                rows: page
                    .rows
                    .iter()
                    .map(|row| (display(row.get(pk)), row_label(other, row)))
                    .collect(),
                query: format!("{}={}", encode(&field.name), encode(id)),
                can_add: session.can(other.slug(), Action::Add),
            });
        }
    }
    Ok(lists)
}

async fn render_form(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    id: Option<&str>,
    row: Option<&Row>,
    error: Option<String>,
) -> Result<Html<String>, AdminError> {
    let fields = form_fields(state, session, resource, row, id.is_none()).await?;
    let links = link_inputs(state, session, resource, row).await?;
    let related = match id {
        Some(id) => related_lists(state, session, resource, id).await?,
        None => Vec::new(),
    };
    render(
        state,
        "form.html",
        Some(session),
        context! {
            section => resource.slug(),
            model => Value::from(Serde(NavModel::new(resource, session))),
            fields => Value::from(Serde(fields)),
            links => Value::from(Serde(links)),
            related => Value::from(Serde(related)),
            id => id,
            error => error,
            // A new record can always be saved here: the page itself required `add`.
            can_save => id.is_none() || session.can(resource.slug(), Action::Change),
            can_delete => session.can(resource.slug(), Action::Delete),
            multipart => !resource.options().file_fields.is_empty(),
            // Loads the editor's script and styles.
            rich_text => visible_fields(resource).any(|f| f.kind == FieldKind::Html),
            quill => Value::from_safe_string(QUILL_PATH.to_owned()),
        },
    )
}

/// Converts a form submission to resource input.
fn form_input(resource: &dyn Resource, form: &FormData) -> Input {
    let mut input: Input = form
        .iter()
        .filter(|(name, _)| name.as_str() != CSRF_FIELD)
        .map(|(name, value)| (name.clone(), Some(value.clone())))
        .collect();
    // Browsers omit unchecked checkboxes entirely, so absence means false here.
    for field in resource
        .fields()
        .iter()
        .filter(|f| f.kind == FieldKind::Boolean)
    {
        input
            .entry(field.name.clone())
            .or_insert_with(|| Some("false".to_owned()));
    }
    for (name, ids) in checked_links(resource, form) {
        input.insert(name, Some(serde_json::Value::from(ids).to_string()));
    }
    input
}

/// The ids ticked for each many-to-many field shown as checkboxes, which are named
/// `<field>__link__<id>`; `<field>__links` marks that the boxes were on the form.
fn checked_links(resource: &dyn Resource, form: &FormData) -> Vec<(String, Vec<String>)> {
    resource
        .links()
        .into_iter()
        .filter(|link| form.contains_key(&format!("{}__links", link.name)))
        .map(|link| {
            let prefix = format!("{}__link__", link.name);
            let mut ids: Vec<String> = form
                .keys()
                .filter_map(|key| key.strip_prefix(&prefix).map(str::to_owned))
                .collect();
            ids.sort();
            (link.name, ids)
        })
        .collect()
}

/// Turns submitted text back into a row so a rejected form can be shown again as typed.
fn echo_row(resource: &dyn Resource, form: &FormData) -> Row {
    let mut row: Row = form
        .iter()
        .filter(|(name, _)| name.as_str() != CSRF_FIELD && !name.contains("__link"))
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect();
    for (name, ids) in checked_links(resource, form) {
        row.insert(name, serde_json::Value::from(ids));
    }
    row
}

async fn new_form(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    Query(raw): Query<HashMap<String, String>>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Add)?;
    // Values to start from, as the "Add" link of a related list gives: `?author_id=3`.
    let start: Row = visible_fields(resource)
        .filter_map(|f| {
            let value = raw.get(&f.name)?;
            Some((f.name.clone(), serde_json::Value::String(value.clone())))
        })
        .collect();
    let row = (!start.is_empty()).then_some(&start);
    render_form(&state, &session, resource, None, row, None).await
}

/// Re-renders the form for errors the user can fix; passes everything else up.
async fn form_error(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    id: Option<&str>,
    form: &FormData,
    error: AdminError,
) -> Result<Response, AdminError> {
    let message = match &error {
        AdminError::BadInput { .. } => error.to_string(),
        AdminError::Db(db) => {
            tracing::warn!(model = resource.slug(), error = %db, "admin write rejected by database");
            "The database rejected this change (check required and unique fields).".to_owned()
        }
        _ => return Err(error),
    };
    let mut row = echo_row(resource, form);
    if let (Some(id), Some(pk)) = (id, pk_name(resource)) {
        row.insert(pk.to_owned(), serde_json::Value::String(id.to_owned()));
    }
    let page = render_form(state, session, resource, id, Some(&row), Some(message)).await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

async fn create(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Add)?;
    let (form, stored) = read_form(&state, &session, resource, request).await?;
    let mut input = form_input(resource, &form);
    stamp_owner(resource, &session, &mut input, true);
    let created = match check_file_ids(&state, resource, &session, &input, None).await {
        Ok(()) => resource.create(&input).await.map(|_| ()),
        Err(error) => Err(error),
    };
    match created {
        Ok(()) => {
            Ok(Redirect::to(&format!("{}/{model}?notice=created", state.base)).into_response())
        }
        Err(error) => {
            forget_files(&state, &stored).await;
            form_error(&state, &session, resource, None, &form, error).await
        }
    }
}

/// Reads a form, ordinary or multipart. Files picked for file fields are stored and their
/// new ids put in the form; the ids are returned too, so a rejected form can remove them.
async fn read_form(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    request: axum::extract::Request,
) -> Result<(FormData, Vec<i32>), AdminError> {
    use axum::extract::FromRequest;
    let multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    if !multipart {
        let Form(form) = Form::<FormData>::from_request(request, &())
            .await
            .map_err(|e| AdminError::BadInput {
                field: "form".into(),
                message: e.body_text(),
            })?;
        session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
        return Ok((form, Vec::new()));
    }
    let malformed = |_| AdminError::BadInput {
        field: "form".into(),
        message: "the form was cut off or malformed".into(),
    };
    let mut parts = axum::extract::Multipart::from_request(request, &())
        .await
        .map_err(|e| AdminError::BadInput {
            field: "form".into(),
            message: e.body_text(),
        })?;
    let file_fields = &resource.options().file_fields;
    let mut form = FormData::new();
    let mut stored = Vec::new();
    let mut uploaded = Vec::new();
    let mut cleared = Vec::new();
    let outcome: Result<(), AdminError> = async {
        while let Some(part) = parts.next_field().await.map_err(malformed)? {
            let name = part.name().unwrap_or_default().to_owned();
            if let Some(target) = name
                .strip_suffix("__upload")
                .filter(|t| file_fields.iter().any(|f| f == t))
            {
                // An empty picker still sends a part, without a file name.
                if part.file_name().is_none_or(str::is_empty) {
                    continue;
                }
                let uploads = state
                    .uploads
                    .as_ref()
                    .ok_or_else(|| AdminError::Config("file fields need uploads".into()))?;
                let saved = uploads.store_field(part, &session.username).await?;
                stored.push(saved.id);
                uploaded.push((target.to_owned(), saved.id));
            } else if let Some(target) = name.strip_suffix("__clear") {
                cleared.push(target.to_owned());
            } else {
                let text = part.text().await.map_err(malformed)?;
                form.insert(name, text);
            }
        }
        session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))
    }
    .await;
    if let Err(error) = outcome {
        forget_files(state, &stored).await;
        return Err(error);
    }
    for target in cleared {
        form.insert(target, String::new());
    }
    for (target, id) in uploaded {
        form.insert(target, id.to_string());
    }
    Ok((form, stored))
}

/// Removes files stored for a form that was then rejected.
async fn forget_files(state: &AdminState, ids: &[i32]) {
    let Some(uploads) = &state.uploads else {
        return;
    };
    for id in ids {
        if let Err(error) = uploads.remove(*id).await {
            tracing::warn!(file = id, %error, "could not remove the file of a rejected form");
        }
    }
}

#[derive(Deserialize)]
struct FileParams {
    #[serde(default)]
    inline: Option<String>,
}

/// The file a row's file field points to, for anyone who may view the row.
async fn file_content(
    State(state): Shared,
    session: Session,
    Path((model, id, field)): Path<(String, String, String)>,
    Query(params): Query<FileParams>,
) -> Result<Response, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    if !resource.options().file_fields.contains(&field) {
        return Err(AdminError::NotFound(format!("file field `{field}`")));
    }
    let uploads = state
        .uploads
        .as_ref()
        .ok_or_else(|| AdminError::NotFound("file uploads are off".into()))?;
    let row = owned_row(resource, &session, &id).await?;
    let file_id = display(row.get(&field))
        .parse::<i32>()
        .map_err(|_| AdminError::NotFound("no file".into()))?;
    let found = uploads
        .files_by_id(&[file_id])
        .await?
        .remove(&file_id)
        .ok_or_else(|| AdminError::NotFound("no file".into()))?;
    uploads.respond(&found, params.inline.is_some()).await
}

async fn edit_form(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    let row = owned_row(resource, &session, &id).await?;
    render_form(&state, &session, resource, Some(&id), Some(&row), None).await
}

async fn update(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Change)?;
    let current = owned_row(resource, &session, &id).await?;
    let (form, stored) = read_form(&state, &session, resource, request).await?;
    let mut input = form_input(resource, &form);
    stamp_owner(resource, &session, &mut input, false);
    let saved = match check_file_ids(&state, resource, &session, &input, Some(&current)).await {
        Ok(()) => resource.update(&id, &input, WriteMode::Replace).await,
        Err(error) => Err(error),
    };
    match saved {
        Ok(()) => Ok(Redirect::to(&format!("{}/{model}?notice=saved", state.base)).into_response()),
        Err(error) => {
            forget_files(&state, &stored).await;
            form_error(&state, &session, resource, Some(&id), &form, error).await
        }
    }
}

async fn delete(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    Form(form): Form<FormData>,
) -> Result<Redirect, AdminError> {
    session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Delete)?;
    owned_row(resource, &session, &id).await?;
    resource.delete(&id).await?;
    Ok(Redirect::to(&format!(
        "{}/{model}?notice=deleted",
        state.base
    )))
}

// ---- bulk actions --------------------------------------------------------------------------

/// The actions this account may run on selected rows: the model's own (needing `change`),
/// then "Delete selected" (needing `delete`).
fn bulk_choices(resource: &dyn Resource, session: &Session) -> Vec<(String, String)> {
    let slug = resource.slug();
    let mut choices = Vec::new();
    if session.can(slug, Action::Change) {
        choices.extend(
            resource
                .options()
                .actions
                .iter()
                .map(|a| (a.name.clone(), a.label.clone())),
        );
    }
    if session.can(slug, Action::Delete) {
        choices.push(("delete".to_owned(), "Delete selected".to_owned()));
    }
    choices
}

/// The list's own query string without what a redirect adds, for returning to it.
fn back_query(raw: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = raw
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "notice" | "done" | "n" | "failed" | "page"))
        .collect();
    pairs.sort();
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    query.finish()
}

/// What a bulk action did, from the redirect after it.
fn bulk_message(resource: &dyn Resource, params: &ListParams) -> Option<String> {
    let name = params.done.as_deref()?;
    let n = params.n.unwrap_or(0);
    let failed = params.failed.unwrap_or(0);
    let rows = |n: u64| format!("{n} {}", if n == 1 { "row" } else { "rows" });
    let mut text = if name == "delete" {
        format!("Deleted {}.", rows(n))
    } else {
        let action = resource.options().actions.iter().find(|a| a.name == name)?;
        format!("{}: {}.", action.label, rows(n))
    };
    if failed > 0 {
        text.push_str(&if name == "delete" {
            format!(
                " {} could not be deleted, probably because other rows point to them.",
                rows(failed)
            )
        } else {
            format!(" {} failed.", rows(failed))
        });
    }
    Some(text)
}

/// The most rows one bulk action touches.
const MAX_BULK: usize = 1000;

async fn bulk(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, AdminError> {
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(&body).into_owned().collect();
    let value = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    session.check_csrf(value(CSRF_FIELD))?;
    let resource = state.resource(&model)?.as_ref();
    let mut ids: Vec<String> = fields
        .iter()
        .filter(|(key, value)| key == "ids" && !value.is_empty())
        .map(|(_, value)| value.clone())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_BULK {
        return Err(AdminError::BadInput {
            field: "ids".into(),
            message: format!("at most {MAX_BULK} rows at a time"),
        });
    }
    // Back to the same list: only its own parameters, re-encoded.
    let back: String = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in
            url::form_urlencoded::parse(value("back").unwrap_or_default().as_bytes())
        {
            if !matches!(key.as_ref(), "notice" | "done" | "n" | "failed" | "page") {
                query.append_pair(&key, &value);
            }
        }
        query.finish()
    };
    let list = format!("{}/{model}", state.base);
    let name = value("action").unwrap_or_default().to_owned();
    if ids.is_empty() || name.is_empty() {
        let target = if back.is_empty() {
            list
        } else {
            format!("{list}?{back}")
        };
        return Ok(Redirect::to(&target).into_response());
    }

    let (mut done, mut failed) = (0_u64, 0_u64);
    // Rows of other accounts are left out, as if they had not been picked.
    if owner_filter(resource, &session).is_some() {
        let mut own = Vec::new();
        for id in ids {
            if owned_row(resource, &session, &id).await.is_ok() {
                own.push(id);
            } else {
                failed += 1;
            }
        }
        ids = own;
    }
    if name == "delete" {
        session.require(&model, Action::Delete)?;
        for id in &ids {
            match resource.delete(id).await {
                Ok(()) => done += 1,
                Err(error) => {
                    tracing::warn!(%model, %id, %error, "bulk delete skipped a row");
                    failed += 1;
                }
            }
        }
        tracing::info!(%model, done, failed, username = %session.username, "bulk delete");
    } else {
        session.require(&model, Action::Change)?;
        let action = resource
            .options()
            .actions
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| AdminError::BadInput {
                field: "action".into(),
                message: format!("`{name}` is not an action of this model"),
            })?;
        match &action.kind {
            crate::resource::ActionKind::Set { field, value } => {
                let input: Input = HashMap::from([(field.clone(), Some(value.clone()))]);
                for id in &ids {
                    match resource.update(id, &input, WriteMode::Patch).await {
                        Ok(()) => done += 1,
                        Err(error) => {
                            tracing::warn!(%model, %id, %error, action = %name, "bulk action skipped a row");
                            failed += 1;
                        }
                    }
                }
            }
            crate::resource::ActionKind::Custom(run) => match run(ids.clone()).await {
                Ok(()) => done = ids.len() as u64,
                Err(error) => {
                    tracing::error!(%model, %error, action = %name, "bulk action failed");
                    failed = ids.len() as u64;
                }
            },
        }
        tracing::info!(%model, action = %name, done, failed, username = %session.username, "bulk action");
    }
    let mut target = format!("{list}?");
    if !back.is_empty() {
        target.push_str(&back);
        target.push('&');
    }
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    target.push_str(&format!("done={encoded}&n={done}&failed={failed}"));
    Ok(Redirect::to(&target).into_response())
}

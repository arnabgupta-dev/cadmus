//! HTTP handlers. They only translate between HTTP and the `Resource`/auth seams.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::RETRY_AFTER;
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
use crate::resource::{Field, FieldKind, FormData, Input, ListQuery, Resource, Row, WriteMode};

type Shared = State<Arc<AdminState>>;

/// Longest cell text shown on a list page.
const CELL_CHARS: usize = 80;
const CSRF_FIELD: &str = "_csrf";

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.base.clone();
    Router::new()
        .route(&b, get(index))
        .route(&format!("{b}/"), get(index))
        .route(&format!("{b}/login"), get(login_page).post(login_submit))
        .route(&format!("{b}/logout"), post(logout))
        .route(&format!("{b}/{{model}}"), get(list))
        .route(&format!("{b}/{{model}}/new"), get(new_form).post(create))
        .route(
            &format!("{b}/{{model}}/{{id}}"),
            get(edit_form).post(update),
        )
        .route(&format!("{b}/{{model}}/{{id}}/delete"), post(delete))
        .with_state(state)
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
        // Whether to link to the Users and Groups pages.
        manages_accounts => state.accounts.is_some()
            && session.is_some_and(|s| s.access.is_superuser()),
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

// ---- login -------------------------------------------------------------------------------

async fn login_page(State(state): Shared) -> Result<Html<String>, AdminError> {
    render(
        &state,
        "login.html",
        None,
        context! { totp_required => state.auth.totp_required },
    )
}

async fn login_submit(
    State(state): Shared,
    jar: CookieJar,
    parts: Parts,
    Form(form): Form<auth::Credentials>,
) -> Result<Response, AdminError> {
    let attempt = auth::login(&state, &form, state.auth.client_ip(&parts)).await;
    match attempt {
        Ok(token) => {
            let cookie = Cookie::build((COOKIE, token))
                .path(state.base.clone())
                .http_only(true)
                .same_site(SameSite::Strict)
                .secure(state.auth.cookie_secure)
                .build();
            Ok((jar.add(cookie), Redirect::to(&format!("{}/", state.base))).into_response())
        }
        Err(AdminError::Unauthorized) => {
            tracing::warn!(username = %form.username, "admin login failed");
            let page = render(
                &state,
                "login.html",
                None,
                context! {
                    totp_required => state.auth.totp_required,
                    error => "Invalid credentials.",
                },
            )?;
            Ok((StatusCode::UNAUTHORIZED, page).into_response())
        }
        Err(AdminError::RateLimited { retry_after_secs }) => {
            let minutes = retry_after_secs.div_ceil(60);
            let page = render(
                &state,
                "login.html",
                None,
                context! {
                    totp_required => state.auth.totp_required,
                    error => format!(
                        "Too many attempts. Try again in {minutes} minute{}.",
                        if minutes == 1 { "" } else { "s" }
                    ),
                },
            )?;
            let retry = [(RETRY_AFTER, retry_after_secs.to_string())];
            Ok((StatusCode::TOO_MANY_REQUESTS, retry, page).into_response())
        }
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
            .list(&ListQuery::first(1))
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
        _ => return None,
    })
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
}

/// A column heading of a list, with the sort order that clicking it asks for.
#[derive(Serialize)]
struct ColumnHead {
    label: String,
    /// `asc` or `desc` if the list is ordered by this column.
    active: Option<&'static str>,
    /// The `sort` parameter of the heading's link.
    next: String,
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

/// The name of a row where another model points to it: its display field (by default the
/// first text column), or `#id` if that is empty or there is none.
fn row_label(resource: &dyn Resource, row: &Row) -> String {
    let chosen = resource.options().display_field.as_deref();
    let field = visible_fields(resource).find(|f| match chosen {
        Some(name) => f.name == name,
        None => f.kind == FieldKind::Text && f.reference.is_none(),
    });
    field
        .map(|f| display(row.get(&f.name)))
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

async fn list(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    let query = list_query(&state, resource, &params);
    let page = resource.list(&query).await?;

    let chosen = &resource.options().list_display;
    let columns: Vec<&Field> = if chosen.is_empty() {
        visible_fields(resource).collect()
    } else {
        // Keep the configured order; silently skip names that are hidden or unknown.
        chosen
            .iter()
            .filter_map(|name| visible_fields(resource).find(|f| &f.name == name))
            .collect()
    };
    // For each foreign-key column, the names of the rows this page points to: one query per
    // such column, however many rows are shown.
    let mut names: Vec<Option<RelatedNames<'_>>> = Vec::new();
    for field in &columns {
        names.push(match related(&state, &session, field) {
            Some(target) => {
                let ids = page
                    .rows
                    .iter()
                    .map(|row| display(row.get(&field.name)))
                    .filter(|id| !id.is_empty())
                    .collect();
                Some((target, related_labels(target, ids).await?))
            }
            None => None,
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
                .map(|(f, names)| {
                    let value = display(row.get(&f.name));
                    let named = names
                        .as_ref()
                        .and_then(|(target, labels)| Some((target, labels.get(&value)?)));
                    match named {
                        Some((target, label)) => ListCell {
                            text: truncate(label.clone()),
                            link: Some((target.slug().to_owned(), value)),
                            flag: None,
                        },
                        // Not a foreign key, or one that points at nothing.
                        None => ListCell {
                            flag: (f.kind == FieldKind::Boolean)
                                .then(|| row.get(&f.name).and_then(boolean_value))
                                .flatten(),
                            text: truncate(value),
                            link: None,
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
            notice => notice_text(params.notice.as_deref()),
            rows => Value::from(Serde(&rows)),
            total => page.total,
            page => query.page + 1,
            pages => pages,
            q => query.search.unwrap_or_default(),
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
    current: &str,
) -> Result<(Option<Vec<Choice>>, Option<String>), AdminError> {
    let Some(pk) = pk_name(target) else {
        return Ok((None, None));
    };
    let page = target.list(&ListQuery::first(MAX_CHOICES)).await?;
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
        // `datetime-local` wants `YYYY-MM-DDTHH:MM:SS`; databases usually print a space.
        FieldKind::DateTime => text.replacen(' ', "T", 1).chars().take(19).collect(),
        FieldKind::Json => value
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_default(),
        _ => text,
    }
}

/// Builds the form description for a new (`row == None`) or existing record.
async fn form_fields<'a>(
    state: &AdminState,
    session: &Session,
    resource: &'a dyn Resource,
    row: Option<&Row>,
) -> Result<Vec<FormField<'a>>, AdminError> {
    let creating = row.is_none();
    let readonly_names = &resource.options().readonly_fields;
    let mut fields = Vec::new();
    // A database-generated key has nothing to show before the row exists.
    for f in visible_fields(resource).filter(|f| !(creating && f.auto)) {
        let readonly = !f.editable
            || f.auto
            || (f.primary_key && !creating)
            || readonly_names.contains(&f.name);
        let value = row
            .map(|r| input_value(f, r.get(&f.name)))
            .unwrap_or_default();
        let (choices, hint) = match related(state, session, f) {
            Some(target) => reference_input(target, &value).await?,
            None => (None, None),
        };
        fields.push(FormField {
            name: &f.name,
            label: &f.label,
            kind: f.kind,
            value,
            required: !f.nullable
                && !readonly
                && !matches!(
                    f.kind,
                    FieldKind::Boolean | FieldKind::Text | FieldKind::LongText
                ),
            readonly,
            choices,
            hint,
        });
    }
    Ok(fields)
}

async fn render_form(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    id: Option<&str>,
    row: Option<&Row>,
    error: Option<String>,
) -> Result<Html<String>, AdminError> {
    let fields = form_fields(state, session, resource, row).await?;
    render(
        state,
        "form.html",
        Some(session),
        context! {
            section => resource.slug(),
            model => Value::from(Serde(NavModel::new(resource, session))),
            fields => Value::from(Serde(fields)),
            id => id,
            error => error,
            // A new record can always be saved here: the page itself required `add`.
            can_save => id.is_none() || session.can(resource.slug(), Action::Change),
            can_delete => session.can(resource.slug(), Action::Delete),
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
    input
}

/// Turns submitted text back into a row so a rejected form can be shown again as typed.
fn echo_row(form: &FormData) -> Row {
    form.iter()
        .filter(|(name, _)| name.as_str() != CSRF_FIELD)
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect()
}

async fn new_form(
    State(state): Shared,
    session: Session,
    Path(model): Path<String>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Add)?;
    render_form(&state, &session, resource, None, None, None).await
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
    let mut row = echo_row(form);
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
    Form(form): Form<FormData>,
) -> Result<Response, AdminError> {
    session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Add)?;
    match resource.create(&form_input(resource, &form)).await {
        Ok(_) => {
            Ok(Redirect::to(&format!("{}/{model}?notice=created", state.base)).into_response())
        }
        Err(error) => form_error(&state, &session, resource, None, &form, error).await,
    }
}

async fn edit_form(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
) -> Result<Html<String>, AdminError> {
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::View)?;
    let row = resource
        .get(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("{model} `{id}`")))?;
    render_form(&state, &session, resource, Some(&id), Some(&row), None).await
}

async fn update(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    Form(form): Form<FormData>,
) -> Result<Response, AdminError> {
    session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
    let resource = state.resource(&model)?.as_ref();
    session.require(&model, Action::Change)?;
    match resource
        .update(&id, &form_input(resource, &form), WriteMode::Replace)
        .await
    {
        Ok(()) => Ok(Redirect::to(&format!("{}/{model}?notice=saved", state.base)).into_response()),
        Err(error) => form_error(&state, &session, resource, Some(&id), &form, error).await,
    }
}

async fn delete(
    State(state): Shared,
    session: Session,
    Path((model, id)): Path<(String, String)>,
    Form(form): Form<FormData>,
) -> Result<Redirect, AdminError> {
    session.check_csrf(form.get(CSRF_FIELD).map(String::as_str))?;
    let resource = state.resource(&model)?;
    session.require(&model, Action::Delete)?;
    resource.delete(&id).await?;
    Ok(Redirect::to(&format!(
        "{}/{model}?notice=deleted",
        state.base
    )))
}

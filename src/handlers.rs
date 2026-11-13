//! HTTP handlers. They only translate between HTTP and the `Resource`/auth seams.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
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

#[derive(Serialize)]
struct NavModel<'a> {
    slug: &'a str,
    label: &'a str,
    /// Whether the logged-in account may create records of this model.
    can_add: bool,
}

impl<'a> NavModel<'a> {
    fn new(resource: &'a dyn Resource, session: &Session) -> Self {
        Self {
            slug: resource.slug(),
            label: resource.label(),
            can_add: session.can(resource.slug(), Action::Add),
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
    Form(form): Form<auth::Credentials>,
) -> Result<Response, AdminError> {
    let attempt = auth::login(
        &state.auth,
        &state.users,
        &form.username,
        &form.password,
        form.totp.as_deref(),
    )
    .await;
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

async fn index(State(state): Shared, session: Session) -> Result<Html<String>, AdminError> {
    render(&state, "index.html", Some(&session), context! {})
}

#[derive(Deserialize)]
pub(crate) struct ListParams {
    #[serde(default)]
    pub(crate) page: Option<u64>,
    #[serde(default)]
    pub(crate) per_page: Option<u64>,
    #[serde(default)]
    pub(crate) q: Option<String>,
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
    }
}

#[derive(Serialize)]
struct ListRow {
    id: Option<String>,
    cells: Vec<String>,
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
    let pk = pk_name(resource);
    let rows: Vec<ListRow> = page
        .rows
        .iter()
        .map(|row| ListRow {
            id: pk.map(|name| display(row.get(name))),
            cells: columns
                .iter()
                .map(|f| truncate(display(row.get(&f.name))))
                .collect(),
        })
        .collect();
    let pages = page.total.div_ceil(query.per_page).max(1);

    render(
        &state,
        "list.html",
        Some(&session),
        context! {
            model => Value::from(Serde(NavModel::new(resource, &session))),
            can_change => session.can(&model, Action::Change),
            columns => columns.iter().map(|f| f.label.clone()).collect::<Vec<_>>(),
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
fn form_fields<'a>(resource: &'a dyn Resource, row: Option<&Row>) -> Vec<FormField<'a>> {
    let creating = row.is_none();
    let readonly_names = &resource.options().readonly_fields;
    visible_fields(resource)
        // A database-generated key has nothing to show before the row exists.
        .filter(|f| !(creating && f.auto))
        .map(|f| {
            let readonly = !f.editable
                || f.auto
                || (f.primary_key && !creating)
                || readonly_names.contains(&f.name);
            FormField {
                name: &f.name,
                label: &f.label,
                kind: f.kind,
                value: row
                    .map(|r| input_value(f, r.get(&f.name)))
                    .unwrap_or_default(),
                required: !f.nullable
                    && !readonly
                    && !matches!(
                        f.kind,
                        FieldKind::Boolean | FieldKind::Text | FieldKind::LongText
                    ),
                readonly,
            }
        })
        .collect()
}

fn render_form(
    state: &AdminState,
    session: &Session,
    resource: &dyn Resource,
    id: Option<&str>,
    row: Option<&Row>,
    error: Option<String>,
) -> Result<Html<String>, AdminError> {
    render(
        state,
        "form.html",
        Some(session),
        context! {
            model => Value::from(Serde(NavModel::new(resource, session))),
            fields => Value::from(Serde(form_fields(resource, row))),
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
    render_form(&state, &session, resource, None, None, None)
}

/// Re-renders the form for errors the user can fix; passes everything else up.
fn form_error(
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
    let page = render_form(state, session, resource, id, Some(&row), Some(message))?;
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
        Ok(_) => Ok(Redirect::to(&format!("{}/{model}", state.base)).into_response()),
        Err(error) => form_error(&state, &session, resource, None, &form, error),
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
    render_form(&state, &session, resource, Some(&id), Some(&row), None)
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
        Ok(()) => Ok(Redirect::to(&format!("{}/{model}", state.base)).into_response()),
        Err(error) => form_error(&state, &session, resource, Some(&id), &form, error),
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
    Ok(Redirect::to(&format!("{}/{model}", state.base)))
}

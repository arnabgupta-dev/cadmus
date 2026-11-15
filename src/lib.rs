//! Cadmus: a batteries-included web framework for axum and SeaORM.
//!
//! Register SeaORM entities as models and the crate serves admin pages (list, search,
//! create, edit, delete) and REST endpoints for each one, behind accounts with permissions,
//! and keeps their tables up to date with migrations. A project is configured with one
//! [`Settings`] value (see [`settings`]) and [`App::from_settings`]; the pieces below can also
//! be put together by hand. Look and feel
//! ([`UiSettings`]) and authentication ([`AuthConfig`], [`EnvUserStore`]) can be configured
//! entirely from environment variables.
//!
//! ```ignore
//! let admin = Admin::builder()
//!     .settings(UiSettings::from_env()?)
//!     .auth(AuthConfig::from_env()?, EnvUserStore::from_env()?)
//!     .register(SeaOrmResource::<post::Entity>::new(db.clone()))
//!     .build()?;
//! let app = axum::Router::new().merge(admin.router());
//! ```

mod access;
mod account_api;
mod accounts;
mod api;
mod app;
mod auth;
mod docs;
mod error;
mod handlers;
mod home;
mod html;
mod limit;
mod login;
mod login_api;
mod mail;
pub mod migrations;
mod oauth;
mod resource;
mod seaorm;
mod server;
mod sessions;
pub mod settings;
mod storage;
mod uploads;
mod users;

use std::sync::Arc;

pub use access::{Access, Action};
pub use app::App;
pub use auth::{
    AdminUser, ApiKeyOwner, AuthConfig, CurrentUser, EnvUserStore, UserStore, hash_password,
    hash_password_async, totp_uri, verify_totp,
};
pub use docs::DocsConfig;
pub use error::{AdminError, ApiError, ApiResult};
pub use home::HomeConfig;
pub use html::clean_html;
pub use login_api::{LoginOutcome, Logins, TokenPair};
pub use mail::{Email, LogMailer, Mailer, SmtpMailer};
pub use resource::{
    Field, FieldKind, Filter, FilterOp, Input, LinkField, ListQuery, ModelOptions, Page, Reference,
    Resource, Row, WriteMode,
};
pub use seaorm::{SeaOrmResource, invalid};
pub use settings::{NavLink, Settings, Theme, UiSettings};
pub use storage::{Sink, Storage};
pub use uploads::{Progress, Uploads, file, session};
pub use users::{
    DbUserStore, change_password, create_superuser, disable_totp, enable_totp, group, identity,
    new_api_key, new_recovery_codes, promote, set_email, user,
};

/// Template names and their built-in sources.
const TEMPLATES: [(&str, &str); 16] = [
    ("base.html", include_str!("../templates/base.html")),
    ("login.html", include_str!("../templates/login.html")),
    ("index.html", include_str!("../templates/index.html")),
    ("list.html", include_str!("../templates/list.html")),
    ("form.html", include_str!("../templates/form.html")),
    ("error.html", include_str!("../templates/error.html")),
    ("home.html", include_str!("../templates/home.html")),
    (
        "permissions.html",
        include_str!("../templates/permissions.html"),
    ),
    (
        "accounts_list.html",
        include_str!("../templates/accounts_list.html"),
    ),
    ("api_keys.html", include_str!("../templates/api_keys.html")),
    (
        "user_form.html",
        include_str!("../templates/user_form.html"),
    ),
    ("password.html", include_str!("../templates/password.html")),
    (
        "group_form.html",
        include_str!("../templates/group_form.html"),
    ),
    (
        "two_factor.html",
        include_str!("../templates/two_factor.html"),
    ),
    (
        "docs_swagger.html",
        include_str!("../templates/docs_swagger.html"),
    ),
    (
        "docs_redoc.html",
        include_str!("../templates/docs_redoc.html"),
    ),
];

/// URL segments the admin uses itself; a model cannot take these as its slug.
const RESERVED_SLUGS: [&str; 11] = [
    "api",
    "auth",
    "docs",
    "files",
    "login",
    "logout",
    "openapi.json",
    "password",
    "redoc",
    "static",
    "uploads",
];

pub(crate) struct AdminState {
    pub(crate) base: String,
    pub(crate) api_base: String,
    pub(crate) settings: UiSettings,
    pub(crate) auth: AuthConfig,
    pub(crate) limiter: limit::LoginLimiter,
    pub(crate) users: Arc<dyn UserStore>,
    pub(crate) resources: Vec<Arc<dyn Resource>>,
    pub(crate) uploads: Option<Uploads>,
    /// Where the built-in account tables live; `None` when accounts come from elsewhere.
    pub(crate) accounts: Option<sea_orm::DatabaseConnection>,
    /// The API documentation, unless it is turned off.
    pub(crate) docs: Option<docs::Docs>,
    pub(crate) templates: minijinja::Environment<'static>,
    /// Sends login links and password resets.
    pub(crate) mailer: Arc<dyn mail::Mailer>,
}

impl AdminState {
    pub(crate) fn resource(&self, slug: &str) -> Result<&Arc<dyn Resource>, AdminError> {
        self.resources
            .iter()
            .find(|r| r.slug() == slug)
            .ok_or_else(|| AdminError::NotFound(format!("model `{slug}`")))
    }
}

/// A configured admin panel. Create with [`Admin::builder`], mount with [`Admin::router`].
#[derive(Clone)]
pub struct Admin {
    pub(crate) state: Arc<AdminState>,
}

impl Admin {
    /// Starts building an admin.
    #[must_use]
    pub fn builder() -> AdminBuilder {
        AdminBuilder::default()
    }

    /// Routes for the panel, already prefixed with the base path; `merge` it into your app.
    ///
    /// Errors from these routes are JSON under the API path and pages under the admin path.
    /// To get the same for routes of your own, and to turn panics into `500`s, pass the
    /// merged router through [`Admin::handle_errors`].
    pub fn router(&self) -> axum::Router {
        self.handle_errors(self.routes())
    }

    /// Lets the handlers in `router`, routes of your own, take [`CurrentUser`] to find out
    /// who sent a request. [`App::routes`] does this for you.
    pub fn identify(&self, router: axum::Router) -> axum::Router {
        router.layer(axum::Extension(auth::Identify(Arc::clone(&self.state))))
    }

    /// Adds the central error handling to `router`: panics become `500`s, and error
    /// responses (including unknown URLs and wrong methods) are JSON under the API path and
    /// pages under the admin path. Responses outside both paths are left alone.
    pub fn handle_errors(&self, router: axum::Router) -> axum::Router {
        router.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&self.state),
            error::middleware,
        ))
    }

    pub(crate) fn routes(&self) -> axum::Router {
        let router = self.page_routes();
        match self.upload_routes() {
            Some(uploads) => router.merge(uploads),
            None => router,
        }
    }

    /// The OpenAPI description of the REST API, as served at `openapi.json`; `None` if the
    /// documentation is turned off.
    #[must_use]
    pub fn openapi(&self) -> Option<&serde_json::Value> {
        self.state.docs.as_ref().map(docs::Docs::document)
    }

    /// Everything except file transfers: requests that should be answered quickly.
    pub(crate) fn page_routes(&self) -> axum::Router {
        let mut router =
            handlers::router(Arc::clone(&self.state)).merge(api::router(Arc::clone(&self.state)));
        if self.state.docs.is_some() {
            router = router.merge(docs::router(Arc::clone(&self.state)));
        }
        if !self.state.auth.methods.providers.is_empty() {
            router = router.merge(oauth::router(Arc::clone(&self.state)));
        }
        router = router.merge(login::router(Arc::clone(&self.state)));
        if self.state.accounts.is_some() {
            router.merge(accounts::router(Arc::clone(&self.state)))
        } else {
            router
        }
    }

    /// File uploads and downloads, which take as long as the files are big.
    pub(crate) fn upload_routes(&self) -> Option<axum::Router> {
        let service = self.state.uploads.as_ref()?;
        Some(uploads::router(Arc::clone(&self.state), service))
    }
}

/// Builder for [`Admin`].
#[derive(Default)]
pub struct AdminBuilder {
    base: Option<String>,
    api_base: Option<String>,
    uploads: Option<Uploads>,
    accounts: Option<sea_orm::DatabaseConnection>,
    docs: DocsConfig,
    settings: UiSettings,
    auth: Option<(AuthConfig, Arc<dyn UserStore>)>,
    resources: Vec<Arc<dyn Resource>>,
    mailer: Option<Arc<dyn Mailer>>,
}

impl AdminBuilder {
    /// URL prefix for the panel (default `/admin`).
    #[must_use]
    pub fn base_path(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into());
        self
    }

    /// Enables file uploads and downloads under the API base path.
    #[must_use]
    pub fn uploads(mut self, uploads: Uploads) -> Self {
        self.uploads = Some(uploads);
        self
    }

    /// Adds the Users and Groups pages, for superusers to manage the built-in accounts in
    /// `db`. Use it together with [`DbUserStore`] over the same database.
    #[must_use]
    pub fn accounts(mut self, db: sea_orm::DatabaseConnection) -> Self {
        self.accounts = Some(db);
        self
    }

    /// Sends login links and password resets (by default they are written to the log).
    #[must_use]
    pub fn mailer(mut self, mailer: impl Mailer + 'static) -> Self {
        self.mailer = Some(Arc::new(mailer));
        self
    }

    pub(crate) fn mailer_shared(mut self, mailer: Option<Arc<dyn Mailer>>) -> Self {
        self.mailer = mailer;
        self
    }

    /// Settings for the API documentation (on and open to everyone by default).
    #[must_use]
    pub fn docs(mut self, docs: DocsConfig) -> Self {
        self.docs = docs;
        self
    }

    /// URL prefix for the REST API (default `/api`).
    #[must_use]
    pub fn api_base_path(mut self, base: impl Into<String>) -> Self {
        self.api_base = Some(base.into());
        self
    }

    pub(crate) fn register_shared(mut self, resource: Arc<dyn Resource>) -> Self {
        self.resources.push(resource);
        self
    }

    pub(crate) fn auth_shared(mut self, config: AuthConfig, users: Arc<dyn UserStore>) -> Self {
        self.auth = Some((config, users));
        self
    }

    /// Look-and-feel settings.
    #[must_use]
    pub fn settings(mut self, settings: UiSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Authentication settings and the account store. Required: there is no unauthenticated mode.
    #[must_use]
    pub fn auth(mut self, config: AuthConfig, users: impl UserStore + 'static) -> Self {
        self.auth = Some((config, Arc::new(users)));
        self
    }

    /// Adds a model to the panel.
    #[must_use]
    pub fn register(mut self, resource: impl Resource + 'static) -> Self {
        self.resources.push(Arc::new(resource));
        self
    }

    /// Validates the configuration and compiles the templates.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if auth is missing, the base path is malformed, a model
    /// slug is reserved or duplicated, or a template override cannot be read; and
    /// [`AdminError::Template`] if a template does not parse.
    pub fn build(self) -> Result<Admin, AdminError> {
        let (auth, users) = self.auth.ok_or_else(|| {
            AdminError::Config(
                "call .auth(...): the admin cannot be served without authentication".into(),
            )
        })?;

        if (!auth.methods.providers.is_empty() || auth.methods.magic_link)
            && self.accounts.is_none()
        {
            return Err(AdminError::Config(
                "auth.methods: logging in through other sites or by e-mail uses the built-in \
                 accounts, which a custom UserStore replaces"
                    .into(),
            ));
        }

        let base = base_path(self.base, "/admin")?;
        let api_base = base_path(self.api_base, "/api")?;
        if base == api_base {
            return Err(AdminError::Config(
                "the admin and API base paths must differ".into(),
            ));
        }

        for (i, resource) in self.resources.iter().enumerate() {
            let slug = resource.slug();
            if RESERVED_SLUGS.contains(&slug) {
                return Err(AdminError::Config(format!(
                    "model slug `{slug}` is reserved"
                )));
            }
            if self.resources[..i].iter().any(|r| r.slug() == slug) {
                return Err(AdminError::Config(format!(
                    "model slug `{slug}` is registered twice"
                )));
            }
            check_actions(resource.as_ref())?;
            check_file_fields(resource.as_ref(), self.uploads.is_some())?;
            check_rich_text(resource.as_ref())?;
        }

        let mut templates = minijinja::Environment::new();
        for (name, builtin) in TEMPLATES {
            let source = match &self.settings.template_dir {
                Some(dir) if dir.join(name).is_file() => std::fs::read_to_string(dir.join(name))
                    .map_err(|e| {
                        AdminError::Config(format!("cannot read template override {name}: {e}"))
                    })?,
                _ => builtin.to_owned(),
            };
            templates.add_template_owned(name, source)?;
        }

        let docs = self.docs.is_enabled().then(|| {
            docs::Docs::new(
                self.docs,
                &api_base,
                &self.settings,
                &self.resources,
                self.uploads.is_some(),
                auth.api_keys,
            )
        });

        Ok(Admin {
            state: Arc::new(AdminState {
                base,
                api_base,
                settings: self.settings,
                limiter: limit::LoginLimiter::new(auth.login_max_attempts, auth.login_lockout_secs),
                auth,
                users,
                resources: self.resources,
                uploads: self.uploads,
                accounts: self.accounts,
                docs,
                templates,
                mailer: self.mailer.unwrap_or_else(|| Arc::new(mail::LogMailer)),
            }),
        })
    }
}

/// Checks a model's bulk actions: distinct names, and columns that exist and can be edited.
fn check_actions(resource: &dyn Resource) -> Result<(), AdminError> {
    let slug = resource.slug();
    let actions = &resource.options().actions;
    for (i, action) in actions.iter().enumerate() {
        let error = |message: String| {
            AdminError::Config(format!(
                "model `{slug}`, action `{}`: {message}",
                action.name
            ))
        };
        let valid = !action.name.is_empty()
            && action
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        if !valid || action.name == "delete" {
            return Err(error(
                "names use letters, digits, `_` and `-`, and `delete` is built in".into(),
            ));
        }
        if actions[..i].iter().any(|other| other.name == action.name) {
            return Err(error("listed twice".into()));
        }
        if let resource::ActionKind::Set { field, .. } = &action.kind {
            let options = resource.options();
            let editable = resource.fields().iter().any(|f| {
                &f.name == field
                    && f.editable
                    && !f.primary_key
                    && !options.readonly_fields.contains(field)
                    && !options.hidden_fields.contains(field)
            });
            if !editable {
                return Err(error(format!(
                    "`{field}` is not a column the admin can edit"
                )));
            }
        }
    }
    Ok(())
}

/// Checks a model's file fields: editable integer columns, and uploads turned on.
fn check_file_fields(resource: &dyn Resource, uploads: bool) -> Result<(), AdminError> {
    let options = resource.options();
    for name in &options.file_fields {
        let error = |message: &str| {
            AdminError::Config(format!(
                "model `{}`, file field `{name}`: {message}",
                resource.slug()
            ))
        };
        let usable = resource.fields().iter().any(|f| {
            &f.name == name
                && f.kind == FieldKind::Integer
                && f.editable
                && !f.primary_key
                && !options.hidden_fields.contains(name)
        });
        if !usable {
            return Err(error("must be an integer column the admin can edit"));
        }
        if !uploads {
            return Err(error("file uploads are off; set uploads.storage_url"));
        }
    }
    Ok(())
}

/// Checks that a model's rich-text fields are text columns, which have become HTML.
fn check_rich_text(resource: &dyn Resource) -> Result<(), AdminError> {
    for name in &resource.options().rich_text {
        let usable = resource
            .fields()
            .iter()
            .any(|f| &f.name == name && f.kind == FieldKind::Html);
        if !usable {
            return Err(AdminError::Config(format!(
                "model `{}`, rich-text field `{name}`: must be a text column",
                resource.slug()
            )));
        }
    }
    Ok(())
}

/// Normalises and validates a URL prefix.
fn base_path(configured: Option<String>, default: &str) -> Result<String, AdminError> {
    let base = configured.unwrap_or_else(|| default.to_owned());
    let base = base.trim_end_matches('/').to_owned();
    let valid = base.starts_with('/')
        && base.len() > 1
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'));
    if !valid {
        return Err(AdminError::Config(format!(
            "base path must look like {default} (letters, digits, `-`, `_`, `/`), got `{base}`"
        )));
    }
    Ok(base)
}

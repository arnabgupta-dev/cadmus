//! The framework entry point: one [`App`] owns the database, the registered models, the admin,
//! the REST API and the management commands.

use std::io::{BufRead, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, middleware};
use clap::{Parser, Subcommand};
use sea_orm::schema::SchemaBuilder;
use sea_orm::{
    ActiveModelBehavior, ConnectOptions, Database, DatabaseConnection, EntityTrait, IntoActiveModel,
};

use crate::auth::totp_uri;
use crate::auth::{AuthConfig, UserStore};
use crate::docs::DocsConfig;
use crate::error::AdminError;
use crate::home::{self, Home, HomeConfig};
use crate::migrations::runner::{self, AppMigrations, LegacySync};
use crate::migrations::{
    Answers, Detect, EntityTable, Migration, MigrationEvent, NewMigration, Operation, Questions,
    State, builtin, describe, diff, render_list, render_migration, table_of,
};
use crate::resource::{ModelOptions, Resource};
use crate::seaorm::SeaOrmResource;
use crate::settings::{Settings, UiSettings};
use crate::storage::Storage;
use crate::uploads::Uploads;
use crate::users::{
    DbUserStore, change_password, create_superuser, disable_totp, enable_totp, promote, set_email,
};
use crate::{Admin, base_path, server};

type SchemaStep = Box<dyn Fn(SchemaBuilder) -> SchemaBuilder + Send + Sync>;
type TableStep = Box<dyn Fn() -> Result<EntityTable, String> + Send + Sync>;

/// An application: register models, add your own routes, then call [`App::run`].
///
/// ```ignore
/// #[tokio::main]
/// async fn main() -> Result<(), AdminError> {
///     App::from_env()
///         .await?
///         .model::<post::Entity>(ModelOptions::new().label("Posts"))
///         .routes(my_routes())
///         .run()
///         .await
/// }
/// ```
///
/// Each registered model gets its table (created or altered by `migrate`), its admin pages and
/// its REST endpoints.
pub struct App {
    db: DatabaseConnection,
    auth: AuthConfig,
    settings: UiSettings,
    bind: String,
    admin_base: Option<String>,
    api_base: Option<String>,
    users: Arc<dyn UserStore>,
    /// Whether `users` is still the built-in store, which the Users and Groups pages edit.
    builtin_users: bool,
    resources: Vec<Arc<dyn Resource>>,
    schema: Vec<SchemaStep>,
    /// The registered models' tables, for `makemigrations`.
    tables: Vec<TableStep>,
    /// The project's migrations; `None` keeps the tables in sync with the models instead.
    migrations: Option<Vec<Migration>>,
    uploads: Option<Uploads>,
    routes: Router,
    request_timeout: Option<Duration>,
    cors_origins: Vec<String>,
    docs: DocsConfig,
    home: HomeConfig,
    /// `None` reads `LOG_LEVEL` when the server starts.
    log_level: Option<tracing::Level>,
    /// `None` writes messages to the log.
    mailer: Option<Arc<dyn crate::mail::Mailer>>,
}

impl App {
    /// Builds an app over an existing connection, with accounts in the built-in `auth_user` table.
    #[must_use]
    pub fn new(db: DatabaseConnection, auth: AuthConfig) -> Self {
        Self {
            users: Arc::new(DbUserStore::new(db.clone())),
            builtin_users: true,
            db,
            auth,
            settings: UiSettings::default(),
            bind: "127.0.0.1:3000".to_owned(),
            admin_base: None,
            api_base: None,
            resources: Vec::new(),
            schema: Vec::new(),
            tables: Vec::new(),
            migrations: None,
            uploads: None,
            routes: Router::new(),
            request_timeout: Some(Duration::from_secs(30)),
            cors_origins: Vec::new(),
            docs: DocsConfig::new(),
            home: HomeConfig::new(),
            log_level: None,
            mailer: None,
        }
    }

    /// Builds an app from the project's [`Settings`], connecting to its database. This is how
    /// a project made by `cadmus new` starts:
    ///
    /// ```ignore
    /// let app = App::from_settings(settings::settings()?).await?;
    /// ```
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] naming the first invalid setting, and [`AdminError::Db`]
    /// if the database cannot be reached.
    pub async fn from_settings(settings: Settings) -> Result<Self, AdminError> {
        // Check everything that needs no connection first, so a typo is reported before a
        // slow or failing connection attempt.
        check_settings(&settings)?;
        let database = &settings.database;
        let mut options = ConnectOptions::new(database.url.clone());
        if let Some(max) = database.max_connections {
            options.max_connections(max);
        }
        if let Some(min) = database.min_connections {
            options.min_connections(min);
        }
        if let Some(wait) = database.acquire_timeout {
            options.acquire_timeout(wait);
        }
        let db = Database::connect(options).await?;
        Self::with_settings(db, settings)
    }

    /// Same as [`App::from_settings`] over a connection you opened yourself;
    /// `settings.database` is then ignored.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] naming the first invalid setting.
    pub fn with_settings(db: DatabaseConnection, settings: Settings) -> Result<Self, AdminError> {
        if settings.secret_key.is_empty() {
            return Err(AdminError::Config(
                "secret_key is empty: set it in settings.rs, e.g. `secret_key: env(\"SECRET_KEY\")?`"
                    .into(),
            ));
        }
        let Settings {
            secret_key,
            database: _,
            server,
            auth,
            admin,
            api,
            docs,
            home,
            uploads,
            mail,
            log_level,
        } = settings;

        let mut auth_config = AuthConfig::new(secret_key)
            .map_err(|e| setting_error("secret_key", e))?
            .token_ttl_secs(auth.token_lifetime.as_secs().max(1))
            .totp_required(auth.totp_required)
            .cookie_secure(auth.cookie_secure)
            .login_limit(auth.login_max_attempts, auth.login_lockout.as_secs().max(1))
            .api_keys(auth.api_keys);
        auth_config = auth_config.with_methods(crate::oauth::Methods::new(
            auth.methods,
            auth.signup,
            auth.signup_groups,
            server.public_url.as_deref(),
        )?);
        if let Some(header) = server.client_ip_header.as_deref().filter(|h| !h.is_empty()) {
            auth_config = auth_config
                .client_ip_header(header)
                .map_err(|e| setting_error("server.client_ip_header", e))?;
        }

        let ui = admin.ui()?;
        let mut app = Self::new(db, auth_config).settings(ui);
        app.bind = server.bind;
        app.admin_base = Some(admin.path);
        app.api_base = Some(api.path);
        app.request_timeout = server.request_timeout.filter(|limit| !limit.is_zero());
        app = app
            .cors_origins(&server.cors_origins)
            .map_err(|e| setting_error("server.cors_origins", e))?;

        let mut docs_config = DocsConfig::new()
            .enabled(docs.enabled)
            .version(docs.version);
        match (docs.username, docs.password) {
            (Some(username), Some(password)) if !username.is_empty() && !password.is_empty() => {
                docs_config = docs_config.protect(username, password);
            }
            (None, None) => {}
            _ => {
                return Err(AdminError::Config(
                    "docs.username and docs.password must be set together".into(),
                ));
            }
        }
        if let Some(title) = docs.title {
            docs_config = docs_config.title(title);
        }
        if let Some(description) = docs.description {
            docs_config = docs_config.description(description);
        }
        if docs.swagger_js_url.is_some()
            || docs.swagger_css_url.is_some()
            || docs.redoc_js_url.is_some()
        {
            docs_config = docs_config.assets(
                docs.swagger_js_url
                    .unwrap_or_else(|| crate::docs::SWAGGER_JS.to_owned()),
                docs.swagger_css_url
                    .unwrap_or_else(|| crate::docs::SWAGGER_CSS.to_owned()),
                docs.redoc_js_url
                    .unwrap_or_else(|| crate::docs::REDOC_JS.to_owned()),
            );
        }
        app.docs = docs_config;

        let mut home_config = HomeConfig::new().enabled(home.enabled);
        if let Some(title) = home.title {
            home_config = home_config.title(title);
        }
        if let Some(tagline) = home.tagline {
            home_config = home_config.tagline(tagline);
        }
        app.home = home_config;

        if let Some(url) = uploads.storage_url.filter(|url| !url.is_empty()) {
            let storage = Storage::from_url(&url, std::env::vars())
                .map_err(|e| setting_error("uploads.storage_url", e))?;
            app.uploads = Some(
                Uploads::new(app.db.clone(), storage)
                    .max_file_bytes(uploads.max_file_bytes)
                    .max_chunk_bytes(
                        usize::try_from(uploads.max_chunk_bytes).unwrap_or(usize::MAX),
                    ),
            );
        }
        if let Some(url) = mail.url.filter(|url| !url.trim().is_empty()) {
            let from = mail
                .from
                .filter(|from| !from.trim().is_empty())
                .ok_or_else(|| {
                    AdminError::Config(
                        "mail.from is not set: who should messages come from, e.g. \
                     `Shop <noreply@shop.example.com>`?"
                            .into(),
                    )
                })?;
            app.mailer = Some(Arc::new(crate::mail::SmtpMailer::new(&url, &from)?));
        }
        app.log_level = Some(log_level);
        Ok(app)
    }

    /// Builds an app from environment variables alone; new projects use
    /// [`App::from_settings`] instead. A `.env` file in the working directory is loaded
    /// first, without overriding variables that are already set.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `DATABASE_URL` | **Required.** e.g. `sqlite://db.sqlite3?mode=rwc`, `postgres://…` |
    /// | `APP_BIND` | Listen address (default `127.0.0.1:3000`) |
    /// | `ADMIN_BASE_PATH` | Admin URL prefix (default `/admin`) |
    /// | `API_BASE_PATH` | REST API URL prefix (default `/api`) |
    /// | `DB_MAX_CONNECTIONS`, `DB_MIN_CONNECTIONS` | Connection pool size |
    /// | `DB_ACQUIRE_TIMEOUT_SECS` | How long a request waits for a pooled connection |
    /// | `STORAGE_URL` | Enables file uploads; see [`Storage::from_url`] |
    /// | `UPLOAD_MAX_FILE_BYTES`, `UPLOAD_MAX_CHUNK_BYTES` | Upload limits |
    /// | `REQUEST_TIMEOUT_SECS` | Longest a request may take; `0` for no limit (default 30) |
    /// | `CORS_ALLOWED_ORIGINS` | Origins whose pages may call the app from a browser, comma-separated, or `*` (default: none) |
    ///
    /// plus everything read by [`AuthConfig::from_env`], [`UiSettings::from_env`] and
    /// [`DocsConfig::from_env`].
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] for missing or invalid variables and [`AdminError::Db`] if
    /// the database cannot be reached.
    pub async fn from_env() -> Result<Self, AdminError> {
        // A missing `.env` is normal in production, where the platform sets the variables.
        let _ = dotenvy::dotenv();
        let url = std::env::var("DATABASE_URL")
            .map_err(|_| AdminError::Config("DATABASE_URL is not set".into()))?;
        check_database_url("DATABASE_URL", &url)?;
        let db = Database::connect(pool_options(url)?).await?;
        let uploads = Uploads::from_env(db.clone())?;
        let mut app = Self::new(db, AuthConfig::from_env()?).settings(UiSettings::from_env()?);
        if let Ok(bind) = std::env::var("APP_BIND") {
            app.bind = bind;
        }
        app.admin_base = std::env::var("ADMIN_BASE_PATH").ok();
        app.api_base = std::env::var("API_BASE_PATH").ok();
        app.uploads = uploads;
        if let Ok(text) = std::env::var("REQUEST_TIMEOUT_SECS") {
            let secs = text.parse::<u64>().map_err(|_| {
                AdminError::Config("REQUEST_TIMEOUT_SECS must be a non-negative integer".into())
            })?;
            app = app.request_timeout((secs > 0).then(|| Duration::from_secs(secs)));
        }
        if let Ok(text) = std::env::var("CORS_ALLOWED_ORIGINS") {
            app.cors_origins = server::parse_origins(&text)?;
        }
        app.docs = DocsConfig::from_env()?;
        app.home = HomeConfig::from_env()?;
        Ok(app)
    }

    /// The database connection, for your own handlers.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Look-and-feel settings for the admin.
    #[must_use]
    pub fn settings(mut self, settings: UiSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Listen address for `serve`.
    #[must_use]
    pub fn bind(mut self, address: impl Into<String>) -> Self {
        self.bind = address.into();
        self
    }

    /// Enables file uploads on `storage` with default limits.
    #[must_use]
    pub fn storage(mut self, storage: Storage) -> Self {
        self.uploads = Some(Uploads::new(self.db.clone(), storage));
        self
    }

    /// Enables file uploads with a fully configured service.
    #[must_use]
    pub fn uploads(mut self, uploads: Uploads) -> Self {
        self.uploads = Some(uploads);
        self
    }

    /// Replaces the built-in account store. The Users and Groups pages are then left out,
    /// because they edit the built-in tables.
    #[must_use]
    pub fn users(mut self, users: impl UserStore + 'static) -> Self {
        self.users = Arc::new(users);
        self.builtin_users = false;
        self
    }

    /// Sends login links and password resets through `mailer`, e.g. your own
    /// [`crate::Mailer`] over an e-mail service's HTTP API. Without one, `mail.url` decides;
    /// with neither, messages are written to the log.
    #[must_use]
    pub fn mailer(mut self, mailer: impl crate::mail::Mailer + 'static) -> Self {
        self.mailer = Some(Arc::new(mailer));
        self
    }

    /// Settings for the public page at `/`, which shows whether the service is up. On by
    /// default; it also sends people who open an API URL in a browser there.
    ///
    /// Your own `/` route takes precedence over it. If your routes set a fallback with
    /// `Router::fallback`, turn the home page off, since both answer unmatched requests.
    #[must_use]
    pub fn home(mut self, home: HomeConfig) -> Self {
        self.home = home;
        self
    }

    /// Settings for the API documentation served under the API path at `docs`, `redoc` and
    /// `openapi.json`. By default it is on and open to everyone.
    #[must_use]
    pub fn docs(mut self, docs: DocsConfig) -> Self {
        self.docs = docs;
        self
    }

    /// Sets how long a request may take before it is answered with `503` and abandoned;
    /// `None` for no limit. The default is 30 seconds.
    ///
    /// It applies to the admin, the API and your own routes, but not to file uploads and
    /// downloads, which take as long as the files are big. Only the time until the response
    /// starts is counted, so streamed responses are not cut short.
    #[must_use]
    pub fn request_timeout(mut self, limit: Option<Duration>) -> Self {
        self.request_timeout = limit;
        self
    }

    /// Lets pages served from these origins (e.g. `https://app.example.com`) call the app
    /// from a browser; `*` allows every origin. By default no cross-origin access is allowed.
    ///
    /// Browser clients send the bearer token; cookies are never accepted across origins.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if an entry is not an origin.
    pub fn cors_origins<I, S>(mut self, origins: I) -> Result<Self, AdminError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut all = Vec::new();
        for origin in origins {
            all.extend(server::parse_origins(origin.as_ref())?);
        }
        self.cors_origins = all;
        Ok(self)
    }

    /// Registers a SeaORM entity: its table is managed by `migrate`, and it appears in the
    /// admin and the REST API.
    #[must_use]
    pub fn model<E>(mut self, options: ModelOptions) -> Self
    where
        E: EntityTrait,
        E::Model: IntoActiveModel<E::ActiveModel>,
        E::ActiveModel: ActiveModelBehavior + Send,
    {
        // Link tables of many-to-many fields come after the model, as they point to it.
        let links: Vec<_> = options
            .many_to_many
            .iter()
            .map(|m| m.link.clone())
            .collect();
        let resource = SeaOrmResource::<E>::new(self.db.clone()).options(options);
        self.resources.push(Arc::new(resource));
        self.schema
            .push(Box::new(|builder| builder.register(E::default())));
        self.tables.push(Box::new(table_of::<E>));
        for link in links {
            let schema_link = link.clone();
            self.schema
                .push(Box::new(move |builder| schema_link.register(builder)));
            self.tables.push(Box::new(move || link.table()));
        }
        self
    }

    /// Keeps the project's tables with versioned migrations: `migrate` applies these (written
    /// by `makemigrations` into `src/migrations/`) instead of syncing tables to the models.
    ///
    /// ```ignore
    /// mod migrations;
    ///
    /// App::from_settings(settings).await?
    ///     .migrations(migrations::all())
    ///     .model::<post::Entity>(ModelOptions::new())
    /// ```
    #[must_use]
    pub fn migrations(mut self, migrations: Vec<Migration>) -> Self {
        self.migrations = Some(migrations);
        self
    }

    /// Registers a resource that is not a SeaORM entity (admin and API only; no table).
    #[must_use]
    pub fn resource(mut self, resource: impl Resource + 'static) -> Self {
        self.resources.push(Arc::new(resource));
        self
    }

    /// Adds your own axum routes next to the generated ones.
    #[must_use]
    pub fn routes(mut self, routes: Router) -> Self {
        self.routes = self.routes.merge(routes);
        self
    }

    /// Brings the database up to date: applies the built-in tables' migrations, then the
    /// project's (see [`App::migrations`]). A project without migrations has its tables
    /// created or extended to match the models instead, as with SeaORM's schema sync.
    ///
    /// # Errors
    /// Returns [`AdminError::Migration`] if a migration fails (it is rolled back where the
    /// database allows), and [`AdminError::Db`] if the database cannot be reached.
    pub async fn migrate(&self) -> Result<(), AdminError> {
        for event in self.migrate_to(None, false).await? {
            if matches!(event, MigrationEvent::Unmigrated(_)) {
                tracing::warn!("{event}");
            } else {
                tracing::info!("{event}");
            }
        }
        Ok(())
    }

    /// Like [`App::migrate`], but moves the project to `target`: a migration name or its
    /// number (`0002`), whose later migrations are rolled back, or `zero` to roll back all.
    /// With `fake`, the project's migrations are recorded or forgotten without running them.
    ///
    /// # Errors
    /// As [`App::migrate`]; also [`AdminError::Config`] for an unknown target.
    pub async fn migrate_to(
        &self,
        target: Option<&str>,
        fake: bool,
    ) -> Result<Vec<MigrationEvent>, AdminError> {
        let mut events = Vec::new();
        runner::ensure_history(&self.db).await?;
        for app in self.builtin_apps() {
            let migrations = app.migrations.len();
            runner::forward(&self.db, &app, migrations, false, &mut events).await?;
        }
        let Some(list) = &self.migrations else {
            if target.is_some() || fake {
                return Err(AdminError::Config(
                    "the project has no migrations; run makemigrations first".into(),
                ));
            }
            if !self.schema.is_empty() {
                let mut builder = self.db.get_schema_builder();
                for step in &self.schema {
                    builder = step(builder);
                }
                builder.sync(&self.db).await?;
                events.push(MigrationEvent::Synced);
            }
            return Ok(events);
        };
        check_names(list)?;
        let keep = match target {
            None => list.len(),
            Some(target) => find_target(list, target)?,
        };
        let app = AppMigrations {
            label: "app",
            migrations: list,
            legacy: None,
        };
        runner::backward(&self.db, &app, keep, fake, &mut events).await?;
        runner::forward(&self.db, &app, keep, fake, &mut events).await?;
        if keep == list.len() {
            let changes = self.pending_changes()?;
            if !changes.is_empty() {
                events.push(MigrationEvent::Unmigrated(changes));
            }
        }
        Ok(events)
    }

    /// The built-in apps with migrations: accounts always, uploads when configured.
    fn builtin_apps(&self) -> Vec<AppMigrations<'static>> {
        let leak =
            |list: Vec<Migration>| -> &'static [Migration] { Box::leak(list.into_boxed_slice()) };
        let db = self.db.clone();
        let auth_sync: LegacySync<'static> = Box::new(move || {
            let db = db.clone();
            Box::pin(async move {
                crate::users::register_schema(db.get_schema_builder())
                    .sync(&db)
                    .await
            })
        });
        let mut apps = vec![AppMigrations {
            label: "auth",
            migrations: leak(builtin::auth()),
            legacy: Some(("auth_user", auth_sync)),
        }];
        if self.uploads.is_some() {
            let db = self.db.clone();
            let uploads_sync: LegacySync<'static> = Box::new(move || {
                let db = db.clone();
                Box::pin(async move {
                    Uploads::register_schema(db.get_schema_builder())
                        .sync(&db)
                        .await
                })
            });
            apps.push(AppMigrations {
                label: "uploads",
                migrations: leak(builtin::uploads()),
                legacy: Some(("storage_file", uploads_sync)),
            });
        }
        apps
    }

    /// The registered models' tables, leaving out the built-in ones.
    fn model_tables(&self) -> Result<Vec<EntityTable>, AdminError> {
        let mut builtin_names: Vec<String> = Vec::new();
        for tables in [builtin::auth_tables(), builtin::upload_tables()] {
            builtin_names.extend(
                tables
                    .map_err(AdminError::Migration)?
                    .into_iter()
                    .map(|t| t.table.name),
            );
        }
        let mut tables: Vec<EntityTable> = Vec::new();
        for step in &self.tables {
            let table = step().map_err(AdminError::Migration)?;
            if !builtin_names.contains(&table.table.name)
                && !tables.iter().any(|t| t.table.name == table.table.name)
            {
                tables.push(table);
            }
        }
        Ok(tables)
    }

    fn model_changes(&self, questions: &mut dyn Questions) -> Result<Vec<Operation>, AdminError> {
        let state = State::after(self.migrations.as_deref().unwrap_or_default())
            .map_err(AdminError::Migration)?;
        diff(&state, &self.model_tables()?, questions).map_err(AdminError::Migration)
    }

    /// The changes to the models that no migration makes yet, one line each; empty when the
    /// migrations are up to date.
    ///
    /// # Errors
    /// Returns [`AdminError::Migration`] if a model or migration cannot be read.
    pub fn pending_changes(&self) -> Result<Vec<String>, AdminError> {
        Ok(describe(&self.model_changes(&mut Detect)?))
    }

    /// Writes a migration for the model changes into `dir` (normally `src/migrations`) and
    /// rewrites its `mod.rs`, as `makemigrations` does without asking questions: nothing is
    /// taken as renamed. Returns `None` when there is nothing to do.
    ///
    /// # Errors
    /// Returns [`AdminError::Migration`] if the change needs an answer (a value for existing
    /// rows) or the files in `dir` are not the migrations the app was built with, and
    /// [`AdminError::Io`] if the files cannot be written.
    pub fn make_migration(
        &self,
        dir: impl AsRef<std::path::Path>,
        name: Option<&str>,
    ) -> Result<Option<NewMigration>, AdminError> {
        self.make(dir.as_ref(), name, false, &mut Answers::new(true))
    }

    fn make(
        &self,
        dir: &std::path::Path,
        name: Option<&str>,
        empty: bool,
        questions: &mut dyn Questions,
    ) -> Result<Option<NewMigration>, AdminError> {
        let registered = self.migrations.as_deref().unwrap_or_default();
        let loaded: Vec<String> = registered
            .iter()
            .map(|m| format!("m{}", m.name()))
            .collect();
        let on_disk = migration_modules(dir)?;
        if on_disk != loaded {
            let message = if self.migrations.is_none() {
                format!(
                    "{} has migrations, but the app does not load them: add `mod migrations;` \
                     to main.rs and `.migrations(migrations::all())` to the App",
                    dir.display()
                )
            } else {
                format!(
                    "the files in {} ({}) are not the migrations the app was built with ({}); \
                     check that its mod.rs lists every file, and build again",
                    dir.display(),
                    on_disk.join(", "),
                    loaded.join(", ")
                )
            };
            return Err(AdminError::Migration(message));
        }
        check_names(registered)?;
        let operations = if empty {
            Vec::new()
        } else {
            self.model_changes(questions)?
        };
        if operations.is_empty() && !empty {
            return Ok(None);
        }
        let number = registered
            .iter()
            .filter_map(Migration::number)
            .max()
            .unwrap_or(0)
            + 1;
        let suffix = match name {
            Some(name) => slug(name),
            None => auto_name(registered.is_empty(), &operations),
        };
        let full = format!("{number:04}_{suffix}");
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("m{full}.rs"));
        std::fs::write(&path, render_migration(&full, &operations))?;
        let mut modules = loaded;
        modules.push(format!("m{full}"));
        std::fs::write(dir.join("mod.rs"), render_list(&modules))?;
        Ok(Some(NewMigration {
            name: full,
            path,
            changes: describe(&operations),
        }))
    }

    /// The OpenAPI description of the REST API as JSON, as served at `openapi.json`. Use it
    /// to generate clients without starting the server.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the configuration is inconsistent, or if the
    /// documentation is turned off.
    pub fn openapi(&self) -> Result<serde_json::Value, AdminError> {
        self.admin()?
            .openapi()
            .cloned()
            .ok_or_else(|| AdminError::Config("the API documentation is turned off".into()))
    }

    /// The admin and API as configured so far.
    fn admin(&self) -> Result<Admin, AdminError> {
        let mut builder = Admin::builder()
            .mailer_shared(self.mailer.clone())
            .settings(self.settings.clone())
            .docs(self.docs.clone())
            .auth_shared(self.auth.clone(), Arc::clone(&self.users));
        if let Some(base) = &self.admin_base {
            builder = builder.base_path(base.clone());
        }
        if let Some(base) = &self.api_base {
            builder = builder.api_base_path(base.clone());
        }
        if let Some(uploads) = &self.uploads {
            builder = builder.uploads(uploads.clone());
        }
        if self.builtin_users {
            builder = builder.accounts(self.db.clone());
        }
        for resource in &self.resources {
            builder = builder.register_shared(Arc::clone(resource));
        }
        builder.build()
    }

    /// The complete router: admin, REST API and your own routes.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the configuration is inconsistent (see
    /// [`crate::AdminBuilder::build`]).
    pub fn router(&self) -> Result<Router, AdminError> {
        let admin = self.admin()?;

        // From the inside out. The time limit covers everything except file transfers.
        let mut router = admin.page_routes();
        if self.home.is_enabled() {
            let home = Arc::new(Home::new(
                self.home.clone(),
                Arc::clone(&admin.state),
                self.db.clone(),
            ));
            // A fallback rather than a route, so that a `/` route of the application wins.
            router = router.fallback(move |request| home::fallback(Arc::clone(&home), request));
        }
        let mut router = router.merge(self.routes.clone());
        if let Some(limit) = self.request_timeout {
            router = router.layer(middleware::from_fn_with_state(limit, server::timeout));
        }
        if let Some(uploads) = admin.upload_routes() {
            router = router.merge(uploads);
        }
        // Errors, including the time limit's, are put into the format of their area.
        router = admin.handle_errors(router);
        // Outside that, so that error responses carry the cross-origin headers too and
        // preflight requests are answered before anything asks for a login.
        if !self.cors_origins.is_empty() {
            router = router.layer(server::cors(&self.cors_origins));
        }
        if self.home.is_enabled() {
            let api = base_path(self.api_base.clone(), "/api")?;
            let pages = if self.docs.is_enabled() {
                ["docs", "redoc", "openapi.json"]
                    .iter()
                    .map(|page| format!("{api}/{page}"))
                    .collect()
            } else {
                Vec::new()
            };
            let rules = Arc::new(server::BrowserRedirect {
                api_base: api,
                pages,
            });
            router = router.layer(middleware::from_fn_with_state(
                rules,
                server::browser_redirect,
            ));
        }
        // Outermost, to log the response that is actually sent.
        Ok(router.layer(middleware::from_fn(server::access_log)))
    }

    /// Serves the app until Ctrl-C.
    ///
    /// # Errors
    /// Returns [`AdminError::Io`] if the address cannot be bound or the server fails.
    pub async fn serve(self) -> Result<(), AdminError> {
        let router = self.router()?;
        let listener = tokio::net::TcpListener::bind(&self.bind).await?;
        println!("listening on http://{}", listener.local_addr()?);
        // Connection addresses are what the login limiter counts attempts by.
        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        axum::serve(listener, service)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        Ok(())
    }

    /// Runs the management command given on the command line
    /// (`serve`, `makemigrations`, `migrate`, `showmigrations`, `sqlmigrate`, `createsuperuser`,
    /// `changepassword`, `promote`, `totp`, `apikey`, `openapi`, `routes`, `check`).
    ///
    /// # Errors
    /// Returns whatever the chosen command fails with.
    pub async fn run(self) -> Result<(), AdminError> {
        let command = Cli::parse().command;
        init_logging(self.log_level)?;
        match command {
            Command::Serve { bind } => match bind {
                Some(address) => self.bind(address).serve().await,
                None => self.serve().await,
            },
            Command::Migrate { target, fake } => {
                let events = self.migrate_to(target.as_deref(), fake).await?;
                let quiet = events
                    .iter()
                    .all(|e| matches!(e, MigrationEvent::Unmigrated(_)));
                for event in &events {
                    println!("{event}");
                }
                if quiet {
                    println!("no migrations to apply");
                }
                Ok(())
            }
            Command::Makemigrations {
                name,
                empty,
                check,
                no_input,
            } => {
                if check {
                    let changes = self.pending_changes()?;
                    if changes.is_empty() {
                        println!("no changes");
                        return Ok(());
                    }
                    for change in &changes {
                        println!("  {change}");
                    }
                    return Err(AdminError::Migration(
                        "the models have changes that no migration makes yet".into(),
                    ));
                }
                let dir = migrations_dir();
                let made = self.make(&dir, name.as_deref(), empty, &mut Answers::new(no_input))?;
                let Some(made) = made else {
                    println!("no changes");
                    return Ok(());
                };
                println!("wrote {}", made.path.display());
                for change in &made.changes {
                    println!("  {change}");
                }
                if self.migrations.is_none() {
                    println!();
                    println!("to use it, add to main.rs:");
                    println!("  mod migrations;");
                    println!("and to the App: .migrations(migrations::all())");
                }
                println!("next: cargo run -- migrate");
                Ok(())
            }
            Command::Showmigrations => {
                let has_history = runner::has_table(&self.db, runner::HISTORY, &[]).await;
                let project = self.migrations.clone().unwrap_or_default();
                let mut apps: Vec<(&str, Vec<Migration>)> = self
                    .builtin_apps()
                    .into_iter()
                    .map(|a| (a.label, a.migrations.to_vec()))
                    .collect();
                apps.push(("app", project));
                for (label, migrations) in apps {
                    let done = if has_history {
                        runner::applied(&self.db, label).await?
                    } else {
                        std::collections::BTreeSet::new()
                    };
                    println!("{label}");
                    if migrations.is_empty() {
                        println!("  (no migrations)");
                    }
                    for migration in &migrations {
                        let mark = if done.contains(migration.name()) {
                            "X"
                        } else {
                            " "
                        };
                        println!("  [{mark}] {}", migration.name());
                    }
                }
                Ok(())
            }
            Command::Sqlmigrate { name, backwards } => {
                let list = self.migrations.as_deref().unwrap_or_default();
                let keep = find_target(list, &name)?;
                if keep == 0 {
                    return Err(AdminError::Config(format!("no migration named `{name}`")));
                }
                let sql = runner::sql_of(list, keep - 1, self.db.get_database_backend(), backwards)
                    .map_err(AdminError::Migration)?;
                for statement in sql {
                    println!("{statement}");
                }
                Ok(())
            }
            Command::Createsuperuser {
                username,
                email,
                password_stdin,
            } => {
                // Without password login the account logs in through another site, matched by
                // its e-mail address, so it gets a password nobody knows.
                let methods = &self.auth.methods;
                let password = if methods.password || password_stdin {
                    read_password(password_stdin)?
                } else {
                    if email.is_none() {
                        return Err(AdminError::BadInput {
                            field: "email".into(),
                            message: "password login is turned off, so give the address the \
                                      account logs in with: --email you@example.com"
                                .into(),
                        });
                    }
                    rand::random::<[u8; 32]>()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect()
                };
                create_superuser(&self.db, &username, &password).await?;
                if let Some(email) = email {
                    set_email(&self.db, &username, &email).await?;
                }
                println!("created superuser `{username}`");
                if !methods.password {
                    let names: Vec<&str> =
                        methods.providers.iter().map(|p| p.label.as_str()).collect();
                    println!(
                        "it logs in with {} using that e-mail address",
                        names.join(" or ")
                    );
                }
                Ok(())
            }
            Command::Changepassword {
                username,
                password_stdin,
            } => {
                let password = read_password(password_stdin)?;
                change_password(&self.db, &username, &password).await?;
                println!("password changed for `{username}`");
                Ok(())
            }
            Command::Totp { username, disable } => {
                if disable {
                    disable_totp(&self.db, &username).await?;
                    println!("two-factor authentication is off for `{username}`");
                    return Ok(());
                }
                let secret = enable_totp(&self.db, &username).await?;
                let uri = totp_uri(&self.settings.site_title, &username, &secret);
                if let Ok(code) = qrcode::QrCode::new(&uri) {
                    let picture = code
                        .render::<qrcode::render::unicode::Dense1x2>()
                        .quiet_zone(true)
                        .build();
                    println!("{picture}");
                }
                println!("two-factor authentication is on for `{username}`");
                println!(
                    "scan the code above with an authenticator app, or enter the key by hand:"
                );
                println!("  key: {secret}");
                println!("  uri: {uri}");
                let codes = crate::users::new_recovery_codes(&self.db, &username).await?;
                println!();
                println!("recovery codes: each logs in once if the phone is lost. Keep them safe;");
                println!("they are not shown again (new ones can be made on the two-factor page):");
                for code in codes {
                    println!("  {code}");
                }
                Ok(())
            }
            Command::Apikey {
                username,
                name,
                days,
                read_only,
            } => {
                let key = crate::users::new_api_key(
                    &self.db,
                    &username,
                    &name,
                    (days > 0).then_some(days),
                    read_only,
                )
                .await?;
                println!("API key for `{username}` (shown only now; it is not stored):");
                println!("  {key}");
                println!("send it as: Authorization: Bearer {key}");
                if !self.auth.api_keys {
                    println!("note: auth.api_keys is off in the settings, so it does not work yet");
                }
                Ok(())
            }
            Command::Promote { username } => {
                promote(&self.db, &username).await?;
                println!("`{username}` is now a superuser");
                Ok(())
            }
            Command::Openapi => {
                let document = self.openapi()?;
                let text = serde_json::to_string_pretty(&document)
                    .map_err(|e| AdminError::Config(format!("cannot print the document: {e}")))?;
                println!("{text}");
                Ok(())
            }
            Command::Routes => {
                drop(self.router()?);
                let admin = base_path(self.admin_base.clone(), "/admin")?;
                let api = base_path(self.api_base.clone(), "/api")?;
                if self.home.is_enabled() {
                    println!("GET    /");
                }
                println!("POST   {api}/auth/token");
                if self.docs.is_enabled() {
                    println!("GET    {api}/docs");
                    println!("GET    {api}/redoc");
                    println!("GET    {api}/openapi.json");
                }
                for resource in &self.resources {
                    let slug = resource.slug();
                    println!("GET    {admin}/{slug}");
                    println!("GET    {api}/{slug}");
                    println!("POST   {api}/{slug}");
                    println!("GET    {api}/{slug}/{{id}}");
                    println!("PUT    {api}/{slug}/{{id}}");
                    println!("PATCH  {api}/{slug}/{{id}}");
                    println!("DELETE {api}/{slug}/{{id}}");
                }
                Ok(())
            }
            Command::Check => {
                drop(self.router()?);
                println!("configuration is valid ({} models)", self.resources.len());
                Ok(())
            }
        }
    }
}

/// Finishes when the server should stop: on Ctrl-C, or on the termination signal that
/// `docker stop`, systemd and Kubernetes send. Requests in progress then complete.
async fn shutdown_signal() {
    // If a handler cannot be installed, keep serving rather than exit.
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    futures_util::future::select(Box::pin(interrupt), Box::pin(terminate)).await;
    println!("shutting down: finishing requests in progress");
}

/// Where `makemigrations` writes: `src/migrations` of the crate being run.
fn migrations_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from)
        .join("src")
        .join("migrations")
}

/// The migration modules in `dir` (`m0001_initial`, ...), in order.
fn migration_modules(dir: &std::path::Path) -> Result<Vec<String>, AdminError> {
    let mut modules = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(modules),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(module) = name.strip_suffix(".rs")
            && module.starts_with('m')
            && module[1..].starts_with(|c: char| c.is_ascii_digit())
        {
            modules.push(module.to_owned());
        }
    }
    modules.sort();
    Ok(modules)
}

/// Migration names must start with a number, each higher than the last.
fn check_names(migrations: &[Migration]) -> Result<(), AdminError> {
    let mut last = 0;
    for migration in migrations {
        let Some(number) = migration.number() else {
            return Err(AdminError::Migration(format!(
                "migration `{}` must start with its number, like 0002_post_slug",
                migration.name()
            )));
        };
        if number <= last {
            return Err(AdminError::Migration(format!(
                "migration `{}` is out of order: each must have a higher number than the last",
                migration.name()
            )));
        }
        last = number;
    }
    Ok(())
}

/// How many migrations stay applied when moving to `target`.
fn find_target(migrations: &[Migration], target: &str) -> Result<usize, AdminError> {
    if target == "zero" {
        return Ok(0);
    }
    if let Some(i) = migrations.iter().position(|m| m.name() == target) {
        return Ok(i + 1);
    }
    let matches: Vec<usize> = migrations
        .iter()
        .enumerate()
        .filter(|(_, m)| m.name().starts_with(&format!("{target}_")))
        .map(|(i, _)| i)
        .collect();
    match matches[..] {
        [i] => Ok(i + 1),
        [] => Err(AdminError::Config(format!("no migration named `{target}`"))),
        _ => Err(AdminError::Config(format!(
            "`{target}` matches several migrations; give the whole name"
        ))),
    }
}

/// A name for a migration file: lower case letters, digits and underscores.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed: String = out.trim_matches('_').chars().take(50).collect();
    if trimmed.is_empty() {
        "changes".into()
    } else {
        trimmed
    }
}

/// A migration name made up from its changes.
fn auto_name(initial: bool, operations: &[Operation]) -> String {
    if initial {
        return "initial".into();
    }
    let name = match operations {
        [Operation::CreateTable(t)] => t.name.clone(),
        [Operation::DropTable(t)] => format!("delete_{}", t.name),
        [Operation::RenameTable { to, .. }] => format!("rename_{to}"),
        [Operation::AddColumn { table, column, .. }] => format!("{table}_{}", column.name),
        [Operation::DropColumn { table, column }] => format!("remove_{table}_{}", column.name),
        [Operation::RenameColumn { table, to, .. }] => format!("rename_{table}_{to}"),
        [Operation::AlterColumn { table, to, .. }] => format!("alter_{table}_{}", to.name),
        _ => format!("auto_{}", chrono::Utc::now().format("%Y%m%d_%H%M")),
    };
    slug(&name)
}

/// The checks of [`App::with_settings`] that need no database, run before connecting.
fn check_settings(settings: &Settings) -> Result<(), AdminError> {
    if settings.database.url.is_empty() {
        return Err(AdminError::Config(
            "database.url is empty: set it in settings.rs, e.g. `url: env(\"DATABASE_URL\")?`"
                .into(),
        ));
    }
    if settings.secret_key.is_empty() {
        return Err(AdminError::Config(
            "secret_key is empty: set it in settings.rs, e.g. `secret_key: env(\"SECRET_KEY\")?`"
                .into(),
        ));
    }
    check_database_url("database.url", &settings.database.url)?;
    AuthConfig::new(settings.secret_key.clone()).map_err(|e| setting_error("secret_key", e))?;
    settings.admin.ui()?;
    base_path(Some(settings.admin.path.clone()), "/admin")
        .map_err(|e| setting_error("admin.path", e))?;
    base_path(Some(settings.api.path.clone()), "/api").map_err(|e| setting_error("api.path", e))?;
    for origin in &settings.server.cors_origins {
        server::parse_origins(origin).map_err(|e| setting_error("server.cors_origins", e))?;
    }
    Ok(())
}

/// Checks that the database `url` names is one this build can talk to: the drivers are
/// Cargo features, chosen when the project is compiled.
fn check_database_url(setting: &str, url: &str) -> Result<(), AdminError> {
    let scheme = url
        .split_once(':')
        .map_or("", |(scheme, _)| scheme)
        .to_ascii_lowercase();
    let (feature, built) = match scheme.as_str() {
        "sqlite" => ("sqlite", cfg!(feature = "sqlite")),
        "postgres" | "postgresql" => ("postgres", cfg!(feature = "postgres")),
        "mysql" => ("mysql", cfg!(feature = "mysql")),
        "mariadb" => {
            return Err(AdminError::Config(format!(
                "{setting}: MariaDB uses mysql:// addresses, e.g. mysql://user:password@host/name"
            )));
        }
        _ => {
            return Err(AdminError::Config(format!(
                "{setting}: `{scheme}` is not a supported database; use sqlite://, \
                 postgres:// or mysql:// (MariaDB too)"
            )));
        }
    };
    if built {
        return Ok(());
    }
    Err(AdminError::Config(format!(
        "{setting} is a {feature} address, but cadmus was built without its `{feature}` \
         feature. In Cargo.toml: cadmus = {{ ..., default-features = false, features = \
         [\"{feature}\"] }}"
    )))
}

/// Prefixes a configuration error with the setting it is about.
fn setting_error(setting: &str, error: AdminError) -> AdminError {
    match error {
        AdminError::Config(message) => AdminError::Config(format!("{setting}: {message}")),
        other => other,
    }
}

/// Prints the framework's log messages (failed requests, panics, rejected logins) to standard
/// error. Does nothing if the application has already installed a `tracing` subscriber.
///
/// `LOG_LEVEL` is `error`, `warn`, `info` (default), `debug` or `trace`. SQL statements are
/// logged from `debug` up.
fn init_logging(configured: Option<tracing::Level>) -> Result<(), AdminError> {
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let level = match (configured, std::env::var("LOG_LEVEL")) {
        (Some(level), _) => level,
        (None, Ok(text)) => text.parse::<Level>().map_err(|_| {
            AdminError::Config("LOG_LEVEL must be error, warn, info, debug or trace".into())
        })?,
        (None, Err(_)) => Level::INFO,
    };
    let sql = if level >= Level::DEBUG {
        level
    } else {
        Level::WARN
    };
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(level)
        .with_target("sqlx", sql);
    // Fails only when a subscriber is already installed, which is the application's choice.
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                // Colour only when a person is looking, not in a log file.
                .with_ansi(std::io::stderr().is_terminal()),
        )
        .with(filter)
        .try_init();
    Ok(())
}

/// Connection-pool settings from the environment.
fn pool_options(url: String) -> Result<ConnectOptions, AdminError> {
    let number = |key: &str| -> Result<Option<u64>, AdminError> {
        match std::env::var(key) {
            Ok(text) => text
                .parse::<u64>()
                .map(Some)
                .map_err(|_| AdminError::Config(format!("{key} must be a non-negative integer"))),
            Err(_) => Ok(None),
        }
    };
    let mut options = ConnectOptions::new(url);
    if let Some(max) = number("DB_MAX_CONNECTIONS")? {
        options.max_connections(u32::try_from(max).unwrap_or(u32::MAX));
    }
    if let Some(min) = number("DB_MIN_CONNECTIONS")? {
        options.min_connections(u32::try_from(min).unwrap_or(u32::MAX));
    }
    if let Some(secs) = number("DB_ACQUIRE_TIMEOUT_SECS")? {
        options.acquire_timeout(std::time::Duration::from_secs(secs));
    }
    Ok(options)
}

#[derive(Parser)]
#[command(about = "Management commands", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    Serve {
        /// Listen address, overriding APP_BIND.
        #[arg(long)]
        bind: Option<String>,
    },
    /// Apply the migrations the database has not had yet (or, with a target, roll back to it).
    Migrate {
        /// Move the project's migrations to this one (a name or number such as 0002), rolling
        /// back later ones; `zero` rolls back all of them.
        target: Option<String>,
        /// Record or forget migrations without running them, for tables changed by hand.
        #[arg(long)]
        fake: bool,
    },
    /// Write a migration for the changes to the models, into src/migrations.
    Makemigrations {
        /// Name the migration (default: made up from the changes).
        #[arg(long)]
        name: Option<String>,
        /// Write a migration with no changes, to fill in by hand.
        #[arg(long)]
        empty: bool,
        /// Write nothing; fail if the models have changes without a migration (for CI).
        #[arg(long)]
        check: bool,
        /// Ask nothing: assume nothing was renamed.
        #[arg(long)]
        no_input: bool,
    },
    /// List the migrations and whether the database has had each.
    Showmigrations,
    /// Print the SQL a project migration runs on this database.
    Sqlmigrate {
        /// The migration's name or number.
        name: String,
        /// The SQL that rolls it back instead.
        #[arg(long)]
        backwards: bool,
    },
    /// Create a superuser: an account that can do everything, including managing accounts.
    Createsuperuser {
        /// Login name.
        #[arg(long)]
        username: String,
        /// E-mail address; the account can log in with it too.
        #[arg(long)]
        email: Option<String>,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change an account's password.
    Changepassword {
        /// Login name.
        #[arg(long)]
        username: String,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Turn two-factor authentication on for an account and print the code to scan.
    Totp {
        /// Login name.
        #[arg(long)]
        username: String,
        /// Turn it off instead, e.g. after a device was lost.
        #[arg(long)]
        disable: bool,
    },
    /// Make an API key for an account, for programs calling the REST API as it.
    Apikey {
        /// Login name of the account the key acts as.
        #[arg(long)]
        username: String,
        /// What uses the key, e.g. "deploy script".
        #[arg(long)]
        name: String,
        /// Days until it expires; 0 for never.
        #[arg(long, default_value_t = 90)]
        days: u32,
        /// Only allow reading: no adding, changing or deleting.
        #[arg(long)]
        read_only: bool,
    },
    /// Make an existing account an active superuser.
    Promote {
        /// Login name.
        #[arg(long)]
        username: String,
    },
    /// Print the OpenAPI description of the REST API as JSON.
    Openapi,
    /// List the generated URLs.
    Routes,
    /// Validate the configuration without starting the server.
    Check,
}

fn read_password(from_stdin: bool) -> Result<String, AdminError> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_owned());
    }
    let first = rpassword::prompt_password("Password: ")?;
    let second = rpassword::prompt_password("Password (again): ")?;
    if first != second {
        return Err(AdminError::BadInput {
            field: "password".into(),
            message: "the two passwords do not match".into(),
        });
    }
    Ok(first)
}

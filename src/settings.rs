//! Settings: the whole project's configuration ([`Settings`]), and the look of the admin
//! ([`UiSettings`]).

use std::path::PathBuf;

use serde::Serialize;

use crate::error::AdminError;

/// Colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the browser/OS preference.
    #[default]
    Auto,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// A link added to the navigation bar.
#[derive(Debug, Clone, Serialize)]
pub struct NavLink {
    /// Link text.
    pub label: String,
    /// Target URL.
    pub url: String,
}

/// Look-and-feel settings. Build in code, or load with [`UiSettings::from_env`].
#[derive(Debug, Clone, Serialize)]
pub struct UiSettings {
    pub(crate) site_title: String,
    pub(crate) accent_color: String,
    pub(crate) logo_url: Option<String>,
    pub(crate) theme: Theme,
    pub(crate) custom_css_url: Option<String>,
    pub(crate) footer_text: Option<String>,
    pub(crate) page_size: u64,
    pub(crate) nav_links: Vec<NavLink>,
    /// Whether pages load their typeface from Google Fonts; otherwise the system's is used.
    pub(crate) web_fonts: bool,
    #[serde(skip)]
    pub(crate) template_dir: Option<PathBuf>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            site_title: "Administration".to_owned(),
            accent_color: "#4f46e5".to_owned(),
            logo_url: None,
            theme: Theme::Auto,
            custom_css_url: None,
            footer_text: None,
            page_size: 25,
            nav_links: Vec::new(),
            web_fonts: true,
            template_dir: None,
        }
    }
}

impl UiSettings {
    /// Default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads settings from the environment; unset variables keep their defaults.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `ADMIN_SITE_TITLE` | Title in the header and browser tab |
    /// | `ADMIN_ACCENT_COLOR` | Hex colour, e.g. `#0a7f5a` |
    /// | `ADMIN_LOGO_URL` | Image shown next to the title |
    /// | `ADMIN_THEME` | `auto`, `light` or `dark` |
    /// | `ADMIN_CUSTOM_CSS_URL` | Extra stylesheet loaded after the built-in one |
    /// | `ADMIN_FOOTER_TEXT` | Text in the page footer |
    /// | `ADMIN_PAGE_SIZE` | Rows per list page |
    /// | `ADMIN_NAV_LINKS` | Extra links: `Label=/url,Other=https://…` |
    /// | `ADMIN_TEMPLATE_DIR` | Directory whose files override built-in templates |
    /// | `ADMIN_WEB_FONTS` | `false` uses the system typeface instead of loading Fira Sans from Google Fonts |
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a variable is set to an invalid value.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`UiSettings::from_env`] but reads from `lookup`, so tests need no real environment.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is invalid.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let mut settings = Self::default();
        if let Some(v) = lookup("ADMIN_SITE_TITLE") {
            settings.site_title = v;
        }
        if let Some(v) = lookup("ADMIN_ACCENT_COLOR") {
            settings = settings.accent_color(&v)?;
        }
        settings.logo_url = lookup("ADMIN_LOGO_URL");
        if let Some(v) = lookup("ADMIN_THEME") {
            settings.theme = match v.to_ascii_lowercase().as_str() {
                "auto" => Theme::Auto,
                "light" => Theme::Light,
                "dark" => Theme::Dark,
                other => {
                    return Err(AdminError::Config(format!(
                        "ADMIN_THEME must be auto, light or dark, got `{other}`"
                    )));
                }
            };
        }
        settings.custom_css_url = lookup("ADMIN_CUSTOM_CSS_URL");
        settings.footer_text = lookup("ADMIN_FOOTER_TEXT");
        if let Some(v) = lookup("ADMIN_PAGE_SIZE") {
            settings.page_size = v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                AdminError::Config("ADMIN_PAGE_SIZE must be a positive integer".into())
            })?;
        }
        if let Some(v) = lookup("ADMIN_NAV_LINKS") {
            for pair in v.split(',').filter(|p| !p.trim().is_empty()) {
                let (label, url) = pair.split_once('=').ok_or_else(|| {
                    AdminError::Config("ADMIN_NAV_LINKS entries must look like Label=/url".into())
                })?;
                settings = settings.nav_link(label.trim(), url.trim());
            }
        }
        if let Some(v) = lookup("ADMIN_WEB_FONTS") {
            settings.web_fonts = match v.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => {
                    return Err(AdminError::Config(
                        "ADMIN_WEB_FONTS must be true or false".into(),
                    ));
                }
            };
        }
        settings.template_dir = lookup("ADMIN_TEMPLATE_DIR").map(PathBuf::from);
        Ok(settings)
    }

    /// Sets the title shown in the header and browser tab.
    #[must_use]
    pub fn site_title(mut self, title: impl Into<String>) -> Self {
        self.site_title = title.into();
        self
    }

    /// Sets the accent colour.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] unless `color` is `#rgb` or `#rrggbb`; the value is written
    /// into a stylesheet, so anything else is refused rather than escaped.
    pub fn accent_color(mut self, color: &str) -> Result<Self, AdminError> {
        let hex = color.strip_prefix('#').unwrap_or("");
        if !(matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())) {
            return Err(AdminError::Config(format!(
                "accent colour must be #rgb or #rrggbb, got `{color}`"
            )));
        }
        self.accent_color = color.to_owned();
        Ok(self)
    }

    /// Loads Fira Sans from Google Fonts (the default), or uses the system typeface, which
    /// makes no request to another site.
    #[must_use]
    pub fn web_fonts(mut self, enabled: bool) -> Self {
        self.web_fonts = enabled;
        self
    }

    /// Sets the logo image URL.
    #[must_use]
    pub fn logo_url(mut self, url: impl Into<String>) -> Self {
        self.logo_url = Some(url.into());
        self
    }

    /// Sets the colour scheme.
    #[must_use]
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Loads an extra stylesheet after the built-in one.
    #[must_use]
    pub fn custom_css_url(mut self, url: impl Into<String>) -> Self {
        self.custom_css_url = Some(url.into());
        self
    }

    /// Sets the footer text.
    #[must_use]
    pub fn footer_text(mut self, text: impl Into<String>) -> Self {
        self.footer_text = Some(text.into());
        self
    }

    /// Sets the default rows per list page.
    #[must_use]
    pub fn page_size(mut self, rows: u64) -> Self {
        self.page_size = rows.max(1);
        self
    }

    /// Adds a link to the navigation bar.
    #[must_use]
    pub fn nav_link(mut self, label: impl Into<String>, url: impl Into<String>) -> Self {
        self.nav_links.push(NavLink {
            label: label.into(),
            url: url.into(),
        });
        self
    }

    /// Overrides built-in templates with same-named files from `dir`
    /// (`base.html`, `login.html`, `index.html`, `list.html`, `form.html`).
    #[must_use]
    pub fn template_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.template_dir = Some(dir.into());
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Project settings
// ---------------------------------------------------------------------------------------------

pub use std::time::Duration;
/// How much the server logs: `LogLevel::ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
pub use tracing::Level as LogLevel;

/// Every setting of a project in one value, the counterpart of Django's `settings.py`.
///
/// A project generated by `cadmus new` fills it in `src/settings.rs` and starts with
/// [`crate::App::from_settings`]. Set what you need and leave the rest to
/// `..Default::default()`; secrets come from the environment through [`env()`]:
///
/// ```no_run
/// use cadmus::AdminError;
/// use cadmus::settings::*;
///
/// fn settings() -> Result<Settings, AdminError> {
///     Ok(Settings {
///         secret_key: env("SECRET_KEY")?,
///         database: DatabaseSettings {
///             url: env("DATABASE_URL")?,
///             ..Default::default()
///         },
///         admin: AdminSettings {
///             site_title: "Shop".into(),
///             ..Default::default()
///         },
///         ..Default::default()
///     })
/// }
/// ```
///
/// Values are checked when the app is built, so a mistake stops the server at start-up with a
/// message naming the setting.
#[derive(Clone)]
pub struct Settings {
    /// Signs login tokens. At least 32 bytes; keep it secret and out of the code, e.g.
    /// `env("SECRET_KEY")?`. `cadmus secret` prints a new one.
    pub secret_key: String,
    /// Where the data lives.
    pub database: DatabaseSettings,
    /// Listening, time limits, cross-origin access and proxies.
    pub server: ServerSettings,
    /// Login sessions, two-factor and attempt limits.
    pub auth: AuthSettings,
    /// The admin's address and look.
    pub admin: AdminSettings,
    /// The REST API.
    pub api: ApiSettings,
    /// The API documentation pages.
    pub docs: DocsSettings,
    /// The public status page at `/`.
    pub home: HomeSettings,
    /// File uploads.
    pub uploads: UploadSettings,
    /// Sending e-mail: login links and password resets.
    pub mail: MailSettings,
    /// How much is logged. SQL statements are logged from `DEBUG` up.
    pub log_level: LogLevel,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            secret_key: String::new(),
            database: DatabaseSettings::default(),
            server: ServerSettings::default(),
            auth: AuthSettings::default(),
            admin: AdminSettings::default(),
            api: ApiSettings::default(),
            docs: DocsSettings::default(),
            home: HomeSettings::default(),
            uploads: UploadSettings::default(),
            mail: MailSettings::default(),
            log_level: LogLevel::INFO,
        }
    }
}

// Hand-written so the secret key can never reach a log line.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("secret_key", &"<hidden>")
            .field("database", &self.database)
            .field("server", &self.server)
            .field("auth", &self.auth)
            .field("admin", &self.admin)
            .field("api", &self.api)
            .field("docs", &self.docs)
            .field("home", &self.home)
            .field("uploads", &self.uploads)
            .field("mail", &self.mail)
            .field("log_level", &self.log_level)
            .finish()
    }
}

/// Database connection settings.
#[derive(Clone, Default)]
pub struct DatabaseSettings {
    /// e.g. `sqlite://db.sqlite3?mode=rwc` or `postgres://user:password@host/name`. Usually
    /// `env("DATABASE_URL")?`, since it often holds a password.
    pub url: String,
    /// Most connections kept open (default: the driver's).
    pub max_connections: Option<u32>,
    /// Fewest connections kept open (default: the driver's).
    pub min_connections: Option<u32>,
    /// How long a request waits for a free connection (default: the driver's).
    pub acquire_timeout: Option<Duration>,
}

// The URL usually holds a password.
impl std::fmt::Debug for DatabaseSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = self.url.split_once(':').map_or("", |(scheme, _)| scheme);
        f.debug_struct("DatabaseSettings")
            .field("url", &format_args!("{scheme}:<hidden>"))
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .finish()
    }
}

/// How the server listens and which requests it accepts.
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// Address and port `serve` listens on (default `127.0.0.1:3000`; `0.0.0.0:3000` to be
    /// reachable from other machines).
    pub bind: String,
    /// Longest a request may take before it is answered with `503`; `None` for no limit
    /// (default 30 seconds). File transfers are not limited.
    pub request_timeout: Option<Duration>,
    /// Origins whose pages may call the app from a browser, e.g. `https://app.example.com`,
    /// or `*` for all (default: none).
    pub cors_origins: Vec<String>,
    /// Behind a reverse proxy, the header it puts the client's address in, e.g.
    /// `X-Forwarded-For`; login limits then count per client rather than per proxy.
    pub client_ip_header: Option<String>,
    /// The address people reach the site at, e.g. `https://shop.example.com` (or
    /// `http://localhost:3000` on your machine). Needed by login methods that send people to
    /// another site and back, such as Google.
    pub public_url: Option<String>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3000".to_owned(),
            request_timeout: Some(Duration::from_secs(30)),
            cors_origins: Vec::new(),
            client_ip_header: None,
            public_url: None,
        }
    }
}

/// Login sessions and their protection.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// How people log in, in the order the login page offers them (default: password only).
    ///
    /// ```no_run
    /// # use cadmus::settings::*;
    /// # fn f() -> Result<Vec<AuthMethod>, cadmus::AdminError> { Ok(
    /// vec![
    ///     AuthMethod::Password,
    ///     AuthMethod::Google(OAuthClient::from_env("GOOGLE")?),
    ///     AuthMethod::Facebook(OAuthClient::from_env("FACEBOOK")?),
    /// ]
    /// # ) }
    /// ```
    pub methods: Vec<AuthMethod>,
    /// Whether someone without an account may get one by logging in through another site
    /// (default: no, only existing accounts may log in).
    pub signup: Signup,
    /// Groups that accounts created by [`Signup`] are put in, by name. New accounts are staff
    /// without permissions of their own, so these groups decide what they may do.
    pub signup_groups: Vec<String>,
    /// How long a login lasts (default 1 hour).
    pub token_lifetime: Duration,
    /// Refuse accounts without two-factor authentication (default `false`).
    pub totp_required: bool,
    /// Send the login cookie over HTTPS only (default `true`). Turn it off only on your own
    /// machine, where the server speaks plain HTTP.
    pub cookie_secure: bool,
    /// Login attempts allowed per account in one lockout period; `0` for no limit (default 5).
    pub login_max_attempts: u32,
    /// Length of the lockout period (default 5 minutes).
    pub login_lockout: Duration,
    /// Whether accounts may make API keys for programs calling the REST API (default
    /// `true`). Turned off, existing keys stop working.
    pub api_keys: bool,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            methods: vec![AuthMethod::Password],
            signup: Signup::Closed,
            signup_groups: Vec::new(),
            token_lifetime: Duration::from_secs(3600),
            totp_required: false,
            cookie_secure: true,
            login_max_attempts: 5,
            login_lockout: Duration::from_secs(300),
            api_keys: true,
        }
    }
}

/// A way to log in.
///
/// Logging in through another site matches the account by the e-mail address that site
/// vouches for, and remembers the link, so later logins work even if the address changes.
/// Each site needs an app registered with it; its *redirect URI* is
/// `<server.public_url><admin.path>/auth/<name>/callback`, e.g.
/// `https://shop.example.com/admin/auth/google/callback`.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// Username or e-mail address and password. Adds "Forgot your password?", which e-mails
    /// a link to choose a new one.
    Password,
    /// A login link e-mailed to the account's address, valid once for 15 minutes. Sent with
    /// [`MailSettings`]; needs `server.public_url` for the link.
    MagicLink,
    /// Sign in with Google (OpenID Connect).
    Google(OAuthClient),
    /// Sign in with a Microsoft account (OpenID Connect).
    Microsoft {
        /// The registered app.
        client: OAuthClient,
        /// `common` (work, school and personal accounts), `organizations`, `consumers`, or
        /// your directory's ID to admit only its members. With one directory its e-mail
        /// addresses are trusted; with the shared ones, only if the app is set up to send
        /// the optional `xms_edov` claim.
        tenant: String,
    },
    /// Sign in with GitHub. The account's primary, verified e-mail address is used.
    GitHub(OAuthClient),
    /// Log in with Facebook.
    Facebook(OAuthClient),
    /// Any other OpenID Connect provider: Okta, Auth0, Keycloak, GitLab, Apple, …
    Oidc(OidcProvider),
}

impl AuthMethod {
    /// The name used in URLs, e.g. `google`.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Password => "password",
            Self::MagicLink => "magic-link",
            Self::Google(_) => "google",
            Self::Microsoft { .. } => "microsoft",
            Self::GitHub(_) => "github",
            Self::Facebook(_) => "facebook",
            Self::Oidc(provider) => &provider.name,
        }
    }
}

/// An app registered with another site, which lets this one ask it who is logging in.
#[derive(Clone)]
pub struct OAuthClient {
    /// The app's public identifier.
    pub client_id: String,
    /// The app's secret; keep it in the environment.
    pub client_secret: String,
}

impl OAuthClient {
    /// An app with this identifier and secret.
    #[must_use]
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Reads `<PREFIX>_CLIENT_ID` and `<PREFIX>_CLIENT_SECRET` from the environment, e.g.
    /// `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` for `from_env("GOOGLE")`.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] naming the variable that is missing.
    pub fn from_env(prefix: &str) -> Result<Self, AdminError> {
        Ok(Self {
            client_id: env(&format!("{prefix}_CLIENT_ID"))?,
            client_secret: env(&format!("{prefix}_CLIENT_SECRET"))?,
        })
    }
}

// Hand-written so the secret can never reach a log line.
impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<hidden>")
            .finish()
    }
}

/// An OpenID Connect provider not built in.
#[derive(Debug, Clone)]
pub struct OidcProvider {
    /// Used in URLs: lowercase letters, digits and `-`, e.g. `okta`.
    pub name: String,
    /// Shown on the login button: "Continue with `label`".
    pub label: String,
    /// The provider's issuer URL, e.g. `https://acme.okta.com`; its settings are read from
    /// `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// The registered app.
    pub client: OAuthClient,
    /// Whether the provider's e-mail addresses may be trusted when it does not say they
    /// were verified (no `email_verified` claim). Only for providers whose users cannot
    /// choose their own address, such as your company's directory.
    pub trust_email: bool,
}

impl OidcProvider {
    /// A provider named `name` (in URLs) and `label` (on the button).
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        issuer: impl Into<String>,
        client: OAuthClient,
    ) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            issuer: issuer.into(),
            client,
            trust_email: false,
        }
    }
}

/// Who may get an account by logging in through another site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Signup {
    /// Nobody: only existing accounts may log in (the default). Give people an account with
    /// their e-mail address first, on the Users page.
    #[default]
    Closed,
    /// People whose verified address is at one of these domains, e.g. `acme.com`.
    Domains(Vec<String>),
    /// Anyone with a verified address.
    Open,
}

/// The admin's address and look.
#[derive(Debug, Clone)]
pub struct AdminSettings {
    /// URL prefix (default `/admin`).
    pub path: String,
    /// Title in the header and browser tab (default "Administration").
    pub site_title: String,
    /// Accent colour, `#rgb` or `#rrggbb` (default `#4f46e5`).
    pub accent_color: String,
    /// Image shown next to the title.
    pub logo_url: Option<String>,
    /// Light, dark, or the visitor's preference (default).
    pub theme: Theme,
    /// Extra stylesheet loaded after the built-in one.
    pub custom_css_url: Option<String>,
    /// Text in the page footer.
    pub footer_text: Option<String>,
    /// Rows per list page (default 25).
    pub page_size: u64,
    /// Extra links in the navigation.
    pub nav_links: Vec<NavLink>,
    /// Load Fira Sans from Google Fonts (default); `false` uses the system typeface and makes
    /// no request to another site.
    pub web_fonts: bool,
    /// Directory whose files replace the built-in templates of the same name.
    pub template_dir: Option<PathBuf>,
}

impl Default for AdminSettings {
    fn default() -> Self {
        let ui = UiSettings::default();
        Self {
            path: "/admin".to_owned(),
            site_title: ui.site_title,
            accent_color: ui.accent_color,
            logo_url: ui.logo_url,
            theme: ui.theme,
            custom_css_url: ui.custom_css_url,
            footer_text: ui.footer_text,
            page_size: ui.page_size,
            nav_links: ui.nav_links,
            web_fonts: ui.web_fonts,
            template_dir: ui.template_dir,
        }
    }
}

impl AdminSettings {
    /// The look, checked.
    pub(crate) fn ui(&self) -> Result<UiSettings, AdminError> {
        if self.page_size == 0 {
            return Err(AdminError::Config(
                "admin.page_size must be at least 1".into(),
            ));
        }
        let accent = UiSettings::default()
            .accent_color(&self.accent_color)
            .map_err(|e| AdminError::Config(format!("admin.accent_color: {}", config_text(e))))?
            .accent_color;
        Ok(UiSettings {
            site_title: self.site_title.clone(),
            accent_color: accent,
            logo_url: self.logo_url.clone(),
            theme: self.theme,
            custom_css_url: self.custom_css_url.clone(),
            footer_text: self.footer_text.clone(),
            page_size: self.page_size,
            nav_links: self.nav_links.clone(),
            web_fonts: self.web_fonts,
            template_dir: self.template_dir.clone(),
        })
    }
}

/// The REST API.
#[derive(Debug, Clone)]
pub struct ApiSettings {
    /// URL prefix (default `/api`).
    pub path: String,
}

impl Default for ApiSettings {
    fn default() -> Self {
        Self {
            path: "/api".to_owned(),
        }
    }
}

/// The API documentation at `<api path>/docs`, `/redoc` and `/openapi.json`.
#[derive(Clone)]
pub struct DocsSettings {
    /// Serve the documentation (default `true`).
    pub enabled: bool,
    /// Set together with `password` to ask for them before showing the documentation.
    pub username: Option<String>,
    /// See `username`; usually `Some(env("DOCS_PASSWORD")?)`.
    pub password: Option<String>,
    /// Title of the API (default: the site title followed by "API").
    pub title: Option<String>,
    /// Version shown in the documentation (default `0.1.0`).
    pub version: String,
    /// Introduction shown at the top; Markdown.
    pub description: Option<String>,
    /// Where Swagger UI's script is loaded from, to host it yourself (default: jsDelivr).
    pub swagger_js_url: Option<String>,
    /// Where Swagger UI's stylesheet is loaded from (default: jsDelivr).
    pub swagger_css_url: Option<String>,
    /// Where ReDoc's script is loaded from (default: jsDelivr).
    pub redoc_js_url: Option<String>,
}

impl Default for DocsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            username: None,
            password: None,
            title: None,
            version: "0.1.0".to_owned(),
            description: None,
            swagger_js_url: None,
            swagger_css_url: None,
            redoc_js_url: None,
        }
    }
}

// Hand-written so the password can never reach a log line.
impl std::fmt::Debug for DocsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocsSettings")
            .field("enabled", &self.enabled)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("title", &self.title)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// The public status page at `/`.
#[derive(Debug, Clone)]
pub struct HomeSettings {
    /// Serve the page (default `true`); it also sends browsers that open an API URL there.
    pub enabled: bool,
    /// Name shown on the page (default: the site title).
    pub title: Option<String>,
    /// One line under the name.
    pub tagline: Option<String>,
}

impl Default for HomeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            title: None,
            tagline: None,
        }
    }
}

/// Sending e-mail. Until `url` is set, messages are written to the log instead, so on your
/// own machine login links can be copied from the terminal.
#[derive(Clone, Default)]
pub struct MailSettings {
    /// The SMTP server, e.g. `smtps://user:password@smtp.example.com` (TLS, port 465) or
    /// `smtp://user:password@smtp.example.com:587?tls=required` (STARTTLS). It holds a
    /// password, so usually `env_opt("MAIL_URL")`.
    pub url: Option<String>,
    /// Who messages come from, e.g. `Shop <noreply@shop.example.com>`. Required with `url`.
    pub from: Option<String>,
}

// The URL holds a password.
impl std::fmt::Debug for MailSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailSettings")
            .field("url", &self.url.as_ref().map(|_| "<hidden>"))
            .field("from", &self.from)
            .finish()
    }
}

/// File uploads. Off until `storage_url` is set.
#[derive(Debug, Clone)]
pub struct UploadSettings {
    /// Where files are kept: `file:///absolute/path`, `s3://bucket/prefix`,
    /// `az://container/prefix` or `gs://bucket/prefix`. The backend's credentials
    /// (`AWS_ACCESS_KEY_ID`, …) are read from the environment.
    pub storage_url: Option<String>,
    /// Largest file accepted, in bytes (default 1 GiB).
    pub max_file_bytes: u64,
    /// Largest chunk of a resumable upload, in bytes (default 8 MiB).
    pub max_chunk_bytes: u64,
}

impl Default for UploadSettings {
    fn default() -> Self {
        Self {
            storage_url: None,
            max_file_bytes: crate::uploads::DEFAULT_MAX_FILE_BYTES,
            max_chunk_bytes: crate::uploads::DEFAULT_MAX_CHUNK_BYTES as u64,
        }
    }
}

/// Loads `.env` from the working directory once, without overriding variables already set.
/// A missing file is normal in production, where the platform sets the variables.
fn load_dotenv() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

/// Reads a required environment variable (after loading `.env`).
///
/// # Errors
/// Returns [`AdminError::Config`] naming the variable if it is unset or empty.
pub fn env(key: &str) -> Result<String, AdminError> {
    env_opt(key).ok_or_else(|| AdminError::Config(format!("{key} is not set (add it to .env)")))
}

/// Reads an optional environment variable (after loading `.env`); empty counts as unset.
#[must_use]
pub fn env_opt(key: &str) -> Option<String> {
    load_dotenv();
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// Reads an environment variable as any type that parses from text (`bool`, numbers,
/// [`LogLevel`], `String`, …), or returns `default` when it is unset.
///
/// Booleans also accept `1`/`0` and `yes`/`no`.
///
/// # Errors
/// Returns [`AdminError::Config`] naming the variable if it is set but does not parse.
pub fn env_or<T: std::str::FromStr + 'static>(key: &str, default: T) -> Result<T, AdminError> {
    let Some(text) = env_opt(key) else {
        return Ok(default);
    };
    let normalised = if std::any::TypeId::of::<T>() == std::any::TypeId::of::<bool>() {
        match text.trim().to_ascii_lowercase().as_str() {
            "1" | "yes" | "true" | "on" => "true".to_owned(),
            "0" | "no" | "false" | "off" => "false".to_owned(),
            _ => text.clone(),
        }
    } else {
        text.trim().to_owned()
    };
    normalised
        .parse::<T>()
        .map_err(|_| AdminError::Config(format!("{key} has an invalid value `{text}`")))
}

/// The message of a configuration error, without its prefix.
fn config_text(error: AdminError) -> String {
    match error {
        AdminError::Config(message) => message,
        other => other.to_string(),
    }
}

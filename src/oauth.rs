//! Logging in through another site: OpenID Connect (Google, Microsoft and any other
//! provider) and OAuth 2 (GitHub, Facebook).
//!
//! The browser goes to the provider with a random `state`, a `nonce` and a PKCE challenge,
//! all remembered in a short-lived signed cookie, and comes back to the callback with a code.
//! The server trades the code for the person's identity directly with the provider, over
//! TLS, matches it to an account (see [`crate::users::resolve_identity`]) and logs them in,
//! asking for their authenticator code first if they have one.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use crate::AdminState;
use crate::auth::constant_time_eq;
use crate::error::AdminError;
use crate::settings::{AppSettings, AuthMethod, OAuthClient, Signup};

const FLOW_COOKIE: &str = "cadmus_oauth";
const FLOW_PURPOSE: &str = "oauth-flow";
/// How long someone may take at the provider's page.
const FLOW_SECS: u64 = 600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Clock difference tolerated when checking a provider's token expiry.
const SKEW_SECS: u64 = 60;

const GOOGLE_ISSUER: &str = "https://accounts.google.com";
const GITHUB_AUTHORIZE: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN: &str = "https://github.com/login/oauth/access_token";
const GITHUB_API: &str = "https://api.github.com";
const FACEBOOK_AUTHORIZE: &str = "https://www.facebook.com/v21.0/dialog/oauth";
const FACEBOOK_TOKEN: &str = "https://graph.facebook.com/v21.0/oauth/access_token";
const FACEBOOK_ME: &str = "https://graph.facebook.com/v21.0/me";
/// Microsoft directories that admit accounts from many organisations.
const MICROSOFT_SHARED_TENANTS: [&str; 3] = ["common", "organizations", "consumers"];

// ---- configuration -------------------------------------------------------------------------

/// The login methods a site offers, checked and ready to use.
pub(crate) struct Methods {
    pub(crate) password: bool,
    pub(crate) magic_link: bool,
    pub(crate) providers: Vec<Arc<Provider>>,
    pub(crate) signup: Signup,
    pub(crate) signup_groups: Vec<String>,
    /// Without a trailing slash.
    public_url: Option<String>,
    /// The app whose users log in through the API; its `url` without a trailing slash.
    pub(crate) app: AppSettings,
    /// Built on first use, so sites without providers never load the system's certificates.
    http: OnceLock<Result<reqwest::Client, String>>,
}

impl Methods {
    pub(crate) fn password_only() -> Self {
        Self {
            password: true,
            magic_link: false,
            providers: Vec::new(),
            signup: Signup::Closed,
            signup_groups: Vec::new(),
            public_url: None,
            app: AppSettings::default(),
            http: OnceLock::new(),
        }
    }

    /// Sets the app whose users log in through the API, checking its addresses.
    pub(crate) fn with_app(mut self, mut app: AppSettings) -> Result<Self, AdminError> {
        if let Some(url) = app
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            app.url = Some(check_url("auth.app.url", url)?);
        } else {
            app.url = None;
        }
        for uri in &mut app.redirect_uris {
            let trimmed = uri.trim().to_owned();
            let parsed = url::Url::parse(&trimmed).map_err(|_| {
                AdminError::Config(format!("auth.app.redirect_uris: `{trimmed}` is not a URL"))
            })?;
            if parsed.fragment().is_some() || matches!(parsed.scheme(), "javascript" | "data") {
                return Err(AdminError::Config(format!(
                    "auth.app.redirect_uris: `{trimmed}` cannot be a redirect address"
                )));
            }
            *uri = trimmed;
        }
        for (name, link) in [
            ("login", &app.links.login),
            ("register", &app.links.register),
            ("reset", &app.links.reset),
        ] {
            if !link.contains("{token}") {
                return Err(AdminError::Config(format!(
                    "auth.app.links.{name}: must contain `{{token}}`, got `{link}`"
                )));
            }
        }
        self.app = app;
        Ok(self)
    }

    /// The app's address for an e-mailed link, from one of [`AppLinks`]' templates; `None`
    /// when no app address is set.
    #[allow(dead_code)] // The e-mailed links for apps come in the next commit.
    pub(crate) fn app_link(&self, template: &str, token: &str) -> Option<String> {
        let base = self.app.url.as_deref()?;
        let path = template.replace("{token}", token);
        Some(if path.contains("://") {
            path
        } else {
            format!("{base}{path}")
        })
    }

    /// Checks the settings; errors name the setting at fault.
    pub(crate) fn new(
        methods: Vec<AuthMethod>,
        signup: Signup,
        signup_groups: Vec<String>,
        public_url: Option<&str>,
    ) -> Result<Self, AdminError> {
        let error = |message: String| AdminError::Config(format!("auth.methods: {message}"));
        if methods.is_empty() {
            return Err(error(
                "no login method is enabled, so nobody could log in".into(),
            ));
        }
        let mut password = false;
        let mut magic_link = false;
        let mut providers: Vec<Arc<Provider>> = Vec::new();
        for method in methods {
            let name = method.name().to_owned();
            if name == "password" && password
                || name == "magic-link" && magic_link
                || providers.iter().any(|provider| provider.name == name)
            {
                return Err(error(format!("`{name}` is listed twice")));
            }
            match method {
                AuthMethod::Password => password = true,
                AuthMethod::MagicLink => magic_link = true,
                other => providers.push(Arc::new(Provider::new(other).map_err(error)?)),
            }
        }
        let public_url = match public_url.map(str::trim).filter(|url| !url.is_empty()) {
            Some(url) => Some(check_public_url(url)?),
            None if !providers.is_empty() => {
                return Err(AdminError::Config(format!(
                    "server.public_url is not set, but `{}` sends people to another site and \
                     needs to know where to send them back, e.g. `http://localhost:3000`",
                    providers[0].name
                )));
            }
            None if magic_link => {
                return Err(AdminError::Config(
                    "server.public_url is not set, but login links need the site's address, \
                     e.g. `http://localhost:3000`"
                        .into(),
                ));
            }
            None => None,
        };
        let signup = match signup {
            Signup::Domains(domains) => {
                let domains: Vec<String> = domains
                    .iter()
                    .map(|domain| domain.trim().trim_start_matches('@').to_lowercase())
                    .filter(|domain| !domain.is_empty())
                    .collect();
                if domains.is_empty() {
                    return Err(AdminError::Config(
                        "auth.signup: Signup::Domains needs at least one domain".into(),
                    ));
                }
                Signup::Domains(domains)
            }
            other => other,
        };
        Ok(Self {
            password,
            magic_link,
            providers,
            signup,
            signup_groups,
            public_url,
            app: AppSettings::default(),
            http: OnceLock::new(),
        })
    }

    pub(crate) fn provider(&self, name: &str) -> Option<&Arc<Provider>> {
        self.providers.iter().find(|provider| provider.name == name)
    }

    /// Whether an account may be created for this verified address.
    pub(crate) fn signup_allows(&self, email: &str) -> bool {
        match &self.signup {
            Signup::Closed => false,
            Signup::Open => true,
            Signup::Domains(domains) => email
                .rsplit_once('@')
                .is_some_and(|(_, domain)| domains.iter().any(|allowed| allowed == domain)),
        }
    }

    pub(crate) fn http(&self) -> Result<&reqwest::Client, String> {
        self.http
            .get_or_init(build_http_client)
            .as_ref()
            .map_err(Clone::clone)
    }

    /// An absolute URL for `path` on this site, for links in e-mails. Falls back to the bare
    /// path when no public address is set (only password logins then).
    pub(crate) fn absolute(&self, path: &str) -> String {
        format!("{}{path}", self.public_url.as_deref().unwrap_or_default())
    }

    /// Whether e-mails can contain links back to the site.
    pub(crate) fn has_public_url(&self) -> bool {
        self.public_url.is_some()
    }

    fn redirect_uri(&self, base: &str, provider: &Provider) -> String {
        format!(
            "{}{base}/auth/{}/callback",
            self.public_url.as_deref().unwrap_or_default(),
            provider.name
        )
    }
}

/// An HTTP client whose TLS uses `ring` and the operating system's certificate store.
fn build_http_client() -> Result<reqwest::Client, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
        .map_err(|e| format!("cannot load the system's certificates: {e}"))?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("cadmus/", env!("CARGO_PKG_VERSION")))
        // Providers answer directly; following redirects would only hide mistakes.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

fn check_public_url(text: &str) -> Result<String, AdminError> {
    check_url("server.public_url", text)
}

fn check_url(setting: &str, text: &str) -> Result<String, AdminError> {
    let error = |why: &str| AdminError::Config(format!("{setting}: {why}, got `{text}`"));
    let url = url::Url::parse(text).map_err(|_| error("not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(error("must be an http:// or https:// address"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(error("must not have a query or fragment"));
    }
    Ok(text.trim_end_matches('/').to_owned())
}

/// Whether `url` may be used without TLS: only on this machine, for development and tests.
fn is_local(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}

/// A provider people can log in with.
pub(crate) struct Provider {
    pub(crate) name: String,
    pub(crate) label: String,
    client: OAuthClient,
    kind: Kind,
}

enum Kind {
    Oidc(Box<Oidc>),
    GitHub,
    Facebook,
}

struct Oidc {
    /// As configured, without a trailing slash.
    issuer: String,
    /// Whether an address the provider does not mark as verified may still be trusted.
    trust_email: bool,
    /// Microsoft's shared directories: the token's issuer names the account's own directory.
    microsoft_shared: bool,
    microsoft: bool,
    discovery: OnceCell<Discovery>,
    /// The provider's signing keys, for ID tokens an app sends; with when they were read.
    keys: std::sync::Mutex<Option<(Vec<jsonwebtoken::jwk::Jwk>, std::time::Instant)>>,
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
    /// Where the provider publishes the keys its ID tokens are signed with.
    #[serde(default)]
    jwks_uri: Option<String>,
}

impl Provider {
    fn new(method: AuthMethod) -> Result<Self, String> {
        let name = method.name().to_owned();
        let oidc = |issuer: String, trust_email, microsoft, microsoft_shared| {
            Kind::Oidc(Box::new(Oidc {
                issuer,
                trust_email,
                microsoft_shared,
                microsoft,
                discovery: OnceCell::new(),
                keys: std::sync::Mutex::new(None),
            }))
        };
        let (label, client, kind) = match method {
            AuthMethod::Password | AuthMethod::MagicLink => unreachable!("handled by the caller"),
            AuthMethod::Google(client) => (
                "Google".to_owned(),
                client,
                oidc(GOOGLE_ISSUER.to_owned(), false, false, false),
            ),
            AuthMethod::Microsoft { client, tenant } => {
                let tenant = tenant.trim().to_owned();
                if tenant.is_empty()
                    || !tenant
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
                {
                    return Err(format!(
                        "microsoft: tenant must be `common`, `organizations`, `consumers` or a \
                         directory ID, got `{tenant}`"
                    ));
                }
                let shared = MICROSOFT_SHARED_TENANTS.contains(&tenant.as_str());
                let issuer = format!("https://login.microsoftonline.com/{tenant}/v2.0");
                // One directory vouches for its own members' addresses; the shared ones
                // let anyone pick an address, unless `xms_edov` says it was verified.
                (
                    "Microsoft".to_owned(),
                    client,
                    oidc(issuer, !shared, true, shared),
                )
            }
            AuthMethod::GitHub(client) => ("GitHub".to_owned(), client, Kind::GitHub),
            AuthMethod::Facebook(client) => ("Facebook".to_owned(), client, Kind::Facebook),
            AuthMethod::Oidc(provider) => {
                let valid_name = provider
                    .name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase())
                    && provider
                        .name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                // Built-in methods, and the API's own addresses under /api/auth/.
                let reserved = [
                    "password",
                    "magic-link",
                    "google",
                    "microsoft",
                    "github",
                    "facebook",
                    "token",
                    "totp",
                    "refresh",
                    "logout",
                    "me",
                    "link",
                    "register",
                ];
                if !valid_name || reserved.contains(&provider.name.as_str()) {
                    return Err(format!(
                        "`{}` cannot name a provider: use lowercase letters, digits and `-`, \
                         and not the name of a built-in method",
                        provider.name
                    ));
                }
                if provider.label.trim().is_empty() {
                    return Err(format!("{}: label is empty", provider.name));
                }
                let issuer = url::Url::parse(provider.issuer.trim()).map_err(|_| {
                    format!(
                        "{}: issuer `{}` is not a URL",
                        provider.name, provider.issuer
                    )
                })?;
                if issuer.scheme() != "https" && !(issuer.scheme() == "http" && is_local(&issuer)) {
                    return Err(format!(
                        "{}: issuer must be an https:// address",
                        provider.name
                    ));
                }
                (
                    provider.label.trim().to_owned(),
                    provider.client,
                    oidc(
                        provider.issuer.trim().trim_end_matches('/').to_owned(),
                        provider.trust_email,
                        false,
                        false,
                    ),
                )
            }
        };
        if client.client_id.trim().is_empty() || client.client_secret.trim().is_empty() {
            return Err(format!(
                "{name}: client_id and client_secret must both be set"
            ));
        }
        Ok(Self {
            name,
            label,
            client,
            kind,
        })
    }

    /// The provider's endpoints, read once from its discovery document.
    async fn discovery<'a>(
        &self,
        oidc: &'a Oidc,
        http: &reqwest::Client,
    ) -> Result<&'a Discovery, String> {
        oidc.discovery
            .get_or_try_init(|| async {
                let url = format!("{}/.well-known/openid-configuration", oidc.issuer);
                let document: Discovery = get_json(http.get(&url)).await?;
                let expected = if oidc.microsoft_shared {
                    "https://login.microsoftonline.com/{tenantid}/v2.0"
                } else {
                    &oidc.issuer
                };
                if document.issuer.trim_end_matches('/') != expected.trim_end_matches('/') {
                    return Err(format!(
                        "{url} names the issuer `{}`, not `{expected}`",
                        document.issuer
                    ));
                }
                Ok(document)
            })
            .await
    }

    /// Where to send the browser to log in.
    async fn authorize_url(
        &self,
        http: &reqwest::Client,
        flow: &Flow,
        redirect_uri: &str,
    ) -> Result<String, String> {
        let (endpoint, scope) = match &self.kind {
            Kind::Oidc(oidc) => (
                self.discovery(oidc, http)
                    .await?
                    .authorization_endpoint
                    .clone(),
                "openid email profile",
            ),
            Kind::GitHub => (GITHUB_AUTHORIZE.to_owned(), "read:user user:email"),
            Kind::Facebook => (FACEBOOK_AUTHORIZE.to_owned(), "email public_profile"),
        };
        let mut url = url::Url::parse(&endpoint).map_err(|e| format!("{endpoint}: {e}"))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.client.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", scope)
                .append_pair("state", &flow.state)
                .append_pair("code_challenge", &pkce_challenge(&flow.verifier))
                .append_pair("code_challenge_method", "S256");
            if matches!(self.kind, Kind::Oidc(_)) {
                query.append_pair("nonce", &flow.nonce);
            }
        }
        Ok(url.into())
    }

    /// Trades the code for the identity of whoever logged in. `verifier` is the PKCE
    /// verifier the login started with, if it used one; `nonce` the one sent to the provider,
    /// if any, which the ID token must then carry.
    pub(crate) async fn identify(
        &self,
        http: &reqwest::Client,
        code: &str,
        verifier: Option<&str>,
        nonce: Option<&str>,
        redirect_uri: &str,
    ) -> Result<Identity, Failure> {
        let mut exchange = vec![
            ("client_id", self.client.client_id.as_str()),
            ("client_secret", self.client.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(verifier) = verifier {
            exchange.push(("code_verifier", verifier));
        }
        match &self.kind {
            Kind::Oidc(oidc) => {
                self.identify_oidc(oidc, http, code, verifier, nonce, redirect_uri)
                    .await
            }
            Kind::GitHub => {
                let token: Value = get_json(
                    http.post(GITHUB_TOKEN)
                        .header(header::ACCEPT, "application/json")
                        .form(&exchange),
                )
                .await
                .map_err(Failure::Broken)?;
                let access = access_token(&token)?;
                let api = |path: &str| {
                    http.get(format!("{GITHUB_API}{path}"))
                        .bearer_auth(&access)
                        .header(header::ACCEPT, "application/vnd.github+json")
                };
                let user: Value = get_json(api("/user")).await.map_err(Failure::Broken)?;
                let emails: Value = get_json(api("/user/emails"))
                    .await
                    .map_err(Failure::Broken)?;
                github_identity(&self.name, &user, &emails)
            }
            Kind::Facebook => {
                let token: Value = get_json(http.get(FACEBOOK_TOKEN).query(&exchange))
                    .await
                    .map_err(Failure::Broken)?;
                let access = access_token(&token)?;
                // Proves the request comes from the app's owner, not just a token holder.
                let mut mac = Hmac::<Sha256>::new_from_slice(self.client.client_secret.as_bytes())
                    .map_err(|e| Failure::Broken(e.to_string()))?;
                mac.update(access.as_bytes());
                let proof = hex(&mac.finalize().into_bytes());
                let me: Value = get_json(http.get(FACEBOOK_ME).query(&[
                    ("fields", "id,email,first_name,last_name"),
                    ("access_token", access.as_str()),
                    ("appsecret_proof", proof.as_str()),
                ]))
                .await
                .map_err(Failure::Broken)?;
                facebook_identity(&self.name, &me)
            }
        }
    }

    async fn identify_oidc(
        &self,
        oidc: &Oidc,
        http: &reqwest::Client,
        code: &str,
        verifier: Option<&str>,
        nonce: Option<&str>,
        redirect_uri: &str,
    ) -> Result<Identity, Failure> {
        let discovery = self.discovery(oidc, http).await.map_err(Failure::Broken)?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(verifier) = verifier {
            form.push(("code_verifier", verifier));
        }
        let methods = &discovery.token_endpoint_auth_methods_supported;
        // HTTP Basic is the standard's default; some providers only take the form fields.
        let basic = methods.is_empty() || methods.iter().any(|m| m == "client_secret_basic");
        let mut request = http.post(&discovery.token_endpoint);
        if basic {
            // RFC 6749 §2.3.1: both parts are form-encoded before Basic encoding.
            request = request.basic_auth(
                form_encode(&self.client.client_id),
                Some(form_encode(&self.client.client_secret)),
            );
            form.push(("client_id", self.client.client_id.as_str()));
        } else {
            form.push(("client_id", self.client.client_id.as_str()));
            form.push(("client_secret", self.client.client_secret.as_str()));
        }
        let token: Value = get_json(request.form(&form))
            .await
            .map_err(Failure::Broken)?;
        let id_token = token
            .get("id_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Failure::Broken(format!("no id_token in the token response: {token}"))
            })?;
        // The token came straight from the provider over TLS, which OpenID Connect accepts in
        // place of checking its signature (Core §3.1.3.7); every claim is still checked.
        let claims = jwt_payload(id_token)
            .ok_or_else(|| Failure::Broken("the id_token is not a JWT".into()))?;
        check_id_token(
            oidc,
            discovery,
            &[self.client.client_id.as_str()],
            // A login that sent no nonce still refuses a token that carries one.
            Some(nonce.unwrap_or_default()),
            &claims,
            unix_now(),
        )
        .map_err(Failure::Broken)?;
        let mut identity = oidc_identity(&self.name, oidc, &claims)?;
        // Some providers keep the address out of the token; ask for it.
        if identity.email.is_none()
            && let (Some(endpoint), Some(access)) = (
                discovery.userinfo_endpoint.as_deref(),
                token.get("access_token").and_then(Value::as_str),
            )
        {
            let info: Value = get_json(http.get(endpoint).bearer_auth(access))
                .await
                .map_err(Failure::Broken)?;
            if info.get("sub").and_then(Value::as_str) == Some(identity.subject.as_str()) {
                let more = oidc_identity(&self.name, oidc, &info)?;
                identity.email = more.email;
                identity.email_verified = more.email_verified;
                identity.first_name = identity.first_name.or(more.first_name);
                identity.last_name = identity.last_name.or(more.last_name);
            }
        }
        Ok(identity)
    }
}

// ---- ID tokens from apps -----------------------------------------------------------------------

/// How long signing keys are kept before they are read again.
const KEYS_SECS: u64 = 3600;

impl Provider {
    /// Whether this provider issues ID tokens an app can send (OpenID Connect providers).
    pub(crate) fn issues_id_tokens(&self) -> bool {
        matches!(self.kind, Kind::Oidc(_))
    }

    /// The identity in an ID token an app received from the provider. Unlike a token this
    /// server fetched itself, it came through the app, so its signature is checked against
    /// the provider's published keys, then every claim as usual. Tokens issued to
    /// `client_id` or one of [`OAuthClient::audiences`] are accepted; `nonce`, if given,
    /// must match.
    pub(crate) async fn identify_id_token(
        &self,
        http: &reqwest::Client,
        id_token: &str,
        nonce: Option<&str>,
    ) -> Result<Identity, Failure> {
        let Kind::Oidc(oidc) = &self.kind else {
            return Err(Failure::Broken(format!(
                "{} does not issue ID tokens",
                self.label
            )));
        };
        let discovery = self.discovery(oidc, http).await.map_err(Failure::Broken)?;
        let claims = self
            .verified_claims(oidc, discovery, http, id_token)
            .await
            .map_err(Failure::Broken)?;
        let audiences: Vec<&str> = std::iter::once(self.client.client_id.as_str())
            .chain(self.client.audiences.iter().map(String::as_str))
            .collect();
        check_id_token(oidc, discovery, &audiences, nonce, &claims, unix_now())
            .map_err(Failure::Broken)?;
        oidc_identity(&self.name, oidc, &claims)
    }

    /// The claims of `token` if a current key of the provider signed it.
    async fn verified_claims(
        &self,
        oidc: &Oidc,
        discovery: &Discovery,
        http: &reqwest::Client,
        token: &str,
    ) -> Result<Value, String> {
        use jsonwebtoken::{AlgorithmFamily, DecodingKey, Validation};
        let header =
            jsonwebtoken::decode_header(token).map_err(|e| format!("not an ID token: {e}"))?;
        // Only public-key signatures: an HMAC one would use the client secret, which an app
        // must not have.
        if header.alg.family() == AlgorithmFamily::Hmac {
            return Err(format!(
                "ID tokens signed with {:?} are refused",
                header.alg
            ));
        }
        let jwks_uri = discovery
            .jwks_uri
            .as_deref()
            .ok_or("the provider publishes no signing keys (no jwks_uri)")?;
        let mut key = None;
        for refresh in [false, true] {
            let keys = self.signing_keys(oidc, http, jwks_uri, refresh).await?;
            key = keys
                .iter()
                .find(|jwk| match (&header.kid, &jwk.common.key_id) {
                    (Some(wanted), Some(kid)) => wanted == kid,
                    // Without key IDs, a provider with a single key.
                    (None, _) => keys.len() == 1,
                    (Some(_), None) => false,
                })
                .cloned();
            // An unknown key may be a new one: read the keys again, once.
            if key.is_some() {
                break;
            }
        }
        let key = key.ok_or("the ID token is signed with a key the provider does not publish")?;
        let key = DecodingKey::from_jwk(&key).map_err(|e| format!("unusable signing key: {e}"))?;
        let mut validation = Validation::new(header.alg);
        // Checked by `check_id_token`, against every accepted client ID.
        validation.validate_aud = false;
        validation.leeway = SKEW_SECS;
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        jsonwebtoken::decode::<Value>(token, &key, &validation)
            .map(|data| data.claims)
            .map_err(|e| format!("the ID token's signature or dates are not valid: {e}"))
    }

    /// The provider's signing keys, read at most once an hour unless `refresh`.
    async fn signing_keys(
        &self,
        oidc: &Oidc,
        http: &reqwest::Client,
        jwks_uri: &str,
        refresh: bool,
    ) -> Result<Vec<jsonwebtoken::jwk::Jwk>, String> {
        if !refresh {
            let cached = oidc.keys.lock().map_err(|e| e.to_string())?;
            if let Some((keys, read)) = cached.as_ref()
                && read.elapsed() < Duration::from_secs(KEYS_SECS)
            {
                return Ok(keys.clone());
            }
        }
        let document: Value = get_json(http.get(jwks_uri)).await?;
        // Keys this library cannot use (encryption keys, unknown types) are skipped.
        let keys: Vec<jsonwebtoken::jwk::Jwk> = document
            .get("keys")
            .and_then(Value::as_array)
            .map(|keys| {
                keys.iter()
                    .filter(|key| key.get("use").and_then(Value::as_str) != Some("enc"))
                    .filter_map(|key| serde_json::from_value(key.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        *oidc.keys.lock().map_err(|e| e.to_string())? =
            Some((keys.clone(), std::time::Instant::now()));
        Ok(keys)
    }
}

// ---- identities ----------------------------------------------------------------------------

/// Who logged in, as a provider describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) provider: String,
    /// The provider's permanent identifier for the person.
    pub(crate) subject: String,
    /// Lower case.
    pub(crate) email: Option<String>,
    pub(crate) email_verified: bool,
    pub(crate) first_name: Option<String>,
    pub(crate) last_name: Option<String>,
}

/// Why a login through a provider did not work.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Logged; the person sees a general message.
    Broken(String),
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// `"Ada King Lovelace"` -> `("Ada King", "Lovelace")`.
fn split_name(name: Option<String>) -> (Option<String>, Option<String>) {
    match name.as_deref().map(str::trim) {
        Some(full) if !full.is_empty() => match full.rsplit_once(' ') {
            Some((first, last)) => (Some(first.trim().to_owned()), Some(last.to_owned())),
            None => (Some(full.to_owned()), None),
        },
        _ => (None, None),
    }
}

fn check_id_token(
    oidc: &Oidc,
    discovery: &Discovery,
    audiences: &[&str],
    nonce: Option<&str>,
    claims: &Value,
    now: u64,
) -> Result<(), String> {
    let issuer = text(claims, "iss").unwrap_or_default();
    let expected = if oidc.microsoft_shared {
        let tenant = text(claims, "tid").unwrap_or_default();
        discovery.issuer.replace("{tenantid}", &tenant)
    } else {
        discovery.issuer.clone()
    };
    if issuer.trim_end_matches('/') != expected.trim_end_matches('/') {
        return Err(format!("id_token issuer `{issuer}` is not `{expected}`"));
    }
    let ours = |aud: Option<&str>| aud.is_some_and(|aud| audiences.contains(&aud));
    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => ours(Some(aud)),
        Some(Value::Array(auds)) => {
            auds.iter().any(|aud| ours(aud.as_str()))
                && (auds.len() == 1 || ours(text(claims, "azp").as_deref()))
        }
        _ => false,
    };
    if !audience_ok {
        return Err("id_token is meant for another app".into());
    }
    let expires = claims.get("exp").and_then(Value::as_u64).unwrap_or(0);
    if expires + SKEW_SECS < now {
        return Err("id_token has expired".into());
    }
    if let Some(nonce) = nonce {
        let sent = text(claims, "nonce").unwrap_or_default();
        if !constant_time_eq(sent.as_bytes(), nonce.as_bytes()) {
            return Err("id_token nonce does not match this login".into());
        }
    }
    Ok(())
}

fn oidc_identity(provider: &str, oidc: &Oidc, claims: &Value) -> Result<Identity, Failure> {
    let subject =
        text(claims, "sub").ok_or_else(|| Failure::Broken("the identity has no `sub`".into()))?;
    let email = text(claims, "email").map(|email| email.to_lowercase());
    let verified = if claims.get("email_verified").is_some() {
        truthy(claims.get("email_verified"))
    } else if oidc.microsoft {
        oidc.trust_email || truthy(claims.get("xms_edov"))
    } else {
        oidc.trust_email
    };
    let (mut first_name, mut last_name) = (text(claims, "given_name"), text(claims, "family_name"));
    if first_name.is_none() && last_name.is_none() {
        (first_name, last_name) = split_name(text(claims, "name"));
    }
    Ok(Identity {
        provider: provider.to_owned(),
        subject,
        email_verified: verified && email.is_some(),
        email,
        first_name,
        last_name,
    })
}

fn github_identity(provider: &str, user: &Value, emails: &Value) -> Result<Identity, Failure> {
    let subject = user
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| Failure::Broken(format!("GitHub user without an id: {user}")))?
        .to_string();
    let listed = emails.as_array().map(Vec::as_slice).unwrap_or_default();
    let verified = |entry: &&Value| truthy(entry.get("verified"));
    // The primary address if it is verified, otherwise any verified one.
    let email = listed
        .iter()
        .filter(verified)
        .find(|entry| truthy(entry.get("primary")))
        .or_else(|| listed.iter().find(verified))
        .and_then(|entry| text(entry, "email"))
        .map(|email| email.to_lowercase());
    let (first_name, last_name) = split_name(text(user, "name"));
    Ok(Identity {
        provider: provider.to_owned(),
        subject,
        email_verified: email.is_some(),
        email,
        first_name,
        last_name,
    })
}

fn facebook_identity(provider: &str, me: &Value) -> Result<Identity, Failure> {
    let subject = text(me, "id")
        .ok_or_else(|| Failure::Broken(format!("Facebook profile without an id: {me}")))?;
    // Facebook only shares addresses its users have confirmed.
    let email = text(me, "email").map(|email| email.to_lowercase());
    Ok(Identity {
        provider: provider.to_owned(),
        subject,
        email_verified: email.is_some(),
        email,
        first_name: text(me, "first_name"),
        last_name: text(me, "last_name"),
    })
}

fn access_token(response: &Value) -> Result<String, Failure> {
    text(response, "access_token").ok_or_else(|| {
        Failure::Broken(format!("no access_token in the token response: {response}"))
    })
}

/// Sends `request` and reads a JSON answer, turning HTTP errors into a message.
async fn get_json<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, String> {
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let url = response.url().clone();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let mut excerpt: String = body.chars().take(300).collect();
        if excerpt.len() < body.len() {
            excerpt.push('…');
        }
        return Err(format!("{url} answered {status}: {excerpt}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("{url} sent something that is not JSON: {e}"))
}

/// The claims of a JWT, without checking its signature.
fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn form_encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- the browser's round trip ----------------------------------------------------------------

/// What the server remembers between sending the browser away and its return.
#[derive(Serialize, Deserialize)]
struct Flow {
    provider: String,
    state: String,
    nonce: String,
    verifier: String,
    exp: u64,
}

pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let b = state.base.clone();
    Router::new()
        .route(&format!("{b}/auth/{{provider}}/start"), get(start))
        .route(&format!("{b}/auth/{{provider}}/callback"), get(callback))
        .with_state(state)
}

type Shared = State<Arc<AdminState>>;

fn flow_cookie_path(state: &AdminState) -> String {
    format!("{}/auth/", state.base)
}

async fn start(
    State(state): Shared,
    Path(name): Path<String>,
    jar: CookieJar,
) -> Result<Response, AdminError> {
    let methods = &state.auth.methods;
    let provider = methods
        .provider(&name)
        .ok_or_else(|| AdminError::NotFound(format!("login method `{name}`")))?;
    let flow = Flow {
        provider: name,
        state: random_token(),
        nonce: random_token(),
        verifier: random_token(),
        exp: unix_now() + FLOW_SECS,
    };
    let redirect_uri = methods.redirect_uri(&state.base, provider);
    let target = match methods.http() {
        Ok(http) => provider.authorize_url(http, &flow, &redirect_uri).await,
        Err(problem) => Err(problem),
    };
    let target = match target {
        Ok(target) => target,
        Err(problem) => {
            tracing::warn!(provider = %provider.name, %problem, "login provider unavailable");
            return crate::handlers::login_error(
                &state,
                StatusCode::BAD_GATEWAY,
                format!(
                    "{} is not answering. Try again in a moment.",
                    provider.label
                ),
            );
        }
    };
    // `Lax`, not `Strict`: the browser must send it back when the provider redirects here.
    let cookie = Cookie::build((FLOW_COOKIE, state.auth.seal(FLOW_PURPOSE, &flow)?))
        .path(flow_cookie_path(&state))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.auth.cookie_secure)
        .build();
    Ok((jar.add(cookie), Redirect::to(&target)).into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): Shared,
    Path(name): Path<String>,
    jar: CookieJar,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, AdminError> {
    let methods = &state.auth.methods;
    let provider = methods
        .provider(&name)
        .ok_or_else(|| AdminError::NotFound(format!("login method `{name}`")))?;
    let flow = jar
        .get(FLOW_COOKIE)
        .and_then(|cookie| state.auth.open::<Flow>(FLOW_PURPOSE, cookie.value()))
        .filter(|flow| flow.provider == name);
    // Each round trip is used once, whatever happens next.
    let jar = jar.remove(Cookie::build((FLOW_COOKIE, "")).path(flow_cookie_path(&state)));
    let refuse = |status: StatusCode, message: String| -> Result<Response, AdminError> {
        let page = crate::handlers::login_error(&state, status, message)?;
        Ok((jar.clone(), page).into_response())
    };

    let Some(flow) = flow else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "That login took too long or was started elsewhere. Try again.".into(),
        );
    };
    if query.error.is_some() {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("Logging in with {} was cancelled.", provider.label),
        );
    }
    let code = match (&query.state, &query.code) {
        (Some(sent), Some(code)) if constant_time_eq(sent.as_bytes(), flow.state.as_bytes()) => {
            code
        }
        _ => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "That login link is not valid. Try again.".into(),
            );
        }
    };

    let redirect_uri = methods.redirect_uri(&state.base, provider);
    let identity = match methods.http() {
        Ok(http) => {
            provider
                .identify(
                    http,
                    code,
                    Some(&flow.verifier),
                    Some(&flow.nonce),
                    &redirect_uri,
                )
                .await
        }
        Err(problem) => Err(Failure::Broken(problem)),
    };
    let identity = match identity {
        Ok(identity) => identity,
        Err(Failure::Broken(problem)) => {
            tracing::warn!(provider = %provider.name, %problem, "login through provider failed");
            return refuse(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Logging in with {} did not work. Try again in a moment.",
                    provider.label
                ),
            );
        }
    };

    let db = state.accounts.as_ref().ok_or_else(|| {
        AdminError::Config("logging in through providers needs the built-in accounts".into())
    })?;
    let account = match crate::users::resolve_identity(
        db,
        methods,
        &provider.label,
        &identity,
        true,
    )
    .await?
    {
        Ok(account) => account,
        Err(message) => {
            tracing::warn!(provider = %provider.name, email = ?identity.email, "login through provider refused");
            return refuse(StatusCode::FORBIDDEN, message);
        }
    };
    crate::login::complete(&state, jar, &account.username, true).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn oidc(trust_email: bool, microsoft: bool, shared: bool) -> Oidc {
        Oidc {
            issuer: "https://id.example.com".into(),
            trust_email,
            microsoft_shared: shared,
            microsoft,
            discovery: OnceCell::new(),
            keys: std::sync::Mutex::new(None),
        }
    }

    fn discovery(issuer: &str) -> Discovery {
        Discovery {
            issuer: issuer.into(),
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            userinfo_endpoint: None,
            token_endpoint_auth_methods_supported: Vec::new(),
            jwks_uri: None,
        }
    }

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn id_token_claims_are_all_checked() {
        let provider = oidc(false, false, false);
        let doc = discovery("https://id.example.com");
        let good = json!({"iss": "https://id.example.com", "aud": "app", "exp": 1000, "nonce": "n1", "sub": "s"});
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &good, 900).is_ok());
        // Slightly late clocks are tolerated, expired tokens are not.
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &good, 1050).is_ok());
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &good, 1100).is_err());
        assert!(check_id_token(&provider, &doc, &["other"], Some("n1"), &good, 900).is_err());
        assert!(check_id_token(&provider, &doc, &["app"], Some("n2"), &good, 900).is_err());
        let mut forged = good.clone();
        forged["iss"] = json!("https://evil.example.com");
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &forged, 900).is_err());
        let mut shared = good.clone();
        shared["aud"] = json!(["app", "other"]);
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &shared, 900).is_err());
        shared["azp"] = json!("app");
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &shared, 900).is_ok());
        // Any of the accepted client IDs, such as a mobile app's.
        let mut mobile = good.clone();
        mobile["aud"] = json!("ios");
        assert!(check_id_token(&provider, &doc, &["app", "ios"], Some("n1"), &mobile, 900).is_ok());
        assert!(check_id_token(&provider, &doc, &["app"], Some("n1"), &mobile, 900).is_err());
        // No nonce to compare: an app that sent none.
        assert!(check_id_token(&provider, &doc, &["app"], None, &good, 900).is_ok());
    }

    #[test]
    fn microsoft_shared_tenants_accept_the_accounts_own_directory() {
        let provider = oidc(false, true, true);
        let doc = discovery("https://login.microsoftonline.com/{tenantid}/v2.0");
        let claims = json!({
            "iss": "https://login.microsoftonline.com/abc-123/v2.0", "tid": "abc-123",
            "aud": "app", "exp": 1000, "nonce": "n", "sub": "s", "email": "Ada@Example.com"
        });
        assert!(check_id_token(&provider, &doc, &["app"], Some("n"), &claims, 900).is_ok());
        let mut other = claims.clone();
        other["tid"] = json!("def-456");
        assert!(check_id_token(&provider, &doc, &["app"], Some("n"), &other, 900).is_err());

        // Without `xms_edov`, a shared-directory address is not trusted.
        let identity = oidc_identity("microsoft", &provider, &claims).unwrap();
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
        assert!(!identity.email_verified);
        let mut vouched = claims.clone();
        vouched["xms_edov"] = json!(true);
        assert!(
            oidc_identity("microsoft", &provider, &vouched)
                .unwrap()
                .email_verified
        );
        // One directory vouches for its own members.
        assert!(
            oidc_identity("microsoft", &oidc(true, true, false), &claims)
                .unwrap()
                .email_verified
        );
    }

    #[test]
    fn oidc_addresses_count_only_when_verified() {
        let provider = oidc(false, false, false);
        let claims = json!({"sub": "1", "email": "a@b.c", "email_verified": "true", "name": "Ada King Lovelace"});
        let identity = oidc_identity("okta", &provider, &claims).unwrap();
        assert!(identity.email_verified);
        assert_eq!(identity.first_name.as_deref(), Some("Ada King"));
        assert_eq!(identity.last_name.as_deref(), Some("Lovelace"));
        let unverified = json!({"sub": "1", "email": "a@b.c", "email_verified": false});
        assert!(
            !oidc_identity("okta", &provider, &unverified)
                .unwrap()
                .email_verified
        );
        let silent = json!({"sub": "1", "email": "a@b.c"});
        assert!(
            !oidc_identity("okta", &provider, &silent)
                .unwrap()
                .email_verified
        );
        assert!(
            oidc_identity("okta", &oidc(true, false, false), &silent)
                .unwrap()
                .email_verified
        );
        assert!(oidc_identity("okta", &provider, &json!({"email": "a@b.c"})).is_err());
    }

    #[test]
    fn github_uses_the_primary_verified_address() {
        let user = json!({"id": 42, "login": "ada", "name": "Ada Lovelace"});
        let emails = json!([
            {"email": "old@example.com", "primary": false, "verified": true},
            {"email": "Ada@Example.com", "primary": true, "verified": true},
        ]);
        let identity = github_identity("github", &user, &emails).unwrap();
        assert_eq!(identity.subject, "42");
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
        assert!(identity.email_verified);
        assert_eq!(identity.last_name.as_deref(), Some("Lovelace"));

        let unverified_primary = json!([
            {"email": "new@example.com", "primary": true, "verified": false},
            {"email": "old@example.com", "primary": false, "verified": true},
        ]);
        let identity = github_identity("github", &user, &unverified_primary).unwrap();
        assert_eq!(identity.email.as_deref(), Some("old@example.com"));

        let none = json!([{"email": "new@example.com", "primary": true, "verified": false}]);
        let identity = github_identity("github", &user, &none).unwrap();
        assert_eq!(identity.email, None);
        assert!(!identity.email_verified);
    }

    #[test]
    fn facebook_profile_is_read() {
        let me = json!({"id": "1017", "email": "Ada@Example.com", "first_name": "Ada", "last_name": "Lovelace"});
        let identity = facebook_identity("facebook", &me).unwrap();
        assert_eq!(identity.subject, "1017");
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
        assert!(identity.email_verified);
        let no_email = facebook_identity("facebook", &json!({"id": "1"})).unwrap();
        assert!(!no_email.email_verified);
    }

    #[test]
    fn settings_mistakes_are_named() {
        let google = || AuthMethod::Google(OAuthClient::new("id", "secret"));
        let check = |methods: Vec<AuthMethod>, url: Option<&str>| match Methods::new(
            methods,
            Signup::Closed,
            Vec::new(),
            url,
        ) {
            Err(AdminError::Config(message)) => message,
            Err(other) => panic!("{other}"),
            Ok(_) => String::new(),
        };
        assert!(check(vec![], None).contains("no login method"));
        assert!(check(vec![google()], None).starts_with("server.public_url is not set"));
        assert!(check(vec![google()], Some("ftp://x")).starts_with("server.public_url"));
        assert!(check(vec![google(), google()], Some("http://localhost")).contains("twice"));
        assert!(check(vec![AuthMethod::Password, AuthMethod::Password], None).contains("twice"));
        assert!(
            check(
                vec![AuthMethod::GitHub(OAuthClient::new("", "x"))],
                Some("http://localhost")
            )
            .contains("client_id")
        );
        let custom = |name: &str, issuer: &str| {
            AuthMethod::Oidc(crate::settings::OidcProvider::new(
                name,
                "Acme",
                issuer,
                OAuthClient::new("id", "secret"),
            ))
        };
        assert!(
            check(
                vec![custom("Acme", "https://id.acme.com")],
                Some("http://localhost")
            )
            .contains("cannot name")
        );
        assert!(
            check(
                vec![custom("google", "https://id.acme.com")],
                Some("http://localhost")
            )
            .contains("cannot name")
        );
        assert!(
            check(
                vec![custom("acme", "http://id.acme.com")],
                Some("http://localhost")
            )
            .contains("https://")
        );
        assert!(
            check(
                vec![custom("acme", "http://127.0.0.1:9000")],
                Some("http://localhost")
            )
            .is_empty()
        );
        assert!(
            check(
                vec![custom("acme", "https://id.acme.com")],
                Some("https://shop.example.com/")
            )
            .is_empty()
        );

        let domains = Methods::new(
            vec![AuthMethod::Password],
            Signup::Domains(vec!["@Acme.com".into()]),
            Vec::new(),
            None,
        )
        .unwrap();
        assert!(domains.signup_allows("ada@acme.com"));
        assert!(!domains.signup_allows("ada@acme.com.evil.io"));
        assert!(!domains.signup_allows("ada@notacme.com"));
    }
}

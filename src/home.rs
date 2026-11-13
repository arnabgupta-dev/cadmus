//! The public page at `/`: whether the service is up, nothing more. It links nowhere, so it
//! does not point visitors at the admin or the API documentation.
//!
//! The same URL answers monitors: ask for `application/json` to get the status as data.
//! Both answer `503` when the database cannot be reached, so a load balancer can use it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::Request;
use axum::http::header::{ACCEPT, ALLOW, CACHE_CONTROL};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use minijinja::context;
use minijinja::value::Serde;
use sea_orm::DatabaseConnection;
use serde_json::json;

use crate::AdminState;
use crate::error::AdminError;

/// How long the database may take to answer before it counts as down.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Settings for the home page. Build in code, or load with [`HomeConfig::from_env`].
#[derive(Debug, Clone)]
pub struct HomeConfig {
    enabled: bool,
    title: Option<String>,
    tagline: Option<String>,
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            title: None,
            tagline: None,
        }
    }
}

impl HomeConfig {
    /// The home page on, titled after the site.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the settings from the environment.
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `HOME_PAGE_ENABLED` | `false` leaves `/` to your own routes (default `true`) |
    /// | `HOME_TITLE` | Name shown on the page (default: the site title) |
    /// | `HOME_TAGLINE` | One line under the name |
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if `HOME_PAGE_ENABLED` is not a boolean.
    pub fn from_env() -> Result<Self, AdminError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`HomeConfig::from_env`] but reads from `lookup`.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if a value is invalid.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, AdminError> {
        let set = |key: &str| lookup(key).filter(|v| !v.trim().is_empty());
        let mut config = Self::new();
        if let Some(value) = set("HOME_PAGE_ENABLED") {
            config.enabled = match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => {
                    return Err(AdminError::Config(
                        "HOME_PAGE_ENABLED must be true or false".into(),
                    ));
                }
            };
        }
        config.title = set("HOME_TITLE");
        config.tagline = set("HOME_TAGLINE");
        Ok(config)
    }

    /// Turns the page on or off. Off, `/` and browser visits to the API behave as before.
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The name shown on the page.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// One line under the name.
    #[must_use]
    pub fn tagline(mut self, tagline: impl Into<String>) -> Self {
        self.tagline = Some(tagline.into());
        self
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// What the page needs to answer.
pub(crate) struct Home {
    config: HomeConfig,
    state: Arc<AdminState>,
    db: DatabaseConnection,
    started: Instant,
}

impl Home {
    pub(crate) fn new(config: HomeConfig, state: Arc<AdminState>, db: DatabaseConnection) -> Self {
        Self {
            config,
            state,
            db,
            started: Instant::now(),
        }
    }
}

/// Answers everything no route matched: the home page at `/`, an empty `404` elsewhere (which
/// the error handling then formats for its area).
pub(crate) async fn fallback(home: Arc<Home>, request: Request) -> Response {
    if request.uri().path() != "/" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    let wants_json = request
        .headers()
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json") && !accept.contains("text/html"));
    let mut response = respond(&home, wants_json).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    crate::server::protect(&mut response);
    response
}

async fn respond(home: &Home, wants_json: bool) -> Response {
    let begun = Instant::now();
    let reachable = matches!(
        tokio::time::timeout(PING_TIMEOUT, home.db.ping()).await,
        Ok(Ok(()))
    );
    let latency_ms = (begun.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
    let uptime_secs = home.started.elapsed().as_secs();
    let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, code) = if reachable {
        ("operational", StatusCode::OK)
    } else {
        tracing::warn!("home page: the database did not answer");
        ("degraded", StatusCode::SERVICE_UNAVAILABLE)
    };

    if wants_json {
        let database = if reachable {
            json!({ "status": "connected", "latency_ms": latency_ms })
        } else {
            json!({ "status": "unreachable" })
        };
        let body = json!({
            "status": status,
            "database": database,
            "uptime_secs": uptime_secs,
            "checked_at": checked_at,
        });
        return (code, Json(body)).into_response();
    }

    let settings = &home.state.settings;
    let title = home
        .config
        .title
        .clone()
        .unwrap_or_else(|| settings.site_title.clone());
    let page = home
        .state
        .templates
        .get_template("home.html")
        .and_then(|t| {
            t.render(context! {
                site => Serde(settings),
                title,
                tagline => &home.config.tagline,
                operational => reachable,
                latency_ms,
                uptime_secs,
                uptime => human_duration(uptime_secs),
                checked_at,
            })
        });
    match page {
        Ok(html) => (code, Html(html)).into_response(),
        Err(error) => AdminError::from(error).into_response(),
    }
}

/// `93784` -> `1d 2h 3m`; the two largest units only.
fn human_duration(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m {}s", secs % 60),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::human_duration;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(125), "2m 5s");
        assert_eq!(human_duration(3_700), "1h 1m");
        assert_eq!(human_duration(93_784), "1d 2h");
    }
}

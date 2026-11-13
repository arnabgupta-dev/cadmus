//! What the app needs around its routes to be put on the internet: a time limit per request,
//! a line in the log per request, cross-origin access for browser clients, and the response
//! headers that keep the admin out of frames and caches.

use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, REFERRER_POLICY,
    RETRY_AFTER, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::error::AdminError;

/// Answers `503` if the handler has not produced a response in time, and stops running it.
///
/// Only the handler is timed, not the sending of the response body, so downloads and other
/// streamed responses are not cut short.
pub(crate) async fn timeout(
    State(limit): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(%method, %path, limit_secs = limit.as_secs_f32(), "request timed out");
            AdminError::custom(
                StatusCode::SERVICE_UNAVAILABLE,
                "timeout",
                "the request took too long",
            )
            .into_response()
        }
    }
}

/// Logs one line per request: method, path, status and how long it took.
///
/// The query string is left out, since it can carry search terms and other user input.
pub(crate) async fn access_log(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());
    let response = next.run(request).await;
    tracing::info!(
        %method,
        %path,
        status = response.status().as_u16(),
        ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "request"
    );
    response
}

/// Headers for every response from the admin and the API.
///
/// - `nosniff`: browsers must not guess a different content type than the one sent.
/// - No framing: another site cannot show the admin inside a frame and trick a logged-in
///   user into clicking (clickjacking).
/// - `same-origin` referrer: admin URLs are not sent to sites the admin links to.
/// - `no-store`: pages and data that needed a login are not kept in caches.
///
/// A header the handler set itself is left alone.
pub(crate) fn protect(response: &mut Response) {
    let defaults = [
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (X_FRAME_OPTIONS, "DENY"),
        (CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        (REFERRER_POLICY, "same-origin"),
        (CACHE_CONTROL, "no-store"),
    ];
    for (name, value) in defaults {
        response
            .headers_mut()
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
}

/// Parses `CORS_ALLOWED_ORIGINS`: `*`, or origins such as `https://app.example.com` separated
/// by commas.
///
/// # Errors
/// [`AdminError::Config`] for anything that is not an origin (scheme, host and optional port).
pub(crate) fn parse_origins(value: &str) -> Result<Vec<String>, AdminError> {
    let mut origins = Vec::new();
    for origin in value.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        if origin != "*" {
            let parsed = url::Url::parse(origin).ok();
            let exact = parsed.is_some_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.origin().ascii_serialization() == origin
            });
            if !exact {
                return Err(AdminError::Config(format!(
                    "`{origin}` is not an origin: write it like https://app.example.com, with no path or trailing slash"
                )));
            }
        }
        origins.push(origin.to_owned());
    }
    Ok(origins)
}

/// Lets pages served from `origins` call this app from the browser.
///
/// Cookies are never allowed across origins: browser clients authenticate with the bearer
/// token, and the admin's cookie stays usable from the admin's own pages only.
pub(crate) fn cors(origins: &[String]) -> CorsLayer {
    let allowed = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
    };
    let upload_offset = HeaderName::from_static("upload-offset");
    CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE, upload_offset.clone()])
        .expose_headers([LOCATION, RETRY_AFTER, upload_offset])
        .max_age(Duration::from_secs(600))
}

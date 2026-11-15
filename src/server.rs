//! What the app needs around its routes to be put on the internet: a time limit per request,
//! a line in the log per request, cross-origin access for browser clients, the response
//! headers that keep the admin out of frames and caches and its pages to their own scripts,
//! HTTPS-only (HSTS), and compression of API responses.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION,
    REFERRER_POLICY, RETRY_AFTER, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate};
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

/// Where browsers that open an API URL are sent instead, and which URLs under the API path
/// are pages meant for browsers.
pub(crate) struct BrowserRedirect {
    pub(crate) api_base: String,
    pub(crate) pages: Vec<String>,
}

/// Sends a person who opens an API URL in the browser to the home page. Programs calling the
/// API are unaffected: anything that sends a token, or is not a top-level page visit (such
/// as `fetch` from a page or `curl`), reaches the API as usual.
pub(crate) async fn browser_redirect(
    State(rules): State<Arc<BrowserRedirect>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let under_api = path
        .strip_prefix(&rules.api_base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    let headers = request.headers();
    let visit = matches!(*request.method(), Method::GET | Method::HEAD)
        && under_api
        && !rules.pages.iter().any(|page| page == path)
        && !headers.contains_key(AUTHORIZATION)
        && match headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) {
            // Browsers say how a request was made; a typed URL or a link is `navigate`.
            Some(mode) => mode == "navigate",
            // Older browsers do not; they still ask for HTML, which API clients do not.
            None => headers
                .get(ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|accept| accept.contains("text/html")),
        };
    if visit {
        return Redirect::to("/").into_response();
    }
    next.run(request).await
}

/// Headers for every response from the admin and the API.
///
/// - `nosniff`: browsers must not guess a different content type than the one sent.
/// - No framing: another site cannot show the admin inside a frame and trick a logged-in
///   user into clicking (clickjacking).
/// - `same-origin` referrer: admin URLs are not sent to sites the admin links to.
/// - `same-origin` opener: a page the admin opens cannot reach back into it.
/// - `no-store`: pages and data that needed a login are not kept in caches.
///
/// A header the handler set itself is left alone.
pub(crate) fn protect(response: &mut Response) {
    let defaults = [
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (X_FRAME_OPTIONS, "DENY"),
        (CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        (REFERRER_POLICY, "same-origin"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (CACHE_CONTROL, "no-store"),
    ];
    for (name, value) in defaults {
        response
            .headers_mut()
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
}

struct Nonce {
    value: String,
    used: Cell<bool>,
}

tokio::task_local! {
    /// The current request's script nonce, while [`with_csp`] runs it.
    static NONCE: Nonce;
}

/// The nonce a page's `<script>` tags must carry. Asking for it is what turns the policy on
/// for the response ([`with_csp`]); outside a request it is empty.
pub(crate) fn csp_nonce() -> String {
    NONCE
        .try_with(|nonce| {
            nonce.used.set(true);
            nonce.value.clone()
        })
        .unwrap_or_default()
}

/// The Content Security Policy for a page rendered with `nonce`: scripts only from the site
/// itself and from the page's own `<script nonce>` tags, so text that slips into a page
/// cannot run. Styles, fonts and images may come from elsewhere (Google Fonts, a logo or a
/// stylesheet of yours, images in rich text).
fn page_policy(nonce: &str) -> String {
    format!(
        "default-src 'self'; script-src 'self' 'nonce-{nonce}'; \
         style-src 'self' 'unsafe-inline' https:; font-src 'self' https: data:; \
         img-src * data: blob:; connect-src 'self'; object-src 'none'; base-uri 'self'; \
         frame-ancestors 'none'"
    )
}

/// Runs the request with a fresh script nonce. A response whose page asked for it (see
/// [`csp_nonce`]) gets the Content Security Policy that names it.
pub(crate) async fn with_csp<F: Future<Output = Response>>(run: F) -> Response {
    // Hex, so that it reads the same in the header and in the HTML attribute.
    let value: String = rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let nonce = Nonce {
        value: value.clone(),
        used: Cell::new(false),
    };
    let (mut response, used) = NONCE
        .scope(nonce, async {
            let response = run.await;
            (response, NONCE.with(|nonce| nonce.used.get()))
        })
        .await;
    if used && let Ok(policy) = HeaderValue::from_str(&page_policy(&value)) {
        response
            .headers_mut()
            .insert(CONTENT_SECURITY_POLICY, policy);
    }
    response
}

/// Tells browsers to use only HTTPS for this site from now on (HSTS), unless the response
/// says otherwise itself. Browsers ignore the header on plain-HTTP responses.
pub(crate) async fn hsts(
    State(value): State<HeaderValue>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry(STRICT_TRANSPORT_SECURITY)
        .or_insert(value);
    response
}

/// Marks a response as safe to compress.
///
/// Only REST API responses (and the admin's own script files) are. Admin pages are not: they
/// carry a CSRF token next to text the visitor controls, which is what the BREACH attack
/// needs. API requests carry their credentials in a header that browsers never add by
/// themselves, so another site cannot make a browser send the many requests BREACH needs.
#[derive(Clone, Copy)]
pub(crate) struct Compressible;

/// Marks the REST API's responses [`Compressible`], except file downloads: those are often
/// compressed already, and may be large.
pub(crate) async fn mark_api(
    State(api_base): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let api = path
        .strip_prefix(&*api_base)
        .is_some_and(|rest| rest.starts_with('/') && !rest.ends_with("/content"));
    let mut response = next.run(request).await;
    if api {
        response.extensions_mut().insert(Compressible);
    }
    response
}

/// Compresses responses marked [`Compressible`] with gzip or Brotli, as the client accepts.
/// Tiny bodies, images and event streams are left alone.
pub(crate) fn compression() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new()
        .gzip(true)
        .br(true)
        .compress_when(DefaultPredicate::new().and(Marked))
}

#[derive(Clone, Copy)]
struct Marked;

impl Predicate for Marked {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        response.extensions().get::<Compressible>().is_some()
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

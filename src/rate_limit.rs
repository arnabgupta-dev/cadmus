//! Request rate limits: how many requests one caller may make in a period.
//!
//! A caller is an account (logged in with a token or the admin cookie), an API key, or, for
//! everyone else, the client's address. Each has its own allowance, which refills smoothly
//! (the "generic cell rate algorithm": one timestamp per caller). Limits are configured with
//! [`crate::settings::RateLimitSettings`]; the counters live in a [`RateLimitStore`], by
//! default [`MemoryStore`], in this process's memory.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;

use crate::AdminState;
use crate::auth::{CheckedApiKey, credentials};
use crate::error::{AdminError, ApiError};

/// How many requests are allowed in how long, e.g. `Rate::per_minute(120)`.
///
/// Requests may come all at once, up to the count; the allowance then refills evenly over
/// the period. It parses from text such as `120/min`, `10/s`, `5000/hour` or `20000/day`, so
/// it can come from the environment with `env_or("RATE_LIMIT_ANONYMOUS", ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    count: u32,
    period: Duration,
}

impl Rate {
    /// `count` requests per `period`. A count of zero, or a zero period, allows nothing.
    #[must_use]
    pub const fn new(count: u32, period: Duration) -> Self {
        Self { count, period }
    }

    /// `count` requests per second.
    #[must_use]
    pub const fn per_second(count: u32) -> Self {
        Self::new(count, Duration::from_secs(1))
    }

    /// `count` requests per minute.
    #[must_use]
    pub const fn per_minute(count: u32) -> Self {
        Self::new(count, Duration::from_secs(60))
    }

    /// `count` requests per hour.
    #[must_use]
    pub const fn per_hour(count: u32) -> Self {
        Self::new(count, Duration::from_secs(3600))
    }

    /// `count` requests per day.
    #[must_use]
    pub const fn per_day(count: u32) -> Self {
        Self::new(count, Duration::from_secs(86_400))
    }

    /// How many requests the period allows.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// The period.
    #[must_use]
    pub const fn period(&self) -> Duration {
        self.period
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.period.as_secs();
        match secs {
            1 => write!(f, "{}/s", self.count),
            60 => write!(f, "{}/min", self.count),
            3600 => write!(f, "{}/hour", self.count),
            86_400 => write!(f, "{}/day", self.count),
            _ => write!(f, "{}/{}s", self.count, secs),
        }
    }
}

/// Why a text is not a [`Rate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRateError(String);

impl fmt::Display for ParseRateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a rate; write it like `120/min`, `10/s`, `5000/hour` or `20000/day`",
            self.0
        )
    }
}

impl std::error::Error for ParseRateError {}

impl FromStr for Rate {
    type Err = ParseRateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || ParseRateError(text.to_owned());
        let (count, unit) = text.trim().split_once('/').ok_or_else(error)?;
        let count: u32 = count.trim().parse().map_err(|_| error())?;
        let unit = unit.trim().to_ascii_lowercase();
        // `10/5min` style: a number before the unit multiplies it.
        let digits = unit.chars().take_while(char::is_ascii_digit).count();
        let (times, unit) = unit.split_at(digits);
        let times: u64 = if times.is_empty() {
            1
        } else {
            times.parse().map_err(|_| error())?
        };
        let secs = match unit {
            "s" | "sec" | "second" | "seconds" => 1,
            "m" | "min" | "minute" | "minutes" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            _ => return Err(error()),
        };
        if times == 0 {
            return Err(error());
        }
        Ok(Self::new(count, Duration::from_secs(secs * times)))
    }
}

/// A limit for some paths, overriding the defaults; see
/// [`crate::settings::RateLimitSettings::rules`].
#[derive(Debug, Clone)]
pub struct RateRule {
    prefix: String,
    methods: Vec<Method>,
    rate: Rate,
}

impl RateRule {
    /// `rate` for every caller of the paths starting with `path` (`/api/orders` covers
    /// `/api/orders` and `/api/orders/12`, but not `/api/orders-archive`).
    #[must_use]
    pub fn new(path: impl Into<String>, rate: Rate) -> Self {
        Self {
            prefix: path.into().trim_end_matches('/').to_owned(),
            methods: Vec::new(),
            rate,
        }
    }

    /// Only for these methods, e.g. `["POST", "PATCH"]` (default: all).
    ///
    /// # Panics
    /// If a name is not a valid HTTP method.
    #[must_use]
    pub fn methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.methods = methods
            .into_iter()
            .map(|m| {
                Method::from_bytes(m.as_ref().to_ascii_uppercase().as_bytes())
                    .unwrap_or_else(|_| panic!("`{}` is not an HTTP method", m.as_ref()))
            })
            .collect();
        self
    }

    fn matches(&self, method: &Method, path: &str) -> bool {
        under(path, &self.prefix) && (self.methods.is_empty() || self.methods.contains(method))
    }
}

/// What a [`RateLimitStore`] decided about one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may go ahead.
    pub allowed: bool,
    /// The limit: the rate's count.
    pub limit: u32,
    /// Requests left right now.
    pub remaining: u32,
    /// Seconds until the full allowance is back.
    pub reset_secs: u64,
    /// For a refused request, seconds until one more is allowed.
    pub retry_after_secs: u64,
}

/// Where the counters live. [`MemoryStore`] keeps them in this process; with several
/// instances behind a load balancer, implement this over a shared store such as Redis so
/// that they count together, and pass it to [`crate::App::rate_limit_store`].
#[async_trait]
pub trait RateLimitStore: Send + Sync {
    /// Counts one request of the caller `key` against `rate`, and says whether it is allowed.
    ///
    /// # Errors
    /// A failure of the store. The request is then let through (and the error logged), so
    /// that an unreachable store does not take the site down.
    async fn hit(&self, key: &str, rate: Rate) -> Result<Decision, AdminError>;
}

/// Beyond this many callers, those whose allowance is full again are forgotten; if that is
/// not enough, all are. It bounds memory when someone sprays addresses.
const MAX_TRACKED: usize = 100_000;

/// Counters in this process's memory: the default [`RateLimitStore`]. A restart clears them,
/// and each instance counts on its own.
#[derive(Default)]
pub struct MemoryStore {
    /// For each caller, when its allowance will be full again (the "theoretical arrival
    /// time" of the cell rate algorithm).
    full_at: Mutex<HashMap<String, Instant>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn hit_at(&self, key: &str, rate: Rate, now: Instant) -> Decision {
        if rate.count == 0 || rate.period.is_zero() {
            return Decision {
                allowed: false,
                limit: 0,
                remaining: 0,
                reset_secs: 0,
                retry_after_secs: rate.period.as_secs().max(1),
            };
        }
        // Each request uses up `interval`; up to `period` may be used up at once.
        let interval = rate.period / rate.count;
        let mut full_at = self
            .full_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if full_at.len() >= MAX_TRACKED && !full_at.contains_key(key) {
            full_at.retain(|_, at| *at > now);
            if full_at.len() >= MAX_TRACKED {
                full_at.clear();
            }
        }
        let current = full_at
            .get(key)
            .copied()
            .filter(|at| *at > now)
            .unwrap_or(now);
        let next = current + interval;
        let used = next - now;
        if used > rate.period {
            let wait = used - rate.period;
            return Decision {
                allowed: false,
                limit: rate.count,
                remaining: 0,
                reset_secs: ceil_secs(current - now),
                retry_after_secs: ceil_secs(wait).max(1),
            };
        }
        full_at.insert(key.to_owned(), next);
        let left = rate.period - used;
        Decision {
            allowed: true,
            limit: rate.count,
            remaining: u32::try_from(left.as_nanos() / interval.as_nanos().max(1))
                .unwrap_or(u32::MAX),
            reset_secs: ceil_secs(used),
            retry_after_secs: 0,
        }
    }
}

#[async_trait]
impl RateLimitStore for MemoryStore {
    async fn hit(&self, key: &str, rate: Rate) -> Result<Decision, AdminError> {
        Ok(self.hit_at(key, rate, Instant::now()))
    }
}

fn ceil_secs(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

/// The limits in force, as the app was configured.
#[derive(Clone)]
pub(crate) struct RateLimits {
    pub(crate) enabled: bool,
    pub(crate) anonymous: Rate,
    pub(crate) logged_in: Rate,
    pub(crate) admin: Option<Rate>,
    pub(crate) rules: Vec<RateRule>,
    pub(crate) store: Arc<dyn RateLimitStore>,
}

impl Default for RateLimits {
    fn default() -> Self {
        let settings = crate::settings::RateLimitSettings::default();
        Self {
            enabled: settings.enabled,
            anonymous: settings.anonymous,
            logged_in: settings.logged_in,
            admin: settings.admin,
            rules: settings.rules,
            store: Arc::new(MemoryStore::new()),
        }
    }
}

/// Who is asking, as far as limits go.
enum Caller {
    /// A login: the account's username.
    Account(String),
    /// A valid API key: its digest.
    Key(String),
    /// Anyone else, by address.
    Address(IpAddr),
}

impl Caller {
    fn key(&self) -> String {
        match self {
            Self::Account(username) => format!("user:{username}"),
            Self::Key(digest) => format!("key:{}", &digest[..digest.len().min(16)]),
            Self::Address(address) => format!("ip:{address}"),
        }
    }

    fn logged_in(&self) -> bool {
        !matches!(self, Self::Address(_))
    }
}

/// Finds out who sent the request, checking its credentials only as far as needed: a login
/// token's signature (no database), or an API key (one lookup, which is kept for the
/// handler). A forged or expired credential counts as no credential, so it is limited by
/// address like any visitor.
async fn caller(state: &AdminState, parts: &mut Parts) -> Option<Caller> {
    let (token, api_key) = credentials(&parts.headers);
    if let Some(key) = api_key {
        if state.auth.api_keys
            && let Some(digest) = crate::auth::api_key_digest(key)
        {
            let owner = match state.users.api_key(&digest).await {
                Ok(owner) => owner,
                Err(error) => {
                    tracing::warn!(%error, "rate limit: cannot look up an API key");
                    None
                }
            };
            let valid = owner.is_some();
            parts.extensions.insert(CheckedApiKey {
                digest: digest.clone(),
                owner,
            });
            if valid {
                return Some(Caller::Key(digest));
            }
        }
    } else if let Some(token) = token {
        if let Some(claims) = state.auth.verify(token) {
            return Some(Caller::Account(claims.sub));
        }
    } else if let Some(claims) = crate::auth::cookie_claims(&parts.headers, state) {
        return Some(Caller::Account(claims.sub));
    }
    state.auth.client_ip(parts).map(Caller::Address)
}

/// Whether `path` is `base` or below it.
fn under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Which allowance a request uses: a rule's, a model's, the admin's or the default one, and
/// the name of that allowance (so that each has its own counter).
fn allowance(
    state: &AdminState,
    limits: &RateLimits,
    method: &Method,
    path: &str,
    caller: &Caller,
) -> Option<(Rate, String)> {
    if let Some((index, rule)) = limits
        .rules
        .iter()
        .enumerate()
        .find(|(_, rule)| rule.matches(method, path))
    {
        return Some((rule.rate, format!("rule{index}")));
    }
    if let Some(rest) = path.strip_prefix(&state.api_base)
        && let Some(slug) = rest.strip_prefix('/').and_then(|r| r.split('/').next())
        && let Some(resource) = state.resources.iter().find(|r| r.slug() == slug)
        && let Some(rate) = resource.options().rate_limit
    {
        return Some((rate, format!("model:{slug}")));
    }
    if under(path, &state.base) && !under(path, &state.api_base) {
        return limits.admin.map(|rate| (rate, "admin".to_owned()));
    }
    let rate = if caller.logged_in() {
        limits.logged_in
    } else {
        limits.anonymous
    };
    Some((rate, "default".to_owned()))
}

static LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
static REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
static RESET: HeaderName = HeaderName::from_static("ratelimit-reset");

/// The names of the headers that describe a caller's allowance, for CORS.
pub(crate) fn header_names() -> [HeaderName; 3] {
    [LIMIT.clone(), REMAINING.clone(), RESET.clone()]
}

fn describe(response: &mut Response, decision: &Decision) {
    let headers = response.headers_mut();
    headers.insert(LIMIT.clone(), HeaderValue::from(decision.limit));
    headers.insert(REMAINING.clone(), HeaderValue::from(decision.remaining));
    headers.insert(RESET.clone(), HeaderValue::from(decision.reset_secs));
}

/// Counts the request against `rate` for `caller_key` under `name`: `Ok` with the
/// allowance to describe (`None` if the store failed), or `Err` with the refusal's decision.
async fn check(
    store: &dyn RateLimitStore,
    name: &str,
    caller_key: &str,
    rate: Rate,
) -> Result<Option<Decision>, Decision> {
    match store.hit(&format!("{name}|{caller_key}"), rate).await {
        Ok(decision) if decision.allowed => Ok(Some(decision)),
        Ok(decision) => Err(decision),
        Err(error) => {
            tracing::warn!(%error, "rate limit store failed; letting the request through");
            Ok(None)
        }
    }
}

/// `429`, as JSON under the API and as text elsewhere.
fn refusal(decision: &Decision, api: bool) -> Response {
    let error = AdminError::RateLimited {
        retry_after_secs: decision.retry_after_secs,
    };
    let mut response = if api {
        ApiError::from(error).into_response()
    } else {
        error.into_response()
    };
    describe(&mut response, decision);
    response
}

/// The app-wide limits: every request but CORS preflights counts against the caller's
/// allowance.
pub(crate) async fn middleware(
    State(state): State<Arc<AdminState>>,
    request: Request,
    next: Next,
) -> Response {
    let limits = &state.rate_limits;
    if !limits.enabled || request.method() == Method::OPTIONS {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let Some(caller) = caller(&state, &mut parts).await else {
        // No credential and no address (a router served without connection info): nothing
        // to count by.
        return next.run(Request::from_parts(parts, body)).await;
    };
    let path = parts.uri.path().to_owned();
    let Some((rate, name)) = allowance(&state, limits, &parts.method, &path, &caller) else {
        return next.run(Request::from_parts(parts, body)).await;
    };
    let api = under(&path, &state.api_base);
    let decision = match check(&*limits.store, &name, &caller.key(), rate).await {
        Ok(decision) => decision,
        Err(refused) => return refusal(&refused, api),
    };
    let mut response = next.run(Request::from_parts(parts, body)).await;
    // A route's own `RateLimit` has already described its allowance, which is the one
    // that matters there.
    if let Some(decision) = decision
        && !response.headers().contains_key(&LIMIT)
    {
        describe(&mut response, &decision);
    }
    response
}

/// A limit of its own for routes of yours, on top of the app-wide one:
///
/// ```ignore
/// Router::new()
///     .route("/sms/send", post(send_code))
///     .layer(cadmus::RateLimit::new("sms", Rate::per_minute(5)))
/// ```
///
/// Each caller (account, API key or address, as for the app-wide limits) gets `rate` under
/// the name, so two layers with different names count separately. It works in routes given
/// to [`crate::App::routes`] (or passed through [`crate::Admin::identify`]).
#[derive(Clone)]
pub struct RateLimit {
    name: Arc<str>,
    rate: Rate,
}

impl RateLimit {
    /// `rate` per caller, counted under `name`.
    #[must_use]
    pub fn new(name: impl Into<String>, rate: Rate) -> Self {
        Self {
            name: format!("route:{}", name.into()).into(),
            rate,
        }
    }
}

impl<S> tower::Layer<S> for RateLimit {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limit: self.clone(),
        }
    }
}

/// The service made by [`RateLimit`].
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    limit: RateLimit,
}

impl<S> tower::Service<Request> for RateLimitService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // The clone that was polled ready is the one that is called.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let limit = self.limit.clone();
        Box::pin(async move {
            let Some(crate::auth::Identify(state)) =
                request.extensions().get::<crate::auth::Identify>().cloned()
            else {
                tracing::warn!("RateLimit only works in routes given to App::routes; not limiting");
                return inner.call(request).await;
            };
            if request.method() == Method::OPTIONS {
                return inner.call(request).await;
            }
            let (mut parts, body) = request.into_parts();
            let Some(caller) = caller(&state, &mut parts).await else {
                return inner.call(Request::from_parts(parts, body)).await;
            };
            let api = under(parts.uri.path(), &state.api_base);
            let store = Arc::clone(&state.rate_limits.store);
            match check(&*store, &limit.name, &caller.key(), limit.rate).await {
                Ok(decision) => {
                    let mut response = inner.call(Request::from_parts(parts, body)).await?;
                    if let Some(decision) = decision {
                        describe(&mut response, &decision);
                    }
                    Ok(response)
                }
                Err(refused) => Ok(refusal(&refused, api)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_from_text() {
        assert_eq!("120/min".parse(), Ok(Rate::per_minute(120)));
        assert_eq!(" 10 / s ".parse(), Ok(Rate::per_second(10)));
        assert_eq!("5000/hour".parse(), Ok(Rate::per_hour(5000)));
        assert_eq!("20000/day".parse(), Ok(Rate::per_day(20000)));
        assert_eq!(
            "10/5min".parse(),
            Ok(Rate::new(10, Duration::from_secs(300)))
        );
        for bad in ["", "120", "abc/min", "10/fortnight", "10/0min", "-1/min"] {
            assert!(bad.parse::<Rate>().is_err(), "{bad}");
        }
        assert_eq!(Rate::per_minute(120).to_string(), "120/min");
        assert_eq!(
            Rate::new(10, Duration::from_secs(300)).to_string(),
            "10/300s"
        );
    }

    #[test]
    fn a_burst_up_to_the_count_then_an_even_refill() {
        let store = MemoryStore::new();
        let rate = Rate::per_minute(3);
        let start = Instant::now();
        for left in [2, 1, 0] {
            let d = store.hit_at("a", rate, start);
            assert!(d.allowed);
            assert_eq!(d.remaining, left);
        }
        let refused = store.hit_at("a", rate, start);
        assert!(!refused.allowed);
        assert_eq!(refused.retry_after_secs, 20);
        assert_eq!(refused.reset_secs, 60);
        // Another caller is not affected.
        assert!(store.hit_at("b", rate, start).allowed);
        // One request comes back every 20 seconds.
        assert!(
            !store
                .hit_at("a", rate, start + Duration::from_secs(19))
                .allowed
        );
        let back = store.hit_at("a", rate, start + Duration::from_secs(20));
        assert!(back.allowed);
        assert_eq!(back.remaining, 0);
        // After the whole period, the whole allowance.
        let full = store.hit_at("a", rate, start + Duration::from_secs(120));
        assert_eq!(full.remaining, 2);
    }

    #[test]
    fn a_zero_rate_allows_nothing() {
        let store = MemoryStore::new();
        assert!(
            !store
                .hit_at("a", Rate::per_minute(0), Instant::now())
                .allowed
        );
    }

    #[test]
    fn refused_requests_do_not_use_up_the_allowance() {
        let store = MemoryStore::new();
        let rate = Rate::per_second(1);
        let start = Instant::now();
        assert!(store.hit_at("a", rate, start).allowed);
        for _ in 0..10 {
            assert!(!store.hit_at("a", rate, start).allowed);
        }
        assert!(
            store
                .hit_at("a", rate, start + Duration::from_secs(1))
                .allowed
        );
    }

    #[test]
    fn rules_match_paths_and_methods() {
        let rule = RateRule::new("/api/orders/", Rate::per_minute(1)).methods(["post"]);
        assert!(rule.matches(&Method::POST, "/api/orders"));
        assert!(rule.matches(&Method::POST, "/api/orders/12"));
        assert!(!rule.matches(&Method::GET, "/api/orders"));
        assert!(!rule.matches(&Method::POST, "/api/orders-archive"));
    }
}

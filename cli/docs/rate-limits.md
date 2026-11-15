---
title: Rate limits
section: Guides
order: 155
summary: How many requests each caller may make, per route, per model and for your own routes, and sharing the counters between instances.
---

Every request to the REST API and to your own routes counts against the caller's
allowance. Over it, the answer is `429 Too Many Requests` with a `Retry-After` header (JSON
under the API: `{"error": {"code": "rate_limited", ...}}`). This is on by default.

## Who counts as one caller

| Caller | Default allowance |
|---|---|
| An account that is logged in (a token, or the admin's cookie) | 600 requests a minute |
| An API key | 600 requests a minute, separate from its account's logins |
| Anyone else, by client address | 120 requests a minute |

A token or key that is not valid counts as no credential, so made-up tokens cannot get
around the address limit. The allowance can be used all at once and refills evenly: with
120 a minute, a caller that used it up gets one more request every half second.

Every counted response says where the caller stands:

```text
RateLimit-Limit: 120
RateLimit-Remaining: 117
RateLimit-Reset: 2
```

`RateLimit-Reset` is the seconds until the whole allowance is back. Browsers on other origins
can read these headers (see `server.cors_origins`).

**Behind a reverse proxy**, every request seems to come from the proxy, so all visitors
share one allowance. Set `server.client_ip_header` (e.g. `X-Forwarded-For`), as for
[login attempt limits](two-factor.md#login-attempt-limits).

## Changing the limits

In `src/settings.rs`:

```rust
rate_limit: RateLimitSettings {
    anonymous: env_or("RATE_LIMIT_ANONYMOUS", Rate::per_minute(120))?,
    logged_in: env_or("RATE_LIMIT_LOGGED_IN", Rate::per_minute(600))?,
    // Admin pages are not limited unless you set this:
    admin: Some(Rate::per_minute(300)),
    rules: vec![
        // Stricter for placing orders, more generous for search.
        RateRule::new("/api/orders", Rate::per_minute(10)).methods(["POST"]),
        RateRule::new("/api/search", Rate::per_minute(1200)),
    ],
    ..Default::default()
},
```

- Rates are `Rate::per_second(n)`, `per_minute`, `per_hour`, `per_day`, or
  `Rate::new(n, Duration::from_secs(...))`. From the environment they are written `120/min`,
  `10/s`, `5000/hour`, `20000/day` or `10/5min`.
- A rule covers the paths starting with its path (`/api/orders` and `/api/orders/12`, not
  `/api/orders-archive`), for all methods unless `.methods(...)` says. The first rule that
  matches is used, instead of the defaults, with its own counter, for every caller alike.
- `enabled: false` turns request limits off. Login attempts keep their own, stricter limit
  (`auth.login_max_attempts`).

### For one model

A model's REST API endpoints can have their own limit, in `src/models/mod.rs`:

```rust
.model::<report::Entity>(ModelOptions::new().rate_limit(Rate::per_minute(20)))
```

### For your own routes

Your routes count against the app-wide allowance like everything else. To give one a limit
of its own on top of that, add the `RateLimit` layer in `src/routes.rs`:

```rust
use cadmus::{Rate, RateLimit};

Router::new()
    .route("/sms/send", post(send_code))
    .layer(RateLimit::new("sms", Rate::per_minute(5)))
```

Each name has its own counters, per caller. The response's headers then describe this
limit.

## Not counted

- Browsers' cross-origin preflight requests (`OPTIONS`).
- The admin's pages, unless `admin` is set.
- Requests whose address is unknown: a router you serve yourself without connection
  information, with no `client_ip_header`.

## Several instances

Counters live in each server process's memory: with several instances behind a load
balancer, each counts on its own (a caller gets the allowance once per instance), and a
restart clears them. To count together, keep them in a shared store such as Redis by
implementing `RateLimitStore` and passing it to the app in `src/main.rs`:

```rust
let app = App::from_settings(settings::settings()?)
    .await?
    .rate_limit_store(RedisStore::new(&env("REDIS_URL")?).await?);
```

A sketch with the [`redis`](https://docs.rs/redis) crate, counting in fixed windows (adapt
it to your version of the crate):

```rust
use async_trait::async_trait;
use axum::http::StatusCode;
use cadmus::{AdminError, Decision, Rate, RateLimitStore};

pub struct RedisStore(redis::aio::ConnectionManager);

impl RedisStore {
    pub async fn new(url: &str) -> Result<Self, AdminError> {
        let client = redis::Client::open(url).map_err(unavailable)?;
        Ok(Self(client.get_connection_manager().await.map_err(unavailable)?))
    }
}

fn unavailable(error: redis::RedisError) -> AdminError {
    AdminError::custom(StatusCode::SERVICE_UNAVAILABLE, "unavailable", error.to_string())
}

#[async_trait]
impl RateLimitStore for RedisStore {
    async fn hit(&self, key: &str, rate: Rate) -> Result<Decision, AdminError> {
        let period = rate.period().as_secs().max(1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let key = format!("ratelimit:{key}:{}", now / period);
        let (count, ttl): (u32, i64) = redis::pipe()
            .atomic()
            .incr(&key, 1)
            .expire(&key, period as i64)
            .ignore()
            .ttl(&key)
            .query_async(&mut self.0.clone())
            .await
            .map_err(unavailable)?;
        let reset_secs = ttl.max(1) as u64;
        let allowed = count <= rate.count();
        Ok(Decision {
            allowed,
            limit: rate.count(),
            remaining: rate.count().saturating_sub(count),
            reset_secs,
            retry_after_secs: if allowed { 0 } else { reset_secs },
        })
    }
}
```

If the store fails, the request is let through and the error logged, so an unreachable
Redis does not take the site down.

## Related

- [Login attempt limits](two-factor.md#login-attempt-limits).
- [Settings](settings.md).
- [Deploying](deploying.md).

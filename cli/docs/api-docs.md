---
title: API documentation
section: Guides
order: 170
summary: The interactive documentation every project serves at /api/docs, protecting it, and adding your own routes.
---

Every project documents its own API:

| Address | What it is |
|---|---|
| `/api/docs` | Swagger UI: browse the endpoints and try them out |
| `/api/redoc` | ReDoc: the same, as a reference page |
| `/api/openapi.json` | the OpenAPI 3.0 description, for generating client code |

It is generated from the models: every endpoint, the fields returned, the fields that may be
sent and which are required, the permission each needs, and file uploads when they are on.
Hidden fields never appear, and read-only fields appear only in what is returned.

## Trying requests

In Swagger UI, press **Authorize** and log in with a username and password, or paste an API
key under `apiKey`. Accounts with two-factor login can paste a token from
`POST /api/auth/token` under `bearer`.

## Protecting it

The documentation is public by default; it shows model and field names, never data. To ask
for a username and password first (the browser's own prompt), set both, with the password in
`.env`:

```rust
docs: DocsSettings {
    username: Some("docs".into()),
    password: Some(env("DOCS_PASSWORD")?),
    ..Default::default()
},
```

These only open the documentation; they are not an account. Guesses are limited like
logins. `enabled: false` removes the documentation entirely.

## Generating clients

```sh
cargo run -- openapi > openapi.json
```

writes the description without starting the server, for client generators such as
`openapi-generator` or `openapi-typescript`.

## Your own routes

They are not discovered automatically. Add them to the description:

```rust
App::from_settings(settings::settings()?)
    .await?
    .docs(DocsConfig::new().extend(|doc| {
        doc["paths"]["/health"] = serde_json::json!({
            "get": { "summary": "Health check", "security": [], "responses": { "200": { "description": "ok" } } }
        });
    }))
```

## Other settings

- `docs.title`, `docs.version` and `docs.description` (Markdown) set the heading.
- The viewers load from a CDN (jsDelivr, pinned versions), so the two pages need internet
  access in the browser. To host them yourself, set `docs.swagger_js_url`,
  `docs.swagger_css_url` and `docs.redoc_js_url`. `openapi.json` needs nothing external.

---
title: The REST API
section: Guides
order: 60
summary: The JSON endpoints every model gets - listing, searching, filtering, sorting, creating, changing, deleting, and errors.
---

Everything in the admin can also be done by a program, over HTTP with JSON. Every model gets
these endpoints under `/api`, with the model's table name in the path:

| Request | What it does |
|---|---|
| `GET /api/post` | list posts |
| `POST /api/post` | create one; answers `201` with the new record and a `Location` header |
| `GET /api/post/1` | read one |
| `PUT /api/post/1` | replace one: every required field must be sent |
| `PATCH /api/post/1` | change only the fields sent |
| `DELETE /api/post/1` | delete one; answers `204` |

Every request needs a login token or an API key; see [Logging in](authentication.md) and, for
the users of your own app, [Logins for apps](app-logins.md). The
API follows the same permissions as the admin.

## Listing

```sh
curl -H "Authorization: Bearer $TOKEN" "http://localhost:3000/api/post?q=rust&sort=-id&page=2&per_page=50"
```

```json
{
  "data": [{ "id": 12, "title": "Rust tips", "published": true, "views": 40 }],
  "meta": { "total": 31, "page": 2, "per_page": 50 }
}
```

| Parameter | Meaning |
|---|---|
| `q` | search in the model's `search_fields` (ignores case) |
| `sort` | a field to sort by; `-` in front for highest first (`sort=-views`) |
| `page` | which page, from 1 |
| `per_page` | how many per page, at most 100 |

## Filtering

Any visible field can be filtered on, with the same choices as the admin's filters:

| Example | Matches |
|---|---|
| `?published=true` | an exact value |
| `?author_id=3&published=false` | several at once (all must match) |
| `?views__gt=100`, `__gte`, `__lt`, `__lte` | comparisons |
| `?created_at__gte=2026-01-01` | dates and date-times compare too |
| `?summary__isnull=true` | empty (or `false`: not empty) |
| `?created_at__range=7d` | `today`, `7d`, `30d`, `month` or `year` (UTC) |

An unknown field, or a value the field cannot hold, is refused with `422`. A field named
`page`, `per_page`, `q` or `sort` cannot be filtered on.

## Creating and changing

```sh
curl -X POST http://localhost:3000/api/post \
     -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
     -d '{"title": "Hello", "body": "First post", "published": false, "views": 0}'
```

- Dates and times are ISO 8601: `"2026-03-04"`, `"2026-03-04T05:06:07"`.
- Links to other models are sent as the id: `"author_id": 3`; many-to-many fields as a list
  of ids: `"tags": [1, 3]` (see [Relations](relations.md#many-to-many)).
- Decimals are read and written as text, so no digit is lost: `"price": "19.99"`. UUIDs are
  text too; a UUID primary key left out is made up.
- Lists (PostgreSQL arrays) are JSON arrays: `"labels": ["red", "green"]`.
- A field with choices refuses any other value.
- Rich-text fields are HTML strings, cleaned of scripts, styles and unknown tags before they
  are stored (see [Rich text](models.md#rich-text)).
- Fields that are unknown, hidden or read-only are refused rather than silently ignored.
- `PUT` needs every required field; `PATCH` changes only what is sent.

## Errors

Errors come back as JSON:

```json
{ "error": { "code": "invalid_field", "message": "title is required", "field": "title" } }
```

| Status | Code | Meaning |
|---|---|---|
| `400` | `bad_request` | the request is malformed |
| `401` | `unauthorized` | no valid token or key was sent |
| `403` | `forbidden` | the account may not do this |
| `404` | `not_found` | no such record or model |
| `409` | `conflict` | conflicts with other data: a duplicate, or a record others point to |
| `413` | `too_large` | an upload over the size limit |
| `422` | `invalid_field` | a field is missing, unknown, read-only or of the wrong type |
| `429` | `rate_limited` | too many login attempts; wait for the `Retry-After` seconds |

## Calling it from a browser app

A web app on another domain needs permission to call the API: set
`server.cors_origins: vec!["https://app.example.com".into()]` in the settings. Such apps
send a token or an API key; the admin's login cookie is never accepted by the API.

## Documentation

Every project documents its own API at `/api/docs` (Swagger UI), `/api/redoc` and
`/api/openapi.json`. See [API documentation](api-docs.md).

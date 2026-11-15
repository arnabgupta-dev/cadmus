---
title: Deploying
section: Guides
order: 190
summary: Building a release, running it on a server behind HTTPS, and the checklist before going live.
---

A Cadmus project builds into one executable. Copy it to the server with your settings in
environment variables, run its migrations, and start it.

## Building

```sh
cargo build --release
# the program is target/release/<project name>
```

On the server:

```sh
export DATABASE_URL=postgres://...
export SECRET_KEY=...
./blog migrate          # before starting each new version
./blog serve
```

## What is already taken care of

- **Time limit:** a request that takes longer than 30 seconds gets `503` and is stopped
  (`server.request_timeout`; `None` for no limit). Uploads and downloads through the API
  are exempt; a file sent on an admin form is not (see
  [File uploads](file-uploads.md#limits)).
- **Request log:** one line per request with method, address, status and duration (the
  query string is left out).
- **Protective headers** on every admin and API response: no embedding in other sites, no
  content-type guessing, no caching of pages that needed a login, no reaching back from
  pages the admin opens (`Cross-Origin-Opener-Policy`).
- **Content Security Policy** on admin pages: scripts run only from the site itself and
  from the page's own `<script>` tags, which carry a new random nonce on every response. Text
  that slips into a page cannot run.
- **HTTPS only (HSTS):** every response tells browsers to use only HTTPS for the site for a
  year (`server.hsts`; `None` to not send it). Browsers ignore it over plain HTTP, so it does
  not get in the way on your machine; but once a browser has seen it over HTTPS, it will not
  go back to HTTP for that site for a year.
- **Compression** of REST API responses (see [Compression](#compression)).
- **Cross-origin access (CORS)** is off until you set `server.cors_origins`.
- **Graceful shutdown** on Ctrl-C and on the stop signal sent by `docker stop`, systemd
  and Kubernetes: requests in progress finish first.
- **Health check:** `/` answers `503` when the database is down (see [Settings](settings.md)).

## Before going live

1. **HTTPS.** Serve over HTTPS (a reverse proxy such as nginx or Caddy, or your platform's),
   and leave `DEBUG` unset: the generated settings only allow the login cookie over plain
   HTTP when `DEBUG=true`.
2. **Behind a proxy,** set `server.client_ip_header` (see
   [Two-factor login and limits](two-factor.md)), and `server.bind` to `0.0.0.0:3000` if the
   proxy runs on another machine or container.
3. **Secrets.** Set `DATABASE_URL` and `SECRET_KEY` on the server, keep `.env` out of git,
   and use a different `SECRET_KEY` for each environment (`cadmus secret` prints one).
   Changing it logs everyone out.
4. **Migrations.** Run `migrate` once per deployment, before starting the new version (see
   [Migrations](#migrations) below).
5. **Uploads.** If you use the local folder backend, keep that folder on storage that
   survives redeploys, or use S3, Azure or Google Cloud.
6. **E-mail.** Set `MAIL_URL` and `PUBLIC_URL` so login links and password resets work.
7. **Several copies of the server?** Login attempt limits are counted per copy; limit the
   login addresses at the proxy too (see [Two-factor login and limits](two-factor.md)).

## Migrations

`migrate` does not take a lock, so two copies running it at the same moment can both try
to apply the same migration. With a single server, running it just before `serve` (as in the
container below) is fine. With several copies, run it **once**, as its own step:

- a release or pre-deploy command (Fly.io `release_command`, Heroku `release`, Render's
  pre-deploy command),
- a Kubernetes `Job`, or a Helm `pre-upgrade` hook, that finishes before the new pods start,
- a step in your CI/CD pipeline, before the rollout.

During a rolling deployment the old version keeps running against the new tables for a
while, so make each deployment's changes ones the old version survives: add a field
(nullable, or with a default) in one deployment and start using it in the next; stop using a
field in one deployment and remove it in the next. Back up the database before a migration
that removes or rewrites data.

## Containers

A minimal `Dockerfile`:

```dockerfile
FROM rust:1 AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:stable-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/blog /usr/local/bin/blog
ENV APP_BIND=0.0.0.0:3000
EXPOSE 3000
CMD ["sh", "-c", "blog migrate && exec blog serve"]
```

This runs `migrate` every time the container starts, which suits one container. With
several, take `blog migrate &&` out and run it once per deployment (see
[Migrations](#migrations)).

The generated settings read the address to listen on from `APP_BIND`; inside a container it
must be `0.0.0.0:3000` to be reachable from outside.

## Compression

REST API responses are compressed with gzip or Brotli when the client accepts it
(`Accept-Encoding`); `server.compress_api: false` turns that off. File downloads, images and
tiny responses are left as they are.

Admin pages are not compressed, on purpose: they contain a security token next to text the
visitor controls (the search box), which is what the BREACH attack on compressed HTTPS needs.
API requests are safe from it because their credentials travel in a header that browsers
never add on their own, so another site cannot make a browser send the many requests the
attack needs. If your proxy compresses responses, have it skip `/admin` pages.

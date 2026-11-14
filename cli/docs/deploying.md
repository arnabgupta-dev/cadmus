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
  (`server.request_timeout`; `None` for no limit). Uploads and downloads are exempt.
- **Request log:** one line per request with method, address, status and duration (the
  query string is left out).
- **Protective headers** on every admin and API response: no embedding in other sites, no
  content-type guessing, no caching of pages that needed a login.
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
4. **Migrations.** Run `migrate` as part of each deployment, before starting the new version.
5. **Uploads.** If you use the local folder backend, keep that folder on storage that
   survives redeploys, or use S3, Azure or Google Cloud.
6. **E-mail.** Set `MAIL_URL` and `PUBLIC_URL` so login links and password resets work.

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

The generated settings read the address to listen on from `APP_BIND`; inside a container it
must be `0.0.0.0:3000` to be reachable from outside.

## Compression

Responses are not compressed, on purpose: admin pages contain a security token next to text
the visitor controls (the search box), which is what the BREACH attack on compressed HTTPS
needs. Let your proxy compress static files only, if anything.

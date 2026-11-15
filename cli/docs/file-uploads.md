---
title: File uploads
section: Guides
order: 140
summary: Storing files on disk or in S3, Azure or Google Cloud - file fields on forms, uploads through the API, and resumable uploads.
---

Set `uploads.storage_url` in the settings to turn uploads on. The address chooses where
files are kept:

| Address | Where | Cargo feature |
|---|---|---|
| `file:///absolute/path` | a folder on the server | built in |
| `s3://bucket/prefix` | Amazon S3 and compatible services | `s3` |
| `az://container/prefix` | Azure Blob Storage | `azure` |
| `gs://bucket/prefix` | Google Cloud Storage | `gcs` |

```rust
uploads: UploadSettings {
    storage_url: Some("file:///var/lib/blog/media".into()),
    ..Default::default()
},
```

Cloud credentials are read from the usual environment variables (`AWS_ACCESS_KEY_ID`,
`AWS_REGION`, `AWS_ENDPOINT`, `AZURE_STORAGE_ACCOUNT_NAME`, ...). For the cloud backends,
turn the feature on: `cadmus = { version = "0.2", features = ["s3"] }`.

## Limits

`uploads.max_file_bytes` (default 1 GiB) and `uploads.max_chunk_bytes` (default 8 MiB, for
resumable uploads). Whether the time limit (`server.request_timeout`, 30 seconds by default)
applies depends on the way in:

| Way in | Largest request | Time limit |
|---|---|---|
| A file field on an admin form | `max_file_bytes` | yes: the whole form must arrive in time |
| `POST /api/files` | `max_file_bytes` | no |
| `POST /api/uploads`, each `PATCH` piece, finishing | `max_chunk_bytes` per piece | no |
| `GET /api/files/{id}/content` (downloads) | | no |
| Your own routes | axum's default (2 MiB) unless you set one | yes |

So very large files should go through the API, ideally as resumable uploads.

## File fields on forms

```sh
cadmus generate model Product name:string photo:file
```

A file field holds a file's id. On the admin form it is a file picker that shows the current
file, a preview for PNG, JPEG, GIF, WebP and AVIF images, and "Remove the file". Lists show
the file's name, linked to it. Anyone who may view the record may download its file. Files
from a form that is then rejected are removed.

A hand-written integer field becomes a file field with `ModelOptions::file_fields(["photo"])`.

Files sent through an admin form must finish within `server.request_timeout` (see
[Limits](#limits)), so very large files should go through the API. Replacing or removing a file on a
form leaves the old one stored (it is still listed at `/api/files`).

## Through the API

| Request | What it does |
|---|---|
| `POST /api/files` (multipart form, field `file`) | uploads a file; answers `201` with its record |
| `GET /api/files`, `GET /api/files/{id}` | file details |
| `GET /api/files/{id}/content` | downloads the file |
| `DELETE /api/files/{id}` | deletes it |

To fill a file field through the API, upload the file, then send its `id` in the field.
Uploads need the `files` permissions (see [Accounts and permissions](accounts.md)).

## Resumable uploads

Large files can be sent in pieces, and the upload continues after a dropped connection:

1. `POST /api/uploads` with `{"filename", "size", "content_type"}` gives an upload `id`.
2. `PATCH /api/uploads/{id}` with the header `Upload-Offset: N` and the next bytes, in
   order. A wrong offset gets `409` with the right one.
3. `GET /api/uploads/{id}` tells how much has arrived: that is where a client resumes.
4. The piece that completes the file answers `201` with the file's record.
5. `DELETE /api/uploads/{id}` cancels.

Uploads survive server restarts and can continue on another server. Stored names are chosen
by the server; the original file name is kept only as a label.

## In your own code

`Storage` (`put`, `get`, `delete`, `sink`) and `Uploads` are available for your own
handlers; see the [API reference](https://docs.rs/cadmus).

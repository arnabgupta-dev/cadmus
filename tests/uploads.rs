//! Upload tests: multipart, resumable chunked uploads, limits and hostile input.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use cadmus::{App, AuthConfig, Storage, Uploads, create_superuser};
use http_body_util::BodyExt;
use sea_orm::DatabaseConnection;
use tower::ServiceExt;

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const PASSWORD: &str = "correct horse battery staple";
const BOUNDARY: &str = "----test-boundary-7MA4YWxkTrZu0gW";

mod common;
use common::database;

/// Builds a router over `db` and `uploads`; migrating and creating the account only once.
async fn router(db: &DatabaseConnection, uploads: Uploads, first: bool) -> Router {
    let app = App::new(db.clone(), AuthConfig::new(SECRET).unwrap()).uploads(uploads);
    if first {
        app.migrate().await.unwrap();
        create_superuser(db, "root", PASSWORD).await.unwrap();
    }
    app.router().unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> (Response<()>, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (Response::from_parts(parts, ()), bytes.to_vec())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
}

async fn token(app: &Router) -> String {
    let request = Request::post("/api/auth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"username":"root","password":"{PASSWORD}"}}"#
        )))
        .unwrap();
    let (response, body) = send(app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    json(&body)["token"].as_str().unwrap().to_owned()
}

fn authed(method: &str, path: &str, token: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
}

fn multipart(token: &str, filename: &str, content_type: &str, data: &[u8]) -> Request<Body> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nignored\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    authed("POST", "/api/files", token)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn chunk(
    app: &Router,
    token: &str,
    id: &str,
    offset: u64,
    data: &[u8],
) -> (Response<()>, serde_json::Value) {
    let request = authed("PATCH", &format!("/api/uploads/{id}"), token)
        .header("Upload-Offset", offset.to_string())
        .body(Body::from(data.to_vec()))
        .unwrap();
    let (response, body) = send(app, request).await;
    (response, json(&body))
}

async fn begin(
    app: &Router,
    token: &str,
    filename: &str,
    size: u64,
) -> (Response<()>, serde_json::Value) {
    let request = authed("POST", "/api/uploads", token)
        .body(Body::from(format!(
            r#"{{"filename":"{filename}","size":{size},"content_type":"application/octet-stream"}}"#
        )))
        .unwrap();
    let (response, body) = send(app, request).await;
    (response, json(&body))
}

async fn total_files(app: &Router, token: &str) -> u64 {
    let (_, body) = send(
        app,
        authed("GET", "/api/files", token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    json(&body)["meta"]["total"].as_u64().unwrap()
}

/// Deterministic, non-repeating-looking bytes, so a mis-ordered or truncated file cannot pass.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 251) % 256) as u8).collect()
}

#[tokio::test]
async fn multipart_upload_round_trip_and_delete() {
    let db = database().await;
    let app = router(&db, Uploads::new(db.clone(), Storage::memory()), true).await;
    let token = token(&app).await;
    let data = pattern(70_000);

    let (response, body) = send(
        &app,
        multipart(&token, "report.PDF", "application/pdf", &data),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let file = json(&body);
    assert_eq!(file["filename"], "report.PDF");
    assert_eq!(file["size"], 70_000);
    assert_eq!(file["created_by"], "root");
    assert_eq!(response.headers()[header::LOCATION], "/api/files/1");

    let (response, body) = send(
        &app,
        authed("GET", "/api/files/1/content", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body, data);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "70000");
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"report.PDF\""
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");

    assert_eq!(total_files(&app, &token).await, 1);
    let (response, _) = send(
        &app,
        authed("DELETE", "/api/files/1", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    for path in ["/api/files/1", "/api/files/1/content"] {
        let (response, _) = send(
            &app,
            authed("GET", path, &token).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(total_files(&app, &token).await, 0);
}

#[tokio::test]
async fn large_multipart_upload_uses_the_streaming_path_intact() {
    let db = database().await;
    let app = router(&db, Uploads::new(db.clone(), Storage::memory()), true).await;
    let token = token(&app).await;
    // Above the 5 MiB single-request threshold, and not a multiple of the part size.
    let data = pattern(12 * 1024 * 1024 + 12_345);

    let (response, _) = send(
        &app,
        multipart(&token, "big.bin", "application/octet-stream", &data),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let (_, body) = send(
        &app,
        authed("GET", "/api/files/1/content", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body.len(), data.len());
    assert!(body == data, "downloaded bytes differ from the upload");
}

#[tokio::test]
async fn uploads_over_the_limit_are_refused_and_leave_nothing() {
    let db = database().await;
    let uploads = Uploads::new(db.clone(), Storage::memory())
        .max_file_bytes(1000)
        .max_chunk_bytes(16);
    let app = router(&db, uploads, true).await;
    let token = token(&app).await;

    let (response, _) = send(
        &app,
        multipart(
            &token,
            "big.bin",
            "application/octet-stream",
            &pattern(1001),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Far over the limit: rejected by the body limit before it is buffered.
    let (response, _) = send(
        &app,
        multipart(
            &token,
            "huge.bin",
            "application/octet-stream",
            &pattern(500_000),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Exactly at the limit is fine.
    let (response, _) = send(
        &app,
        multipart(&token, "ok.bin", "application/octet-stream", &pattern(1000)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(total_files(&app, &token).await, 1);

    let (response, error) = begin(&app, &token, "big.bin", 1001).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error["error"]["code"], "too_large");

    let (_, started) = begin(&app, &token, "a.bin", 40).await;
    let id = started["id"].as_str().unwrap();
    assert_eq!(started["max_chunk_bytes"], 16);
    // A chunk bigger than the chunk limit: the same JSON error as the rest of the API.
    let (response, error) = chunk(&app, &token, id, 0, &pattern(17)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error["error"]["code"], "too_large", "{error}");
    assert_eq!(
        chunk(&app, &token, id, 0, &pattern(16)).await.0.status(),
        StatusCode::OK
    );
    assert_eq!(
        chunk(&app, &token, id, 16, &pattern(16)).await.0.status(),
        StatusCode::OK
    );
    // 32 received, 8 remaining: 9 bytes would run past the declared size.
    let (response, error) = chunk(&app, &token, id, 32, &pattern(9)).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error["error"]["code"], "too_large");
    assert_eq!(total_files(&app, &token).await, 1);
}

#[tokio::test]
async fn resumable_upload_survives_a_restart_and_rejects_wrong_offsets() {
    let db = database().await;
    let storage = Storage::memory();
    let uploads = || Uploads::new(db.clone(), storage.clone()).max_chunk_bytes(1024);
    let app = router(&db, uploads(), true).await;
    let token = token(&app).await;
    let data = pattern(2500);

    let (response, started) = begin(&app, &token, "video.mp4", 2500).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["upload-offset"], "0");
    let id = started["id"].as_str().unwrap().to_owned();

    let (response, progress) = chunk(&app, &token, &id, 0, &data[..1000]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(progress["received"], 1000);
    assert_eq!(progress["complete"], false);

    // Replaying a chunk, skipping ahead, or omitting the offset must not corrupt the file.
    let (response, error) = chunk(&app, &token, &id, 0, &data[..1000]).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(error["error"]["message"], "expected offset 1000");
    assert_eq!(
        chunk(&app, &token, &id, 2000, &data[2000..])
            .await
            .0
            .status(),
        StatusCode::CONFLICT
    );
    let no_offset = authed("PATCH", &format!("/api/uploads/{id}"), &token)
        .body(Body::from(vec![1u8; 4]))
        .unwrap();
    assert_eq!(
        send(&app, no_offset).await.0.status(),
        StatusCode::BAD_REQUEST
    );
    drop(app);

    // "Restart": a brand-new app over the same database and storage.
    let app = router(&db, uploads(), false).await;
    let (response, body) = send(
        &app,
        authed("GET", &format!("/api/uploads/{id}"), &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["upload-offset"], "1000");
    assert_eq!(json(&body)["received"], 1000);

    assert_eq!(
        chunk(&app, &token, &id, 1000, &data[1000..2000])
            .await
            .0
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        total_files(&app, &token).await,
        0,
        "file must not exist before the last chunk"
    );
    let (response, file) = chunk(&app, &token, &id, 2000, &data[2000..]).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(file["size"], 2500);
    assert_eq!(file["filename"], "video.mp4");

    let url = file["url"].as_str().unwrap().to_owned();
    let (_, body) = send(
        &app,
        authed("GET", &url, &token).body(Body::empty()).unwrap(),
    )
    .await;
    assert!(body == data, "assembled file differs from the upload");

    // The session is gone once the file exists.
    let (response, _) = send(
        &app,
        authed("GET", &format!("/api/uploads/{id}"), &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        chunk(&app, &token, &id, 2500, &[]).await.0.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(total_files(&app, &token).await, 1);
}

#[tokio::test]
async fn aborted_uploads_cannot_be_continued() {
    let db = database().await;
    let app = router(&db, Uploads::new(db.clone(), Storage::memory()), true).await;
    let token = token(&app).await;
    let (_, started) = begin(&app, &token, "a.bin", 100).await;
    let id = started["id"].as_str().unwrap();
    assert_eq!(
        chunk(&app, &token, id, 0, &pattern(50)).await.0.status(),
        StatusCode::OK
    );

    let abort = authed("DELETE", &format!("/api/uploads/{id}"), &token)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, abort).await.0.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        chunk(&app, &token, id, 50, &pattern(50)).await.0.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        chunk(&app, &token, "does-not-exist", 0, &pattern(5))
            .await
            .0
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(total_files(&app, &token).await, 0);
}

#[tokio::test]
async fn hostile_filenames_cannot_escape_the_storage_directory() {
    let root = std::env::temp_dir().join(format!("cadmus-upload-test-{}", std::process::id()));
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let storage = Storage::from_url(&format!("file://{}", store.display()), Vec::new()).unwrap();

    let db = database().await;
    let app = router(&db, Uploads::new(db.clone(), storage), true).await;
    let token = token(&app).await;

    let (response, body) = send(
        &app,
        multipart(
            &token,
            "../../../escaped.html",
            "text/html",
            b"<script>alert(1)</script>",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json(&body)["filename"], "escaped.html");
    let (_, started) = begin(&app, &token, "..\\\\..\\\\evil.sh", 4).await;
    let id = started["id"].as_str().unwrap();
    assert_eq!(
        chunk(&app, &token, id, 0, b"boom").await.0.status(),
        StatusCode::CREATED
    );

    // Everything written must be inside the store directory, under generated names.
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, out)
            } else {
                out.push(path)
            }
        }
    }
    let mut written = Vec::new();
    files(&root, &mut written);
    assert_eq!(written.len(), 2, "{written:?}");
    for path in &written {
        assert!(
            path.starts_with(store.join("files")),
            "{path:?} is outside the store"
        );
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("escaped") && !name.contains("evil"),
            "{name}"
        );
    }

    // An uploaded HTML file is only ever served as a download.
    let (response, _) = send(
        &app,
        authed("GET", "/api/files/1/content", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn uploads_require_authentication_and_configuration() {
    let db = database().await;
    let app = router(&db, Uploads::new(db.clone(), Storage::memory()), true).await;
    for (method, path) in [
        ("GET", "/api/files"),
        ("POST", "/api/files"),
        ("POST", "/api/uploads"),
        ("GET", "/api/files/1/content"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&app, request).await.0.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    let token = token(&app).await;
    assert_eq!(total_files(&app, &token).await, 0);

    // Without storage configured the endpoints do not exist.
    let plain = App::new(db.clone(), AuthConfig::new(SECRET).unwrap())
        .router()
        .unwrap();
    let (response, _) = send(
        &plain,
        authed("GET", "/api/files", &token)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    assert!(Storage::from_url("not a url", Vec::new()).is_err());
    // S3 support is behind a Cargo feature that these tests do not enable.
    assert!(Storage::from_url("s3://bucket/prefix", Vec::new()).is_err());
}

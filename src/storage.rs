//! File storage behind one interface: local disk, memory, S3, Azure Blob or GCS, chosen by URL.
//!
//! Built on [`object_store`], so every backend gets the same streaming and multipart behaviour.

use std::sync::Arc;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{GetResult, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};

use crate::error::AdminError;

/// Files smaller than this are written with a single request instead of a multipart upload
/// (S3 refuses multipart parts below 5 MiB, except the last).
const MULTIPART_THRESHOLD: usize = 5 * 1024 * 1024;
/// In-flight part uploads per file; more parts wait, which is what applies backpressure to
/// the client.
const PART_CONCURRENCY: usize = 4;

/// A storage backend plus the key prefix this app writes under.
#[derive(Clone, Debug)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}

impl Storage {
    /// Opens the backend named by `url`:
    ///
    /// | URL | Backend | Cargo feature |
    /// |---|---|---|
    /// | `file:///var/lib/app/uploads` | local directory | (built in) |
    /// | `memory:///` | in-process, for tests | (built in) |
    /// | `s3://bucket/prefix` | Amazon S3 and compatibles | `s3` |
    /// | `az://container/prefix` | Azure Blob Storage | `azure` |
    /// | `gs://bucket/prefix` | Google Cloud Storage | `gcs` |
    ///
    /// `options` are backend settings such as `aws_access_key_id`, `aws_region`,
    /// `aws_endpoint` or `azure_storage_account_name`; unknown keys are ignored, so passing
    /// the whole environment is fine.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the URL is invalid, names a backend whose feature is
    /// not enabled, or the backend rejects its settings.
    pub fn from_url<I>(url: &str, options: I) -> Result<Self, AdminError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let parsed = url::Url::parse(url)
            .map_err(|e| AdminError::Config(format!("invalid storage URL: {e}")))?;
        let (store, prefix) = object_store::parse_url_opts(&parsed, options).map_err(|e| {
            AdminError::Config(format!(
                "cannot open storage `{}` (is the matching Cargo feature enabled?): {e}",
                parsed.scheme()
            ))
        })?;
        Ok(Self {
            store: Arc::from(store),
            prefix,
        })
    }

    /// Reads `STORAGE_URL` and the backend's own variables (`AWS_ACCESS_KEY_ID`,
    /// `AZURE_STORAGE_ACCOUNT_NAME`, …). Returns `None` when `STORAGE_URL` is unset.
    ///
    /// # Errors
    /// See [`Storage::from_url`].
    pub fn from_env() -> Result<Option<Self>, AdminError> {
        match std::env::var("STORAGE_URL") {
            Ok(url) => Self::from_url(&url, std::env::vars()).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Wraps any [`ObjectStore`], writing under `prefix`.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self {
            store,
            prefix: Path::from(prefix),
        }
    }

    /// An in-process backend for tests.
    #[must_use]
    pub fn memory() -> Self {
        Self::new(Arc::new(object_store::memory::InMemory::new()), "")
    }

    fn path(&self, key: &str) -> Path {
        key.split('/')
            .filter(|part| !part.is_empty())
            .fold(self.prefix.clone(), |path, part| path.join(part))
    }

    /// Writes a whole object.
    ///
    /// # Errors
    /// Returns [`AdminError::Storage`] if the backend fails.
    pub async fn put(&self, key: &str, bytes: Bytes) -> Result<(), AdminError> {
        self.store
            .put(&self.path(key), PutPayload::from(bytes))
            .await?;
        Ok(())
    }

    /// Opens an object for reading; stream it with [`GetResult::into_stream`].
    ///
    /// # Errors
    /// Returns [`AdminError::NotFound`] if there is no such object, [`AdminError::Storage`] on
    /// other backend failures.
    pub async fn get(&self, key: &str) -> Result<GetResult, AdminError> {
        match self.store.get(&self.path(key)).await {
            Ok(result) => Ok(result),
            Err(object_store::Error::NotFound { .. }) => {
                Err(AdminError::NotFound("stored object".into()))
            }
            Err(other) => Err(other.into()),
        }
    }

    /// Deletes an object. Deleting one that is already gone succeeds.
    ///
    /// # Errors
    /// Returns [`AdminError::Storage`] if the backend fails.
    pub async fn delete(&self, key: &str) -> Result<(), AdminError> {
        match self.store.delete(&self.path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    /// Starts a streaming write to `key`.
    #[must_use]
    pub fn sink(&self, key: &str) -> Sink {
        Sink {
            storage: self.clone(),
            key: key.to_owned(),
            buffer: Vec::new(),
            multipart: None,
            written: 0,
        }
    }
}

/// A streaming write of unknown length.
///
/// Small objects are written with one request; larger ones switch to a multipart upload with
/// bounded concurrency, so memory use stays flat however large the file is. Nothing is visible
/// at the key until [`Sink::finish`] succeeds.
pub struct Sink {
    storage: Storage,
    key: String,
    buffer: Vec<u8>,
    multipart: Option<WriteMultipart>,
    written: u64,
}

impl Sink {
    /// Bytes accepted so far.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Appends bytes.
    ///
    /// # Errors
    /// Returns [`AdminError::Storage`] if the backend fails.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), AdminError> {
        self.written += bytes.len() as u64;
        if self.multipart.is_none() && self.buffer.len() + bytes.len() <= MULTIPART_THRESHOLD {
            self.buffer.extend_from_slice(bytes);
            return Ok(());
        }
        if self.multipart.is_none() {
            let upload = self
                .storage
                .store
                .put_multipart(&self.storage.path(&self.key))
                .await?;
            let mut writer = WriteMultipart::new_with_chunk_size(upload, MULTIPART_THRESHOLD);
            writer.write(&std::mem::take(&mut self.buffer));
            self.multipart = Some(writer);
        }
        if let Some(writer) = &mut self.multipart {
            writer.wait_for_capacity(PART_CONCURRENCY).await?;
            writer.write(bytes);
        }
        Ok(())
    }

    /// Completes the write and returns the total size.
    ///
    /// # Errors
    /// Returns [`AdminError::Storage`] if the backend fails; a failed multipart upload is
    /// aborted by the backend client.
    pub async fn finish(self) -> Result<u64, AdminError> {
        match self.multipart {
            Some(writer) => {
                writer.finish().await?;
            }
            None => {
                self.storage
                    .put(&self.key, Bytes::from(self.buffer))
                    .await?;
            }
        }
        Ok(self.written)
    }

    /// Abandons the write, releasing any parts already uploaded.
    pub async fn abort(self) {
        if let Some(writer) = self.multipart
            && let Err(error) = writer.abort().await
        {
            tracing::warn!(%error, "could not abort multipart upload");
        }
    }
}

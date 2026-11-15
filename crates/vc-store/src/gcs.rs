//! Google Cloud Storage [`BlobStore`] on the official `google-cloud-storage` crate.
//!
//! * `put_if_absent` uploads with `ifGenerationMatch=0` and the known crc32c (GCS rejects a
//!   corrupted upload), then checks the crc32c GCS reports. A precondition failure means the
//!   object exists: identical bytes → [`PutOutcome::AlreadyIdentical`], else
//!   [`Error::Conflict`].
//! * Reads and writes use the JSON API (`Storage`); list and delete use `StorageControl`.

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model_ext::ReadRange;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result};

/// A GCS bucket as a blob store.
#[derive(Clone, Debug)]
pub struct GcsBlobStore {
    storage: Storage,
    control: StorageControl,
    /// `projects/_/buckets/<name>`.
    bucket_path: String,
    bucket: String,
}

/// How to reach GCS.
#[derive(Debug, Clone, Default)]
pub struct GcsOptions {
    /// Endpoint override (`http://host:port` for fake-gcs-server); `None` = Google.
    pub endpoint: Option<String>,
    /// Use anonymous credentials (emulators) instead of Application Default Credentials.
    pub anonymous: bool,
}

fn storage_err(op: &str, path: &str, e: &google_cloud_storage::Error) -> Error {
    Error::Storage(format!("gcs {op} {path}: {e}"))
}

fn is_status(e: &google_cloud_storage::Error, http: u16) -> bool {
    e.http_status_code() == Some(http)
}

fn code_is(e: &google_cloud_storage::Error, name: &str) -> bool {
    e.status().is_some_and(|s| s.code.name() == name)
}

fn is_not_found(e: &google_cloud_storage::Error) -> bool {
    is_status(e, 404) || code_is(e, "NOT_FOUND")
}

impl GcsBlobStore {
    /// A store for `bucket` (a bare bucket name, e.g. `voyager-crawler-raw`).
    pub async fn new(bucket: &str, opts: GcsOptions) -> Result<Self> {
        let mut sb = Storage::builder();
        let mut cb = StorageControl::builder();
        if let Some(ep) = &opts.endpoint {
            sb = sb.with_endpoint(ep.clone());
            cb = cb.with_endpoint(ep.clone());
        }
        if opts.anonymous {
            sb = sb.with_credentials(
                google_cloud_auth::credentials::anonymous::Builder::new().build(),
            );
            cb = cb.with_credentials(
                google_cloud_auth::credentials::anonymous::Builder::new().build(),
            );
        }
        let storage = sb
            .build()
            .await
            .map_err(|e| Error::Storage(format!("gcs client: {e}")))?;
        let control = cb
            .build()
            .await
            .map_err(|e| Error::Storage(format!("gcs control client: {e}")))?;
        Ok(GcsBlobStore {
            storage,
            control,
            bucket_path: format!("projects/_/buckets/{bucket}"),
            bucket: bucket.to_string(),
        })
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    async fn read_all(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        let mut req = self.storage.read_object(&self.bucket_path, path);
        if let Some(r) = &range {
            if r.start > r.end {
                return Err(Error::Invalid(format!("range {r:?}")));
            }
            if r.start == r.end {
                return Ok(Bytes::new());
            }
            req = req.set_read_range(ReadRange::segment(r.start, r.end - r.start));
        }
        let mut resp = req.send().await.map_err(|e| {
            if is_not_found(&e) {
                Error::NotFound(path.to_string())
            } else if is_status(&e, 416) {
                Error::Invalid(format!("range {range:?} out of bounds for {path}"))
            } else {
                storage_err("read", path, &e)
            }
        })?;
        let mut out = Vec::new();
        while let Some(chunk) = resp.next().await {
            out.extend_from_slice(&chunk.map_err(|e| storage_err("read", path, &e))?);
        }
        if let Some(r) = range
            && out.len() as u64 != r.end - r.start
        {
            return Err(Error::Invalid(format!(
                "range {r:?} out of bounds for {path} (got {} bytes)",
                out.len()
            )));
        }
        Ok(Bytes::from(out))
    }
}

#[async_trait]
impl BlobStore for GcsBlobStore {
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome> {
        if crc32c::crc32c(&bytes) != crc32c {
            return Err(Error::Storage(format!("crc32c mismatch for {path}")));
        }
        let res = self
            .storage
            .write_object(&self.bucket_path, path, bytes.clone())
            .set_if_generation_match(0)
            .with_known_crc32c(crc32c)
            .send_buffered()
            .await;
        match res {
            Ok(obj) => {
                if let Some(c) = obj.checksums.as_ref().and_then(|c| c.crc32c)
                    && c != crc32c
                {
                    return Err(Error::Storage(format!(
                        "gcs reported crc32c {c:#x} for {path}, expected {crc32c:#x}"
                    )));
                }
                Ok(PutOutcome::Created)
            }
            Err(e) if is_status(&e, 412) || is_failed_precondition(&e) => {
                let existing = self.read_all(path, None).await?;
                if existing == bytes {
                    Ok(PutOutcome::AlreadyIdentical)
                } else {
                    Err(Error::Conflict(format!(
                        "{path} exists with different bytes"
                    )))
                }
            }
            Err(e) => Err(storage_err("write", path, &e)),
        }
    }

    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        self.read_all(path, range).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        match self
            .control
            .delete_object()
            .set_bucket(&self.bucket_path)
            .set_object(path)
            .send()
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(storage_err("delete", path, &e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let resp = self
                .control
                .list_objects()
                .set_parent(&self.bucket_path)
                .set_prefix(prefix)
                .set_page_token(token.clone())
                .send()
                .await
                .map_err(|e| storage_err("list", prefix, &e))?;
            out.extend(resp.objects.into_iter().map(|o| o.name));
            if resp.next_page_token.is_empty() {
                break;
            }
            token = resp.next_page_token;
        }
        out.sort();
        Ok(out)
    }
}

fn is_failed_precondition(e: &google_cloud_storage::Error) -> bool {
    code_is(e, "FAILED_PRECONDITION")
}

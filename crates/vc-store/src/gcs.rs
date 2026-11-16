//! Google Cloud Storage [`BlobStore`] on the official `google-cloud-storage` crate.
//!
//! * `put_if_absent` uploads with `ifGenerationMatch=0` and the known crc32c (GCS rejects a
//!   corrupted upload), then checks the crc32c GCS reports. A precondition failure means the
//!   object exists: identical bytes → [`PutOutcome::AlreadyIdentical`], else
//!   [`Error::Conflict`].
//! * Reads and writes use the `Storage` client; list and delete call the JSON API directly
//!   with reqwest (`StorageControl` speaks gRPC only, which emulators such as
//!   fake-gcs-server do not serve), authenticated with the same credentials.

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_auth::credentials::{CacheableResource, Credentials};
use google_cloud_storage::client::Storage;
use google_cloud_storage::model_ext::ReadRange;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result};

/// A GCS bucket as a blob store.
#[derive(Clone, Debug)]
pub struct GcsBlobStore {
    storage: Storage,
    http: reqwest::Client,
    creds: Credentials,
    /// JSON API base, e.g. `https://storage.googleapis.com`.
    json_base: String,
    list_page_size: Option<u32>,
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
        let creds = if opts.anonymous {
            google_cloud_auth::credentials::anonymous::Builder::new().build()
        } else {
            google_cloud_auth::credentials::Builder::default()
                .build()
                .map_err(|e| Error::Storage(format!("gcs credentials: {e}")))?
        };
        let mut sb = Storage::builder().with_credentials(creds.clone());
        if let Some(ep) = &opts.endpoint {
            sb = sb.with_endpoint(ep.clone());
        }
        let storage = sb
            .build()
            .await
            .map_err(|e| Error::Storage(format!("gcs client: {e}")))?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Storage(format!("gcs http client: {e}")))?;
        let json_base = opts
            .endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
            .trim_end_matches('/')
            .to_string();
        Ok(GcsBlobStore {
            storage,
            http,
            creds,
            json_base,
            list_page_size: None,
            bucket_path: format!("projects/_/buckets/{bucket}"),
            bucket: bucket.to_string(),
        })
    }

    /// Lists at most `n` objects per JSON API page (tests exercise pagination with it).
    #[doc(hidden)]
    pub fn with_list_page_size(mut self, n: u32) -> Self {
        self.list_page_size = Some(n.max(1));
        self
    }

    /// `{base}/storage/v1/b/{bucket}/o[/{object}]`, every segment percent-encoded.
    fn object_url(&self, object: Option<&str>) -> Result<url::Url> {
        let mut u = url::Url::parse(&self.json_base)
            .map_err(|e| Error::Storage(format!("gcs endpoint {}: {e}", self.json_base)))?;
        {
            let mut seg = u
                .path_segments_mut()
                .map_err(|()| Error::Storage(format!("gcs endpoint {}", self.json_base)))?;
            seg.pop_if_empty()
                .extend(["storage", "v1", "b", &self.bucket, "o"]);
            if let Some(o) = object {
                seg.push(o);
            }
        }
        Ok(u)
    }

    /// The auth headers for a JSON API call (none for anonymous credentials).
    async fn auth_headers(&self) -> Result<http::HeaderMap> {
        match self
            .creds
            .headers(http::Extensions::new())
            .await
            .map_err(|e| Error::Storage(format!("gcs credentials: {e}")))?
        {
            CacheableResource::New { data, .. } => Ok(data),
            CacheableResource::NotModified => Ok(http::HeaderMap::new()),
        }
    }

    async fn send_json(
        &self,
        req: reqwest::RequestBuilder,
        op: &str,
        path: &str,
    ) -> Result<reqwest::Response> {
        req.headers(self.auth_headers().await?)
            .send()
            .await
            .map_err(|e| Error::Storage(format!("gcs {op} {path}: {e}")))
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
        let url = self.object_url(Some(path))?;
        let resp = self
            .send_json(self.http.delete(url), "delete", path)
            .await?;
        match resp.status().as_u16() {
            200..=299 | 404 => Ok(()),
            s => Err(Error::Storage(format!("gcs delete {path}: HTTP {s}"))),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut url = self.object_url(None)?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("prefix", prefix);
                q.append_pair("fields", "items(name),nextPageToken");
                if let Some(n) = self.list_page_size {
                    q.append_pair("maxResults", &n.to_string());
                }
                if let Some(t) = &token {
                    q.append_pair("pageToken", t);
                }
            }
            let resp = self.send_json(self.http.get(url), "list", prefix).await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(Error::Storage(format!(
                    "gcs list {prefix}: HTTP {}",
                    status.as_u16()
                )));
            }
            let page: ListPage = resp
                .json()
                .await
                .map_err(|e| Error::Storage(format!("gcs list {prefix}: {e}")))?;
            out.extend(page.items.into_iter().map(|o| o.name));
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// The public JSON API endpoint.
const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPage {
    #[serde(default)]
    items: Vec<ListItem>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct ListItem {
    name: String,
}

fn is_failed_precondition(e: &google_cloud_storage::Error) -> bool {
    code_is(e, "FAILED_PRECONDITION")
}

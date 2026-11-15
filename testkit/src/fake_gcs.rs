//! **fake-gcs**: the `fsouza/fake-gcs-server` helper and a [`BlobStore`] over its JSON API.
//!
//! * [`fake_gcs()`] starts one `fsouza/fake-gcs-server` container per test process (HTTP, no
//!   auth; labelled like the Postgres container so stale ones are reaped) and creates a fresh
//!   bucket per call. Set `VC_TEST_GCS_URL` (e.g. `http://localhost:4443`) to use a running
//!   server instead (CI service, E2E compose).
//! * [`FakeGcsStore`] implements [`BlobStore`] with the GCS JSON API that `fake-gcs-server`
//!   speaks: `ifGenerationMatch=0` uploads, ranged reads, delete, prefix listing. E2E runners use
//!   it to point the raw-first oracle at the fake GCS.

use std::ops::Range;
use std::sync::OnceLock;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::StatusCode;
use serde_json::Value;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use url::Url;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result};

/// The image used by [`fake_gcs()`].
pub const FAKE_GCS_IMAGE: &str = "fsouza/fake-gcs-server";
/// Its pinned tag.
pub const FAKE_GCS_TAG: &str = "1.56.1";

struct GcsServer {
    base: String,
    _container: Option<Container<GenericImage>>,
}

static GCS: OnceLock<GcsServer> = OnceLock::new();

fn start() -> GcsServer {
    if let Ok(url) = std::env::var("VC_TEST_GCS_URL") {
        return GcsServer {
            base: url.trim_end_matches('/').to_string(),
            _container: None,
        };
    }
    crate::pg::reap_orphans();
    let c = GenericImage::new(FAKE_GCS_IMAGE, FAKE_GCS_TAG)
        .with_exposed_port(4443.tcp())
        .with_wait_for(WaitFor::message_on_stderr("server started at"))
        .with_label(crate::pg::LABEL, "1")
        .with_label(crate::pg::LABEL_PID, std::process::id().to_string())
        .with_cmd(["-scheme", "http", "-port", "4443", "-backend", "memory"])
        .start()
        .expect("start fsouza/fake-gcs-server (is Docker running?)");
    let host = c.get_host().expect("container host").to_string();
    let port = c.get_host_port_ipv4(4443).expect("mapped port");
    GcsServer {
        base: format!("http://{host}:{port}"),
        _container: Some(c),
    }
}

/// A fresh bucket on the per-process fake GCS server.
pub async fn fake_gcs() -> FakeGcsStore {
    let base = GCS
        .get_or_init(|| std::thread::spawn(start).join().expect("start fake gcs"))
        .base
        .clone();
    let bucket = format!("vct-{}", ulid::Ulid::generate().to_string().to_lowercase());
    let store = FakeGcsStore::new(&base, &bucket);
    store.create_bucket().await.expect("create fake gcs bucket");
    store
}

/// A [`BlobStore`] over one bucket of a GCS-JSON-API server (fake-gcs-server).
#[derive(Debug, Clone)]
pub struct FakeGcsStore {
    base: String,
    bucket: String,
    http: reqwest::Client,
}

fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

impl FakeGcsStore {
    /// A store for `bucket` on the server at `base` (e.g. `http://localhost:4443`).
    pub fn new(base: &str, bucket: &str) -> FakeGcsStore {
        FakeGcsStore {
            base: base.trim_end_matches('/').to_string(),
            bucket: bucket.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// The server base URL.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Creates the bucket (an existing one is fine).
    pub async fn create_bucket(&self) -> Result<()> {
        let r = self
            .http
            .post(format!("{}/storage/v1/b", self.base))
            .json(&serde_json::json!({ "name": self.bucket }))
            .send()
            .await
            .map_err(storage_err)?;
        if r.status().is_success() || r.status() == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(storage_err(format!("create bucket: {}", r.status())))
        }
    }

    fn object_url(&self, prefix: &str, path: &str) -> Url {
        let mut u = Url::parse(&format!("{}{prefix}", self.base)).expect("valid base url");
        u.path_segments_mut().expect("http url").extend([
            "storage",
            "v1",
            "b",
            &self.bucket,
            "o",
            path,
        ]);
        u
    }
}

#[async_trait]
impl BlobStore for FakeGcsStore {
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome> {
        if crc32c::crc32c(&bytes) != crc32c {
            return Err(Error::Storage(format!("crc32c mismatch for {path}")));
        }
        let mut u =
            Url::parse(&format!("{}/upload/storage/v1/b", self.base)).map_err(storage_err)?;
        u.path_segments_mut()
            .expect("http url")
            .extend([&self.bucket, "o"]);
        u.query_pairs_mut()
            .append_pair("uploadType", "media")
            .append_pair("name", path)
            .append_pair("ifGenerationMatch", "0");
        let r = self
            .http
            .post(u)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .map_err(storage_err)?;
        match r.status() {
            s if s.is_success() => Ok(PutOutcome::Created),
            StatusCode::PRECONDITION_FAILED => {
                let existing = self.get(path, None).await?;
                if existing == bytes {
                    Ok(PutOutcome::AlreadyIdentical)
                } else {
                    Err(Error::Conflict(format!(
                        "{path} exists with different bytes"
                    )))
                }
            }
            s => Err(storage_err(format!("upload {path}: {s}"))),
        }
    }

    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        let mut u = self.object_url("", path);
        u.query_pairs_mut().append_pair("alt", "media");
        let mut req = self.http.get(u);
        if let Some(r) = &range {
            if r.start > r.end {
                return Err(Error::Invalid(format!("range {r:?} is inverted")));
            }
            if r.start == r.end {
                // Existence check + empty slice.
                let full = self.get(path, None).await?;
                if r.end > full.len() as u64 {
                    return Err(Error::Invalid(format!("range {r:?} out of bounds")));
                }
                return Ok(Bytes::new());
            }
            req = req.header("range", format!("bytes={}-{}", r.start, r.end - 1));
        }
        let resp = req.send().await.map_err(storage_err)?;
        match resp.status() {
            StatusCode::NOT_FOUND => Err(Error::NotFound(path.to_string())),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                Err(Error::Invalid(format!("range {range:?} out of bounds")))
            }
            s if s.is_success() => {
                let b = resp.bytes().await.map_err(storage_err)?;
                if let Some(r) = range
                    && b.len() as u64 != r.end - r.start
                {
                    return Err(Error::Invalid(format!("range {r:?} out of bounds")));
                }
                Ok(b)
            }
            s => Err(storage_err(format!("get {path}: {s}"))),
        }
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let r = self
            .http
            .delete(self.object_url("", path))
            .send()
            .await
            .map_err(storage_err)?;
        match r.status() {
            s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
            s => Err(storage_err(format!("delete {path}: {s}"))),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut u = Url::parse(&format!("{}/storage/v1/b", self.base)).map_err(storage_err)?;
            u.path_segments_mut()
                .expect("http url")
                .extend([&self.bucket, "o"]);
            {
                let mut q = u.query_pairs_mut();
                q.append_pair("prefix", prefix);
                if let Some(t) = &token {
                    q.append_pair("pageToken", t);
                }
            }
            let r = self.http.get(u).send().await.map_err(storage_err)?;
            if !r.status().is_success() {
                return Err(storage_err(format!("list {prefix}: {}", r.status())));
            }
            let v: Value = r.json().await.map_err(storage_err)?;
            for it in v
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(n) = it.get("name").and_then(Value::as_str) {
                    out.push(n.to_string());
                }
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        out.sort();
        Ok(out)
    }
}

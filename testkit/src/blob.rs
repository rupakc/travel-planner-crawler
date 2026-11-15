//! In-memory `BlobStore` with GCS `ifGenerationMatch=0` semantics.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result};

/// An in-memory blob store.
#[derive(Debug, Default)]
pub struct MemBlobStore {
    objects: Mutex<BTreeMap<String, Bytes>>,
}

impl MemBlobStore {
    /// An empty store.
    pub fn new() -> Self {
        MemBlobStore::default()
    }

    /// Whether `path` exists.
    pub fn contains(&self, path: &str) -> bool {
        self.objects.lock().expect("blob lock").contains_key(path)
    }

    /// All paths, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.objects
            .lock()
            .expect("blob lock")
            .keys()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl BlobStore for MemBlobStore {
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome> {
        if crc32c::crc32c(&bytes) != crc32c {
            return Err(Error::Storage(format!("crc32c mismatch for {path}")));
        }
        let mut m = self.objects.lock().expect("blob lock");
        match m.get(path) {
            Some(existing) if *existing == bytes => Ok(PutOutcome::AlreadyIdentical),
            Some(_) => Err(Error::Conflict(format!(
                "{path} exists with different bytes"
            ))),
            None => {
                m.insert(path.to_string(), bytes);
                Ok(PutOutcome::Created)
            }
        }
    }

    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        let m = self.objects.lock().expect("blob lock");
        let b = m
            .get(path)
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        match range {
            None => Ok(b.clone()),
            Some(r) => {
                let len = b.len() as u64;
                if r.start > r.end || r.end > len {
                    return Err(Error::Invalid(format!("range {r:?} out of bounds ({len})")));
                }
                Ok(b.slice(r.start as usize..r.end as usize))
            }
        }
    }

    async fn delete(&self, path: &str) -> Result<()> {
        self.objects.lock().expect("blob lock").remove(path);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let m = self.objects.lock().expect("blob lock");
        Ok(m.keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

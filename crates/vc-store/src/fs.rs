//! Local-filesystem [`BlobStore`] (development, tests, E2E) with the same create-if-absent
//! semantics as GCS `ifGenerationMatch=0`.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{Error, Result};

/// Local-filesystem blob store.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    /// Root directory.
    pub root: PathBuf,
}

impl FsBlobStore {
    /// A store rooted at `root` (created on first write).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsBlobStore { root: root.into() }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let p = Path::new(path);
        if path.is_empty()
            || path.ends_with('/')
            || !p.components().all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(Error::Invalid(format!("bad object path {path:?}")));
        }
        Ok(self.root.join(p))
    }
}

fn put_blocking(full: &Path, bytes: &[u8]) -> Result<PutOutcome> {
    use std::io::Write;
    let dir = full
        .parent()
        .ok_or_else(|| Error::Invalid("object path has no parent".into()))?;
    std::fs::create_dir_all(dir)?;
    if full.exists() {
        return compare_existing(full, bytes);
    }
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        ulid::Ulid::generate()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // `link` fails if the target exists: an atomic create-if-absent.
    let linked = std::fs::hard_link(&tmp, full);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            std::fs::File::open(dir)?.sync_all()?;
            Ok(PutOutcome::Created)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => compare_existing(full, bytes),
        Err(e) => Err(e.into()),
    }
}

fn compare_existing(full: &Path, bytes: &[u8]) -> Result<PutOutcome> {
    let existing = std::fs::read(full)?;
    if existing == bytes {
        Ok(PutOutcome::AlreadyIdentical)
    } else {
        Err(Error::Conflict(format!(
            "{} exists with different bytes",
            full.display()
        )))
    }
}

fn list_blocking(root: &Path, prefix: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for e in rd {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                let name = e.file_name();
                if name.to_string_lossy().starts_with(".tmp-") {
                    continue;
                }
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| Error::Storage("path outside root".into()))?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                if rel.starts_with(prefix) {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Storage(format!("fs task: {e}")))?
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome> {
        if crc32c::crc32c(&bytes) != crc32c {
            return Err(Error::Storage(format!("crc32c mismatch for {path}")));
        }
        let full = self.resolve(path)?;
        blocking(move || put_blocking(&full, &bytes)).await
    }

    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        let full = self.resolve(path)?;
        let p = path.to_string();
        blocking(move || {
            let b = match std::fs::read(&full) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::NotFound(p));
                }
                Err(e) => return Err(e.into()),
            };
            let b = Bytes::from(b);
            match range {
                None => Ok(b),
                Some(r) => {
                    let len = b.len() as u64;
                    if r.start > r.end || r.end > len {
                        return Err(Error::Invalid(format!("range {r:?} out of bounds ({len})")));
                    }
                    Ok(b.slice(r.start as usize..r.end as usize))
                }
            }
        })
        .await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let full = self.resolve(path)?;
        blocking(move || match std::fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let root = self.root.clone();
        let prefix = prefix.to_string();
        blocking(move || list_blocking(&root, &prefix)).await
    }
}

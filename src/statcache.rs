//! Per-key stat cache for file digests, keyed by (mtime, size). wrap.rs uses it to avoid
//! re-hashing unchanged files across rustc invocations. One JSON file per key under
//! `<cache_dir>/stat/`; persistence is best-effort and never fails a build.

use crate::proto::build::bazel::remote::execution::v2 as re;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[derive(Serialize, Deserialize)]
struct Entry {
    mtime_nanos: i128,
    size: u64,
    hash: String,
}

pub struct StatCache {
    path: std::path::PathBuf,
    entries: BTreeMap<String, Entry>,
}

impl StatCache {
    /// Open (or start fresh) the per-key cache file under `<cache_dir>/stat/`.
    pub fn open(cache_dir: &Path, key: &str) -> Self {
        let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
        let path = cache_dir.join("stat").join(format!("{safe}.json"));
        let entries = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { path, entries }
    }

    /// Digest of `path`, using the cached hash when mtime and size are unchanged.
    pub fn file_digest(&mut self, path: &Path) -> Result<re::Digest> {
        let st = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let key = path.to_string_lossy().into_owned();
        if let Some(e) = self.entries.get(&key) {
            if e.mtime_nanos == mtime_nanos(&st) && e.size == st.len() {
                return Ok(re::Digest { hash: e.hash.clone(), size_bytes: e.size as i64 });
            }
        }
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let hash = sha256_hex(&data);
        self.entries.insert(key, Entry { mtime_nanos: mtime_nanos(&st), size: st.len(), hash: hash.clone() });
        Ok(re::Digest { hash, size_bytes: data.len() as i64 })
    }

    /// Persist the cache. Best-effort: a failure here must not fail a build.
    pub fn save(&self) {
        if let Some(dir) = self.path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(&self.entries).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn mtime_nanos(st: &std::fs::Metadata) -> i128 {
    match st.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
        Some(d) => d.as_secs() as i128 * 1_000_000_000 + d.subsec_nanos() as i128,
        None => 0,
    }
}

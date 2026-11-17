//! Persistent (path, size, mtime, inode) -> SHA-256 cache so unchanged files are hashed once.
//!
//! One JSON file per namespace (a package directory, a deps directory, ...), so a wrapper
//! process only loads the entries it needs. Entries whose mtime is very recent are not stored:
//! a file rewritten within the timestamp granularity would otherwise keep a stale digest.

use crate::proto::build::bazel::remote::execution::v2 as re;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    size: u64,
    mtime_ns: i128,
    ino: u64,
    hash: String,
}

pub struct StatCache {
    file: PathBuf,
    map: HashMap<String, Entry>,
    dirty: bool,
    now_ns: i128,
    pub hashed_bytes: u64,
    pub hits: u64,
    pub misses: u64,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

impl StatCache {
    pub fn open(cache_dir: &Path, namespace: &str) -> StatCache {
        let file = cache_dir.join("stat").join(format!("{}.json", &sha256_hex(namespace.as_bytes())[..24]));
        let map = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let now_ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0);
        StatCache { file, map, dirty: false, now_ns, hashed_bytes: 0, hits: 0, misses: 0 }
    }

    pub fn file_digest(&mut self, p: &Path) -> std::io::Result<re::Digest> {
        let md = std::fs::metadata(p)?;
        let mtime_ns = md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128;
        let key = p.to_string_lossy().to_string();
        if let Some(e) = self.map.get(&key) {
            if e.size == md.len() && e.mtime_ns == mtime_ns && e.ino == md.ino() {
                self.hits += 1;
                return Ok(re::Digest { hash: e.hash.clone(), size_bytes: md.len() as i64 });
            }
        }
        self.misses += 1;
        let mut f = std::fs::File::open(p)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            total += n as u64;
        }
        self.hashed_bytes += total;
        let hash = hex::encode(h.finalize());
        if self.now_ns - mtime_ns > 2_000_000_000 {
            self.map.insert(key, Entry { size: total, mtime_ns, ino: md.ino(), hash: hash.clone() });
            self.dirty = true;
        }
        Ok(re::Digest { hash, size_bytes: total as i64 })
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.file.with_extension(format!("tmp{}", std::process::id()));
        if let Ok(b) = serde_json::to_vec(&self.map) {
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
        self.dirty = false;
    }
}

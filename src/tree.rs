//! Directory trees for REAPI: hash a local directory into Directory protos (and the list of file
//! blobs), build a `Tree` for an output directory, and write a `Tree` from the CAS to disk.

use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::rpc::{self, digest_of, Conn};
use crate::statcache::StatCache;
use anyhow::{anyhow, Context, Result};
use prost::Message;
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Collected {
    pub root: re::Directory,
    pub root_digest: re::Digest,
    /// Every descendant directory (not the root), for use as `Tree.children`.
    pub dirs: Vec<re::Directory>,
    pub files: Vec<(re::Digest, PathBuf)>,
}

fn walk(sc: &mut StatCache, dir: &Path, excludes: &[PathBuf], count: &mut usize, dirs: &mut Vec<re::Directory>, files: &mut Vec<(re::Digest, PathBuf)>) -> Result<(re::Directory, re::Digest), String> {
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    entries.sort();
    let mut d = re::Directory::default();
    for (name, path) in entries {
        *count += 1;
        if *count > 200_000 {
            return Err("input tree too large (over 200000 entries)".into());
        }
        let md = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let ft = md.file_type();
        if ft.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|e| e.to_string())?;
            d.symlinks.push(re::SymlinkNode { name, target: target.to_string_lossy().to_string(), ..Default::default() });
        } else if ft.is_dir() {
            if matches!(name.as_str(), ".git" | ".hg" | ".svn") || excludes.iter().any(|e| *e == path) || path.join("CACHEDIR.TAG").exists() {
                continue;
            }
            let (sub, digest) = walk(sc, &path, excludes, count, dirs, files)?;
            dirs.push(sub);
            d.directories.push(re::DirectoryNode { name, digest: Some(digest) });
        } else if ft.is_file() {
            let dg = sc.file_digest(&path).map_err(|e| format!("hashing {}: {e}", path.display()))?;
            files.push((dg.clone(), path.clone()));
            d.files.push(re::FileNode { name, digest: Some(dg), is_executable: md.mode() & 0o111 != 0, ..Default::default() });
        }
    }
    let digest = digest_of(&d.encode_to_vec());
    Ok((d, digest))
}

pub fn collect(sc: &mut StatCache, dir: &Path, excludes: &[PathBuf], count: &mut usize) -> Result<Collected, String> {
    let (mut dirs, mut files) = (vec![], vec![]);
    let (root, root_digest) = walk(sc, dir, excludes, count, &mut dirs, &mut files)?;
    Ok(Collected { root, root_digest, dirs, files })
}

/// The `Tree` proto (root plus all descendant directories) for an output directory, and its digest.
pub fn tree_blob(c: &Collected) -> (re::Digest, Vec<u8>) {
    let tree = re::Tree { root: Some(c.root.clone()), children: c.dirs.clone() };
    let bytes = tree.encode_to_vec();
    (digest_of(&bytes), bytes)
}

/// Fetch a `Tree` and write it below `dest` (created if missing; existing files are overwritten).
pub async fn write_tree(conn: &Conn, instance: &str, tree_digest: &re::Digest, dest: &Path) -> Result<(usize, u64)> {
    let bytes = rpc::fetch(conn, instance, tree_digest).await.context("fetching output tree")?;
    let tree = re::Tree::decode(&bytes[..]).context("decoding output tree")?;
    let by_digest: HashMap<String, re::Directory> = tree.children.iter().map(|d| (digest_of(&d.encode_to_vec()).hash, d.clone())).collect();
    let root = tree.root.ok_or_else(|| anyhow!("tree has no root"))?;
    let (mut nfiles, mut nbytes) = (0usize, 0u64);
    let mut stack: Vec<(re::Directory, PathBuf)> = vec![(root, dest.to_path_buf())];
    while let Some((dir, path)) = stack.pop() {
        std::fs::create_dir_all(&path)?;
        for f in &dir.files {
            let d = f.digest.clone().ok_or_else(|| anyhow!("file {} without digest", f.name))?;
            let data = if d.size_bytes == 0 { vec![] } else { rpc::fetch(conn, instance, &d).await.with_context(|| format!("fetching {}", f.name))? };
            let target = path.join(&f.name);
            let tmp = path.join(format!(".{}.rbe-tmp-{}", f.name, std::process::id()));
            std::fs::write(&tmp, &data)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(if f.is_executable { 0o755 } else { 0o644 }))?;
            std::fs::rename(&tmp, &target)?;
            nfiles += 1;
            nbytes += data.len() as u64;
        }
        for l in &dir.symlinks {
            let target = path.join(&l.name);
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(&l.target, &target)?;
        }
        for sub in &dir.directories {
            let d = sub.digest.clone().ok_or_else(|| anyhow!("directory {} without digest", sub.name))?;
            let child = by_digest.get(&d.hash).ok_or_else(|| anyhow!("tree is missing directory {}", sub.name))?;
            stack.push((child.clone(), path.join(&sub.name)));
        }
    }
    Ok((nfiles, nbytes))
}

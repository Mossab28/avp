//! Content-addressed storage: blobs and trees, named by blake3. One directory, two levels.

use crate::model::{Hash, Tree};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Objects {
    root: PathBuf,
}

pub fn hash_bytes(bytes: &[u8]) -> Hash {
    blake3::hash(bytes).to_hex().to_string()
}

impl Objects {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Self { root: root.to_path_buf() })
    }

    pub fn path_of(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2]).join(hash)
    }

    pub fn has(&self, hash: &str) -> bool {
        hash.len() == 64 && self.path_of(hash).is_file()
    }

    /// Stores bytes under their hash; returns the hash. Writing is atomic (rename).
    pub fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = hash_bytes(bytes);
        let path = self.path_of(&hash);
        if path.is_file() {
            return Ok(hash);
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = self.root.join(format!(".tmp-{}", ulid::Ulid::new()));
        fs::write(&tmp, bytes)?;
        // Read-only: nothing that opens an object by its path can rewrite history.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        }
        fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()), "bad hash");
        fs::read(self.path_of(hash)).with_context(|| format!("object {hash} missing"))
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<Hash> {
        let bytes = serde_json::to_vec(tree)?;
        self.put(&bytes)
    }

    pub fn get_tree(&self, hash: &str) -> Result<Tree> {
        let bytes = self.get(hash)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Writes a blob at `dest` the way a checkout does: a file of its own, writable, executable when
    /// `mode` says so. A copy and not a hard link: a step that writes into a tracked file, or copies
    /// one and writes the copy, behaves as on a fresh checkout and cannot reach the store. Where the
    /// file system clones (APFS, btrfs, XFS), the copy is a clone.
    pub fn copy_to(&self, hash: &str, dest: &Path, mode: u32) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(dest)?,
            Ok(_) => fs::remove_file(dest)?,
            Err(_) => {}
        }
        let src = self.path_of(hash);
        fs::copy(&src, dest).with_context(|| format!("copy {hash} to {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }))?;
        }
        Ok(())
    }
}

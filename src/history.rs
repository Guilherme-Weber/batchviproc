//! Processed-file history: one SHA-256 hash of the absolute path per line (hex).
//! Uses the same file and hashing as the Electron version, so existing history carries over.
//!
//! - Hashes are kept in memory as raw 32-byte arrays (a fraction of the size of hex strings).
//! - New entries are appended to the file; it is only rewritten when the history is cleared,
//!   so large histories (hundreds of thousands of entries) cost almost nothing to save.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

type Hash = [u8; 32];

pub struct History {
    path: PathBuf,
    hashes: HashSet<Hash>,
    /// Added since the last save, in insertion order.
    unsaved: Vec<Hash>,
    /// The file no longer matches memory (after clear) and must be rewritten.
    needs_rewrite: bool,
}

fn hash_path(file: &Path) -> Hash {
    Sha256::digest(file.to_string_lossy().as_bytes()).into()
}

fn parse_hex(line: &str) -> Option<Hash> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(line, &mut out).ok()?;
    Some(out)
}

impl History {
    pub fn load(path: PathBuf) -> Self {
        let hashes = std::fs::read_to_string(&path)
            .map(|data| data.split('\n').filter_map(|l| parse_hex(l.trim())).collect())
            .unwrap_or_default();
        Self {
            path,
            hashes,
            unsaved: Vec::new(),
            needs_rewrite: false,
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn contains(&self, file: &Path) -> bool {
        self.hashes.contains(&hash_path(file))
    }

    /// Returns true if the file was newly added.
    pub fn add(&mut self, file: &Path) -> bool {
        let hash = hash_path(file);
        let added = self.hashes.insert(hash);
        if added {
            self.unsaved.push(hash);
        }
        added
    }

    pub fn clear(&mut self) {
        self.hashes = HashSet::new();
        self.unsaved = Vec::new();
        self.needs_rewrite = true;
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if !self.needs_rewrite && self.unsaved.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if self.needs_rewrite {
            let lines: Vec<String> = self.hashes.iter().map(hex::encode).collect();
            std::fs::write(&self.path, lines.join("\n"))?;
            self.needs_rewrite = false;
        } else {
            // The file has no trailing newline, so separate from existing content first.
            let has_content = std::fs::metadata(&self.path).is_ok_and(|m| m.len() > 0);
            let mut data = String::with_capacity(self.unsaved.len() * 65);
            for (i, h) in self.unsaved.iter().enumerate() {
                if has_content || i > 0 {
                    data.push('\n');
                }
                data.push_str(&hex::encode(h));
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(data.as_bytes())?;
        }
        self.unsaved.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_node_sha256_of_path() {
        // Same as crypto.createHash('sha256').update('C:\\videos\\a.mp4').digest('hex') in Node.
        let expected = hex::encode(Sha256::digest(r"C:\videos\a.mp4".as_bytes()));
        assert_eq!(hex::encode(hash_path(Path::new(r"C:\videos\a.mp4"))), expected);
    }

    #[test]
    fn appends_new_entries_and_rewrites_only_after_clear() {
        let file = std::env::temp_dir().join(format!("bvp-history-{}.txt", std::process::id()));
        let a = hex::encode(hash_path(Path::new(r"C:\v\a.mp4")));
        std::fs::write(&file, format!("{a}\n\n  zz-not-hex  ")).unwrap();

        let mut h = History::load(file.clone());
        assert_eq!(h.len(), 1);
        assert!(h.contains(Path::new(r"C:\v\a.mp4")));
        assert!(!h.add(Path::new(r"C:\v\a.mp4")));
        assert!(h.add(Path::new(r"C:\v\b.mp4")));
        assert!(h.add(Path::new(r"C:\v\c.mp4")));
        h.save().unwrap();

        // Appended without touching existing lines; reloads to the same set.
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.starts_with(&format!("{a}\n\n  zz-not-hex  \n")));
        assert!(!saved.ends_with('\n'));
        assert_eq!(History::load(file.clone()).len(), 3);

        h.clear();
        assert!(h.add(Path::new(r"C:\v\d.mp4")));
        h.save().unwrap();
        let reloaded = History::load(file.clone());
        assert_eq!(reloaded.len(), 1);
        assert!(reloaded.contains(Path::new(r"C:\v\d.mp4")));
        let _ = std::fs::remove_file(file);
    }
}

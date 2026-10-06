//! A fingerprint of what a project package contains: the manifest byte for byte, and each asset's
//! name and size. A package that was only touched (a sync client rewriting metadata, a permission
//! change, the same bytes saved again) has the same digest as before, so it is not treated as a
//! change.
//!
//! Assets are not read: every save and open takes a fresh digest, and hashing every image of a
//! large project would hold each save for seconds. Anything that edits a project rewrites its
//! manifest, and a PNG whose pixels change all but always changes size, so names and sizes catch
//! the rest from the file system alone.

use sha2::{Digest, Sha256};
use std::path::Path;

/// A fingerprint of a project package (`ProjectDigest`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectDigest {
    pub value: Vec<u8>,
}

impl ProjectDigest {
    /// Reads the package outside file coordination on purpose: it is called after a change was
    /// already seen and it must never wait on a writer. A package caught half written yields a
    /// digest that matches nothing, or an error; both make the caller wait for the next change.
    pub fn compute(url: &Path) -> std::io::Result<ProjectDigest> {
        let mut hasher = Sha256::new();
        let manifest = std::fs::read(url.join("manifest.json"))?;
        hasher.update(&manifest);
        let images = url.join("images");
        let mut names: Vec<String> = match std::fs::read_dir(&images) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        for name in names {
            let metadata = std::fs::metadata(images.join(&name))?;
            if !metadata.is_file() {
                continue;
            }
            hasher.update(name.as_bytes());
            // `withUnsafeBytes(of: UInt64)` in the Swift hashed the file size in the host's byte
            // order; the digest only ever compares against itself, so the same order keeps it stable.
            hasher.update(metadata.len().to_ne_bytes());
        }
        Ok(ProjectDigest { value: hasher.finalize().to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("compositor-digest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("images")).unwrap();
        directory
    }

    #[test]
    fn a_manifest_alone_hashes_its_bytes() {
        let directory = package("manifest");
        std::fs::write(directory.join("manifest.json"), b"hello").unwrap();
        let digest = ProjectDigest::compute(&directory).unwrap();
        // SHA-256("hello").
        let expected = [
            0x2c, 0xf2, 0x4d, 0xba, 0x5f, 0xb0, 0xa3, 0x0e, 0x26, 0xe8, 0x3b, 0x2a, 0xc5, 0xb9, 0xe2, 0x9e,
            0x1b, 0x16, 0x1e, 0x5c, 0x1f, 0xa7, 0x42, 0x5e, 0x73, 0x04, 0x33, 0x62, 0x93, 0x8b, 0x98, 0x24,
        ];
        assert_eq!(digest.value, expected);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn assets_are_hashed_by_sorted_name_and_size() {
        let directory = package("assets");
        std::fs::write(directory.join("manifest.json"), b"hello").unwrap();
        std::fs::write(directory.join("images").join("b.png"), b"beta").unwrap();
        std::fs::write(directory.join("images").join("a.png"), b"a").unwrap();
        // Directories under images are left out, as `isRegularFile` did.
        std::fs::create_dir_all(directory.join("images").join("sub")).unwrap();
        let digest = ProjectDigest::compute(&directory).unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello");
        hasher.update(b"a.png");
        hasher.update(1u64.to_ne_bytes());
        hasher.update(b"b.png");
        hasher.update(4u64.to_ne_bytes());
        let expected: Vec<u8> = hasher.finalize().to_vec();
        assert_eq!(digest.value, expected);

        // Rewriting an asset with the same size does not change the digest.
        std::fs::write(directory.join("images").join("b.png"), b"BETA").unwrap();
        assert_eq!(ProjectDigest::compute(&directory).unwrap(), digest);
        // Changing its size does.
        std::fs::write(directory.join("images").join("b.png"), b"beta!").unwrap();
        assert_ne!(ProjectDigest::compute(&directory).unwrap(), digest);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn touching_the_manifest_changes_the_digest_and_a_missing_one_errors() {
        let directory = package("manifest-change");
        std::fs::write(directory.join("manifest.json"), b"{}").unwrap();
        let before = ProjectDigest::compute(&directory).unwrap();
        std::fs::write(directory.join("manifest.json"), b"[ ]").unwrap();
        assert_ne!(ProjectDigest::compute(&directory).unwrap(), before);

        let missing = std::env::temp_dir().join("compositor-digest-does-not-exist");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(ProjectDigest::compute(&missing).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}

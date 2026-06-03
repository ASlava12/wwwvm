//! Content-addressed storage for paged VM snapshots — the server side of the
//! custom-snapshot platform.
//!
//! A snapshot is stored as a small **manifest** (the non-RAM bytes + the ordered
//! list of its RAM page hashes; see [`wwwvm_vm::paged`]) plus its RAM **pages**,
//! each a file named by its own blake3 hash. Because pages are content-addressed:
//!
//! * **dedup is automatic** — two snapshots sharing a base store its pages once;
//! * **uploads are diffs** — a client uploads only the pages the store lacks
//!   (the recipe-dirtied ones), then the manifest;
//! * **writes are verifiable** — `put_page` recomputes the hash and rejects a
//!   body that doesn't match its claimed name, so a page can't be corrupted or
//!   spoofed (an attacker can't poison a base page another snapshot references).
//!
//! This crate is the storage logic only (filesystem-backed, transport-agnostic).
//! The HTTP service wraps it: admin-token-gated `PUT` for pages/manifests, open
//! `GET` (or serve the directory statically). Page/manifest *reads* are immutable
//! and safe to cache/CDN.

#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A blake3 content hash (also the page's file name, hex-encoded).
pub type Hash = [u8; 32];

/// Why a write was rejected.
#[derive(Debug)]
pub enum PutError {
    /// `put_page`: blake3(body) didn't equal the claimed hash.
    HashMismatch,
    /// A manifest id with disallowed characters (only `[A-Za-z0-9._-]`, non-empty,
    /// no leading dot) — guards against path traversal.
    BadName,
    /// Underlying filesystem error.
    Io(io::Error),
}

impl From<io::Error> for PutError {
    fn from(e: io::Error) -> Self {
        PutError::Io(e)
    }
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PutError::HashMismatch => write!(f, "page body does not match its hash"),
            PutError::BadName => write!(f, "invalid manifest id"),
            PutError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for PutError {}

/// Lowercase-hex encode a 32-byte hash (64 chars) — the page file name and URL.
pub fn to_hex(h: &Hash) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Parse a 64-char lowercase-hex hash, or `None` if malformed. Rejecting non-hex
/// here is also what keeps a page name from ever containing a path separator.
pub fn from_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for (i, o) in out.iter_mut().enumerate() {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        *o = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// A manifest id is safe iff it's non-empty, all `[A-Za-z0-9._-]`, and doesn't
/// start with a dot (so no `.`/`..`/hidden traversal). No path separators.
pub fn valid_manifest_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// A filesystem-backed content-addressed store rooted at a directory with
/// `pages/` and `manifests/` subdirectories.
pub struct Store {
    pages: PathBuf,
    manifests: PathBuf,
}

impl Store {
    /// Open (creating if absent) a store under `root`.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let pages = root.join("pages");
        let manifests = root.join("manifests");
        fs::create_dir_all(&pages)?;
        fs::create_dir_all(&manifests)?;
        Ok(Self { pages, manifests })
    }

    fn page_path(&self, h: &Hash) -> PathBuf {
        self.pages.join(to_hex(h))
    }

    /// Store one page. Verifies `blake3(body) == hash` first (rejecting a
    /// tampered/mislabelled body). Idempotent: `Ok(true)` if newly written,
    /// `Ok(false)` if the store already had it (the common case — most pages of
    /// a derived snapshot are shared with the base).
    pub fn put_page(&self, hash: &Hash, body: &[u8]) -> Result<bool, PutError> {
        if blake3::hash(body).as_bytes() != hash {
            return Err(PutError::HashMismatch);
        }
        let path = self.page_path(hash);
        if path.exists() {
            return Ok(false);
        }
        // Write to a temp sibling then rename, so a concurrent reader never sees
        // a half-written page (and a crash mid-write leaves only a stray .tmp).
        write_atomic(&path, body)?;
        Ok(true)
    }

    /// Whether the store already holds this page (lets a client skip uploading
    /// it). The full "which pages am I missing" check is this over the manifest.
    pub fn has_page(&self, hash: &Hash) -> bool {
        self.page_path(hash).exists()
    }

    /// Fetch a page's bytes, or `None` if absent.
    pub fn get_page(&self, hash: &Hash) -> io::Result<Option<Vec<u8>>> {
        read_opt(&self.page_path(hash))
    }

    /// Store a manifest under `id` (validated). Overwrites — a manifest is named
    /// by the client (e.g. a snapshot slug), not content-addressed.
    pub fn put_manifest(&self, id: &str, body: &[u8]) -> Result<(), PutError> {
        if !valid_manifest_id(id) {
            return Err(PutError::BadName);
        }
        write_atomic(&self.manifests.join(id), body)?;
        Ok(())
    }

    /// Fetch a manifest by id, or `None` if absent. Rejects an invalid id.
    pub fn get_manifest(&self, id: &str) -> Result<Option<Vec<u8>>, PutError> {
        if !valid_manifest_id(id) {
            return Err(PutError::BadName);
        }
        Ok(read_opt(&self.manifests.join(id))?)
    }

    /// List stored manifest ids (sorted), for a "pick a snapshot" UI.
    pub fn list_manifests(&self) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.manifests)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    if valid_manifest_id(name) {
                        ids.push(name.to_string());
                    }
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// Read a file, mapping NotFound to `None`.
fn read_opt(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write `body` to `path` via a temp sibling + rename (atomic on the same fs).
fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        // Unique-per-test dir under the system temp (pid + a counter keep
        // parallel tests from colliding; no Instant/rand needed).
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("wwwvm-snapstore-{}-{n}", std::process::id()))
    }

    fn h(body: &[u8]) -> Hash {
        *blake3::hash(body).as_bytes()
    }

    #[test]
    fn hex_round_trips_and_rejects_bad() {
        let hash = h(b"hello");
        let s = to_hex(&hash);
        assert_eq!(s.len(), 64);
        assert_eq!(from_hex(&s), Some(hash));
        assert_eq!(from_hex("xyz"), None); // too short
        assert_eq!(from_hex(&"g".repeat(64)), None); // non-hex
    }

    #[test]
    fn put_page_verifies_hash() {
        let root = tmp_root();
        let store = Store::open(&root).unwrap();
        let body = b"a page of data";
        let good = h(body);
        // Correct hash writes and reads back.
        assert!(store.put_page(&good, body).unwrap(), "newly written");
        assert!(store.has_page(&good));
        assert_eq!(store.get_page(&good).unwrap().as_deref(), Some(&body[..]));
        // Wrong claimed hash is rejected and nothing is written under it.
        let bogus = h(b"something else");
        assert!(matches!(
            store.put_page(&bogus, body),
            Err(PutError::HashMismatch)
        ));
        assert!(!store.has_page(&bogus));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn put_page_is_idempotent_dedup() {
        let root = tmp_root();
        let store = Store::open(&root).unwrap();
        let body = vec![0u8; 4096];
        let hash = h(&body);
        assert!(store.put_page(&hash, &body).unwrap(), "first write");
        assert!(
            !store.put_page(&hash, &body).unwrap(),
            "already present → dedup"
        );
        assert_eq!(store.get_page(&hash).unwrap().unwrap(), body);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_page_reads_none() {
        let root = tmp_root();
        let store = Store::open(&root).unwrap();
        assert!(!store.has_page(&h(b"nope")));
        assert!(store.get_page(&h(b"nope")).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_round_trip_and_list() {
        let root = tmp_root();
        let store = Store::open(&root).unwrap();
        store.put_manifest("alpine-base", b"manifest-1").unwrap();
        store.put_manifest("task_42", b"manifest-2").unwrap();
        assert_eq!(
            store.get_manifest("alpine-base").unwrap().as_deref(),
            Some(&b"manifest-1"[..])
        );
        assert!(store.get_manifest("absent").unwrap().is_none());
        assert_eq!(
            store.list_manifests().unwrap(),
            vec!["alpine-base".to_string(), "task_42".to_string()]
        );
        // Overwrite is allowed (manifests are named, not content-addressed).
        store.put_manifest("alpine-base", b"v2").unwrap();
        assert_eq!(
            store.get_manifest("alpine-base").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_id_validation_blocks_traversal() {
        assert!(valid_manifest_id("alpine-base"));
        assert!(valid_manifest_id("a.b_c-1"));
        assert!(!valid_manifest_id(""));
        assert!(!valid_manifest_id("../etc/passwd"));
        assert!(!valid_manifest_id("a/b"));
        assert!(!valid_manifest_id(".hidden"));
        assert!(!valid_manifest_id("..")); // starts with dot
        let root = tmp_root();
        let store = Store::open(&root).unwrap();
        assert!(matches!(
            store.put_manifest("../escape", b"x"),
            Err(PutError::BadName)
        ));
        assert!(matches!(store.get_manifest("a/b"), Err(PutError::BadName)));
        let _ = fs::remove_dir_all(&root);
    }
}

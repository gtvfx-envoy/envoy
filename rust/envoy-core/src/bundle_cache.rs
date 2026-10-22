//! Local bundle caching with hash-based deduplication and retention policies.
//!
//! Provides [`BundleCache`] for storing production bundles locally, enabling
//! offline use and faster repeated builds. Content-addressed storage ensures
//! identical bundles are stored only once regardless of how many times they
//! are fetched.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::semver::{SemVer, SemVerError, VersionSpec};

/// Error type for bundle cache operations.
#[derive(Debug, Error)]
pub enum BundleCacheError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid JSON in cache manifest at {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },

    #[error("bundle '{bundle_id}' version '{version}' not found in cache")]
    NotFound { bundle_id: String, version: String },

    #[error("semver error for '{bundle_id}': {source}")]
    SemVer {
        bundle_id: String,
        source: SemVerError,
    },

    #[error("cache root is not a directory: {path}")]
    NotADirectory { path: PathBuf },
}

/// Unique identifier for a bundle (e.g., `"bfd:test"`).
pub type BundleId = String;

/// A resolved bundle entry in the cache.
#[derive(Clone, Debug)]
pub struct CachedBundle {
    /// Content hash used as the storage key.
    pub content_hash: String,
    /// Absolute path to the cached bundle directory.
    pub path: PathBuf,
    /// When this entry was last accessed (Unix timestamp seconds).
    pub last_accessed: u64,
}

/// Metadata stored alongside each cached bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleMeta {
    /// Content hash of the bundle files.
    pub content_hash: String,
    /// When the bundle was first cached (Unix timestamp seconds).
    pub created_at: u64,
    /// When the bundle was last accessed (Unix timestamp seconds).
    pub last_accessed: u64,
    /// Size of the bundle in bytes.
    pub size_bytes: u64,
}

/// Configuration for cache retention behavior.
#[derive(Clone, Debug)]
pub struct RetentionConfig {
    /// Maximum age of a bundle before it is eligible for eviction (None = no limit).
    pub max_age: Option<Duration>,
    /// Maximum total cache size in bytes (None = no limit).
    pub max_size_bytes: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_age: Some(Duration::from_secs(30 * 24 * 3600)), // 30 days
            max_size_bytes: Some(10 * 1024 * 1024 * 1024),      // 10 GB
        }
    }
}

/// In-memory index of cached bundles, persisted as JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheIndex {
    /// Mapping from bundle ID + version to content hash.
    entries: HashMap<String, IndexEntry>,
}

/// Index entry mapping a logical bundle reference to its content hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexEntry {
    bundle_id: String,
    version: String,
    content_hash: String,
}

impl CacheIndex {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Generate a stable key for an (id, version) pair.
    fn entry_key(bundle_id: &str, version: &str) -> String {
        format!("{bundle_id}@{version}")
    }

    fn get(&self, bundle_id: &str, version: &str) -> Option<&IndexEntry> {
        self.entries.get(&Self::entry_key(bundle_id, version))
    }

    fn insert(&mut self, entry: IndexEntry) {
        let key = Self::entry_key(&entry.bundle_id, &entry.version);
        self.entries.insert(key, entry);
    }

    fn remove(&mut self, bundle_id: &str, version: &str) -> Option<IndexEntry> {
        let key = Self::entry_key(bundle_id, version);
        self.entries.remove(&key)
    }
}

/// Local bundle cache with content-addressed storage.
///
/// Bundles are stored under `<cache_root>/<content_hash>/` and indexed by
/// logical bundle ID + version in a JSON manifest at `<cache_root>/.index.json`.
pub struct BundleCache {
    root: PathBuf,
    retention: RetentionConfig,
    index: CacheIndex,
}

impl BundleCache {
    /// Open (or create) a bundle cache at `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, BundleCacheError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|source| BundleCacheError::Io {
            path: root.clone(),
            source,
        })?;

        if !root.is_dir() {
            return Err(BundleCacheError::NotADirectory { path: root });
        }

        let index_path = root.join(".index.json");
        let lock_path = root.join(".index.lock");
        let index = with_shared_index_lock(&lock_path, || load_index(&index_path))?;

        Ok(Self {
            root,
            retention: RetentionConfig::default(),
            index,
        })
    }

    /// Set the retention configuration for this cache.
    pub fn set_retention(&mut self, config: RetentionConfig) {
        self.retention = config;
    }

    /// Return the cache root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute a content hash for a directory by hashing all file contents.
    pub fn compute_content_hash(dir: &Path) -> Result<String, BundleCacheError> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 8192];

        let mut entries: Vec<PathBuf> = collect_files(dir)?;
        entries.sort(); // deterministic ordering

        for entry in entries {
            let rel = entry.strip_prefix(dir).map_err(|_| BundleCacheError::Io {
                path: dir.to_path_buf(),
                source: std::io::Error::other("path not under directory"),
            })?;
            hasher.update(rel.to_string_lossy().as_bytes());

            let mut file = fs::File::open(&entry).map_err(|source| BundleCacheError::Io {
                path: entry.clone(),
                source,
            })?;
            loop {
                let n = file
                    .read(&mut buffer)
                    .map_err(|source| BundleCacheError::Io {
                        path: entry.clone(),
                        source,
                    })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
        }

        Ok(hex_encode(hasher.finalize()))
    }

    /// Store a bundle directory in the cache and return its metadata.
    pub fn store(
        &mut self,
        bundle_id: &str,
        version: &str,
        source_dir: &Path,
    ) -> Result<CachedBundle, BundleCacheError> {
        let content_hash = Self::compute_content_hash(source_dir)?;
        let now = current_timestamp();

        let storage_dir = self.storage_path(&content_hash);
        if !storage_dir.exists() {
            fs::create_dir_all(&storage_dir).map_err(|source| BundleCacheError::Io {
                path: storage_dir.clone(),
                source,
            })?;
            copy_directory(source_dir, &storage_dir)?;
        }

        let size_bytes = directory_size(&storage_dir);

        let meta = BundleMeta {
            content_hash: content_hash.clone(),
            created_at: now,
            last_accessed: now,
            size_bytes,
        };

        // Write metadata sidecar.
        let meta_path = storage_dir.join(".meta.json");
        let meta_json =
            serde_json::to_string_pretty(&meta).map_err(|source| BundleCacheError::Json {
                path: meta_path.clone(),
                source,
            })?;
        fs::write(&meta_path, meta_json).map_err(|source| BundleCacheError::Io {
            path: meta_path,
            source,
        })?;

        let index_path = self.index_path();
        let lock_path = self.lock_file_path();
        let updated_index = with_exclusive_index_lock(&lock_path, || {
            let mut index = load_index(&index_path)?;

            if let Some(existing) = index.get(bundle_id, version) {
                if existing.content_hash == content_hash {
                    return Ok(index);
                }
            }

            index.insert(IndexEntry {
                bundle_id: bundle_id.to_string(),
                version: version.to_string(),
                content_hash: content_hash.clone(),
            });

            persist_index(&index_path, &index)?;
            Ok(index)
        })?;

        self.index = updated_index;

        Ok(CachedBundle {
            content_hash,
            path: storage_dir,
            last_accessed: now,
        })
    }

    /// Retrieve a cached bundle by ID and version.
    pub fn get(&self, bundle_id: &str, version: &str) -> Result<CachedBundle, BundleCacheError> {
        let entry =
            self.index
                .get(bundle_id, version)
                .ok_or_else(|| BundleCacheError::NotFound {
                    bundle_id: bundle_id.to_string(),
                    version: version.to_string(),
                })?;

        let meta_path = self.storage_path(&entry.content_hash).join(".meta.json");
        let now = current_timestamp();

        // Update access time.
        if meta_path.exists() {
            if let Ok(contents) = fs::read_to_string(&meta_path) {
                if let Ok(mut meta) = serde_json::from_str::<BundleMeta>(&contents) {
                    meta.last_accessed = now;
                    if let Ok(json) = serde_json::to_string_pretty(&meta) {
                        let _ = fs::write(&meta_path, json);
                    }
                }
            }
        }

        Ok(CachedBundle {
            content_hash: entry.content_hash.clone(),
            path: self.storage_path(&entry.content_hash),
            last_accessed: now,
        })
    }

    /// Check if a bundle is cached.
    pub fn contains(&self, bundle_id: &str, version: &str) -> bool {
        self.index.get(bundle_id, version).is_some()
    }

    /// Remove a specific bundle from the cache.
    pub fn remove(&mut self, bundle_id: &str, version: &str) -> Result<bool, BundleCacheError> {
        let index_path = self.index_path();
        let lock_path = self.lock_file_path();
        let (updated_index, removed) = with_exclusive_index_lock(&lock_path, || {
            let mut index = load_index(&index_path)?;
            if let Some(entry) = index.remove(bundle_id, version) {
                let storage_dir = self.storage_path(&entry.content_hash);
                if storage_dir.exists() {
                    fs::remove_dir_all(&storage_dir).map_err(|source| BundleCacheError::Io {
                        path: storage_dir,
                        source,
                    })?;
                }
                persist_index(&index_path, &index)?;
                Ok((index, true))
            } else {
                Ok((index, false))
            }
        })?;

        self.index = updated_index;
        Ok(removed)
    }

    /// Resolve the best matching version from cached bundles for a given spec.
    pub fn resolve(
        &self,
        bundle_id: &str,
        spec: &VersionSpec,
    ) -> Result<Option<CachedBundle>, BundleCacheError> {
        // Collect all cached versions for this bundle.
        let mut candidates: Vec<(String, SemVer)> = Vec::new();
        for (key, entry) in &self.index.entries {
            if key.starts_with(&format!("{bundle_id}@")) {
                let version_str = key.strip_prefix(&format!("{bundle_id}@")).unwrap_or("");
                if let Ok(version) = SemVer::parse(version_str) {
                    candidates.push((entry.version.clone(), version));
                }
            }
        }

        // Sort descending and find first match.
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        if let Some((_version_str, best_version)) =
            candidates.into_iter().find(|(_, v)| spec.matches(v))
        {
            self.get(bundle_id, &best_version.to_string()).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Run retention policy: evict expired or oversized entries.
    pub fn compact(&mut self) -> Result<usize, BundleCacheError> {
        let mut evicted = 0;
        let now = current_timestamp();
        let mut total_size: u64 = 0;

        // Calculate total size first.
        for entry in self.index.entries.values() {
            let meta_path = self.storage_path(&entry.content_hash).join(".meta.json");
            if let Ok(contents) = fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<BundleMeta>(&contents) {
                    total_size += meta.size_bytes;
                }
            }
        }

        // Collect evictable entries sorted by last access (oldest first).
        let mut evictable: Vec<(String, String)> = self
            .index
            .entries
            .values()
            .filter_map(|entry| {
                // Skip if this content hash is referenced by another bundle.
                let is_referenced = self.index.entries.values().any(|e| {
                    e.content_hash == entry.content_hash
                        && !(e.bundle_id == entry.bundle_id && e.version == entry.version)
                });
                if !is_referenced {
                    Some((entry.bundle_id.clone(), entry.version.clone()))
                } else {
                    None
                }
            })
            .collect();

        evictable.sort_by(|a, b| {
            let a_hash = self
                .index
                .get(&a.0, &a.1)
                .map(|e| e.content_hash.clone())
                .unwrap_or_default();
            let b_hash = self
                .index
                .get(&b.0, &b.1)
                .map(|e| e.content_hash.clone())
                .unwrap_or_default();
            a_hash.cmp(&b_hash)
        });

        for (bundle_id, version) in evictable {
            // Check age-based retention.
            if let Some(max_age) = self.retention.max_age {
                let content_hash = self
                    .index
                    .get(&bundle_id, &version)
                    .map(|e| e.content_hash.clone())
                    .unwrap_or_default();
                let meta_path = self.storage_path(&content_hash).join(".meta.json");
                if let Ok(contents) = fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<BundleMeta>(&contents) {
                        let age = Duration::from_secs(now.saturating_sub(meta.created_at));
                        if age > max_age {
                            if self.remove(&bundle_id, &version).unwrap_or(false) {
                                evicted += 1;
                            }
                            continue;
                        }
                    }
                }
            }

            // Check size-based retention.
            if let Some(max_size) = self.retention.max_size_bytes {
                if total_size > max_size && self.remove(&bundle_id, &version).unwrap_or(false) {
                    evicted += 1;
                    total_size = total_size.saturating_sub(1); // approximate
                }
            }
        }

        // Persist updated index.
        self.persist_index()?;

        Ok(evicted)
    }

    /// Return the number of bundles in the cache.
    pub fn len(&self) -> usize {
        self.index.entries.len()
    }

    /// Return `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.index.entries.is_empty()
    }

    /// List all cached bundles as (id, version) pairs.
    pub fn list(&self) -> Vec<(&str, &str)> {
        self.index
            .entries
            .values()
            .map(|e| (e.bundle_id.as_str(), e.version.as_str()))
            .collect()
    }

    /// Persist the current index to disk.
    fn persist_index(&self) -> Result<(), BundleCacheError> {
        persist_index(&self.index_path(), &self.index)
    }

    /// Return the storage directory for a content hash.
    fn storage_path(&self, content_hash: &str) -> PathBuf {
        self.root.join(content_hash)
    }

    /// Return the manifest path for this cache.
    fn index_path(&self) -> PathBuf {
        self.root.join(".index.json")
    }

    /// Return the sidecar lock path for this cache's manifest.
    fn lock_file_path(&self) -> PathBuf {
        self.root.join(".index.lock")
    }
}

// ---------------------------------------------------------------------------
// Default cache resolution
// ---------------------------------------------------------------------------
//
// These helpers wire a [`BundleCache`] into envoy's default runtime flow
// (CLI + Python API) so bundles resolved via `runtime::load_registry` and
// `runtime::resolve_cached_bundles` are automatically checked against a local
// cache without every caller needing to construct one by hand.

/// Environment variable that, when set, overrides the bundle cache directory.
pub const BUNDLE_CACHE_DIR_VAR: &str = "ENVOY_BUNDLE_CACHE";

/// Environment variable that, when set to a truthy value (`1`, `true`, or
/// `yes`, case-insensitively), disables the automatic bundle cache entirely.
pub const BUNDLE_CACHE_DISABLE_VAR: &str = "ENVOY_DISABLE_BUNDLE_CACHE";

/// User config setting key for an explicit bundle cache directory. Set to
/// an empty string to fall back to the platform default location.
pub const BUNDLE_CACHE_DIR_SETTING: &str = "bundle_cache_dir";

/// Return the platform-appropriate default bundle cache directory.
///
/// Mirrors [`crate::user_config::default_config_path`]'s platform-detection
/// strategy, but resolves to a *cache*-appropriate location:
/// - Windows reads `%LOCALAPPDATA%`, falling back to `%USERPROFILE%`
/// - non-Windows reads `$XDG_CACHE_HOME`, falling back to `$HOME/.cache`
pub fn default_cache_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("envoy")
            .join("bundle_cache")
    }

    #[cfg(not(target_os = "windows"))]
    {
        let xdg_cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        if let Some(cache_home) = xdg_cache {
            return cache_home.join("envoy").join("bundle_cache");
        }

        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache")
            .join("envoy")
            .join("bundle_cache")
    }
}

/// Return whether an environment variable holds a truthy value (`"1"`,
/// `"true"`, or `"yes"`, case-insensitively).
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

/// Resolve the effective bundle cache directory, or `None` when the cache
/// is disabled.
///
/// Precedence order:
/// 1. [`BUNDLE_CACHE_DISABLE_VAR`] truthy -> disabled (`None`)
/// 2. [`BUNDLE_CACHE_DIR_VAR`] -> explicit path override
/// 3. `bundle_cache_dir` user config setting (only when `use_user_config`
///    is `true`; callers that need to honor an `--ignore-config`-style flag
///    should pass `false`)
/// 4. [`default_cache_root`]
pub fn resolve_bundle_cache_dir(use_user_config: bool) -> Option<PathBuf> {
    if env_flag_enabled(BUNDLE_CACHE_DISABLE_VAR) {
        return None;
    }

    if let Some(path) = std::env::var_os(BUNDLE_CACHE_DIR_VAR) {
        return Some(PathBuf::from(path));
    }

    if use_user_config {
        let user_cfg = crate::user_config::UserConfig::load(None);
        if let Some(configured) = user_cfg.get(BUNDLE_CACHE_DIR_SETTING) {
            if !configured.is_empty() {
                return Some(PathBuf::from(configured));
            }
        }
    }

    Some(default_cache_root())
}

/// Open the default bundle cache for this environment, or `None` when the
/// cache is disabled or fails to open.
///
/// Opening the cache is treated as best-effort: an I/O failure (e.g. a
/// read-only or unavailable cache directory) is logged as a warning and
/// results in `None` rather than aborting the caller, matching the graceful
/// degradation used elsewhere in envoy (e.g. team config discovery).
pub fn open_default_bundle_cache(use_user_config: bool) -> Option<BundleCache> {
    let dir = resolve_bundle_cache_dir(use_user_config)?;
    match BundleCache::new(&dir) {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(
                cache_dir = %dir.display(),
                "Failed to open bundle cache: {error}"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Load the on-disk cache index if present.
fn load_index(index_path: &Path) -> Result<CacheIndex, BundleCacheError> {
    match fs::read_to_string(index_path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|source| BundleCacheError::Json {
            path: index_path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(CacheIndex::new()),
        Err(source) => Err(BundleCacheError::Io {
            path: index_path.to_path_buf(),
            source,
        }),
    }
}

/// Persist a cache index to disk.
fn persist_index(index_path: &Path, index: &CacheIndex) -> Result<(), BundleCacheError> {
    let json = serde_json::to_string_pretty(index).map_err(|source| BundleCacheError::Json {
        path: index_path.to_path_buf(),
        source,
    })?;
    fs::write(index_path, json).map_err(|source| BundleCacheError::Io {
        path: index_path.to_path_buf(),
        source,
    })
}

/// Open the sidecar lock file used to guard index updates.
fn open_lock_file(lock_path: &Path) -> Result<fs::File, BundleCacheError> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|source| BundleCacheError::Io {
            path: lock_path.to_path_buf(),
            source,
        })
}

/// Execute an operation while holding a shared index lock.
fn with_shared_index_lock<T>(
    lock_path: &Path,
    operation: impl FnOnce() -> Result<T, BundleCacheError>,
) -> Result<T, BundleCacheError> {
    let lock_file = open_lock_file(lock_path)?;
    FileExt::lock_shared(&lock_file).map_err(|source| BundleCacheError::Io {
        path: lock_path.to_path_buf(),
        source,
    })?;

    let result = operation();
    let unlock_result = FileExt::unlock(&lock_file).map_err(|source| BundleCacheError::Io {
        path: lock_path.to_path_buf(),
        source,
    });

    match result {
        Ok(value) => {
            unlock_result?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

/// Execute an operation while holding an exclusive index lock.
fn with_exclusive_index_lock<T>(
    lock_path: &Path,
    operation: impl FnOnce() -> Result<T, BundleCacheError>,
) -> Result<T, BundleCacheError> {
    let lock_file = open_lock_file(lock_path)?;
    FileExt::lock(&lock_file).map_err(|source| BundleCacheError::Io {
        path: lock_path.to_path_buf(),
        source,
    })?;

    let result = operation();
    let unlock_result = FileExt::unlock(&lock_file).map_err(|source| BundleCacheError::Io {
        path: lock_path.to_path_buf(),
        source,
    });

    match result {
        Ok(value) => {
            unlock_result?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

fn collect_files(dir: &Path) -> Result<Vec<PathBuf>, BundleCacheError> {
    let mut files = Vec::new();
    collect_files_recursive(dir, dir, &mut files)?;
    Ok(files)
}

fn collect_files_recursive(
    _base: &Path,
    dir: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), BundleCacheError> {
    let entries = fs::read_dir(dir).map_err(|source| BundleCacheError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| BundleCacheError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(_base, &path, files)?;
        } else if !path.file_name().is_some_and(|name| name == ".meta.json") {
            files.push(path);
        }
    }

    Ok(())
}

fn copy_directory(src: &Path, dst: &Path) -> Result<(), BundleCacheError> {
    fs::create_dir_all(dst).map_err(|source| BundleCacheError::Io {
        path: dst.to_path_buf(),
        source,
    })?;

    for entry in fs::read_dir(src).map_err(|source| BundleCacheError::Io {
        path: src.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| BundleCacheError::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_directory(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|source| BundleCacheError::Io {
                path: src_path,
                source,
            })?;
        }
    }

    Ok(())
}

fn directory_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = fs::metadata(&path) {
                    total += metadata.len();
                }
            } else if path.is_dir() {
                total += directory_size(&path);
            }
        }
    }
    total
}

/// Encode a byte slice as lowercase hexadecimal.
fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    static TEMP_CACHE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_cache_dir() -> PathBuf {
        // Use a unique directory per test to avoid conflicts when running in parallel.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = TEMP_CACHE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "envoy_test_cache_{}_{}_{}",
            std::process::id(),
            timestamp,
            sequence
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn temp_cache_dir_returns_unique_paths() {
        assert_ne!(temp_cache_dir(), temp_cache_dir());
    }

    fn create_sample_bundle(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn cache_stores_and_retrieves_bundles() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");

        let bundle_dir = cache_root.join("source_bundle");
        create_sample_bundle(&bundle_dir, "data.txt", "hello world");

        let cached = cache.store("test:bundle", "1.0.0", &bundle_dir).unwrap();
        assert!(cached.path.exists());
        assert_eq!(cached.content_hash.len(), 64); // hex-encoded hash

        let retrieved = cache.get("test:bundle", "1.0.0").unwrap();
        assert_eq!(retrieved.content_hash, cached.content_hash);
        assert_eq!(
            fs::read_to_string(retrieved.path.join("data.txt")).unwrap(),
            "hello world"
        );

        // Cleanup.
        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn store_persists_the_index_across_a_fresh_cache_instance() {
        // Regression test: `store()` previously only updated the in-memory
        // index and never wrote `.index.json`, so a *different* BundleCache
        // instance pointed at the same root (e.g. a separate process, or a
        // separate invocation of `BundleCache::new` in the same process)
        // could never see anything stored by another instance.
        let cache_root = temp_cache_dir();

        {
            let mut cache = BundleCache::new(&cache_root).expect("should create cache");
            let bundle_dir = cache_root.join("source_bundle");
            create_sample_bundle(&bundle_dir, "data.txt", "hello world");
            cache.store("test:bundle", "1.0.0", &bundle_dir).unwrap();
        }

        // Re-open a brand new instance, simulating a separate process.
        let reopened = BundleCache::new(&cache_root).expect("should reopen cache");
        let retrieved = reopened
            .get("test:bundle", "1.0.0")
            .expect("entry stored by the previous instance should be visible");
        assert_eq!(
            fs::read_to_string(retrieved.path.join("data.txt")).unwrap(),
            "hello world"
        );

        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn remove_persists_the_index_across_a_fresh_cache_instance() {
        let cache_root = temp_cache_dir();

        {
            let mut cache = BundleCache::new(&cache_root).expect("should create cache");
            let bundle_dir = cache_root.join("source_bundle");
            create_sample_bundle(&bundle_dir, "data.txt", "hello world");
            cache.store("test:bundle", "1.0.0", &bundle_dir).unwrap();
            assert!(cache.remove("test:bundle", "1.0.0").unwrap());
        }

        let reopened = BundleCache::new(&cache_root).expect("should reopen cache");
        assert!(matches!(
            reopened.get("test:bundle", "1.0.0"),
            Err(BundleCacheError::NotFound { .. })
        ));

        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn cache_deduplicates_identical_content() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");

        let bundle_a = cache_root.join("bundle_a");
        let bundle_b = cache_root.join("bundle_b");
        create_sample_bundle(&bundle_a, "data.txt", "same content");
        create_sample_bundle(&bundle_b, "data.txt", "same content");

        let cached1 = cache.store("test:a", "1.0.0", &bundle_a).unwrap();
        let cached2 = cache.store("test:b", "1.0.0", &bundle_b).unwrap();

        // Same content should share storage.
        assert_eq!(cached1.content_hash, cached2.content_hash);
        assert_eq!(cached1.path, cached2.path);

        // Cleanup.
        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn cache_returns_not_found_for_missing() {
        let cache_root = temp_cache_dir();
        let cache = BundleCache::new(&cache_root).expect("should create cache");

        assert!(cache.get("nonexistent", "1.0.0").is_err());

        // Cleanup.
        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn cache_removes_bundles() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");

        let bundle_dir = cache_root.join("source_bundle");
        create_sample_bundle(&bundle_dir, "data.txt", "remove me");

        cache.store("test:removable", "1.0.0", &bundle_dir).unwrap();
        assert!(cache.contains("test:removable", "1.0.0"));

        let removed = cache.remove("test:removable", "1.0.0").unwrap();
        assert!(removed);
        assert!(!cache.contains("test:removable", "1.0.0"));

        // Cleanup.
        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn cache_list_shows_all_bundles() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");

        let bundle_dir = cache_root.join("source_bundle");
        create_sample_bundle(&bundle_dir, "data.txt", "list me 1");
        cache.store("test:list1", "1.0.0", &bundle_dir).unwrap();

        create_sample_bundle(&bundle_dir, "data.txt", "list me 2");
        cache.store("test:list2", "2.0.0", &bundle_dir).unwrap();

        let list = cache.list();
        assert_eq!(list.len(), 2);

        // Cleanup.
        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn store_creates_lock_file_and_supports_sequential_updates() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");

        let bundle_a = cache_root.join("bundle_a");
        create_sample_bundle(&bundle_a, "data.txt", "first bundle");
        cache.store("test:first", "1.0.0", &bundle_a).unwrap();

        assert!(cache.lock_file_path().exists());

        let bundle_b = cache_root.join("bundle_b");
        create_sample_bundle(&bundle_b, "data.txt", "second bundle");
        cache.store("test:second", "2.0.0", &bundle_b).unwrap();

        let reopened = BundleCache::new(&cache_root).expect("should reopen cache");
        assert!(reopened.contains("test:first", "1.0.0"));
        assert!(reopened.contains("test:second", "2.0.0"));

        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn store_waits_for_an_existing_index_lock() {
        let cache_root = temp_cache_dir();
        let mut cache = BundleCache::new(&cache_root).expect("should create cache");
        let bundle_dir = cache_root.join("source_bundle");
        create_sample_bundle(&bundle_dir, "data.txt", "hello world");

        let lock_path = cache.lock_file_path();
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        FileExt::lock(&lock_file).unwrap();

        let (tx, rx) = mpsc::channel();
        let thread_bundle_dir = bundle_dir.clone();
        let handle = thread::spawn(move || {
            let result = cache.store("test:bundle", "1.0.0", &thread_bundle_dir);
            tx.send(result.is_ok()).unwrap();
        });

        thread::sleep(Duration::from_millis(150));
        assert!(rx.try_recv().is_err());

        FileExt::unlock(&lock_file).unwrap();

        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        handle.join().unwrap();

        let reopened = BundleCache::new(&cache_root).expect("should reopen cache");
        assert!(reopened.contains("test:bundle", "1.0.0"));

        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn concurrent_store_calls_preserve_both_index_entries() {
        let cache_root = temp_cache_dir();
        let bundle_a = cache_root.join("bundle_a");
        let bundle_b = cache_root.join("bundle_b");
        create_sample_bundle(&bundle_a, "data.txt", "first bundle");
        create_sample_bundle(&bundle_b, "data.txt", "second bundle");

        let barrier = Arc::new(Barrier::new(3));

        let root1 = cache_root.clone();
        let path1 = bundle_a.clone();
        let barrier1 = Arc::clone(&barrier);
        let handle1 = thread::spawn(move || {
            let mut cache = BundleCache::new(&root1).unwrap();
            barrier1.wait();
            cache.store("test:first", "1.0.0", &path1).unwrap();
        });

        let root2 = cache_root.clone();
        let path2 = bundle_b.clone();
        let barrier2 = Arc::clone(&barrier);
        let handle2 = thread::spawn(move || {
            let mut cache = BundleCache::new(&root2).unwrap();
            barrier2.wait();
            cache.store("test:second", "2.0.0", &path2).unwrap();
        });

        barrier.wait();
        handle1.join().unwrap();
        handle2.join().unwrap();

        let reopened = BundleCache::new(&cache_root).expect("should reopen cache");
        assert!(reopened.contains("test:first", "1.0.0"));
        assert!(reopened.contains("test:second", "2.0.0"));
        assert_eq!(reopened.len(), 2);

        let _ = fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn content_hash_is_deterministic() {
        let dir = temp_cache_dir().join("hash_test");
        create_sample_bundle(&dir, "a.txt", "content A");
        create_sample_bundle(&dir, "b.txt", "content B");

        let hash1 = BundleCache::compute_content_hash(&dir).unwrap();

        // Same content should produce same hash.
        let dir2 = temp_cache_dir().join("hash_test2");
        create_sample_bundle(&dir2, "a.txt", "content A");
        create_sample_bundle(&dir2, "b.txt", "content B");

        let hash2 = BundleCache::compute_content_hash(&dir2).unwrap();
        assert_eq!(hash1, hash2);

        // Cleanup.
        let _ = fs::remove_dir_all(temp_cache_dir().join("hash_test"));
        let _ = fs::remove_dir_all(temp_cache_dir().join("hash_test2"));
    }
}

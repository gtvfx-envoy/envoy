//! Bundle discovery and stack-backed loading for `envoy-core`.
//!
//! This module ports `py/envoy/_discovery.py` into Rust. It is responsible
//! for:
//! - resolving bundle IDs like `gt:pythoncore`
//! - scanning bundle root directories for checkout and published bundles
//! - loading bundle lists from YAML `.estack` files
//! - exposing small model types used by later environment/command ports
//!
//! Discovery currently supports two sources:
//! 1. Auto-discovery from `ENVOY_BNDL_ROOTS`
//! 2. Explicit runtime stack files
//!
//! Published bundles are detected by a `.bundle` marker file. Checkout
//! bundles are detected by a `.git/` directory. In both cases a valid envoy
//! bundle must also contain a `.envoy/` directory.
//!
//! A bundle's `namespace:name` identity comes from [`bundle_identity`]: the
//! `.bundle` marker for published bundles, the directory layout otherwise.

use std::env;
use std::path::Path;

use rayon::prelude::*;

use crate::discovery::cache::{load_cached_discovery_results, store_cached_discovery_results};
use crate::discovery::scan::discover_bundles_for_root;
use crate::error::Result;

const BUNDLE_ROOTS_VAR: &str = "ENVOY_BNDL_ROOTS";

/// Version sentinel for a bundle that lives directly in a git checkout.
pub const BUNDLE_CHECKOUT: &str = "checkout";

/// Default namespace prefix for bundles.
pub const BUNDLE_DEFAULT_NAMESPACE: &str = "gt";

/// Marker file written by `engit publish`.
pub const BUNDLE_MARKER_FILE: &str = ".bundle";

/// Per-bundle envoy config directory name.
pub const BUNDLE_ENV_DIR: &str = ".envoy";

pub mod bndlid;
pub(crate) mod cache;
pub(crate) mod files;
pub mod identity;
pub(crate) mod scan;
mod tests;
pub mod types;
pub(crate) mod util;

pub(crate) use bndlid::expand_bundle_path;
pub use bndlid::{is_bndlid, resolve_bndlid};
pub use cache::{discovery_cache_key, discovery_cache_lock_path, discovery_cache_path};
pub use files::{get_bundle_commands_files, get_bundle_env_files};
pub use identity::{bundle_identity, BundleIdentity, IdentitySource};
pub use scan::{
    find_bundle_roots, find_git_repos, has_envoy_env, is_git_repo, is_published_bundle,
    validate_bundle,
};
pub use types::{Bundle, BundleInfo};
pub use util::infer_namespace;
pub(crate) use util::{lexical_normalize, resolve_input_path};

/// Discover bundles under the provided root directories.
///
/// Results are cached on disk under envoy's cache root. A cached entry is
/// reused only when it is still fresh and a shallow fingerprint of each root
/// directory matches the current filesystem state. The fingerprint records
/// visible directories near the root plus each directory's `.git`, `.envoy`,
/// and `.bundle` state, while a short TTL keeps the cache conservative for
/// deeper filesystem changes.
pub fn discover_bundles_from_roots(root_dirs: &[String]) -> Vec<BundleInfo> {
    let roots = root_dirs
        .iter()
        .map(|root_str| resolve_input_path(Path::new(root_str)))
        .collect::<Vec<_>>();

    if let Some(cached) = load_cached_discovery_results(&roots, 5) {
        return cached;
    }

    let bundles = roots
        .par_iter()
        .map(|root| discover_bundles_for_root(root, 5))
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    store_cached_discovery_results(&roots, &bundles, 5);
    bundles
}

/// Discover bundles from the current stack, then fall back to bundle roots.
pub fn discover_bundles_auto() -> Result<Vec<BundleInfo>> {
    discover_bundles_auto_with(None)
}

/// [`discover_bundles_auto`] for a caller that is about to run something.
///
/// With a cache root in `snapshots`, a stack that loads is remembered there,
/// and one whose share cannot be reached starts from the last time it ran
/// (see [`Stack::from_selector`](crate::stack::Stack::from_selector)).
pub fn discover_bundles_auto_with(snapshots: Option<&Path>) -> Result<Vec<BundleInfo>> {
    if let Some(stack) = crate::stack::Stack::current_with_snapshots(
        false,
        None,
        crate::stack::DEFAULT_STACK_NAMESPACE,
        crate::stack::DEFAULT_STACK_MAX_DEPTH,
        snapshots,
    )? {
        let bundles = stack.bundle_infos()?;
        if let Some(root) = snapshots {
            if let Err(error) = stack.save_snapshot(root) {
                tracing::debug!("Could not remember stack {}: {error}", stack.name());
            }
        }
        if let Some(info) = stack.snapshot_info() {
            for line in info.describe(stack.name(), stack.registry_version()) {
                eprintln!("{line}");
            }
        }
        return Ok(bundles);
    }

    let roots_str = env::var(BUNDLE_ROOTS_VAR).unwrap_or_default();
    if roots_str.is_empty() {
        return Ok(Vec::new());
    }

    let root_dirs = roots_str
        .split(util::root_separator())
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if root_dirs.is_empty() {
        return Ok(Vec::new());
    }

    Ok(discover_bundles_from_roots(&root_dirs))
}

/// Load bundle definitions from a strict YAML `.estack` file.
pub fn load_bundles_from_stack(stack_file: &Path) -> Result<Vec<BundleInfo>> {
    crate::stack::Stack::new(stack_file)?.bundle_infos()
}

/// Return bundles from an explicit stack file or from auto-discovery.
pub fn get_bundles(stack_file: Option<&Path>) -> Result<Vec<BundleInfo>> {
    match stack_file {
        Some(stack_file) => load_bundles_from_stack(stack_file),
        None => discover_bundles_auto(),
    }
}

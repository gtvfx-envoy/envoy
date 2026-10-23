//! Canonical bundle identity.
//!
//! A bundle's identity is its `namespace:name` pair. A published bundle states
//! it in its `.bundle` marker, and the marker is authoritative: the directory a
//! published bundle sits in is normally a version label, so the directory name
//! says nothing reliable about the bundle. Checkouts and unmarked directories
//! fall back to the directory layout.
//!
//! Every route to a [`Bundle`](super::Bundle) or [`BundleInfo`](super::BundleInfo)
//! (scanning bundle roots, loading a stack, resolving a bundle ID or an
//! explicit path) goes through [`bundle_identity`], so one directory always has
//! one ID.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::util::{infer_namespace, json_value_to_string, json_value_truthy};
use super::{BUNDLE_DEFAULT_NAMESPACE, BUNDLE_MARKER_FILE};

/// Where a bundle's identity was read from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentitySource {
    /// The `.bundle` marker of a published bundle.
    Marker,
    /// The directory layout: the directory name, plus a parent directory that
    /// looks like a namespace. Used by checkouts and unmarked directories.
    Layout,
}

/// A bundle's canonical `namespace:name` identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleIdentity {
    /// Bundle namespace.
    pub namespace: String,
    /// Bundle name.
    pub name: String,
    /// Where the identity was read from.
    pub source: IdentitySource,
}

impl BundleIdentity {
    /// Return the namespaced bundle identifier.
    pub fn bndlid(&self) -> String {
        format!("{}:{}", self.namespace, self.name)
    }
}

/// Return the canonical identity of the bundle rooted at `bundle_root`.
///
/// The `.bundle` marker wins when it names the bundle:
/// - `"bndlid": "<namespace>:<name>"` supplies both parts;
/// - a marker without a namespace (`"bndlid": "<name>"`, or only `"name"`)
///   names the bundle and takes [`BUNDLE_DEFAULT_NAMESPACE`].
///
/// Without a usable marker the identity comes from the directory layout.
pub fn bundle_identity(bundle_root: &Path) -> BundleIdentity {
    marker_identity(bundle_root).unwrap_or_else(|| layout_identity(bundle_root))
}

fn marker_identity(bundle_root: &Path) -> Option<BundleIdentity> {
    let text = fs::read_to_string(bundle_root.join(BUNDLE_MARKER_FILE)).ok()?;
    let data = serde_json::from_str::<Value>(&text).ok()?;

    let declared = data
        .get("bndlid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|bndlid| !bndlid.is_empty());

    if let Some((namespace, name)) = declared.and_then(|bndlid| bndlid.split_once(':')) {
        if !namespace.is_empty() && !name.is_empty() {
            return Some(marker_named(namespace, name));
        }
    }

    let name = data
        .get("name")
        .filter(|value| json_value_truthy(value))
        .map(json_value_to_string)
        .or_else(|| {
            declared
                .filter(|bndlid| !bndlid.contains(':'))
                .map(str::to_string)
        })?;

    Some(marker_named(BUNDLE_DEFAULT_NAMESPACE, &name))
}

fn marker_named(namespace: &str, name: &str) -> BundleIdentity {
    BundleIdentity {
        namespace: namespace.to_string(),
        name: name.to_string(),
        source: IdentitySource::Marker,
    }
}

fn layout_identity(bundle_root: &Path) -> BundleIdentity {
    BundleIdentity {
        namespace: infer_namespace(bundle_root),
        name: bundle_root
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default(),
        source: IdentitySource::Layout,
    }
}

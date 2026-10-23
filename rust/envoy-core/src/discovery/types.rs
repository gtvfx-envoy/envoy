use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{EnvoyError, Result};

use super::bndlid::{is_bndlid, parse_bndlid, resolve_bndlid};
use super::identity::bundle_identity;
use super::util::{json_value_to_string, json_value_truthy, resolve_input_path};

/// Information about a discovered bundle.
#[derive(Clone, Eq, PartialEq)]
pub struct BundleInfo {
    /// Bundle root directory.
    pub root: PathBuf,
    /// Bundle name.
    pub name: String,
    /// Bundle namespace.
    pub namespace: String,
    envoy_env: PathBuf,
    env_files: HashMap<String, PathBuf>,
}

impl BundleInfo {
    /// Create bundle metadata for a discovered bundle root.
    pub fn new(root: PathBuf, name: String, namespace: String) -> Self {
        let envoy_env = root.join(super::BUNDLE_ENV_DIR);
        let env_files = Self::index_env_files_for(&envoy_env);

        Self {
            root,
            name,
            namespace,
            envoy_env,
            env_files,
        }
    }

    /// Return the namespaced bundle identifier.
    pub fn bndlid(&self) -> String {
        format!("{}:{}", self.namespace, self.name)
    }

    /// Return the `.envoy/` directory for this bundle.
    pub fn envoy_env(&self) -> &Path {
        &self.envoy_env
    }

    /// Return all indexed `.json` files in `.envoy/`.
    pub fn env_files(&self) -> &HashMap<String, PathBuf> {
        &self.env_files
    }

    /// Re-scan `.envoy/` and index all `.json` files by filename.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn index_env_files(&self) -> HashMap<String, PathBuf> {
        Self::index_env_files_for(&self.envoy_env)
    }

    fn index_env_files_for(envoy_env: &Path) -> HashMap<String, PathBuf> {
        if !envoy_env.is_dir() {
            return HashMap::new();
        }

        let Ok(read_dir) = fs::read_dir(envoy_env) else {
            return HashMap::new();
        };

        let mut files = HashMap::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension() == Some(OsStr::new("json")) {
                if let Some(file_name) = path.file_name().and_then(OsStr::to_str) {
                    files.insert(file_name.to_string(), path);
                }
            }
        }

        files
    }
}

impl fmt::Debug for BundleInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "BundleInfo(bndlid='{}', root={})",
            self.bndlid(),
            self.root.display()
        )
    }
}

impl fmt::Display for BundleInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.name, self.root.display())
    }
}

/// A discovered envoy bundle.
#[derive(Clone, Eq, PartialEq)]
pub struct Bundle {
    info: BundleInfo,
}

impl Bundle {
    /// Construct a bundle from a bundle ID or filesystem path.
    ///
    /// A bundle ID keeps the ID it was resolved from. A path takes its
    /// identity from [`bundle_identity`](super::bundle_identity); `namespace`,
    /// when given, replaces the namespace of a path (it is ignored for a
    /// bundle ID, which already carries one).
    pub fn new(spec: impl AsRef<Path>, namespace: Option<&str>) -> Result<Self> {
        let spec_path = spec.as_ref();
        let raw_spec = spec_path.as_os_str().to_string_lossy().into_owned();

        let (root, namespace, name) = if is_bndlid(&raw_spec) {
            let (namespace, name) = parse_bndlid(&raw_spec)
                .expect("bundle-id regex must parse after is_bndlid() succeeds");

            (resolve_bndlid(&raw_spec)?, namespace, name)
        } else {
            let root = resolve_input_path(spec_path);
            if !root.is_dir() {
                return Err(EnvoyError::Validation(format!(
                    "Bundle path does not exist: {}",
                    root.display()
                )));
            }
            if !root.join(super::BUNDLE_ENV_DIR).is_dir() {
                return Err(EnvoyError::Validation(format!(
                    "Not a valid bundle (no {}/): {}",
                    super::BUNDLE_ENV_DIR,
                    root.display()
                )));
            }

            let identity = bundle_identity(&root);
            let namespace = namespace
                .map(ToOwned::to_owned)
                .unwrap_or(identity.namespace);

            (root, namespace, identity.name)
        };

        Ok(Self {
            info: BundleInfo::new(root, name, namespace),
        })
    }

    /// Construct a bundle directly from cached discovery info.
    pub(crate) fn from_info(info: BundleInfo) -> Self {
        Self { info }
    }

    /// Return the bundle name.
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Return the bundle namespace.
    pub fn namespace(&self) -> &str {
        &self.info.namespace
    }

    /// Return the namespaced bundle identifier.
    pub fn bndlid(&self) -> String {
        self.info.bndlid()
    }

    /// Return the bundle version.
    ///
    /// Production bundles read `version` from the `.bundle` marker file.
    /// Checkout bundles return [`BUNDLE_CHECKOUT`].
    pub fn version(&self) -> String {
        let marker = self.info.root.join(super::BUNDLE_MARKER_FILE);
        let Ok(text) = fs::read_to_string(marker) else {
            return String::from(super::BUNDLE_CHECKOUT);
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            return String::from(super::BUNDLE_CHECKOUT);
        };

        match data.get("version") {
            Some(value) if json_value_truthy(value) => json_value_to_string(value),
            _ => String::from(super::BUNDLE_CHECKOUT),
        }
    }

    /// Return `true` when the bundle is a published bundle directory.
    pub fn is_production(&self) -> bool {
        self.info.root.join(super::BUNDLE_MARKER_FILE).is_file()
    }

    /// Return `true` when the bundle is a checkout bundle.
    pub fn is_checkout(&self) -> bool {
        !self.is_production()
    }

    /// Return the bundle root directory.
    pub fn path(&self) -> &Path {
        &self.info.root
    }

    /// Return the bundle `.envoy/` directory.
    pub fn envoy_env(&self) -> &Path {
        self.info.envoy_env()
    }

    /// Return all indexed `.json` files in `.envoy/`.
    pub fn env_files(&self) -> HashMap<String, PathBuf> {
        self.info.env_files().clone()
    }

    /// Return the sorted command names defined in `commands.json`.
    ///
    /// Invalid or missing files return an empty list, matching Python.
    pub fn commands(&self) -> Vec<String> {
        let commands_file = self.info.envoy_env().join("commands.json");
        let Ok(text) = fs::read_to_string(commands_file) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        let Value::Object(object) = value else {
            return Vec::new();
        };

        let mut commands = object.keys().cloned().collect::<Vec<_>>();
        commands.sort();
        commands
    }
}

impl fmt::Debug for Bundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Bundle(bndlid='{}', path={})",
            self.bndlid(),
            self.path().display()
        )
    }
}

impl fmt::Display for Bundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

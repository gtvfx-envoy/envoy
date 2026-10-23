use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::discovery::identity::bundle_identity;
use crate::discovery::types::BundleInfo;

/// Return `true` if `path` contains a `.git/` directory.
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").is_dir()
}

/// Return `true` if `path` contains a `.bundle` marker file.
pub fn is_published_bundle(path: &Path) -> bool {
    path.join(super::BUNDLE_MARKER_FILE).is_file()
}

/// Return `true` if `path` contains a `.envoy/` directory.
pub fn has_envoy_env(path: &Path) -> bool {
    path.join(super::BUNDLE_ENV_DIR).is_dir()
}

/// Return `true` if `path` is a valid envoy bundle directory.
pub fn validate_bundle(path: &Path) -> bool {
    path.is_dir() && has_envoy_env(path)
}

/// Recursively find checkout or published bundle roots below `root_dir`.
pub fn find_bundle_roots(root_dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    if !root_dir.is_dir() {
        return Vec::new();
    }

    search_dir(root_dir, 0, max_depth, SearchMode::Bundles)
}

/// Recursively find git repositories below `root_dir`.
pub fn find_git_repos(root_dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    if !root_dir.is_dir() {
        return Vec::new();
    }

    search_dir(root_dir, 0, max_depth, SearchMode::GitRepos)
}

#[derive(Clone, Copy)]
enum SearchMode {
    Bundles,
    GitRepos,
}

fn search_dir(path: &Path, depth: usize, max_depth: usize, mode: SearchMode) -> Vec<PathBuf> {
    if depth > max_depth {
        return Vec::new();
    }

    match mode {
        SearchMode::Bundles if is_git_repo(path) || is_published_bundle(path) => {
            return vec![path.to_path_buf()];
        }
        SearchMode::GitRepos if is_git_repo(path) => {
            return vec![path.to_path_buf()];
        }
        SearchMode::Bundles | SearchMode::GitRepos => {}
    }

    let Ok(read_dir) = fs::read_dir(path) else {
        return Vec::new();
    };

    let mut child_dirs = read_dir
        .flatten()
        .filter_map(|entry| {
            let entry_path = entry.path();
            let is_dir = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if !is_dir {
                return None;
            }

            if entry_path
                .file_name()
                .map(|name| name.to_string_lossy().starts_with('.'))
                .unwrap_or(false)
            {
                return None;
            }

            Some(entry_path)
        })
        .collect::<Vec<_>>();
    child_dirs.sort();

    child_dirs
        .into_par_iter()
        .map(|entry_path| search_dir(&entry_path, depth + 1, max_depth, mode))
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

pub fn discover_bundles_for_root(root: &Path, max_depth: usize) -> Vec<BundleInfo> {
    find_bundle_roots(root, max_depth)
        .into_iter()
        .filter(|candidate_path| validate_bundle(candidate_path))
        .map(|candidate_path| {
            let identity = bundle_identity(&candidate_path);
            BundleInfo::new(candidate_path, identity.name, identity.namespace)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use serde_json::json;
    use serde_json::Value;
    use tempfile::tempdir;

    use crate::discovery::bndlid::{expand_bundle_path, is_bndlid, resolve_bndlid};
    use crate::discovery::cache::{
        discovery_cache_key, discovery_cache_lock_path, discovery_cache_path,
        load_discovery_cache_manifest, save_discovery_cache_manifest, DISCOVERY_CACHE_DISABLE_VAR,
    };
    use crate::discovery::discover_bundles_from_roots;
    use crate::discovery::files::{get_bundle_commands_files, get_bundle_env_files};
    use crate::discovery::identity::{bundle_identity, IdentitySource};
    use crate::discovery::scan::{
        discover_bundles_for_root, find_bundle_roots, find_git_repos, has_envoy_env, is_git_repo,
        is_published_bundle, validate_bundle,
    };
    use crate::discovery::types::{Bundle, BundleInfo};
    use crate::discovery::util::{current_timestamp, infer_namespace};
    use crate::discovery::{BUNDLE_CHECKOUT, BUNDLE_ENV_DIR, BUNDLE_MARKER_FILE, BUNDLE_ROOTS_VAR};
    use crate::error::EnvoyError;
    use crate::path_test::assert_same_path;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&OsStr>) -> Self {
            let previous = env::var_os(key);

            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }

            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    /// Locks the crate-wide `crate::env_test_lock::MUTEX` rather than a
    /// module-local mutex: several modules' tests mutate the same real
    /// process environment variables (e.g. `ENVOY_STACK_ROOTS` is touched by
    /// both `discovery` and `stack_registry`), so a single shared lock is
    /// required to prevent cross-module test races under `cargo test`'s
    /// default parallel execution.
    fn with_env_lock<T>(test_fn: impl FnOnce() -> T) -> T {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        test_fn()
    }

    fn set_discovery_cache_root(cache_root: &Path) -> EnvVarGuard {
        #[cfg(target_os = "windows")]
        {
            EnvVarGuard::set("LOCALAPPDATA", Some(cache_root.as_os_str()))
        }

        #[cfg(not(target_os = "windows"))]
        {
            EnvVarGuard::set("XDG_CACHE_HOME", Some(cache_root.as_os_str()))
        }
    }

    fn join_roots(roots: &[&Path]) -> OsString {
        env::join_paths(roots).expect("failed to join bundle roots")
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(
            path,
            serde_json::to_string_pretty(value).expect("failed to serialize test json"),
        )
        .expect("failed to write test json");
    }

    fn create_checkout_bundle(
        root: &Path,
        namespace: &str,
        name: &str,
        env_files: &[&str],
        commands: Option<Value>,
    ) -> PathBuf {
        let bundle_root = root.join(namespace).join(name);
        fs::create_dir_all(bundle_root.join(".git")).expect("failed to create .git");
        let envoy_env = bundle_root.join(BUNDLE_ENV_DIR);
        fs::create_dir_all(&envoy_env).expect("failed to create .envoy");

        for env_file in env_files {
            write_json(&envoy_env.join(env_file), &json!({"name": env_file}));
        }

        if let Some(commands) = commands {
            write_json(&envoy_env.join("commands.json"), &commands);
        }

        bundle_root
    }

    fn create_published_bundle(
        root: &Path,
        dir_name: &str,
        marker: Value,
        env_files: &[&str],
        commands: Option<Value>,
    ) -> PathBuf {
        let bundle_root = root.join(dir_name);
        let envoy_env = bundle_root.join(BUNDLE_ENV_DIR);
        fs::create_dir_all(&envoy_env).expect("failed to create .envoy");
        write_json(&bundle_root.join(BUNDLE_MARKER_FILE), &marker);

        for env_file in env_files {
            write_json(&envoy_env.join(env_file), &json!({"name": env_file}));
        }

        if let Some(commands) = commands {
            write_json(&envoy_env.join("commands.json"), &commands);
        }

        bundle_root
    }

    fn namespaced_map(bundles: &[BundleInfo]) -> HashMap<String, PathBuf> {
        bundles
            .iter()
            .map(|bundle| (bundle.bndlid(), bundle.root.clone()))
            .collect()
    }

    fn cache_entry_created_at(root_dirs: &[String], max_depth: usize) -> Option<u64> {
        let roots = root_dirs
            .iter()
            .map(|root| crate::discovery::util::resolve_input_path(Path::new(root)))
            .collect::<Vec<_>>();

        let cache_key = discovery_cache_key(&roots, max_depth);
        load_discovery_cache_manifest()
            .entries
            .get(&cache_key)
            .map(|entry| entry.created_at)
    }

    /// Directly overwrite a cache entry's recorded `created_at`, bypassing
    /// real time entirely. Used to make cache-freshness/-staleness tests
    /// deterministic instead of relying on `thread::sleep` against a real
    /// wall clock, which is flaky under heavy parallel test load: sleeps
    /// only guarantee a *minimum* duration and can overshoot arbitrarily
    /// when the OS scheduler is contended, occasionally pushing a "should
    /// still be fresh" entry past the TTL and causing a spurious re-scan.
    fn set_cache_entry_created_at(root_dirs: &[String], max_depth: usize, created_at: u64) {
        let roots = root_dirs
            .iter()
            .map(|root| crate::discovery::util::resolve_input_path(Path::new(root)))
            .collect::<Vec<_>>();

        let cache_key = discovery_cache_key(&roots, max_depth);
        let mut manifest = load_discovery_cache_manifest();
        if let Some(entry) = manifest.entries.get_mut(&cache_key) {
            entry.created_at = created_at;
        }
        save_discovery_cache_manifest(&manifest);
    }

    #[test]
    fn is_bndlid_matches_expected_examples() {
        assert!(is_bndlid("gt:pythoncore"));
        assert!(is_bndlid("tools_team:bundle-name"));
        assert!(!is_bndlid("g:pythoncore"));
        assert!(!is_bndlid("C:\\repo\\bundle"));
        assert!(!is_bndlid("1gt:pythoncore"));
        assert!(!is_bndlid("gt:"));
    }

    #[test]
    fn infer_namespace_uses_parent_directory_or_default() {
        let bundle_root = Path::new("C:\\repo\\gt\\pythoncore");
        let fallback_root = Path::new("C:\\repo\\some-dir\\pythoncore");

        assert_eq!(infer_namespace(bundle_root), "gt");
        assert_eq!(infer_namespace(fallback_root), "gt");
    }

    #[test]
    fn expand_bundle_path_expands_defined_vars_and_rejects_undefined_vars() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let _root_guard = EnvVarGuard::set("TEST_BUNDLE_ROOT", Some(temp.path().as_os_str()));
            let _missing_guard = EnvVarGuard::set("TEST_MISSING_ROOT", None);

            let config_file = temp.path().join("bundles.json");
            assert_eq!(
                expand_bundle_path("${TEST_BUNDLE_ROOT}\\bundle", &config_file),
                Some(format!("{}\\bundle", temp.path().display()))
            );
            assert_eq!(
                expand_bundle_path("${TEST_MISSING_ROOT}\\bundle", &config_file),
                None
            );
        });
    }

    #[test]
    fn predicate_helpers_identify_checkout_and_published_bundles() {
        let temp = tempdir().expect("failed to create temp dir");
        let checkout = create_checkout_bundle(
            temp.path(),
            "gt",
            "pythoncore",
            &["python_env.json"],
            Some(json!({"python": {}})),
        );
        let published = create_published_bundle(
            &temp.path().join("releases"),
            "v1.2.3",
            json!({"bndlid": "tools:render", "version": "1.2.3"}),
            &["render_env.json"],
            None,
        );

        assert!(is_git_repo(&checkout));
        assert!(!is_published_bundle(&checkout));
        assert!(has_envoy_env(&checkout));
        assert!(validate_bundle(&checkout));

        assert!(!is_git_repo(&published));
        assert!(is_published_bundle(&published));
        assert!(has_envoy_env(&published));
        assert!(validate_bundle(&published));
    }

    #[test]
    fn find_bundle_roots_honors_depth_limits_and_skips_hidden_directories() {
        let temp = tempdir().expect("failed to create temp dir");
        let root = temp.path();

        let shallow_checkout = create_checkout_bundle(
            root,
            "gt",
            "pythoncore",
            &["python_env.json"],
            Some(json!({"python": {}})),
        );
        let published = create_published_bundle(
            &root.join("releases"),
            "v1.2.3",
            json!({"bndlid": "tools:render", "version": "1.2.3"}),
            &["render_env.json"],
            None,
        );
        let deep_checkout = create_checkout_bundle(
            &root.join("one").join("two"),
            "gt",
            "too_deep",
            &["too_deep_env.json"],
            None,
        );
        let hidden_checkout = create_checkout_bundle(
            &root.join(".hidden"),
            "gt",
            "skipped",
            &["skipped_env.json"],
            None,
        );

        let roots = find_bundle_roots(root, 3)
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(roots.contains(&shallow_checkout));
        assert!(roots.contains(&published));
        assert!(!roots.contains(&hidden_checkout));

        let limited_roots = find_bundle_roots(root, 2)
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(limited_roots.contains(&shallow_checkout));
        assert!(limited_roots.contains(&published));
        assert!(!limited_roots.contains(&deep_checkout));
    }

    #[test]
    fn find_git_repos_only_returns_checkout_bundles() {
        let temp = tempdir().expect("failed to create temp dir");
        let root = temp.path();

        let checkout = create_checkout_bundle(root, "gt", "pythoncore", &["python_env.json"], None);
        create_published_bundle(
            &root.join("releases"),
            "v1.2.3",
            json!({"bndlid": "tools:render", "version": "1.2.3"}),
            &["render_env.json"],
            None,
        );

        let repos = find_git_repos(root, 5);
        assert_eq!(repos, vec![checkout]);
    }

    #[test]
    fn discover_bundles_from_roots_uses_marker_bndlid_for_published_bundles() {
        // Disable the discovery cache and serialize via `with_env_lock`: this
        // test only cares about bndlid inference, not caching, but
        // `discover_bundles_from_roots` always consults the shared on-disk
        // discovery cache keyed off the platform cache root. Without this
        // guard, this test can run concurrently with the cache-focused tests
        // below while they have that root pointed at an isolated tempdir,
        // racing on the same cache file and corrupting or losing entries.
        with_env_lock(|| {
            let _disable_guard =
                EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, Some(OsStr::new("1")));

            let temp = tempdir().expect("failed to create temp dir");
            let root = temp.path();

            let checkout =
                create_checkout_bundle(root, "gt", "pythoncore", &["python_env.json"], None);
            let published = create_published_bundle(
                &root.join("releases"),
                "v1.2.3",
                json!({"bndlid": "tools:render", "version": "1.2.3"}),
                &["render_env.json"],
                None,
            );

            let bundles = discover_bundles_from_roots(&[root.display().to_string()]);
            let discovered = namespaced_map(&bundles);

            assert_same_path(
                discovered
                    .get("gt:pythoncore")
                    .expect("checkout bundle should be discovered"),
                &checkout,
            );
            assert_same_path(
                discovered
                    .get("tools:render")
                    .expect("published bundle should be discovered"),
                &published,
            );
        });
    }

    #[test]
    fn discover_bundles_from_roots_reuses_fresh_cache_entries() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let cache_temp = tempdir().expect("failed to create cache temp dir");
            let cache_root = cache_temp.path().join("cache-root");
            fs::create_dir_all(&cache_root).expect("failed to create cache root");
            let _cache_root_guard = set_discovery_cache_root(&cache_root);
            let _disable_guard = EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, None);

            create_checkout_bundle(temp.path(), "gt", "pythoncore", &["python_env.json"], None);
            let root_dirs = vec![temp.path().display().to_string()];

            let first = discover_bundles_from_roots(&root_dirs);
            assert_eq!(first.len(), 1);
            assert!(discovery_cache_path().is_file());

            // Backdate the entry by 1 second (well inside the 5-second TTL)
            // deterministically instead of sleeping in real time -- see
            // `set_cache_entry_created_at`'s doc comment for why a real
            // sleep here would be flaky under parallel test load.
            let backdated = current_timestamp().saturating_sub(1);
            set_cache_entry_created_at(&root_dirs, 5, backdated);

            let second = discover_bundles_from_roots(&root_dirs);
            let second_created_at = cache_entry_created_at(&root_dirs, 5)
                .expect("cache entry should remain after cache hit");

            assert_eq!(namespaced_map(&first), namespaced_map(&second));
            assert_eq!(second_created_at, backdated);
        });
    }

    #[test]
    fn discover_bundles_from_roots_creates_cache_lock_file() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let cache_temp = tempdir().expect("failed to create cache temp dir");
            let cache_root = cache_temp.path().join("cache-root");
            fs::create_dir_all(&cache_root).expect("failed to create cache root");
            let _cache_root_guard = set_discovery_cache_root(&cache_root);
            let _disable_guard = EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, None);

            create_checkout_bundle(temp.path(), "gt", "pythoncore", &["python_env.json"], None);
            let root_dirs = vec![temp.path().display().to_string()];

            let bundles = discover_bundles_from_roots(&root_dirs);
            assert_eq!(bundles.len(), 1);
            assert!(discovery_cache_path().is_file());
            assert!(discovery_cache_lock_path().is_file());
        });
    }

    #[test]
    fn discover_bundles_from_roots_preserves_concurrent_cache_entries() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let cache_temp = tempdir().expect("failed to create cache temp dir");
            let cache_root = cache_temp.path().join("cache-root");
            fs::create_dir_all(&cache_root).expect("failed to create cache root");
            let _cache_root_guard = set_discovery_cache_root(&cache_root);
            let _disable_guard = EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, None);

            let first_root = temp.path().join("first-root");
            let second_root = temp.path().join("second-root");
            create_checkout_bundle(&first_root, "gt", "pythoncore", &["python_env.json"], None);
            create_checkout_bundle(&second_root, "tools", "render", &["render_env.json"], None);

            let barrier = Arc::new(Barrier::new(2));
            let first_root_dirs = vec![first_root.display().to_string()];
            let second_root_dirs = vec![second_root.display().to_string()];

            let first_barrier = Arc::clone(&barrier);
            let first_handle = thread::spawn(move || {
                first_barrier.wait();
                discover_bundles_from_roots(&first_root_dirs)
            });

            let second_barrier = Arc::clone(&barrier);
            let second_handle = thread::spawn(move || {
                second_barrier.wait();
                discover_bundles_from_roots(&second_root_dirs)
            });

            let first = first_handle
                .join()
                .expect("first discovery thread should succeed");
            let second = second_handle
                .join()
                .expect("second discovery thread should succeed");

            assert_eq!(first.len(), 1);
            assert_eq!(second.len(), 1);

            let manifest = load_discovery_cache_manifest();
            let first_key = discovery_cache_key(
                &[crate::discovery::util::resolve_input_path(&first_root)],
                5,
            );
            let second_key = discovery_cache_key(
                &[crate::discovery::util::resolve_input_path(&second_root)],
                5,
            );

            assert!(manifest.entries.contains_key(&first_key));
            assert!(manifest.entries.contains_key(&second_key));
        });
    }

    #[test]
    fn discover_bundles_from_roots_invalidates_cache_when_bundle_state_changes() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let cache_temp = tempdir().expect("failed to create cache temp dir");
            let cache_root = cache_temp.path().join("cache-root");
            fs::create_dir_all(&cache_root).expect("failed to create cache root");
            let _cache_root_guard = set_discovery_cache_root(&cache_root);
            let _disable_guard = EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, None);

            let checkout =
                create_checkout_bundle(temp.path(), "gt", "pythoncore", &["python_env.json"], None);
            let root_dirs = vec![temp.path().display().to_string()];

            let first = discover_bundles_from_roots(&root_dirs);
            assert_eq!(first.len(), 1);
            let first_created_at = cache_entry_created_at(&root_dirs, 5)
                .expect("cache entry should exist after first discovery");

            thread::sleep(Duration::from_secs(1));
            fs::remove_dir_all(checkout.join(".git")).expect("failed to remove .git");

            let second = discover_bundles_from_roots(&root_dirs);
            let second_created_at = cache_entry_created_at(&root_dirs, 5)
                .expect("cache entry should be refreshed after invalidation");

            assert!(second.is_empty());
            assert!(second_created_at > first_created_at);
        });
    }

    #[test]
    fn resolve_bndlid_returns_environment_build_errors_for_invalid_inputs() {
        with_env_lock(|| {
            let _roots_guard = EnvVarGuard::set(BUNDLE_ROOTS_VAR, None);

            let error = resolve_bndlid("bad").expect_err("invalid bndlid should fail");
            assert!(matches!(error, EnvoyError::EnvironmentBuild(_)));

            let error = resolve_bndlid("gt:pythoncore")
                .expect_err("missing roots env should fail resolution");
            assert!(matches!(error, EnvoyError::EnvironmentBuild(_)));
        });
    }

    #[test]
    fn resolve_bndlid_falls_back_to_scan_for_published_bundle() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let published = create_published_bundle(
                &temp.path().join("releases"),
                "v1.2.3",
                json!({"bndlid": "tools:render", "version": "1.2.3"}),
                &["render_env.json"],
                None,
            );
            let roots = join_roots(&[temp.path()]);
            let _roots_guard = EnvVarGuard::set(BUNDLE_ROOTS_VAR, Some(roots.as_os_str()));

            let resolved = resolve_bndlid("tools:render").expect("published bundle should resolve");
            assert_same_path(&resolved, &published);
        });
    }

    #[test]
    fn bundle_supports_path_specs_bndlid_specs_and_namespace_overrides() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let checkout = create_checkout_bundle(
                temp.path(),
                "gt",
                "pythoncore",
                &["python_env.json", "maya_env.json"],
                Some(json!({"z_cmd": {}, "a_cmd": {}})),
            );
            let roots = join_roots(&[temp.path()]);
            let _roots_guard = EnvVarGuard::set(BUNDLE_ROOTS_VAR, Some(roots.as_os_str()));

            let by_path = Bundle::new(&checkout, None).expect("bundle path should be valid");
            assert_eq!(by_path.name(), "pythoncore");
            assert_eq!(by_path.namespace(), "gt");
            assert_eq!(by_path.bndlid(), "gt:pythoncore");
            assert_eq!(by_path.version(), BUNDLE_CHECKOUT);
            assert!(by_path.is_checkout());
            assert_eq!(by_path.commands(), vec!["a_cmd", "z_cmd"]);
            assert!(by_path.env_files().contains_key("commands.json"));

            let by_bndlid =
                Bundle::new("gt:pythoncore", Some("ignored")).expect("bundle ID should resolve");
            assert_same_path(by_bndlid.path(), &checkout);

            let overridden = Bundle::new(&checkout, Some("tools"))
                .expect("bundle path with namespace override should be valid");
            assert_eq!(overridden.bndlid(), "tools:pythoncore");
        });
    }

    #[test]
    fn bundle_reads_marker_version_and_production_state() {
        let temp = tempdir().expect("failed to create temp dir");
        let published = create_published_bundle(
            &temp.path().join("releases"),
            "v1.2.3",
            json!({"bndlid": "tools:render", "version": "1.2.3"}),
            &["render_env.json"],
            Some(json!({"render": {}})),
        );

        let bundle = Bundle::new(&published, None).expect("published bundle path should be valid");
        assert_eq!(bundle.version(), "1.2.3");
        assert!(bundle.is_production());
        assert!(!bundle.is_checkout());
    }

    #[test]
    fn bundle_identity_reads_the_marker_not_the_version_directory() {
        let temp = tempdir().expect("failed to create temp dir");
        let published = create_published_bundle(
            &temp.path().join("ext").join("python"),
            "3.11.9-envoy.2",
            json!({"bndlid": "ext:python", "name": "python", "version": "3.11.9-envoy.2"}),
            &["python_env.json"],
            None,
        );

        let identity = bundle_identity(&published);
        assert_eq!(identity.bndlid(), "ext:python");
        assert_eq!(identity.source, IdentitySource::Marker);
    }

    #[test]
    fn bundle_identity_gives_a_namespace_less_marker_the_default_namespace() {
        let temp = tempdir().expect("failed to create temp dir");
        let markers = [
            (
                "both",
                json!({"bndlid": "globals", "name": "globals", "version": "0.3.0"}),
            ),
            ("only_name", json!({"name": "globals", "version": "0.3.0"})),
            (
                "only_bare_bndlid",
                json!({"bndlid": "globals", "version": "0.3.0"}),
            ),
        ];

        for (label, marker) in markers {
            let published = create_published_bundle(
                &temp.path().join(label).join("globals"),
                "0.3.0",
                marker,
                &[],
                None,
            );

            let identity = bundle_identity(&published);
            assert_eq!(identity.bndlid(), "gt:globals", "{label}");
            assert_eq!(identity.source, IdentitySource::Marker, "{label}");
        }
    }

    #[test]
    fn bundle_identity_falls_back_to_the_layout_without_a_naming_marker() {
        let temp = tempdir().expect("failed to create temp dir");
        let checkout = create_checkout_bundle(temp.path(), "tools", "render", &[], None);
        let unnamed = create_published_bundle(
            &temp.path().join("tools"),
            "v2",
            json!({"version": "2.0.0"}),
            &[],
            None,
        );
        let unreadable = temp.path().join("tools").join("broken");
        fs::create_dir_all(&unreadable).expect("failed to create bundle directory");
        fs::write(unreadable.join(BUNDLE_MARKER_FILE), "not json").expect("failed to write marker");

        for (root, expected) in [
            (&checkout, "tools:render"),
            (&unnamed, "tools:v2"),
            (&unreadable, "tools:broken"),
        ] {
            let identity = bundle_identity(root);
            assert_eq!(identity.bndlid(), expected);
            assert_eq!(identity.source, IdentitySource::Layout);
        }
    }

    #[test]
    fn scanning_and_path_construction_agree_on_a_published_identity() {
        let temp = tempdir().expect("failed to create temp dir");
        let published = create_published_bundle(
            &temp.path().join("releases").join("render"),
            "v1.2.3",
            json!({"bndlid": "tools:render", "version": "1.2.3"}),
            &["render_env.json"],
            None,
        );

        let scanned = discover_bundles_for_root(temp.path(), 5);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].bndlid(), "tools:render");

        let by_path = Bundle::new(&published, None).expect("published bundle path should be valid");
        assert_eq!(by_path.bndlid(), "tools:render");
        assert_eq!(by_path.name(), "render");
    }

    #[test]
    fn bundle_new_from_a_bndlid_keeps_that_id_for_a_published_bundle() {
        with_env_lock(|| {
            let temp = tempdir().expect("failed to create temp dir");
            let published = create_published_bundle(
                &temp.path().join("releases"),
                "v1.2.3",
                json!({"bndlid": "tools:render", "version": "1.2.3"}),
                &["render_env.json"],
                None,
            );
            let roots = join_roots(&[temp.path()]);
            let _roots_guard = EnvVarGuard::set(BUNDLE_ROOTS_VAR, Some(roots.as_os_str()));
            let _cache_guard = EnvVarGuard::set(DISCOVERY_CACHE_DISABLE_VAR, Some(OsStr::new("1")));

            let bundle = Bundle::new("tools:render", None).expect("bundle ID should resolve");
            assert_eq!(bundle.bndlid(), "tools:render");
            assert_eq!(bundle.name(), "render");
            assert_same_path(bundle.path(), &published);
        });
    }

    #[test]
    fn get_bundle_file_helpers_collect_expected_files() {
        let temp = tempdir().expect("failed to create temp dir");
        let bundle_root = create_checkout_bundle(
            temp.path(),
            "gt",
            "pythoncore",
            &["python_env.json", "maya_env.json"],
            Some(json!({"python": {}})),
        );

        let info = BundleInfo::new(
            bundle_root.clone(),
            String::from("pythoncore"),
            String::from("gt"),
        );
        let env_files = get_bundle_env_files(std::slice::from_ref(&info));
        let commands_files = get_bundle_commands_files(std::slice::from_ref(&info));

        let expected_env_files = vec![
            bundle_root.join(BUNDLE_ENV_DIR).join("maya_env.json"),
            bundle_root.join(BUNDLE_ENV_DIR).join("python_env.json"),
        ]
        .into_iter()
        .collect::<HashSet<_>>();

        assert_eq!(
            env_files
                .get("pythoncore")
                .expect("bundle env files should be present")
                .iter()
                .cloned()
                .collect::<HashSet<_>>(),
            expected_env_files
        );
        assert_eq!(
            commands_files.get("pythoncore"),
            Some(&bundle_root.join(BUNDLE_ENV_DIR).join("commands.json"))
        );
    }

    #[test]
    fn bundle_info_display_and_debug_match_python_style() {
        let temp = tempdir().expect("failed to create temp dir");
        let bundle_root = create_checkout_bundle(temp.path(), "gt", "pythoncore", &[], None);
        let info = BundleInfo::new(
            bundle_root.clone(),
            String::from("pythoncore"),
            String::from("gt"),
        );

        assert_eq!(
            format!("{info}"),
            format!("pythoncore ({})", bundle_root.display())
        );
        assert_eq!(
            format!("{info:?}"),
            format!(
                "BundleInfo(bndlid='gt:pythoncore', root={})",
                bundle_root.display()
            )
        );
        assert_eq!(info.index_env_files(), info.env_files().clone());
    }
}

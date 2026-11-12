use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;

use envoy_core::discovery::{discover_bundles_auto, Bundle};
use envoy_core::stack::Stack;
use tempfile::tempdir;

struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &OsStr) -> Self {
        let previous = env::var_os(name);
        env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => env::set_var(self.name, value),
            None => env::remove_var(self.name),
        }
    }
}

#[test]
fn supports_envoy_utils_discovery_and_stack_contract() {
    let temp = tempdir().expect("failed to create temp dir");
    let bundle_root = temp.path().join("bundles");
    let bundle_path = bundle_root.join("gt").join("contract_fixture");
    fs::create_dir_all(bundle_path.join(".envoy")).expect("failed to create .envoy");
    fs::create_dir_all(bundle_path.join(".git")).expect("failed to create .git");

    let roots = env::join_paths([bundle_root]).expect("failed to join bundle roots");
    let _roots_guard = EnvVarGuard::set("ENVOY_BNDL_ROOTS", roots.as_os_str());

    let discovered = discover_bundles_auto().expect("bundle discovery should succeed");
    assert!(discovered
        .iter()
        .any(|bundle| bundle.bndlid() == "gt:contract_fixture"));

    let bundle = Bundle::new("gt:contract_fixture", None)
        .expect("bundle ID should resolve through ENVOY_BNDL_ROOTS");
    assert_eq!(bundle.bndlid(), "gt:contract_fixture");

    let source_stack = temp.path().join("contract.estack");
    let stack_contents = format!("bundles:\n  - path: '{}'\n", bundle_path.display());
    fs::write(&source_stack, stack_contents).expect("failed to write stack");

    let stack = Stack::new(&source_stack).expect("stack should load");
    assert_eq!(stack.name(), "contract");
}

/// Names, by the path it uses, every item Envoy Utils calls, so that a change that makes one of
/// them private or removes it fails to compile here and not in Envoy Utils' build. The list is
/// `DIRECT_UTILS_APIS` in `scripts/release_automation.py`; a script test keeps the two together.
#[test]
fn names_every_item_envoy_utils_calls() {
    // Passing an item is enough to require that it can be named from outside this crate.
    fn uses<T>(_: T) {}

    uses(envoy_core::bundle_cache::resolve_bundle_cache_dir);
    uses(None::<envoy_core::bundle_cache::BundleCache>);
    uses(None::<envoy_core::bundle_cache::BundleMeta>);
    uses(envoy_core::discovery::discover_bundles_auto);
    uses(None::<envoy_core::discovery::Bundle>);
    uses(envoy_core::discovery::BUNDLE_DEFAULT_NAMESPACE);
    uses(None::<envoy_core::stack::Stack>);
    uses(envoy_core::stack_registry::STACK_ROOTS_VAR);
    uses(envoy_core::stack_registry::is_stack_name);
    uses(None::<envoy_core::config::ConfigError>);
    uses(None::<envoy_core::config::redirect::Redirect>);
    uses(None::<envoy_core::config::redirect::Problem>);
    uses(None::<envoy_core::config::redirect::Severity>);
    uses(envoy_core::config::redirect::normalize_user);
    uses(envoy_core::config::redirect::is_safe_version);
    uses(envoy_core::config::expand::expand_source);
    uses(envoy_core::config::locate::CONFIG_ROOT_VAR);
    uses(envoy_core::config::locate::REDIRECT_FILE_NAME);
    uses(envoy_core::config::locate::is_safe_team);
    uses(None::<envoy_core::config::layout::Target>);
    uses(envoy_core::config::layout::INSTALLERS);
    uses(envoy_core::config::layout::SHIM_DIR);
    uses(envoy_core::config::layout::POINTER_FILE);
    uses(envoy_core::config::layout::POINTER_SCHEMA);
    uses(envoy_core::config::layout::ARCHIVE_PREFIX);
    uses(envoy_core::config::layout::VERSION_REPORT_PREFIX);
    uses(envoy_core::config::layout::VERSION_FLAG);
    uses(envoy_core::config::layout::app_executable);
    uses(envoy_core::config::layout::launcher_file);
}

//! Where the redirect file may be found.

use std::path::PathBuf;

/// Directory on the share that holds the team's configuration files.
pub const CONFIG_ROOT_VAR: &str = "ENVOY_CONFIG_PROD";

/// Names a team whose redirect file lives in its own subdirectory.
pub const TEAM_VAR: &str = "ENVOY_TEAM";

/// Path of the redirect file itself, when it is not in the usual place.
pub const REDIRECT_FILE_VAR: &str = "ENVOY_REDIRECT_CONFIG";

/// File name of the redirect file.
pub const REDIRECT_FILE_NAME: &str = "redirect.yaml";

/// Places to look for the redirect file, best first.
///
/// An explicit [`REDIRECT_FILE_VAR`] is the only candidate, so a test or an
/// override never falls through to the studio's file. Otherwise the file is
/// looked for in the team's directory under [`CONFIG_ROOT_VAR`], then in the
/// root itself. With neither variable set there is nowhere to look.
pub fn redirect_candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let set = |name: &str| {
        env(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    if let Some(explicit) = set(REDIRECT_FILE_VAR) {
        return vec![PathBuf::from(explicit)];
    }
    let Some(root) = set(CONFIG_ROOT_VAR) else {
        return Vec::new();
    };
    let root = PathBuf::from(root);

    let mut places = Vec::new();
    if let Some(team) = set(TEAM_VAR).filter(|team| is_safe_name(team)) {
        places.push(root.join(team).join(REDIRECT_FILE_NAME));
    }
    places.push(root.join(REDIRECT_FILE_NAME));
    places
}

/// A team name becomes a directory name, so it must not be able to leave the
/// config root.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.chars().all(|c| c == '.')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn an_explicit_file_is_the_only_candidate() {
        let lookup = env(&[
            ("ENVOY_REDIRECT_CONFIG", "C:/test/redirect.yaml"),
            ("ENVOY_CONFIG_PROD", "//server/config"),
        ]);

        assert_eq!(
            redirect_candidates(&lookup),
            [PathBuf::from("C:/test/redirect.yaml")]
        );
    }

    #[test]
    fn a_team_directory_is_tried_before_the_root() {
        let lookup = env(&[
            ("ENVOY_CONFIG_PROD", "//server/config"),
            ("ENVOY_TEAM", "rigging"),
        ]);

        assert_eq!(
            redirect_candidates(&lookup),
            [
                PathBuf::from("//server/config/rigging/redirect.yaml"),
                PathBuf::from("//server/config/redirect.yaml"),
            ]
        );
    }

    #[test]
    fn without_a_team_only_the_root_is_tried() {
        let lookup = env(&[("ENVOY_CONFIG_PROD", "//server/config")]);

        assert_eq!(
            redirect_candidates(&lookup),
            [PathBuf::from("//server/config/redirect.yaml")]
        );
    }

    #[test]
    fn a_team_name_cannot_leave_the_config_root() {
        for team in ["..", "../other", "a/b", "a\\b", ""] {
            let lookup = env(&[("ENVOY_CONFIG_PROD", "/cfg"), ("ENVOY_TEAM", team)]);

            assert_eq!(
                redirect_candidates(&lookup),
                [PathBuf::from("/cfg/redirect.yaml")],
                "{team:?}"
            );
        }
    }

    #[test]
    fn with_nothing_set_there_is_nowhere_to_look() {
        assert!(redirect_candidates(&env(&[])).is_empty());
        assert!(redirect_candidates(&env(&[("ENVOY_CONFIG_PROD", "  ")])).is_empty());
    }
}

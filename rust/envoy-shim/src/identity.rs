//! Who is running envoy.

use envoy_config::normalize_user;

/// Overrides the login name, for testing a user's assignment or for a service
/// account that runs on someone's behalf.
pub const USER_OVERRIDE_VAR: &str = "ENVOY_USER";

/// The login name that decides which version runs, reduced to the form the
/// redirect file uses. Empty when nobody can be identified, which gets the
/// default version.
pub fn current_user(env: &dyn Fn(&str) -> Option<String>) -> String {
    let login = [USER_OVERRIDE_VAR, "USERNAME", "USER", "LOGNAME"]
        .iter()
        .find_map(|name| env(name).filter(|value| !value.trim().is_empty()));

    login
        .map(|login| normalize_user(&login))
        .unwrap_or_default()
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
    fn the_override_wins_and_every_login_is_normalized() {
        assert_eq!(current_user(&env(&[("USERNAME", "CORP\\Alice")])), "alice");
        assert_eq!(current_user(&env(&[("USER", "Bob")])), "bob");
        assert_eq!(current_user(&env(&[("LOGNAME", "carol")])), "carol");
        assert_eq!(
            current_user(&env(&[("ENVOY_USER", "Dave"), ("USERNAME", "alice")])),
            "dave"
        );
    }

    #[test]
    fn nobody_identifiable_gets_an_empty_name() {
        assert_eq!(current_user(&env(&[])), "");
        assert_eq!(current_user(&env(&[("USERNAME", "  ")])), "");
    }
}

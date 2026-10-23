//! Turning a source template into a path.

use std::path::PathBuf;

use crate::error::{ConfigError, Result};

/// Expand `${VAR}` from `env` and `{version}` from `version`.
///
/// A variable that is not set is an error rather than an empty string, since a
/// silently shortened path could point somewhere else entirely. Anything else,
/// including a lone `$` or a `{` that is not `{version}`, is kept as written.
/// Values are inserted as they are and never expanded again.
pub fn expand_source(
    template: &str,
    version: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf> {
    let fail = |reason: String| ConfigError::Expand {
        template: template.to_string(),
        reason,
    };

    let mut expanded = String::with_capacity(template.len() + version.len());
    let mut rest = template;
    while let Some(index) = rest.find(['$', '{']) {
        expanded.push_str(&rest[..index]);
        let tail = &rest[index..];

        if let Some(after) = tail.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err(fail("a '${' is never closed".to_string()));
            };
            let name = &after[..end];
            if !is_variable_name(name) {
                return Err(fail(format!("'{name}' is not a variable name")));
            }
            match env(name) {
                Some(value) => expanded.push_str(&value),
                None => return Err(fail(format!("{name} is not set"))),
            }
            rest = &after[end + 1..];
        } else if let Some(after) = tail.strip_prefix("{version}") {
            expanded.push_str(version);
            rest = after;
        } else {
            // A lone `$` or `{`: both are one byte, so this slice is safe.
            expanded.push_str(&tail[..1]);
            rest = &tail[1..];
        }
    }
    expanded.push_str(rest);

    // The file may be written with forward slashes anywhere, but Windows shows
    // and compares paths with backslashes.
    #[cfg(windows)]
    let expanded = expanded.replace('/', "\\");

    if expanded.trim().is_empty() {
        return Err(fail("it expands to nothing".to_string()));
    }
    Ok(PathBuf::from(expanded))
}

fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    fn variables_and_the_version_are_filled_in() {
        let lookup = env(&[("STUDIO", "//server/bndl")]);

        let path = expand_source("${STUDIO}/envoy/{version}", "v0.7.0", &lookup)
            .expect("the template expands");

        assert_eq!(path, PathBuf::from("//server/bndl/envoy/v0.7.0"));
    }

    #[test]
    fn the_version_may_appear_more_than_once() {
        let path = expand_source("/s/{version}/envoy-{version}", "1.2", &env(&[]))
            .expect("the template expands");

        assert_eq!(path, PathBuf::from("/s/1.2/envoy-1.2"));
    }

    #[test]
    fn a_variable_that_is_not_set_is_an_error_and_names_itself() {
        let error = expand_source("${MISSING}/x/{version}", "1", &env(&[]))
            .expect_err("an unset variable is refused");

        assert!(error.to_string().contains("MISSING is not set"), "{error}");
    }

    #[test]
    fn malformed_templates_are_refused() {
        for template in ["${UNCLOSED/x", "${bad name}/x", "${}/x", "${1ST}/x", ""] {
            assert!(
                expand_source(template, "1", &env(&[("UNCLOSED", "u")])).is_err(),
                "{template:?} should be refused"
            );
        }
    }

    #[test]
    fn other_dollars_and_braces_are_left_alone() {
        let path = expand_source("C:\\$Recycle.Bin\\{other}\\{version}", "9", &env(&[]))
            .expect("the template expands");

        assert_eq!(path, PathBuf::from("C:\\$Recycle.Bin\\{other}\\9"));
    }

    #[test]
    fn a_value_is_inserted_as_it_is_and_never_expanded_again() {
        let lookup = env(&[("TRICKY", "${OTHER}/{version}")]);

        let path = expand_source("${TRICKY}", "v1", &lookup).expect("the template expands");

        assert_eq!(path, PathBuf::from("${OTHER}/{version}"));
    }
}

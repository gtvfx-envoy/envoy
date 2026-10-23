# GitHub Copilot Instructions for gtvfx-envoy repos.

This file contains coding standards and guidelines that GitHub Copilot should follow when providing code suggestions for this repository.

## Style Guide

Generally, we follow the **PEP 8 style guide** with the following specific modifications and additions:

### Naming Conventions

| Element | Convention | Example |
|---------|------------|---------|
| Package and Module Names | `snake_case` | `my_module` |
| Class Names | `UpperCamel` | `MyClass` |
| Function Names | `camelCase` | `myFunction` |
| Property Names (`@property`) | `snake_case` | `my_property` |
| Variable Names | `snake_case` | `my_variable` |
| Class Variable Names | `snake_case` | `class_variable` |
| Constant Class Variable Names | `UPPER_SNAKE` | `CLASS_CONSTANT` |

**Important:** We use `camelCase` for function names to differentiate functions from variables at a glance. This also applies to stored lambda and partial functions.

**Properties use `snake_case`** because they are accessed without parentheses, making them syntactically indistinguishable from attributes. Using `snake_case` signals "read me like data" and matches Python's own stdlib conventions (`Path.parent`, `Popen.returncode`, etc.).

### Never Override Python Built-ins

**CRITICAL RULE:** Never use variable names that shadow Python built-in functions or types (`id`, `type`, `list`, `dict`, `set`, `str`, `input`, `open`, `filter`, `dir`, `exit`, `format`, `hash`, `len`, and similar). Shadowing prevents using that built-in later in the same scope and confuses readers who expect built-in behavior.

```python
# BAD - overrides built-ins
type = "Window"
dir = "C:/temp"
filter = "*.max"

# GOOD - descriptive names instead
asset_type = "Window"
directory = "C:/temp"
file_filter = "*.max"
```

### Line Length

- **Target**: under **80 characters** when possible; **hard limit of 100**.
- Only break a line when it approaches or exceeds the limit — don't break lines that
  already fit comfortably just for the sake of it.

## Docstring Standards

We follow **Google Style Python docstrings** as documented at:
https://sphinxcontrib-napoleon.readthedocs.io/en/latest/example_google.html

**Critical rule:** a multi-line docstring must end with one blank line before the
closing `"""` (a single-line docstring does not need one).

Use these sections, in order, including only the ones relevant to a given docstring:
summary line → extended description (optional) → `Args` → `Returns` (or `Yields` for
generators) → `Raises` → `Note(s)` → `Example(s)` → `Attributes` (classes) → `Todo`.

```python
def exampleFunction(param1: int, param2: str = None) -> bool:
    """Brief one-line summary.

    Args:
        param1: Description of the first parameter.
        param2: Description of the second parameter. Defaults to None.

    Returns:
        Description of the return value.

    Raises:
        ValueError: If param1 is negative.

    Note:
        Any additional caveat worth calling out.

    """
```

- Omit `self`/`cls` from a method's `Args` section.
- For classes, document public attributes in an `Attributes:` section on the class's
  own docstring, not `__init__`'s.
- For `@property` getters, put the type at the start of the summary line, e.g.
  `"""str: Description."""`.
- For usage examples, use doctest-style `>>>` blocks under an `Examples:` section;
  bullet (`-`) each one if there's more than one.
- Note when a method overrides a parent class method (e.g. start the summary with
  `"""Override: ..."`).

### Exception Standards
**Try to catch specific exceptions rather than using a broad `Exception` catch.**

## Compliance

**Never reference the proprietary `bl` tool by name anywhere in this repo** — not in
code, comments, docstrings, docs, or commit messages. If `bl`'s design is useful
inspiration, describe the underlying concept generically instead of naming or
attributing it. (This has already required a compliance fix once — see
`envoy-stretch-goals_plan.md`'s "Compliance issue" section — so treat it as a hard
rule, not a one-off cleanup.)

## Rust (`rust/`)

This repo's core logic is being ported to Rust (`envoy-core`, `envoy-cli`, `envoy-py`).
The Python conventions above apply only to `.py` files (`py/`, `scripts/`); Rust code
follows ordinary Rust/rustdoc idioms instead — snake_case functions/variables,
`UpperCamelCase` types, no `camelCase` — plus the project-specific patterns below,
which reflect what this codebase actually does today, not generic Rust advice.

### Error handling
Each crate defines its own `thiserror`-derived error enum (`EnvoyError` in
`envoy-core`, `TelemetryError` for the telemetry module, etc.) with one variant per
failure mode, rather than `anyhow`/`Box<dyn Error>`. Follow this pattern for new
fallible APIs in `envoy-core`/`envoy-cli`.

### Doc comments
Module-level `//!` docs at the top of each file explain the module's purpose and
design rationale in prose (often with a short example); public items get `///` doc
comments. This is plain rustdoc style, not the Google-style Args/Returns sections used
for Python above.

### Tests
- Unit tests live in a `#[cfg(test)] mod tests` block at the bottom of the same file,
  using `use super::*;`.
- **Any test that mutates a real process environment variable (`std::env::set_var`/
  `remove_var`) must acquire `crate::env_test_lock::MUTEX`** (defined in
  `envoy-core/src/lib.rs`) before mutating, in addition to saving/restoring the
  previous value. `cargo test` runs across parallel threads in one process, and env
  vars are process-global, so two tests in *different* modules that each only guard
  their own local state can still race on the same real var and fail intermittently
  when run as part of the full suite, even though each passes individually run alone.
  This is not hypothetical: a real test in `telemetry/file_drop.rs` needed exactly
  this fix during development.
- Use a small `EnvVarGuard` RAII struct (save the previous value in `::set`, restore
  it in `Drop`) to manage temporary env var changes — this exact pattern already
  recurs per-file across the codebase (`user_config.rs`, `environment.rs`,
  `team_config.rs`, `stack_registry.rs`, etc.); match it rather than inventing a new
  save/restore idiom.

### Validation gate
Before considering a Rust change complete, run the same checks CI does
(`.github/workflows/lint.yml`):
```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
(`envoy-py` needs `LIBRARY_PATH` set for its PyO3/libpython link step on some
self-hosted runners — see `lint.yml` — but plain `envoy-core`/`envoy-cli` changes
don't need that.)

For a change to `scripts/` or `tests/`, run the Python checks CI does:
```powershell
ruff check scripts tests
ruff format --check scripts tests
python -m pytest tests
```

### Windows build noise
`cargo build`/`cargo test` on Windows may print
`note: did not finalize incremental compilation session directory ... Access is
denied. (os error 5)` — this is usually harmless (often antivirus/indexing
transiently locking a newly-written file) as long as the build still reports
`Finished`. Don't treat this note alone as a build failure.

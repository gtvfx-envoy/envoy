# Troubleshooting

## Start Here — `envoy --diagnose`

Before digging into a specific symptom below, run:

```console
en --diagnose               # bundles, team/stack, cache, VCS, telemetry status
en --diagnose python    # also shows the command's full resolved environment
```

This surfaces discovered bundles and commands, resolved team/stack context,
bundle cache location and state (with how each bundle stands with it),
detected VCS backend, and
bundle-root reachability (flagging unreachable network paths) in one report —
often enough to spot the problem before checking the more specific sections
below.

## A Bundle Is Not Being Cached, or the Cache Misbehaves

`en --diagnose` shows beside each bundle whether it is `[cached]`,
`[not cached yet]`, `[refused: ...]` or `[never cached: ...]`, and why.
`en cache status` shows where the cache is and whether caching is on.

| What you see | What it means |
|---|---|
| `[never cached: it has no .bundle marker ...]` | The bundle was copied to the share by hand. Publish it with `engit publish bundle`, which writes the marker |
| `[refused: ... would reach N characters ...]` | A path in the bundle is too long for Windows under the cache directory. Choose a shorter one with `ENVOY_CACHE_ROOT` (or `--cache-root`), or publish the bundle with shorter paths |
| `Bundle caching is unavailable` warning | The cache directory cannot be created or written, for example because it is read-only. Envoy runs from the share meanwhile; point `ENVOY_CACHE_ROOT` somewhere writable |
| `Could not cache ... not enough free space` | The drive holding the cache is full. `en cache list` shows what is using it; `en cache prune` or `en cache clear` frees it, and `ENVOY_CACHE_MAX_SIZE` keeps it bounded |
| The first run of a command is slow | Expected: bundles are copied once. Warm a machine ahead of time with `en --noop` |
| A cached bundle misbehaves | `en cache verify` checks each entry is complete. Delete a damaged entry's directory, or run `en cache clear`; envoy copies it again |
| You want to bypass the cache once | `en --cache off <command>` runs from the share and copies nothing |
| `warning: this directory is on a network location` | The cache directory is on a network location, where a cache saves little and file locks can be unreliable (often a home directory mounted over NFS). Point `ENVOY_CACHE_ROOT` at a local disk |

See [Bundle Caching](caching.md) for the whole picture.

## The Share Is Unreachable

A stack that has run on this machine with caching on starts without its share,
from the bundles in the local cache ([how](caching.md#when-the-share-is-unreachable)).
`en --diagnose` shows `from a snapshot` under the stack when that is what happened.

| What you see | What it means |
|---|---|
| `envoy: the stack root ... cannot be reached; using stack 'studio' (...) as it was 3 hours ago` | Not an error. The share is down, so envoy used the stack as it was the last time a command ran with the share reachable. It is refreshed on the next run once the share is back |
| `envoy: not cached on this machine, so unavailable: ext:pyside@...` | The stack lists bundles that were never copied here, so the commands that need them are missing until the share is back. Prepare a laptop beforehand with `en -s=studio --noop` |
| `Named stack "studio" not found in ENVOY_STACK_ROOTS (the stack root ... cannot be reached), and no earlier run of it is remembered on this machine` | This machine has not run that stack with caching on, or the stack has no published bundle (nothing is remembered for one), so it needs the share. A stack that was selected by path says `Stack path does not exist: ...` in the same place. Check the name against `en --list-stacks` |
| `... none of its bundles are cached on this machine (...)` | The stack ran here, but nothing of it is in the cache, because the commands that ran needed no published bundle, or the cache was cleared. It needs the share once more |
| `Named stack "studio" not found in ENVOY_STACK_ROOTS` with no "cannot be reached" | The share is reachable and does not publish that name. `en --list-stacks` shows what it does publish |
| `... (the stack root ... cannot be reached)` after an error with caching off | `ENVOY_CACHE=off` (or `--cache off`) stops envoy using a remembered stack, so the share is needed |
| `envoy: ENVOY_STACK_CONTEXT selects a stack from ENVOY_STACK_ROOTS, but ... cannot be reached, so no stack was selected` | A context is resolved by searching the stack roots, so it cannot start without them. The note ends with the stacks remembered from earlier runs (a stack chosen by context is remembered under its name); start one with `en --stack <name>`. With none listed, nothing has run here with caching on |
| `I/O error at ...studio.estack: Access is denied` while the share is up | The share answered and refused the file, so this is a permissions or publishing mistake, not an outage. Envoy does not run a remembered copy instead, because that would hide it. Ask whoever published the stack to fix its permissions |

## The Launcher Will Not Start Envoy

`envoy --envoy-shim-resolve` shows which version you are pinned to, where the
redirect file came from, and whether that version is cached. The launcher's own
failures are one line starting `envoy:` and use exit codes `64`, `69`, `70` and
`78` (see [the launcher](launcher.md#exit-codes)).

| What you see | What it means |
|---|---|
| `no redirect file is configured. Set ENVOY_CONFIG_PROD ...` (78) | Set `ENVOY_CONFIG_PROD` to the team's config folder, or `ENVOY_REDIRECT_CONFIG` to a redirect file |
| `the redirect file could not be used, and there is no saved copy ...` (78) | The file is missing, unreadable, or invalid, and this machine has never read a good one. The message lists each place tried and why. Check that the share is reachable and that `redirect.yaml` is in the folder `ENVOY_CONFIG_PROD` names |
| `the redirect file could not be used (...); using the copy saved 3 hours ago` | Not an error. The share was unreachable, or the file on it is invalid or too new for this launcher (the message says which), so the launcher used the last good copy it saved. The administrator can check the file with `--envoy-shim-validate-config` |
| `envoy v0.8.0 is not cached and could not be found (looked in: ...)` (69) | The version is not on this machine and not on any source. The message says why each source was no good: an unset `${VAR}`, or no `bin/envoy` in the folder. Ask the administrator to publish it, or check that the share is reachable |
| `'alice' is assigned to group 'x', which has no version` (78) | The redirect file names a group under `users` that has no entry under `versions` |
| `the redirect file has no 'default' version, and 'bob' has no assignment` (78) | Users who are not listed need a `default` version; add one |
| `the redirect file uses schema 2, but this envoy understands up to schema 1` | The file is newer than this launcher. Update the launcher: `envoy --envoy-shim-update`, or run the installer again. Until then it uses its saved copy of the file |
| `envoy: could not cache envoy v0.8.0 (...); running it from ...` | The local copy failed (disk full, directory not writable). It still ran, from the share. Fix the cache directory (see above) |
| The wrong version runs | `--envoy-shim-resolve` says which version and why. Check for `ENVOY_VERSION`, and for `ENVOY_BIN_LOCK`: a program that envoy started stays on the version that started it |
| An old `envoy` runs instead of the launcher | An older install comes first on `PATH`. `Get-Command envoy -All` (PowerShell) or `which -a envoy` shows the order |
| The launcher did not update after `current.json` changed | Installed launchers look once a day. `envoy --envoy-shim-update` looks now. A launcher run from a checkout, or installed machine-wide, never updates itself, and `ENVOY_SHIM_UPDATE=off` stops it |

Set `ENVOY_VERSION=<version>` to run a particular version for one session
without touching the redirect file.

## "Could not find commands.json"

Envoy has no commands to load. Fix one of:

- Set `ENVOY_BNDL_ROOTS` to point to a directory containing bundle subdirectories
- Use `--stack` / `-s` with a path to a `studio.estack`
- Use `--commands-file` / `-c` with a direct path to `commands.json`
- Run from inside a project that has `.envoy/commands.json`

## Commands Not Appearing in `--list`

```console
en --verbose --list
```

Check the verbose output for:

- Which roots are being scanned
- Which directories have `.git/` but no `.envoy/` (or vice versa)
- Any parse errors in `commands.json`

**Common causes:**

| Symptom | Likely cause |
|---|---|
| Root scanned, no bundles found | Bundles are not direct children of the root (scan is one level deep) |
| Bundle found but no commands | `commands.json` missing, empty, or has a JSON syntax error |
| Bundle not found at all | `.git/` directory missing (required for auto-discovery) |
| Command from wrong bundle | Command conflict — last bundle wins; check with `--verbose` |

## Executable Not Found

In closed mode the subprocess `PATH` comes entirely from bundle env files. If the executable is not found:

1. Ensure the bundle env file sets `+=PATH` pointing to the executable's directory
2. Use `en --which <command>` to see what path resolves against the subprocess `PATH`
3. Use `en -i <command>` temporarily to confirm the executable is present on the system `PATH`

```console
en --which python          # check resolved path
en -i python script.py     # run with system PATH inherited
```

## Environment Variables Not Applying

1. Check JSON syntax in env files — use a JSON linter
2. Run `en --verbose <command>` to see exactly which files are loaded and what values are set
3. In closed mode, `${VARNAME}` references to system variables expand to empty string unless the variable is in `ENVOY_ALLOWLIST` or is a core OS variable
4. Use `en --trace VAR <command>` to see how a specific variable is mutated across files

```console
en --trace PYTHONPATH python
en --verbose python --help
```

## Path Inconsistency (Mixed Slashes)

Envoy normalizes all env file values to OS-native separators before applying them. If you see mixed slashes in a subprocess, the value likely came directly from the system environment (via `ENVOY_ALLOWLIST` or `--inherit-env`) rather than from a bundle env file.

Set the variable explicitly in an env file to ensure normalization is applied:

```json
{
    "SOME_PATH": "${__BUNDLE__}/data"
}
```

## Null/Unresolved Variable Warnings

```
WARNING: Variable 'SITE_PACKAGES' is null in python_env.json — skipping.
WARNING: List item '${ENVOY_SITE_PACKAGES}' in 'PYTHONPATH' (python_env.json)
         contains unresolved references: ENVOY_SITE_PACKAGES — skipping item.
```

These warnings are informational — they indicate a variable or list item was intentionally or accidentally left undefined. To suppress:

- Remove the entry from the JSON file if not needed
- Set the variable before it is referenced (earlier env file, `global_env.json`, or `ENVOY_ALLOWLIST`)
- Use `?=` to make the value conditional instead of referencing an undefined variable

## Envoy Utils Issues

Troubleshooting for `engit` is maintained with
[Envoy Utils](https://github.com/gtvfx-envoy/envoy_utils/blob/v0.1.0/docs/troubleshooting.md).

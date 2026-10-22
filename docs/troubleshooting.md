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

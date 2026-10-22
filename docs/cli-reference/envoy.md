# envoy CLI Reference

The `envoy` command (and its short alias `en`) launches managed subprocesses with configured environments.

## Synopsis

```
envoy [OPTIONS] [command] [args ...]
en    [OPTIONS] [command] [args ...]
envoy cache ACTION [--dry-run]
```

!!! note "Flag position matters"
    Envoy flags must appear **before** the command name. Anything after the command name is passed verbatim to the child process.

    ```powershell
    en -i python -c "print(1)"   # -i goes to envoy, -c goes to python
    en -c file.json python        # -c file.json goes to envoy
    ```

## Options

| Flag | Short | Description |
|---|---|---|
| `--list` | | List all available commands |
| `--info COMMAND` | | Show detailed information about a command |
| `--which COMMAND` | | Resolve the executable path for a command |
| `--commands-file PATH` | `-c` | Path to a specific `commands.json` |
| `--stack NAME_OR_PATH` | `-s` | Named Stack or strict YAML `.estack` path |
| `--set-config KEY=VALUE` | `-sc` | Set a user config value and save |
| `--get-config [KEY]` | `-gc` | Print one or all user config values |
| `--list-configs` | `-lc` | List all known configurable settings |
| `--ignore-config` | `-ic` | Bypass user config for this run |
| `--env ENV_COMMAND` | `-e` | Run command inside a different command's environment |
| `--override-bundle BNDLID=PATH` | `-o` | Substitute a local checkout for a bundle a resolved Stack or discovery would otherwise use. Repeatable |
| `--tag TAG` | | Attach a free-text tag to this invocation's telemetry record, if enabled. Truncated to 200 characters |
| `--incognito` | | Disable telemetry for this invocation only |
| `--shell` | | Drop into an interactive shell inside a command's resolved environment, instead of running it |
| `--inherit-env` | `-i` | Inherit the full system environment (overrides closed mode) |
| `--verbose` | `-v` | Enable verbose logging |
| `--noop` | | Copy the bundles `COMMAND` needs (every published bundle when no command is given) into the [bundle cache](../caching.md), then exit without running anything |
| `--cache on\|off` | | Turn bundle caching on or off for this run |
| `--cache-root PATH` | | Use `PATH` as the bundle cache directory for this run |
| `--no-prune` | | Do not prune the bundle cache in the background during this run |
| `--trace VAR` | | Trace how `VAR` is mutated through env file processing |
| `--diagnose [COMMAND]` | | Show bundle/team/stack/cache/VCS/telemetry diagnostics; add `COMMAND` for its full resolved environment |
| `--docs [BUNDLE]` | | Open the envoy documentation in the default browser, or a specific bundle's own docs |
| `--version` | | Show version and exit |
| `--help` | `-h` | Show help message |

## Commands

### `--list`

List all available commands with their source bundle and alias:

```powershell
en --list
```

```
Available commands:

  python           → python -X dev  [gt:ext:python]
  unreal               (executable on PATH)  [gt:unreal]
  vscode               → C:/.../Code.exe --wait  [gt:globals]
```

### `--info COMMAND`

Show full details for a command:

```powershell
en --info python
```

```
Command: python
Target: linux/x86_64
Configuration: base -> linux -> x86_64
Bundle: gt:ext:python
Executable: python
Environment files:
  - python_env.json
Environment directory: R:/repo/.../pythoncore/.envoy
```

### `--which COMMAND`

Resolve the executable path using the subprocess `PATH` built from the command's env files — the same `PATH` the child process will actually see:

```powershell
en --which python
# C:\Python311\python.exe
```

### `--trace VAR`

Show how a variable is set, modified, and resolved as each env file is loaded:

```powershell
en --trace PYTHONPATH python
```

Useful for debugging unexpected variable values. When the value includes the
Python API that a release of envoy adds on its own, the trace says so.

### `--verbose`

Emit detailed logging for bundle discovery, command loading, environment processing, and executable resolution:

```powershell
en --verbose --list
en --verbose python script.py
```

### `--diagnose [COMMAND]`

Show a full diagnostic report: discovered bundles and commands, resolved
the current Stack and team config (`.envoy/team.json`),
bundle cache location and state (and, beside each bundle, whether it is
cached, not cached yet, refused because its paths are too long for the cache
directory, or never cached), detected VCS backend (Git,
Perforce, or [Lore](https://github.com/EpicGames/lore)) with its pending
change count, telemetry status, and bundle-root reachability (flagging
network/UNC paths). Pass a command name to also see its full resolved
environment (every variable, not just one):

```powershell
en --diagnose
en --diagnose python
```

```
================================================================
envoy diagnose
================================================================

Target: linux/x86_64

Bundles discovered: 2
  - gt:pythoncore            R:/repo/gtvfx-envoy/pythoncore  [not cached yet]
  - gt:unreal                R:/repo/gtvfx-envoy/unreal  [never cached: it has no .bundle marker (a checkout or an unpublished bundle)]

Commands registered: 3
  python, unreal, vscode

Current stack: studio (R:/stacks/studio.estack)
  namespace: bfd:build

Team config: bfd
  prod_bundles_root:  \\server\bundles

Bundle cache: C:\Users\you\.envoy\cache (on, set by the default)
  3 entries, 781.9 MB

VCS detected: git at R:/repo/gtvfx-envoy
  0 pending change(s)

Telemetry (explicit Python API opt-in): disabled (default; call envoy.enable_telemetry(...) to opt in)
Telemetry (automatic envoy.command.run export): disabled (no ENVOY_TELEMETRY_ENDPOINT / OTEL_EXPORTER_OTLP_* resolved, or ENVOY_TELEMETRY_ENABLED=false)
  local spool depth:    0

Bundle root reachability:
  - gt:pythoncore            [local        ] reachable: R:/repo/gtvfx-envoy/pythoncore
  - gt:unreal                [local        ] reachable: R:/repo/gtvfx-envoy/unreal
```

When `ENVOY_TELEMETRY_ENDPOINT` (or a standard `OTEL_EXPORTER_OTLP_*`
variable) resolves, the automatic-export line instead reports the
resolved transport (`http` or `file-drop`), a sanitized endpoint (no
headers/credentials), configuration source, schema version, and local
spool depth. See [Telemetry](../telemetry.md) for the full picture --
what gets recorded, redaction, the local retry spool, and the shared
studio dashboard this feeds.

For a single-variable, step-by-step operator trace instead, use `--trace VAR COMMAND`.

### `--docs [BUNDLE]`

With no argument, opens envoy's own documentation. Pass a discovered
bundle's ID to open that bundle's own documentation instead -- its
`docs/index.html` if present, falling back to its `README.md`:

```powershell
en --docs
en --docs gt:pythoncore
```

There is no dedicated docs-location metadata field; this checks the same
two conventional locations bundles in this organization already use.

### `--env` / `-e`

Run a command inside a different command's environment:

```powershell
en --env python cmd
```

Opens `cmd.exe` with the `python` environment (Windows example; on
Unix-like systems, use `en --env python sh` instead) — useful for
interactive inspection, though `--shell python` below is the more direct
way to do exactly this.

### `--shell`

Drop into an interactive shell (`cmd.exe`/`$SHELL`) inside a command's
resolved environment for inspection, instead of running the command itself:

```powershell
en --shell python
```

Resolves `python`'s environment exactly as a normal run would, then
launches the shell reported by that environment's own `COMSPEC`/`SHELL`
(falling back to the current process's, then a platform default). Type
`exit` to return. Combine with `--env`/`-e` to inspect a *different*
command's environment than the one named.

## Cache Management

Published bundles are copied to a local [cache](../caching.md) the first time a command needs them. These commands look after it. Apart from `warm`, they work on the cache directory alone: they need no stack, are fast, and never create a cache that is not there.

`cache` is treated as the cache command only when one of the actions below follows it. A bundle's own `cache` command keeps working with any other argument.

| Command | What it does |
|---|---|
| `envoy cache status` | Where the cache is, whether caching is on and what decided that, how many entries and how much space, bundles that do not fit, and when it was last pruned |
| `envoy cache list` | Every entry with its size and when it was last used |
| `envoy cache verify` | Check each entry against what was recorded when it was copied: nothing missing, nothing extra, sizes add up. Exits `1` when an entry is damaged. It does not read file contents |
| `envoy cache prune [--dry-run]` | Remove entries unused for the idle period (`ENVOY_CACHE_MAX_IDLE_DAYS`, default 30 days), and the least recently used beyond `ENVOY_CACHE_MAX_SIZE` if set. Never removes an entry a running envoy is using |
| `envoy cache clear [--dry-run]` | Remove every entry that is not in use. Envoy copies again what it needs next time |
| `envoy cache warm [COMMAND]` | Same as `envoy --noop [COMMAND]` |

```powershell
en cache status
en cache prune --dry-run
```

### `--noop [COMMAND]`

Warm the cache ahead of time. `envoy --noop` resolves the stack and copies every published bundle in it into the cache, then exits without launching anything. With a command, `envoy --noop COMMAND` copies only the bundles that command needs, and reports an error if it does not exist.

```powershell
en -s=studio --noop            # everything in the stack
en -s=studio --noop python     # only what python needs
```

```
Cache directory: C:\Users\you\.envoy\cache
Copied 6 bundles (792.6 MB), 0 already cached
Never cached: 1 bundle
  despatch:v0.2.0: it has no .bundle marker (a checkout or an unpublished bundle)
```

The exit code is `1` when a bundle could not be cached, so a scheduled warm-up shows up as failed. Bundles that are never cached (checkouts, unmarked bundles) are listed but do not fail it. With caching off it says so and exits `0`.

### `--cache`, `--cache-root`, `--no-prune`

Per-run overrides of the [cache settings](../caching.md#settings). They go before the command name like every other envoy flag:

```powershell
en --cache off python          # run from the share, copy nothing
en --cache-root D:\envoy_cache python
en --no-prune python           # skip the background prune this run
```

`--cache on|off` beats `ENVOY_CACHE`, the older switch, and the `cache` user setting.

## Discovery Flags

### `--stack` / `-s`

Select a named Stack or explicit `.estack` file:

```powershell
en -s R:/studio/studio.estack --list
en --stack studio python script.py
```

### `--set-stack NAME_OR_PATH`

Validate `NAME_OR_PATH` (the same way `--stack` would resolve it) and
persist it as the `stack` user-config setting, then exit. Equivalent to
`--set-config stack=VALUE`, but rejects a value that doesn't actually
resolve instead of silently saving it:

```powershell
en --set-stack studio
en --set-stack R:/studio/studio.estack
```

### `--get-stack`

Print the current *effective* stack -- honoring the same `ENVOY_STACK` /
`stack` user-config / `ENVOY_STACK_CONTEXT` precedence used by normal
resolution, not just the raw persisted setting -- then exit:

```powershell
en --get-stack
```

### `--list-stacks`

List all named stacks discovered via `ENVOY_STACK_ROOTS`, then exit:

```powershell
en --list-stacks
```

### `--commands-file` / `-c`

Point directly at a `commands.json`, bypassing bundle discovery entirely:

```powershell
en -c R:/my-project/.envoy/commands.json my_command
```

### `--override-bundle` / `-o`

Substitute a local checkout for a bundle a resolved Stack or
`ENVOY_BNDL_ROOTS` discovery would otherwise use, without hand-writing a
custom Stack file. Repeatable; specifying the same `BNDLID` more than once
uses the *last* value:

```powershell
en -o gt:maya=R:/dev/maya python script.py
en --override-bundle gt:maya=R:/dev/maya --override-bundle gt:unreal=R:/dev/unreal --list
```

`BNDLID` must match the `namespace:name` of a bundle actually discovered by
the resolved Stack or bundle-root discovery -- this flag only ever swaps an
already-discovered bundle's root path, it never adds a bundle the Stack
wouldn't otherwise use. `PATH` must point at a directory containing its own
`.envoy/` directory. Either condition failing is a hard error (not a
silently-ignored no-op), reporting every problem across all
`--override-bundle` values at once:

```powershell
en -o gt:does-not-exist=R:/dev/maya --list
# Error: --override-bundle problem(s):
#   'gt:does-not-exist' does not match any discovered bundle
# Run 'envoy --diagnose' to see discovered bundles
```

## Environment Variables

| Variable | Description |
|---|---|
| `ENVOY_BNDL_ROOTS` | Platform-separated roots for bundle auto-discovery (`;` on Windows, `:` on Unix) |
| `ENVOY_STACK` | Named Stack or `.estack` path used when `--stack` is omitted |
| `ENVOY_STACK_ROOTS` | Platform-separated roots containing versioned named Stacks |
| `ENVOY_STACK_CONTEXT` | Colon-separated context used for most-specific namespace matching |
| `ENVOY_ALLOWLIST` | Semicolon- or comma-separated variable names to pass through in closed mode |
| `ENVOY_CONFIG_ROOT` | Absolute directory replacing the shared `~/.envoy` config root; the user config is stored at `<root>/user_config.json` |
| `ENVOY_CACHE` | `on` or `off`: turn [bundle caching](../caching.md) on or off. Wins over every other caching setting |
| `ENVOY_CACHE_ROOT` | Directory holding the bundle cache (default `~/.envoy/cache`) |
| `ENVOY_CACHE_MAX_IDLE_DAYS` | Days a cached bundle may go unused before it is removed (default `30`) |
| `ENVOY_CACHE_MAX_SIZE` | Size the cache may grow to, such as `500GB`; the least recently used bundles are removed beyond it (default: no limit) |
| `ENVOY_BUNDLE_CACHE` | Older name for `ENVOY_CACHE_ROOT`; `ENVOY_CACHE_ROOT` takes precedence |
| `ENVOY_DISABLE_BUNDLE_CACHE` | Older way to turn caching off (`1`/`true`/`yes`); `ENVOY_CACHE` takes precedence |
| `ENVOY_DISABLE_DISCOVERY_CACHE` | Set to `1`/`true`/`yes` to force a fresh bundle-discovery scan, bypassing the on-disk discovery cache |
| `ENVOY_VCS` | Force a specific VCS backend for detection: `git`, `perforce`, or `lore` |
| `ENVOY_PYINIT` | Platform-separated directories whose `*.py` files run once `import envoy` finishes initializing (see [Advanced Topics](../advanced.md#python-api-initialization-hooks-envoy_pyinit)) |
| `ENVOY_PYTHON_API` | `off`, `false`, `no`, or `0` stops a release of envoy adding its own Python API (`py/`) to the end of `PYTHONPATH` for the commands it launches (see [Installation](../installation.md)) |
| `ENVOY_PYTHON_PATH` | Set by a release of envoy for the commands it launches: the directory of the Python API that ships with it |
| `ENVOY_CONFIG_PROD` | The team's config folder, which holds `redirect.yaml` and `shim/`; read by the [launcher](../launcher.md) |
| `ENVOY_TEAM` | A team name for the [launcher](../launcher.md): its `redirect.yaml` is looked for in `<ENVOY_CONFIG_PROD>/<team>` first |
| `ENVOY_REDIRECT_CONFIG` | A redirect file for the [launcher](../launcher.md) to use instead of looking for one |
| `ENVOY_VERSION` | Run this envoy version, ignoring the redirect file ([launcher](../launcher.md)) |
| `ENVOY_USER` | The name the [launcher](../launcher.md) looks up in the redirect file, instead of your login name |
| `ENVOY_SHIM_TIMEOUT_MS` | Milliseconds the [launcher](../launcher.md) waits for the share before using its saved copy of the redirect file (default `2000`) |
| `ENVOY_SHIM_UPDATE` | `off` stops the [launcher](../launcher.md) updating itself |
| `ENVOY_BIN_LOCK` | Set for the programs envoy runs: the envoy that started them. The launcher keeps a nested `envoy` call on that version |
| `ENVOY_BIN_FLOAT` | Set for the programs envoy runs: the launcher that started the outermost envoy |

## User Config Flags

### `--set-config KEY=VALUE` / `-sc`

Save a preference to the user config file:

```powershell
en --set-config stack=R:/studio/envoy/studio.estack
en --set-config verbosity=verbose
en --set-config stack=     # clear the setting
```

### `--get-config [KEY]` / `-gc`

Print the current value of one setting, or all settings if no key is given:

```powershell
en --get-config                   # show all settings
en --get-config stack    # show one setting
```

### `--list-configs` / `-lc`

Show all configurable settings with descriptions and current values:

```powershell
en --list-configs
```

### `--ignore-config` / `-ic`

Run without reading any user config values:

```powershell
en --ignore-config python script.py
```

See [User Configuration](../user-config.md) for the full reference.

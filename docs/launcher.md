# The Envoy Launcher

On a team, everyone should run the same version of envoy, and moving the team to a new version should not mean visiting every machine. The launcher does that. It is a small program (about 1 MB) installed as `envoy` and `en`. Each time it starts it reads one file on the team share, `redirect.yaml`, to learn which envoy version you are pinned to, makes sure that version is on your disk (copying it from the share the first time), and runs it with the arguments you gave. The version for the whole team changes by editing that one file; see [Team Administration](team-administration.md).

You never need to think about it after installing. It prints nothing unless it is copying something or something is wrong.

## Installing

Your administrator publishes the installer on the share, in the `shim` folder under `ENVOY_CONFIG_PROD`. Run it from there:

=== "Windows PowerShell"

    ```powershell
    & "$env:ENVOY_CONFIG_PROD\shim\install-envoy.ps1"
    ```

=== "Linux and macOS"

    ```bash
    sh "$ENVOY_CONFIG_PROD/shim/install-envoy.sh" --modify-profile
    ```

The installer:

- copies the launcher to `~/.envoy/bin` (`%USERPROFILE%\.envoy\bin` on Windows) as `envoy` and `en`;
- puts that folder at the front of `PATH` (on Linux and macOS, only with `--modify-profile`; without it, the installer prints the line to add to your profile);
- prints which envoy version you are pinned to.

Open a new terminal afterwards so `PATH` is picked up. Installing again replaces the launcher, also while it is running, and uninstalling removes it:

```powershell
& "$env:ENVOY_CONFIG_PROD\shim\uninstall-envoy.ps1"                 # keeps the cache
& "$env:ENVOY_CONFIG_PROD\shim\uninstall-envoy.ps1" -RemoveCache    # also deletes ~\.envoy\cache
```

If PowerShell refuses to run a script that came from the internet, unblock it first with `Unblock-File`, or start it as `powershell -ExecutionPolicy Bypass -File <script>`. Launcher binaries are unsigned for now, so Windows may show a SmartScreen warning the first time one runs (see [Unsigned Artifact Notice](installation.md#unsigned-artifact-notice)).

A shared or farm machine can install the launcher for every user with `-Scope Machine` from an elevated prompt. It goes to `%ProgramFiles%\envoy\bin`, which only administrators can write to, and to the front of the machine `PATH`. The installer refuses a folder that anyone else could change, because anything they put in a folder on the machine `PATH` would run for administrators. A machine-wide launcher does not update itself; run the installer again to update it.

You can also install from an unpacked launcher archive (`envoy-shim-v<version>-<platform>`) instead of the share: run the installer script that is inside it. The archive holds the launcher, `install-envoy` and `uninstall-envoy` scripts for the platform, and the license. The Windows archive also holds `verify-launcher.ps1`, which runs envoy through a handful of checks (the launcher is the first `envoy` on `PATH`, the redirect file gives a version, envoy starts and keeps its exit codes, it still starts when the share is slow, and so on) and prints `PASS`, `FAIL` or `SKIP` for each:

```powershell
.\verify-launcher.ps1 -Stack studio -ExpectVersion v0.8.0
```

## Settings

The launcher is configured with environment variables. `ENVOY_CONFIG_PROD` is the only one most people need, and it is normally set once for the whole team.

| Variable | Effect |
|---|---|
| `ENVOY_CONFIG_PROD` | The team's config folder, which holds `redirect.yaml` and `shim/` |
| `ENVOY_TEAM` | A team name. Looks for `<ENVOY_CONFIG_PROD>/<team>/redirect.yaml` first, and uses the studio-wide file only when the team has no file. A team file that is there but cannot be used is never replaced by the studio-wide one |
| `ENVOY_REDIRECT_CONFIG` | A redirect file to use instead of looking for one. Takes precedence over the two above |
| `ENVOY_VERSION` | Run this version, ignoring the redirect file, for one session or one command. For emergencies and testing. A version that is already cached starts without reading the redirect file or the share; one that is not is copied from the first of the file's `sources` that has it |
| `ENVOY_USER` | The name to look up in the redirect file, instead of your login name |
| `ENVOY_SHIM_TIMEOUT_MS` | How long to wait for the share before using the saved copy of the redirect file (default `2000`) |
| `ENVOY_SHIM_UPDATE` | `off` stops the launcher updating itself |
| `ENVOY_CACHE`, `ENVOY_CACHE_ROOT` | The same switches as for [bundle caching](caching.md); the launcher caches envoy versions in the same place |
| `ENVOY_BIN_LOCK` | Set for the programs envoy runs, so that `envoy` run from inside one stays on the version that started it |
| `ENVOY_BIN_FLOAT` | Set for the programs envoy runs: the launcher itself |

## Which version you run

The launcher decides in this order:

1. If `ENVOY_BIN_LOCK` names an envoy (a program started by envoy is calling `envoy` again), that envoy runs. A nested call never changes version. Envoy also helps a nested call find it: in the environment it builds for a command, `PATH` starts with the `bin` folder of the envoy that is running, ahead of any envoy the stack itself lists, so `envoy` and `en` run from inside a command reach the envoy that started it. (`ENVOY_OWN_BIN=off` leaves `PATH` alone.)
2. Otherwise `ENVOY_VERSION`, if set.
3. Otherwise your login name is looked up in the redirect file: if you are listed you get your group's version, and everyone else gets the `default` version.

`envoy --envoy-shim-resolve` shows the answer and the reasoning:

```console
$ envoy --envoy-shim-resolve
user:     alice
group:    preview (assigned)
version:  v0.8.0-rc.1
config:   \\studio\envoy\config\redirect.yaml
cache:    C:\Users\alice\.envoy\cache (on)
launcher: envoy-shim v0.8.0 (installed in C:\Users\alice\.envoy\bin; looked for an update 3 hours ago)
app:      cached at C:\Users\alice\.envoy\cache\bundles\app\envoy\v0.8.0-rc.1 (10.6 MB)
```

## The first run, and every run after

The first time a version is needed, the launcher copies it from the first of the redirect file's `sources` that has it, with the same careful copy as [bundle caching](caching.md) (staged, checked, moved into place, safe to interrupt):

```text
envoy: caching app:envoy@v0.8.0 (9 files, 10.6 MB) on first use...
```

Later runs start the cached copy straight away and do not read the version from the share at all. The launcher adds about 12 ms to a start. Cached versions that nothing has used for 30 days are removed, like cached bundles.

The app is the authority on which version it is. When the launcher has just copied one, it asks it, once, with `envoy --version`. If the answer is not the version the app was published as, it says so and carries on:

```text
envoy: warning: the envoy published as v0.8.0-rc.1 reports itself as v0.7.0-51-g8ec805c. It runs anyway; ask whoever published it to check the version
```

A leading `v` and build metadata (`+g8ec805c`, which says which commit built it) are not part of the comparison. The check happens when the app is copied, so a start from the cache never pays for it and the message appears once on each machine.

Everything after the version is chosen is envoy's own business. The launcher passes your arguments through untouched and ends with envoy's exit code; on Windows it ignores Ctrl+C itself so that envoy decides what to do, and on Linux and macOS it becomes envoy.

## Working offline

When the share cannot be reached, the launcher keeps working from what it has:

- It uses the last good copy of the redirect file it read (saved whenever it reads the share) and says so: `envoy: the redirect file could not be used (...); using the copy saved 3 hours ago`.
- It runs the version you already have cached.
- Envoy itself then starts a stack that has run on the machine from the bundles in the local cache (see [Bundle Caching](caching.md#when-the-share-is-unreachable)), so a laptop that was prepared before leaving the network works end to end.

The same happens when the file on the share is there but cannot be used: it does not parse, was written for a newer launcher than yours, or would leave someone with no version (no `default`, a user in a group that has no version, an unsafe version name). A copy is only ever saved from a file that works for everyone, so a bad edit on the share never replaces it. With no saved copy, a file that has such a problem is still used by the people it works for, and `envoy --envoy-shim-resolve` lists what is wrong.

`envoy --envoy-shim-prefetch [VERSION]` copies a version ahead of time, for a laptop that is about to leave the network. If the version is not cached and the share is down, the launcher stops with a message that names the version and every place it looked.

## Updating the launcher

The share's `shim/current.json` names the launcher version the team runs. About once a day, whenever a launch gets to the point of choosing a version (even if the redirect file turns out to be unusable: a file written for a newer launcher is exactly when an update is needed), an installed launcher compares itself with that file. If they differ, it copies the published launcher next to itself, checks that the copy runs and reports the expected version, and replaces itself. The launch after that one is the new launcher; nothing needs restarting, and a failed update never stops a launch. Launches that start at the same moment take turns, so the launcher is replaced once.

Only a launcher installed in `~/.envoy/bin` updates itself, so a build run from a checkout is never replaced. `envoy --envoy-shim-update` looks right now and says what it found; `ENVOY_SHIM_UPDATE=off` turns updating off.

## Launcher options

These work only as the first argument; everything else is passed to envoy.

| Option | What it does |
|---|---|
| `--envoy-shim-version` | Print the launcher's version |
| `--envoy-shim-resolve` | Show which version you get, and why |
| `--envoy-shim-validate-config FILE [--check-sources]` | Check a redirect file before it is published (see [Team Administration](team-administration.md#checking-a-redirect-file)) |
| `--envoy-shim-prefetch [VERSION]` | Cache a version now, for use offline later |
| `--envoy-shim-update` | Look for a newer launcher on the share now, and install it |
| `--envoy-shim-help` | List these options |

## Exit codes

The launcher's own failures use exit codes that envoy does not normally return, so they are easy to tell apart. Any other code is envoy's own.

| Code | Meaning |
|---|---|
| `64` | A launcher option was used wrongly |
| `69` | The pinned version is not cached and could not be found on any source |
| `70` | Something went wrong that should not have; please report it |
| `78` | The redirect file could not be found or used, and there is no saved copy |

See [Troubleshooting](troubleshooting.md#the-launcher-will-not-start-envoy) for what to do about each.

## Python API

A release of envoy carries its Python API in `py/`, and the envoy the launcher starts adds it to `PYTHONPATH` for the commands it runs, so `import envoy` works inside them. See [Installation](installation.md).

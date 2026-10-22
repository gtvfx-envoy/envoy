# Bundle Caching

Published bundles usually live on a network share. Reading a Python install or a Qt distribution over the network makes every start slow, and stops working when the share is unreachable. Envoy copies the bundles a command uses to local disk the first time it needs them, then runs from that copy.

Your own checkouts are never cached: a bundle without a `.bundle` marker always runs from where it is. A run that needs only such bundles does not even create the cache directory.

## What happens on the first run

```text
envoy: caching ext:python@3.11.9-envoy.2 (5767 files, 140.4 MB) on first use...
  ext:python@3.11.9-envoy.2: 46% (64.6 MB of 140.4 MB)
envoy: cached ext:python@3.11.9-envoy.2 (140.4 MB) in 11.2 s
```

- Only the bundles the command needs are copied: the bundle that defines the command, every bundle's `global_env.json`, and the bundles that supply an environment file the command loads. Listing or inspecting commands (`--list`, `--info`, `--which`, `--trace`, `--diagnose`) reads bundle metadata from the share and copies nothing.
- A copy is built in a staging directory next to the cache and moved into place only when it is complete. Interrupting envoy (Ctrl+C, a crash, a lost connection) never leaves a partial bundle behind.
- Later runs find the copy straight away, with no scan and no checksum of the share.
- If envoy cannot cache a bundle, it prints one warning and runs that bundle from the share. A caching problem never stops a command.

Two envoys that want the same missing bundle at once take turns: one copies it, the other waits and then reuses it.

## Where the cache lives

By default the cache is `~/.envoy/cache` (`%USERPROFILE%\.envoy\cache` on Windows). Bundles are stored by name and version, so a cached bundle is easy to find:

```text
~/.envoy/cache/
  bundles/<namespace>/<name>/<version>/   one bundle, ready to run
```

The version is exactly the one in the bundle's `.bundle` marker, so `v0.7.0` and `0.7.0` are different entries.

## Settings

| Setting | Effect |
|---|---|
| `ENVOY_CACHE=on\|off` | Turn caching on or off. Wins over everything else |
| `ENVOY_CACHE_ROOT` | Directory holding the cache |
| `ENVOY_CACHE_MAX_IDLE_DAYS` | Days a bundle may go unused before it is removed. Default `30` |
| `ENVOY_CACHE_MAX_SIZE` | Size the cache may grow to, such as `500GB`. Unset means no limit |
| `cache` user setting | `on` or `off`, set with `envoy --set-config cache=off` |
| `bundle_cache_dir` user setting | Directory holding the cache, when `ENVOY_CACHE_ROOT` is not set |
| `ENVOY_DISABLE_BUNDLE_CACHE=1` | Older way to turn caching off; `ENVOY_CACHE` takes precedence |
| `ENVOY_BUNDLE_CACHE` | Older name for the cache directory; `ENVOY_CACHE_ROOT` takes precedence |

Caching is on unless something turns it off. The order is `ENVOY_CACHE`, then `ENVOY_DISABLE_BUNDLE_CACHE`, then the `cache` user setting. `--ignore-config` skips the user settings for one run.

Three flags change the settings for a single run, and beat everything above: `--cache on|off`, `--cache-root PATH`, and `--no-prune` (see [the CLI reference](cli-reference/envoy.md#cache-management)).

## Warming a machine ahead of time

The first run of a command copies what it needs, which can take a few seconds to a minute on a slow share. To pay that cost in advance, for example from a login script or a scheduled task, warm the cache:

```powershell
en -s=studio --noop            # every published bundle in the stack
en -s=studio --noop python     # only what python needs
```

Nothing is launched. Bundles already in the cache are skipped, so running it again is quick. It exits with `1` if a bundle could not be cached, so a scheduled run shows up as failed.

## Looking after the cache

```powershell
en cache status               # where it is, what is in it, when it was last pruned
en cache list                 # every entry, its size, when it was last used
en cache verify               # check each entry is complete
en cache prune --dry-run      # what the cleanup below would remove
en cache clear                # remove everything that is not in use
```

`verify` compares each entry with what was recorded when it was copied: no file missing or added, and the sizes add up. It does not read file contents. A damaged entry is fixed by deleting its directory (or `en cache clear`); envoy copies it again the next time it is needed.

These commands work on the cache directory alone, so they are fast and need no stack.

## Cleaning up

A bundle that nothing has used for 30 days (or `ENVOY_CACHE_MAX_IDLE_DAYS`) is removed. When a size limit is set, the least recently used bundles are removed until the cache fits.

Envoy does this itself: at most once a day it prunes in the background while your command runs (skip it with `--no-prune`), and it never removes a bundle that a running envoy is using. There is nothing to schedule. `en cache prune` runs the same cleanup on demand.

## Bundles that are not cached

A bundle runs from the share instead of the cache when:

- **It has no `.bundle` marker.** The marker is what gives a bundle its identity and version. Bundles copied to the share by hand, without publishing them through `engit publish bundle`, have none.
- **Its marker states no usable version.** A version must be made of letters, digits, `.`, `_`, `+`, and `-`.
- **A path inside it is too long for Windows.** The longest path under the cache must stay within 250 characters. Envoy warns once, remembers the answer, and runs the bundle from the share without checking again; the note is forgotten when the cache is cleared or the bundle goes unused for the idle period. Pick a shorter cache directory with `ENVOY_CACHE_ROOT`, or publish the bundle with shorter paths. The default `C:\Users\<you>\.envoy\cache` leaves room for the largest bundles in a typical studio, but a much longer directory, such as one under `%TEMP%`, may not.
- **There is not enough free space** on the drive holding the cache.
- **The cache directory cannot be used**, for example because it is read-only.

`envoy --diagnose` shows where the cache is, whether it is on, what turned it on or off, how much it holds, and beside each bundle whether it is `[cached]`, `[not cached yet]`, `[refused: ...]` or `[never cached: ...]` and why.

## Several users or processes

Any number of envoys on one machine can share a cache directory safely: filling, reading, and pruning coordinate through lock files, and the operating system releases a lock if its holder dies.

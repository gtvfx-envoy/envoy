# Team Administration

This page is for the person who looks after envoy for a team: choosing which version each person runs, publishing new versions, and keeping the [launcher](launcher.md) up to date. Everything is plain files on the team's share, and nothing needs a server.

## What lives on the share

The team's config folder is the one `ENVOY_CONFIG_PROD` points at on every machine.

```text
<ENVOY_CONFIG_PROD>/
  redirect.yaml                 who runs which envoy version
  <team>/redirect.yaml          optional: a file for one team (ENVOY_TEAM=<team>)
  shim/
    current.json                the launcher version the team runs
    install-envoy.ps1           the installers people run (copies of the ones below)
    install-envoy.sh
    uninstall-envoy.ps1
    uninstall-envoy.sh
    v0.8.0/                     one folder per published launcher version
      windows-x86_64/           ... and one per platform inside it
      linux-x86_64-musl/
      macos-aarch64/
      macos-x86_64/
```

Envoy itself is published wherever `sources` in `redirect.yaml` say, one folder per version. The studio's bundle location is a natural place:

```text
<bundle location>/envoy/<version>/
  bin/envoy.exe                 (bin/envoy on Linux and macOS)
  py/ ...                       the rest of an unpacked release
```

## The redirect file

`redirect.yaml` says which envoy version each person runs. Comments are welcome.

```yaml
# Which envoy version each person runs.
schema: 1

# Where a published version lives. The first source that has <source>/bin/envoy wins.
# ${VAR} is replaced from the environment; {version} is the name of the pinned version.
sources:
  - ${ENVOY_STUDIO_BNDLS}/envoy/{version}
  - ${ENVOY_STUDIO_BNDLS}/gt/envoy/{version}

# group -> the exact name of a published version.
versions:
  default: v0.7.0
  techart: v0.8.0

# user -> group. Anyone who is not listed gets "default".
users:
  alice: techart
```

| Field | Meaning |
|---|---|
| `schema` | Required. The layout of the file; currently `1`. A launcher that is older than the file says so and keeps using its saved copy |
| `sources` | Required. Where to look for a version, best first. Each source must contain `{version}`. `${VAR}` is replaced from the environment (a source that uses a variable that is not set is skipped, and the error says why), and `/` becomes `\` on Windows |
| `versions` | Required. Group name to the exact name of a published version. A group called `default` is required |
| `users` | Optional. User name to group. Matching ignores case and any domain, so `CORP\Alice` and `alice@corp.example` both match `alice` |

A version name is the name of its folder: letters, digits, `.`, `_`, `+` and `-`, at most 64 characters. Names that could point outside a source (`..`, a slash) are refused. A name is used exactly as written, so `v0.8.0` and `0.8.0` are different versions.

Anyone who is not listed gets the `default` version. To try a new version with a few people, add a group for it, put those people in it, and leave `default` alone. To move everyone, change `default`.

## Checking a redirect file

Check a file before it goes live:

```console
envoy --envoy-shim-validate-config redirect.yaml --check-sources
```

It reports every problem at once and exits `78` if there is an error:

- a missing `schema`, `sources` or `default` version;
- a source without `{version}`;
- a version name that is not safe;
- a user in a group that has no version, or listed twice;
- keys it does not know (at run time these are only a warning, which `--envoy-shim-resolve` shows).

`--check-sources` also looks for every version the file pins on the sources and warns about any that is not there, which catches the usual mistake of pointing the team at a version nobody has published yet.

## Publishing a change

Write the new file next to the live one and rename it into place, so that nobody ever reads half a file:

```powershell
Copy-Item .\redirect.yaml "$env:ENVOY_CONFIG_PROD\redirect.yaml.tmp"
Move-Item "$env:ENVOY_CONFIG_PROD\redirect.yaml.tmp" "$env:ENVOY_CONFIG_PROD\redirect.yaml" -Force
```

The file is read each time envoy starts, so a change takes effect at everyone's next launch, and moving someone back is the same edit in reverse. If a file is ever unreadable or invalid, launchers fall back to the last good copy they saved. Do not use symbolic links: they behave differently on Windows clients.

## Publishing an envoy version

Copy the release's version folder (`gt/envoy/<version>` from the release archive) to a folder named for the version on one of the `sources`. Do this before the version appears in the redirect file; `--check-sources` confirms it.

A published version is never changed afterwards: publish a new version instead. People who already cached a version keep running their copy.

## Publishing the launcher

Unpack the launcher archive for each platform into its own folder under `shim/<version>/`, copy the installers up to `shim/`, and only then write the pointer, so nobody is ever pointed at a launcher that is not there yet:

```powershell
$shim = "$env:ENVOY_CONFIG_PROD\shim"
$target = "$shim\v0.8.0\windows-x86_64"
New-Item -ItemType Directory -Force $target | Out-Null
Expand-Archive .\envoy-shim-v0.8.0-windows-x86_64.zip -DestinationPath $target
Copy-Item "$target\install-envoy.ps1", "$target\uninstall-envoy.ps1" $shim -Force

'{"schema": 1, "version": "v0.8.0"}' | Set-Content "$shim\current.json.tmp"
Move-Item "$shim\current.json.tmp" "$shim\current.json" -Force
```

On Linux and macOS, unpack the `.tar.gz` into `shim/<version>/<platform>/` with `tar -xzf <archive> -C <folder>` and copy `install-envoy.sh` and `uninstall-envoy.sh` up.

From then on, installed launchers notice the new `current.json` within a day and replace themselves (see [Updating the launcher](launcher.md#updating-the-launcher)). New machines are installed from `shim/install-envoy.ps1`, which uses whatever `current.json` names. To build the archives yourself, run `python scripts/package_shim.py --version v0.8.0 --target-name windows-x86_64 --binary-dir rust/target/release --windows` after `cargo build --release -p envoy-shim`.

## Teams

A team that needs its own pins sets `ENVOY_TEAM=<team>` on its machines. The launcher then looks for `<ENVOY_CONFIG_PROD>/<team>/redirect.yaml` first and falls back to the studio-wide `redirect.yaml` when the team has none. The launcher pointer in `shim/` is shared by everyone.

## Emergencies

- **One person is stuck on a bad version.** Put them in a group pinned to a good one, or have them run `ENVOY_VERSION=<version> envoy ...`, which ignores the redirect file.
- **A version has to be pulled.** Change `default` (and any group that uses it) in the redirect file. People who cached the bad version stop running it at their next launch, because the launcher asks the file each time.
- **The share is down.** Nothing needs doing: launchers run from the redirect file they saved and the version they cached. Machines that never ran the pinned version cannot start it until the share is back, and say so.

## Shared and farm machines

Install the launcher for everyone with `install-envoy.ps1 -Scope Machine` from an elevated prompt. All users of the machine then share one launcher, and each keeps their own cache of envoy versions and bundles under their own `~/.envoy`. A machine-wide launcher does not update itself; run the installer again to update it.

## Who can change what

The launcher runs whatever the files on the share name, as the person who started it. Anyone who can write `redirect.yaml`, `shim/` or the version folders can therefore decide what runs on every machine. Give write access to those locations to the people who administer envoy, and read access to everyone else.

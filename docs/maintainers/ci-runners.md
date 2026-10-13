# CI Runners

A **runner** is the machine that executes a workflow job. This page lists which
runners the three repositories use, what each self-hosted runner must provide,
and how to add or replace one without breaking a release.

## How jobs find a runner

Every job declares `runs-on`. For GitHub-hosted runners that is a single name:

```yaml
runs-on: ubuntu-24.04
```

For self-hosted runners it is a list of labels, and the job is routed only to a
runner that has **every** label in the list:

```yaml
runs-on: [self-hosted, Linux, X64, debian-13, ci]
```

If no online runner carries all the labels, the job waits in the queue with
"Waiting for a runner to pick up this job". A typo in one label, or a runner that
was renamed or removed, looks exactly like this.

## Runner inventory

| Runner | `runs-on` | Used for |
|---|---|---|
| Self-hosted Linux (`gha-linux-01`) | `[self-hosted, Linux, X64, debian-13, ci]` | Envoy lint and tests, plus docs builds and deploys for all three repositories. **No Docker.** |
| Self-hosted Windows (`WINLAB`) | `[self-hosted, X64, Windows, windows-11, ci]` | Windows tests, the Envoy Windows release build, and the Despatch Windows jobs (tests, Prepare Release, release build, impact validation). |
| GitHub-hosted Ubuntu | `ubuntu-24.04` | Tag Release (all three repositories), Prepare Release for Envoy and Envoy Utils, `verify-version`, `publish`, `notify-downstream`, the Envoy Linux release build, and Envoy Utils' lint, Linux release build, and impact checks. |
| GitHub-hosted macOS | `macos-15`, `macos-15-intel` | macOS release builds and macOS PR tests. |
| GitHub-hosted Windows | `windows-2025` | Envoy Utils Windows lint and release build, and the downstream compatibility checks. |

Despatch's Linux and macOS builds use the `ubuntu-latest` and `macos-latest`
aliases instead of pinned versions. Both self-hosted runners are in the
`trusted-ci` runner group. To see which runner served a particular job:

```powershell
gh api repos/gtvfx-envoy/envoy/actions/runs/<run-id>/jobs --jq '.jobs[] | {name, runner_name, labels, conclusion}'
```

## Why the Linux release build is GitHub-hosted

The Envoy Linux release job runs `cross` and a manylinux2014 container build, so
it needs a **container engine** (Docker). The self-hosted Linux runner does not
provide one. When the job was routed there, it failed with:

```text
Error: no container engine found
Suggestion: is docker or podman installed?
```

and the ownership-restoring step failed with `docker: command not found`.
GitHub-hosted `ubuntu-24.04` runners include Docker, start from a clean machine
on every run, and are the same runners Envoy Utils already uses for its
identical `cross` build. See [Build and Release Chain](build-chain.md#linux-x86_64)
for what the job does.

Use this as the rule of thumb: **a job may only be routed to a runner that has
passed a validation build of that exact job.** Passing lint on a runner says
nothing about whether it can run the release jobs.

## What a self-hosted runner must provide

Work from the job list, not from guesses. These are the requirements the current
workflows impose.

### Every self-hosted runner

- The runner service is online and registered with the labels above.
- Outbound HTTPS to GitHub, crates.io, PyPI, and the Rust distribution servers.
- `actions/setup-python` can provide Python 3.11 (3.11.9 for the docs and
  runner-validation jobs).
- A Rust toolchain can be installed by `dtolnay/rust-toolchain` (that is, the
  machine can run `rustup`).

### Linux (`debian-13`)

- A C toolchain: Cargo links through `cc`.
- The `envoy-py` Rust tests need `LIBRARY_PATH` pointing at the Python tool-cache
  `lib` directory. `lint.yml` sets it and explains why: the self-hosted Python
  build's recorded library path points at a GitHub-hosted location that does not
  exist here.
- Docker is **not** provided. Do not route a job that needs containers here
  unless you first follow [Making the Linux runner Docker-capable](#making-the-linux-runner-docker-capable).

### Windows (`windows-11`)

- PowerShell 7 (`pwsh`). Many steps declare `shell: pwsh`.
- Git for Windows with Git Bash at `C:\Program Files\Git\bin\bash.exe`. The Envoy
  release job fails fast with "Git Bash is required by the Rust toolchain action"
  if it is missing.
- Visual Studio Build Tools (the MSVC linker). `lint.yml` sets up the developer
  command environment explicitly so Git's own `link.exe` cannot shadow the real
  linker.
- For the Despatch jobs: the bundle location named by the `ENVOY_STUDIO_BNDLS`
  Actions variable must exist and be readable on the runner. Despatch's build
  Stack references its bundles through that variable, so the variable must be
  set (the Stack fails to load outright otherwise) and the location it names must
  contain the bundles the Stack pins. It can be a directory on the runner or a
  network share. The workflows carry a fallback value; set the variable at
  repository or organization level when you replace the runner.

## Making the Linux runner Docker-capable

Only do this if you decide to run a container-based job on the self-hosted Linux
runner. Moving the Envoy Linux release job back to it, for example, requires all
of the following.

1. Install Docker Engine. On Debian: `sudo apt-get install -y docker.io`.
2. Add the runner's operating-system user to the `docker` group:
   `sudo usermod -aG docker <runner-user>`.
3. Restart the runner service. Group membership is read when the service
   starts, so skipping this gives `permission denied` on the Docker socket. If
   the runner is installed as a service, run `sudo ./svc.sh stop` and then
   `sudo ./svc.sh start` from the runner's directory.
4. Check as the runner user: `sudo -u <runner-user> docker run --rm hello-world`.
5. Make sure the machine can reach `quay.io` (the manylinux image) and `ghcr.io`
   (the `cross` images), and that the `file` command is installed (the release
   job uses it to confirm the binary is static).
6. Run a [validation build](build-chain.md#validation-builds) from a branch that
   points the job at the runner. Do not merge the label change until it passes.

!!! warning "The docker group is root-equivalent"
    Anyone who can run a container can read and write the whole machine. Only
    grant it on a runner dedicated to trusted CI, and review the
    [security notes](#security-notes) below.

## Adding or replacing a runner

1. **Register it** in a runner group limited to the repositories that need it
   (**Settings > Actions > Runners**). Give it labels that describe the operating
   system, the version, and the purpose, such as `debian-13` and `ci`.
2. **Install what the jobs need.** Use the list above and the steps of the jobs
   that will run on it.
3. **Validate the runner**, in two parts:
    - Run the manual `pve04 runner validation` workflow
      (`.github/workflows/pve04-runner-validation.yml`). It checks Python lint and
      the Rust format, Clippy, and test commands. It does **not** exercise the
      release jobs, and it asserts the current Linux runner's name, so copy and
      adjust it for a different runner.
    - Run a [validation build](build-chain.md#validation-builds) of every release
      workflow that will route to the runner, from a branch that already points
      at it. For Despatch, also run **Prepare Release**.
4. **Pin the labels** in the workflows with a pull request, only after step 3
   passes.
5. **Keep the old runner registered** until a validation build has passed on the
   new one. Removing the old runner first leaves release jobs queued or
   misrouted, and you find out at tag time.

## Checking runner health

```powershell
gh api orgs/gtvfx-envoy/actions/runners --jq '.runners[] | {name, status, busy, labels: [.labels[].name]}'
```

This needs organization admin access. The same list is under **Settings >
Actions > Runners**. A runner whose `status` is `offline` leaves its jobs queued.

## Security notes

These repositories are public and the self-hosted runners execute whatever the
workflow tells them to.

- Require approval before workflows from outside collaborators run
  (**Settings > Actions > General > Fork pull request workflows from outside
  collaborators**), so untrusted code cannot start on our machines without a
  maintainer's say-so.
- Keep release triggers on trusted events only: tag pushes and manual dispatch.
- Treat membership of the `docker` group as root access (see above), and avoid
  leaving credentials in persistent workspaces.
- An Actions policy (**Settings > Actions > Policies**) can limit who may trigger
  workflows. If you use one, it must also allow the release App, or every run the
  App starts (the tag build, the downstream impact workflows, and the checks on
  Prepare Release pull requests) fails immediately with `startup_failure`. Allow
  the App's *installation* (actor type `IntegrationInstallation`), not a broader
  group. See
  [Release Troubleshooting](release-troubleshooting.md#runs-that-never-start-startup_failure).

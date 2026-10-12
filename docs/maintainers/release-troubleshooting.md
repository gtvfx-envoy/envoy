# Release Troubleshooting

This page helps you diagnose a failed release run and recover without making
things worse. It is organised as: rules to follow, how to find out what failed,
how to decide between a re-run and a new release, a table of known symptoms, and
a worked example.

## Rules that prevent most problems

1. **Create release tags only with the Tag Release workflow.** A tag pushed by
   hand skips its safety checks.
2. **Never create a GitHub Release by hand.** The **Build & Release** workflow
   creates it after every platform has built. A release made earlier is public
   immediately, shows as *Latest*, and has no assets.
3. **Never move, delete, or reuse a tag** to retry a release. Fix forward with
   the next patch version.
4. **Re-run only failures that are outside the repository** (see
   [Re-run or fix forward](#re-run-or-fix-forward)).
5. **Verify every release** after it publishes: check its assets, then check that
   the downstream workflows started.
6. **Rehearse after changes.** Run a
   [validation build](build-chain.md#validation-builds) after any workflow or
   runner change.

## First five minutes: find out what failed

```powershell
# 1. Find the run.
gh run list --repo gtvfx-envoy/envoy --workflow "Build & Release" --limit 5

# 2. See which jobs and steps failed.
gh run view <run-id> --repo gtvfx-envoy/envoy

# 3. Read only the failing logs.
gh run view <run-id> --repo gtvfx-envoy/envoy --log-failed

# 4. See which runner each job used.
gh api repos/gtvfx-envoy/envoy/actions/runs/<run-id>/jobs --jq '.jobs[] | {name, runner_name, labels, conclusion}'

# 5. Check what, if anything, was published.
gh release view vX.Y.Z --repo gtvfx-envoy/envoy --json assets,isDraft,publishedAt
gh api repos/gtvfx-envoy/envoy/releases/latest --jq .tag_name
```

!!! tip "Read the first failed step"
    Some steps are marked `if: always()` so they run even after a failure. The
    Linux job's "Restore manylinux build ownership" step is one. When the real
    failure is earlier, these steps fail too and add noise. Start from the
    earliest red step.

Do not stop at the first problem you find. Also run step 5 and check the version
the tag points at. A failed run often hides a second, more important issue (see
the [worked example](#worked-example-the-failed-v070-attempt)).

## Re-run or fix forward

**Re-run failed jobs** (the button on the run page, or
`gh run rerun <run-id> --failed`) executes the failed jobs again on the *same
commit*, using the *workflow files from that commit*. Jobs that already passed
are kept.

| The cause is... | Examples | Action |
|---|---|---|
| **Outside the repository** | Software missing on a runner. A runner that was offline. A network or package-registry outage. A missing secret or variable. | Fix the cause, then re-run the failed jobs. The tag and commit do not change. |
| **Inside the tagged commit** | A workflow bug. A compile or test failure. A version that was never bumped. Switching a job to a different runner. | Do **not** move the tag. Merge the fix, then release the next patch version through Prepare Release and Tag Release. |

The rule of thumb: if fixing it means changing a file in the repository, fix
forward. A re-run cannot pick up a workflow fix merged after the tag, and moving
a tag changes what consumers already fetched. The Envoy Utils lockfile records
the exact commit behind the Envoy tag it pins.

After a successful re-run, verify the release as described in
[Build and Release Chain](build-chain.md#release-artifacts) and confirm the
downstream workflows started.

## Symptoms and fixes

### Envoy Build & Release

| Symptom | Likely cause | What to do |
|---|---|---|
| `Error: no container engine found`, or `docker: command not found` | The Linux release job ran on a runner without Docker. | The job must run on a GitHub-hosted `ubuntu-24.04` runner, or the runner must be made Docker-capable ([CI Runners](ci-runners.md#making-the-linux-runner-docker-capable)). A workflow change means fixing forward. |
| `verify-version` fails with `Expected Envoy X, but the workspace is Y`, or `Invalid semantic version` | The tag does not match `rust/Cargo.toml`: Prepare Release was skipped or not merged, or the tag was created by hand. | No build ran and nothing was published. Run Prepare Release for the next version, merge it, then use Tag Release. |
| `verify-version` fails with `Cargo workspace version X disagrees with Cargo.lock` | The lockfile was not refreshed for the new version. | Use Prepare Release, which refreshes it. Do not edit the lockfile by hand. |
| A job waits with "Waiting for a runner to pick up this job" | No online runner has every label in `runs-on`. | See [Checking runner health](ci-runners.md#checking-runner-health) and compare the labels. |
| A platform job fails in a build or test step | A real code or toolchain problem at the tagged commit. | Fix forward. |
| `publish` fails with an unmatched-files error | An expected artifact is missing. Platform upload steps use `if-no-files-found: error`, so look at them first. | Find the platform step that produced nothing. Re-run only if the cause is outside the repository. |
| A release page exists but has no assets | It was created by hand, or something published it before the build finished. | See [Cleaning up a failed release](#cleaning-up-a-failed-release). |

### Prepare Release and Tag Release

| Symptom | Likely cause | What to do |
|---|---|---|
| `Create release automation token` fails | The App secrets (`RELEASE_AUTOMATION_APP_ID`, `RELEASE_AUTOMATION_PRIVATE_KEY`) are missing, are not shared with this repository, or the App is not installed on it. | Check the organization secrets' repository access and the App's installation, then re-run. |
| `Tag vX.Y.Z already exists.` or `Release vX.Y.Z already exists.` | The version was already used. | Choose the next version. Do not delete the tag to reuse it. |
| `Lint has not succeeded for <sha>` | `lint.yml` has not passed on the current head of `main`. | Wait for or fix lint on that exact commit, then re-run Tag Release. |
| `Expected Envoy X, but the workspace is Y` | The Prepare Release pull request is not merged. | Merge it, then re-run Tag Release. |

### Downstream repositories

| Symptom | Likely cause | What to do |
|---|---|---|
| Envoy Utils Prepare Release fails with `candidate versions found which didn't match` | The Envoy tag's crate version is not the tag's version, so the exact pin cannot resolve. The Envoy release is mis-versioned. | Do not repin to it. Publish a corrected Envoy patch release first. |
| Despatch fails with `Expected one Envoy wheel ... found 0` (or `Expected one 'envoy-vX...' asset ... found 0`) | The Envoy release it needs is missing assets, for example a release made by hand. | Check the release with `gh release view`. Fix Envoy first; Despatch consumes only complete releases. |
| Downstream workflows did not start after an Envoy release | `notify-downstream` was skipped (`RELEASE_AUTOMATION_ENABLED` is not `true`, or `publish` did not run) or failed to mint a token. | Fix the cause, then replay with the downstream **Envoy Release Impact** workflow (command below). |
| **Envoy Release Impact** fails in `Validate release and current pin` | The Envoy tag has no GitHub Release, or the commit at the tag differs from the event's `envoy_commit`. | Confirm the Envoy release exists and is complete, then replay. |
| Despatch Prepare Release or a Windows job fails while loading its build Stack | The Windows runner cannot reach the studio bundle share named by `ENVOY_STUDIO_BNDLS`. | See [CI Runners](ci-runners.md#windows-windows-11). |
| A Despatch release has only some of its three assets | Each platform job uploads its own asset, and one job failed. | Re-run the failed jobs if the cause is outside the repository; otherwise fix forward. |

Replay an Envoy release against a downstream repository (use the unprefixed
version):

```powershell
gh workflow run "Envoy Release Impact" --repo gtvfx-envoy/envoy_utils -f envoy_version=X.Y.Z
gh workflow run "Envoy Release Impact" --repo gtvfx-envoy/despatch -f envoy_version=X.Y.Z
```

## Cleaning up a failed release

**A release page with no assets.** Anything that installs "the latest release"
will pick it up and fail. Delete the release page but keep the tag:

```powershell
gh release delete vX.Y.Z --repo gtvfx-envoy/envoy --yes
gh api repos/gtvfx-envoy/envoy/releases/latest --jq .tag_name
```

The second command should now show the previous good version. Leave out
`--cleanup-tag` so the tag stays. Its version number is burned, so release the
next patch version. The annotated tag's message still holds the notes.

**A partial Despatch release.** Either re-run the failed jobs (outside-the-repo
causes) or delete the incomplete release and fix forward.

**Workflow artifacts.** Artifacts from a failed run stay downloadable for the
repository's retention period. They are useful for inspecting what a platform
produced, and they are not release assets.

## Worked example: the failed v0.7.0 attempt

Envoy `v0.7.0` was tagged and its release page created by hand. **Build &
Release** then failed in the Linux job with `no container engine found`.
Investigating turned up three problems, and only the first was in the log:

1. **The Linux job ran on a runner without Docker.** Earlier releases had built
   on a different runner that did have it. The new runner had been validated for
   lint and Cargo checks, but never for the release job.
2. **The tag pointed at the wrong version.** `rust/Cargo.toml` still said
   `0.6.2`: Prepare Release had not been run, and Tag Release, which would have
   refused the tag, was bypassed. Pinning Envoy Utils to that tag fails with
   `candidate versions found which didn't match: 0.6.2`.
3. **A public, empty release existed.** The hand-made release was already
   published and marked *Latest*, with no assets. Despatch's installer would
   have failed on it.

Simply fixing the runner and re-running would have published `v0.7.0` archives
built from a commit whose crate version was still `0.6.2`, and Envoy Utils could
not have pinned them. The recovery path was:

- delete the empty release page, keeping the tag;
- move the Linux release job to a GitHub-hosted runner and add the
  `verify-version` guard, so neither failure can recur silently;
- release the next patch version through Prepare Release and Tag Release,
  instead of re-running or moving the tag.

What to take from it:

- Read past the first failure. Check the tag's version and the release state, not
  only the log.
- Hand-made tags and releases bypass every guard.
- Validate a runner against the *release* jobs before routing them to it.

# Release Troubleshooting

This page helps you diagnose a failed release run and recover without making
things worse. It is organised as: rules to follow, how to find out what failed,
how to decide between a re-run and a new release, a table of known symptoms, what
to do about runs that never start and `required` results, how to clean up, and
two worked examples.

## Rules that prevent most problems

1. **Create release tags only with the Tag Release workflow.** A tag pushed by
   hand skips its safety checks.
2. **Never create a GitHub Release by hand.** The **Build & Release** workflow
   creates it after every platform has built. A release made earlier is public
   immediately, shows as *Latest*, and has no assets.
3. **Do not move, delete, or reuse a tag** to retry a release. Fix forward with
   the next patch version. The one narrow exception is re-pushing the
   *identical* tag object after a tag push that never started a run (see
   [Runs that never start](#runs-that-never-start-startup_failure)).
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

A run that ends in `startup_failure` has no jobs, so there is nothing to re-run:
GitHub answers "This workflow run cannot be retried". See
[Runs that never start](#runs-that-never-start-startup_failure).

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
| The run on the tag shows `startup_failure`, zero jobs, and "This run likely failed because of a workflow file issue" | GitHub refused to start the run. If the same workflow file already passed a validation build, the file is fine, so look at who started the run. A bot actor, with runs started by people working, points at an Actions policy. | See [Runs that never start](#runs-that-never-start-startup_failure). |

### Prepare Release and Tag Release

| Symptom | Likely cause | What to do |
|---|---|---|
| `Create release automation token` fails | The App secrets (`RELEASE_AUTOMATION_APP_ID`, `RELEASE_AUTOMATION_PRIVATE_KEY`) are missing, are not shared with this repository, or the App is not installed on it. | Check the organization secrets' repository access and the App's installation, then re-run. |
| `Tag vX.Y.Z already exists.` or `Release vX.Y.Z already exists.` | The version was already used. | Choose the next version. Do not delete the tag to reuse it. |
| `Lint has not succeeded for <sha>` | `lint.yml` has not passed on the current head of `main`. | Wait for or fix lint on that exact commit, then re-run Tag Release. |
| `Expected Envoy X, but the workspace is Y`, `Expected Envoy Utils X, found Y`, or `Expected Despatch X, found Y` | The Prepare Release pull request is not merged yet, or you entered another project's version, or you ran Tag Release in the wrong repository. | Check which repository the run belongs to and compare your inputs with [Which repository, and which version?](releasing.md#which-repository-and-which-version). Merge the pull request if needed, then run Tag Release again. |
| Tag Release succeeds, but no **Build & Release** run appears or it shows `startup_failure` | The run is started by the release App, and an Actions policy may not allow it. | See [Runs that never start](#runs-that-never-start-startup_failure). Do not start Build & Release by hand; its manual button only makes a validation build. |
| The `Lint` check on a Prepare Release pull request never starts (`startup_failure`) | The same cause: the App created the pull request, so the App started the run. | See [Runs that never start](#runs-that-never-start-startup_failure). |

### Downstream repositories

| Symptom | Likely cause | What to do |
|---|---|---|
| Envoy Utils Prepare Release fails with `candidate versions found which didn't match` | The Envoy tag's crate version is not the tag's version, so the exact pin cannot resolve. The Envoy release is mis-versioned. | Do not repin to it. Publish a corrected Envoy patch release first. |
| Despatch fails with `Expected one Envoy wheel ... found 0` (or `Expected one 'envoy-vX...' asset ... found 0`) | The Envoy release it needs is missing assets, for example a release made by hand. | Check the release with `gh release view`. Fix Envoy first; Despatch consumes only complete releases. |
| Downstream workflows did not start after an Envoy release | `notify-downstream` was skipped (`RELEASE_AUTOMATION_ENABLED` is not `true`, or `publish` did not run) or failed to mint a token. | Fix the cause, then replay with the downstream **Envoy Release Impact** workflow (command below). |
| **Envoy Release Impact** fails in `Validate release and current pin` | The Envoy tag has no GitHub Release, or the commit at the tag differs from the event's `envoy_commit`. | Confirm the Envoy release exists and is complete, then replay. |
| Both downstream **Envoy Release Impact** runs show `startup_failure` right after an Envoy release | The App sent the events, and an Actions policy does not allow the App. | Fix the policy, then send the events again (see [Runs that never start](#runs-that-never-start-startup_failure)). |
| **Envoy Release Impact** reports `release-impact:required` with exit code 101 | Cargo failed. The saved result holds only the exit code, so the cause is not visible in the issue. | See [A `required` result: check the cause](#a-required-result-check-the-cause). |
| Despatch Prepare Release or a Windows job fails while loading its build Stack | The bundle location named by the `ENVOY_STUDIO_BNDLS` Actions variable is missing or unreadable on the Windows runner. | See [CI Runners](ci-runners.md#windows-windows-11). |
| A Despatch release has only some of its three assets | Each platform job uploads its own asset, and one job failed. | Re-run the failed jobs if the cause is outside the repository; otherwise fix forward. |

Replay an Envoy release against a downstream repository (use the unprefixed
version):

```powershell
gh workflow run "Envoy Release Impact" --repo gtvfx-envoy/envoy_utils -f envoy_version=X.Y.Z
gh workflow run "Envoy Release Impact" --repo gtvfx-envoy/despatch -f envoy_version=X.Y.Z
```

## Runs that never start (`startup_failure`)

A run that ends in `startup_failure` was rejected by GitHub before any job
existed. It has no jobs and no log, the run page says "This run likely failed
because of a workflow file issue", and it cannot be re-run. A broken workflow
file is one possible cause. In this release chain the more likely cause is an
**Actions policy** that does not allow the actor who started the run.

### Diagnose it

1. **Check the workflow file.** If a
   [validation build](build-chain.md#validation-builds) of the same workflow
   revision ran jobs, a basic syntax or registration problem is unlikely. It
   does not rule out errors in event-specific expressions or job paths, so
   confirm the actor pattern in the next step.
2. **Look at who started the run.** Compare a run that failed with one that
   worked:

    ```powershell
    gh api repos/gtvfx-envoy/envoy/actions/runs/<run-id> --jq '{event, conclusion, actor: .actor.login, type: .actor.type}'
    ```

    The telltale pattern is that runs started by people work, runs started by the
    release App's bot account (`envoy-release-automation[bot]`, type `Bot`) fail,
    and *unrelated* workflows started by the same event fail too. That points at
    the actor, not at any one workflow file.
3. **List the policies that apply.**

    ```powershell
    gh api -H "X-GitHub-Api-Version: 2026-03-10" /orgs/gtvfx-envoy/actions/policies
    gh api -H "X-GitHub-Api-Version: 2026-03-10" /orgs/gtvfx-envoy/actions/policies/<policy-id>
    ```

    Look for a `restrict_actions_actors` rule. Only the actors in its
    `allowed_actors` list can start the workflows the policy targets.

### Fix the policy

Add the release App's *installation* to the policy's allowed actors.

1. Find the installation ID:

    ```powershell
    gh api orgs/gtvfx-envoy/installations --jq '.installations[] | {id, app_slug}'
    ```

2. In **Settings > Actions > Policies**, edit the policy and add the App as an
   allowed actor. Through the REST API the entry is
   `{"id": <installation-id>, "type": "IntegrationInstallation"}`.

!!! warning "Keep the existing actors"
    Updating a policy through the API replaces its whole list of allowed actors.
    Include every actor that is already allowed, or you will stop people from
    running workflows. Save the policy's current JSON before you change it.

!!! note "Use the installation, not the App"
    In this organization an entry of type `App` (with the App's ID) did not match
    the App's runs, while type `IntegrationInstallation` (with the installation
    ID) did. If the App is ever uninstalled and reinstalled, its installation ID
    changes and the policy needs updating again.

Runs that were already rejected stay rejected, so test the change with a cheap
run that the App starts before you rely on it. Re-sending the downstream events
(below) is one.

### Start the rejected work again

| What was rejected | How to start it again |
|---|---|
| The tag build | Re-push the identical tag object (below). |
| The downstream **Envoy Release Impact** runs | Re-run the Envoy `notify-downstream` job (commands below the table). It sends the events again, and each downstream issue updates instead of duplicating. You can also replay by hand with the command under [Downstream repositories](#downstream-repositories). |
| `Lint` on a Prepare Release pull request | Close and reopen the pull request. The check that Tag Release needs is the `push` run on the merged commit, which whoever merges starts. |

```powershell
# Find the job ID, then re-run just that job. It re-sends the events.
gh api repos/gtvfx-envoy/envoy/actions/runs/<run-id>/jobs --jq '.jobs[] | select(.name=="Notify downstream repositories") | .id'
gh run rerun <run-id> --repo gtvfx-envoy/envoy --job <job-id>
```

#### Re-push the identical tag object

Use this only when **all** of these are true: the tag build is a
`startup_failure` with zero jobs, no GitHub Release exists for the tag, and
nothing has pinned the tag. You push back the *same* tag object, so the tag still
names the same commit and keeps the tagger that Tag Release recorded.

```powershell
# 1. Fetch the exact tag object and note its SHA.
git fetch origin refs/tags/vX.Y.Z:refs/tags/vX.Y.Z
git rev-parse refs/tags/vX.Y.Z

# 2. Delete the remote tag, then push the same object back. The new push event
#    starts the build.
git push origin :refs/tags/vX.Y.Z
git push origin refs/tags/vX.Y.Z

# 3. Confirm the remote tag has the same SHA as in step 1.
git ls-remote origin "refs/tags/vX.Y.Z*"
```

You start this run yourself, so it works even before the policy is fixed. Watch
the new run, then verify the release as usual. Never use this to change what a
tag points at, or after a build has published a release; fix forward instead.

## A `required` result: check the cause

An Envoy Release Impact issue labelled `release-impact:required` means a
compatibility check **failed**. It does not say why. Envoy Utils saves only the
process exit code (101 is Cargo's code for any failed build or test), while
Despatch saves only per-step outcomes. Neither captures the error text, so read
the failing step's log before you change code.

1. Open the validation run linked from the issue.
2. For Envoy Utils, open the failing **Compatibility** job (Linux or Windows) and
   its **Test against candidate Envoy Core** step, and find the first `error:`.
   For Despatch, open the **Validate source and frozen executable** job and find
   the first failing step.
3. Decide what kind of failure it is:

    | The log shows... | It is probably... | Then... |
    |---|---|---|
    | Compile errors that name Envoy Core items, or failing downstream tests | A real incompatibility | Update the downstream source, then prepare its release. |
    | Missing files or paths, network errors, or toolchain errors | A problem in the test setup, not in the code | Fix the setup, then replay the assessment (command under [Downstream repositories](#downstream-repositories)). |

4. To reproduce an Envoy Utils failure on your machine, point its workspace at
   your Envoy checkout at the candidate tag and run its tests:

    ```powershell
    engit dev link rust <path-to-envoy-checkout>
    cargo test --workspace      # run from the Envoy Utils rust/ directory
    engit dev unlink rust
    ```

    Never commit a workspace that is still dev-linked.

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
not have pinned them. The recovery was:

- delete the empty release page;
- move the Linux release job to a GitHub-hosted runner and add the
  `verify-version` guard, so neither failure can recur silently;
- release again through Prepare Release and Tag Release instead of re-running.

In that case the maintainers deleted the bad tag and reused `0.7.0`. That was
safe only because the tag had no release and nothing had pinned it. The default
remains to release the next patch version.

What to take from it:

- Read past the first failure. Check the tag's version and the release state, not
  only the log.
- Hand-made tags and releases bypass every guard.
- Validate a runner against the *release* jobs before routing them to it.

## Worked example: a tag build that never started

Tag Release succeeded and the new tag appeared, but nothing built. The
**Build & Release** run on the tag had ended in `startup_failure` with zero jobs,
and the cause was outside every workflow file:

1. **The actor, not the file.** The release App's bot account had started the
   run. It had also started the failed `Lint` run on the Prepare Release pull
   request and an unrelated Copilot setup workflow triggered by the same tag
   push. Runs started by a person were fine, and the same workflow file had just
   passed a validation build.
2. **An Actions policy.** An organization policy allowed only one team to trigger
   workflows, and the App was not on its list. The policy was newer than the last
   time the App had worked, so nobody had noticed.
3. **The first fix did nothing.** Adding an `App` entry left the downstream
   impact runs, also started by the App, still failing. An
   `IntegrationInstallation` entry worked.

Recovery: allow the App's installation in the policy, re-push the identical tag
object to start the tag build (a `startup_failure` run cannot be re-run), and
re-run Envoy's `notify-downstream` job so the downstream assessments, which had
also been rejected, ran. The build then passed and published normally.

What to take from it:

- Zero jobs plus a bot actor means an Actions policy, not a broken workflow.
- After changing a policy, test it with a cheap run started by the same actor
  before you rely on it.
- Re-check the policies whenever you add one. Anything that pushes tags, opens
  pull requests, or sends events uses the App.

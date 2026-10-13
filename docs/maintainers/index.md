# Maintainer Guide

This section explains how Envoy, Envoy Utils, and Despatch are **built, tested,
and released**. It is written for engineers who maintain the three repositories
and their CI. It is kept separate from the user documentation on purpose:
nothing here is needed to install or use Envoy.

!!! note "Looking for user documentation?"
    Start from the [home page](../index.md). Installation, core concepts, and
    the CLI reference live there.

## What is in this section

| Page | Read it when you want to... |
|---|---|
| [Build and Release Chain](build-chain.md) | Understand what runs, in what order, across all three repositories. |
| [Releasing](releasing.md) | Cut a release by following the step-by-step procedure. |
| [CI Runners](ci-runners.md) | See where each job runs, what a runner must provide, and how to add or replace one. |
| [Release Troubleshooting](release-troubleshooting.md) | Diagnose a failed release run and recover without making things worse. |

A good reading order before your first release is *Build and Release Chain*,
then *Releasing*. Keep *Release Troubleshooting* open while the release runs.

## Assumed background

These pages assume you:

- can read a GitHub Actions workflow file (`.github/workflows/*.yml`) and tell a
  workflow from a job from a step;
- use Git comfortably, including branches and annotated tags;
- have the [GitHub CLI](https://cli.github.com/) (`gh`) installed and signed in,
  with write access to the three repositories;
- can open a failed workflow log and identify which step failed.

You do not need to know Rust or Python internals to run a release.

## Glossary

| Term | Meaning here |
|---|---|
| Workflow, job, step | A workflow is one YAML file under `.github/workflows/`. It contains jobs, which run in parallel unless one declares `needs:` on another. A job is a list of steps that run in order on one machine. |
| Runner | The machine that executes a job. **GitHub-hosted** runners (for example `ubuntu-24.04`) are fresh virtual machines with a fixed set of preinstalled tools. **Self-hosted** runners are machines we operate; they usually persist between jobs and only have the tools we install on them. |
| Label | A tag on a runner. A runner is eligible only if it has every label listed in the job's `runs-on`; it may have additional labels. |
| Tag, annotated tag | A Git ref that names a commit. Release tags are annotated (they carry an author and a message) and look like `v0.7.1`. |
| GitHub Release | The page, and the downloadable files (assets), that GitHub attaches to a tag. Users and downstream repositories install from these assets. |
| SemVer | Semantic versioning: `MAJOR.MINOR.PATCH` with an optional `-prerelease` suffix. Workflow inputs are unprefixed (`0.7.1`); tags add a `v` (`v0.7.1`). |
| Wheel | A built Python package (`.whl`). Envoy's wheels contain a compiled Rust extension. |
| Static (musl) binary | A Linux executable with no shared-library dependencies, so one file runs on any distribution. |
| manylinux wheel | A Linux wheel built against an old glibc inside a standard container, so it installs on older distributions. |
| `cross` | A tool that runs `cargo` inside a prepared Docker container. Envoy uses it to produce the static Linux binary. |
| GitHub App token | A short-lived token minted from the release automation GitHub App (`envoy-release-automation`). Events created with it (for example pushing a tag) start other workflows; events created with the default `GITHUB_TOKEN` do not. The runs it starts are attributed to the App's bot account, `envoy-release-automation[bot]`. |
| `repository_dispatch` | A custom event one repository sends to another. Envoy sends `envoy-released` to Envoy Utils and Despatch after it publishes. |
| Actions policy | An organization or repository rule (**Settings > Actions > Policies**) that limits who may trigger workflows and which events may start them. A run started by an actor the policy does not allow is rejected immediately. |
| `startup_failure` | A run conclusion meaning GitHub rejected the run before any job existed. There are no jobs and no log, and the run cannot be re-run. See [Release Troubleshooting](release-troubleshooting.md#runs-that-never-start-startup_failure). |
| Validation build | A manual run of **Build & Release** that builds everything but never publishes. The *Version label* it asks for is only a name used in artifact file names (default `dev`). |

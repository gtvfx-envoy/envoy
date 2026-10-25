# Envoy Rust Workspace

The Rust workspace provides platform-agnostic native CLIs and the compiled
Python API for Windows, Linux, and macOS.

## Crates

| Crate | Kind | Purpose |
|---|---|---|
| `envoy-core` | Library | Discovery, environments, commands, execution, Stacks, and config |
| `envoy-cli` | Binary/library | Native `envoy` CLI and shared Python CLI dispatch |
| `envoy-py` | PyO3 extension | The installable `envoy` Python package |

The retired Python implementations of the CLIs have been fully replaced.
`envoy-py` retains the public Python API and delegates CLI behavior to the same
Rust implementation used by the native `envoy` binary.

## Versioning

CLI `--version` output and `envoy.__version__` are the version `Cargo.toml`
declares, with a leading `v`. A build from the commit tagged with that version
reports exactly it; any other build adds the commit it came from (`+g8ec805c`).
Source archives without Git metadata report the declared version alone. The
wheel's metadata carries the same version, written for Python. See
[Versions, pre-releases, and what a build reports](../docs/maintainers/releasing.md#versions-pre-releases-and-what-a-build-reports).

## Prerequisites

- Rust stable
- Python 3.10 or newer
- `maturin` for building the Python extension
- A native C linker: MSVC Build Tools on Windows, Xcode Command Line Tools on
  macOS, or the normal compiler toolchain on Linux

For a development environment:

```console
python -m venv .venv
python -m pip install maturin pytest
```

Activate the virtual environment using the command appropriate for the current shell.

## Building

From the repository root, use the same canonical driver on every platform:

```console
python scripts/build_native.py
```

The Windows-local `scripts/build_native.bat` retains the established workflow:
it checks the Windows prerequisites, builds the native release executable,
builds the wheel when `maturin` is available, and installs the local extension
with `maturin develop`. The POSIX `scripts/build_native.sh` forwards to the
portable Python driver. Useful Python-driver options are:

```console
python scripts/build_native.py --skip-wheel
python scripts/build_native.py --develop
python scripts/build_native.py --debug
python scripts/build_native.py --skip-wheel --target x86_64-unknown-linux-musl
```

The checkout launchers in `bin/` prefer release builds, then debug builds, and
may fall back to `python3 -m envoy`.

## Testing and Linting

```console
cd rust
cargo fmt --check
cargo clippy --workspace --exclude envoy-py --all-targets -- -D warnings
cargo test --workspace --exclude envoy-py
cargo test -p envoy-py --lib
cd ..
python -m pytest rust/envoy-py/tests -v
```

The Python tests must run against a wheel or extension built for the current
platform. CI installs the wheel before running the contract and consumer smoke suites.

## Supported Build Targets

| Artifact | Targets |
|---|---|
| Native `envoy` | Windows x64, Linux x64 musl, macOS x64, macOS arm64 |
| `envoy` wheel | Windows x64, manylinux2014 x64, macOS x64, macOS arm64 |

The Ubuntu and Windows self-hosted runners provide primary CI. GitHub-hosted
`macos-15-intel` and `macos-15` runners build and test the two macOS
architectures. Release archives are checksummed and currently unsigned;
signing and notarization can be added without changing their layout.

The self-hosted Windows service account needs Visual Studio C++ x64 Build
Tools and Git for Windows. The self-hosted Ubuntu service account needs access
to a running Docker daemon because the pinned `cross` tool builds and tests the
musl target in a container. Both runners need outbound access for toolchain,
Python package, and GitHub Action downloads.

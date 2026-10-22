# Installation

Envoy releases support Windows, Linux, and macOS. Download the archive or
wheel matching the operating system and CPU from the latest
[GitHub Release](https://github.com/gtvfx-envoy/envoy/releases).

## Full Bundle (Recommended)

| Platform | Archive |
|---|---|
| Windows x64 | `envoy-v<version>-windows-x86_64.zip` |
| Linux x64 | `envoy-v<version>-linux-x86_64-musl.tar.gz` |
| macOS Intel | `envoy-v<version>-macos-x86_64.tar.gz` |
| macOS Apple silicon | `envoy-v<version>-macos-aarch64.tar.gz` |

Each archive contains `gt/envoy/<version>/bin`. Add that directory to `PATH`.
The archive also carries the Python API in `py/`. The `envoy` in that
directory adds `py/` to the end of `PYTHONPATH` for every command it launches,
and sets `ENVOY_PYTHON_PATH` to it, so `import envoy` works without installing
the wheel and without an envoy bundle in the stack. A copy of the API that the
stack provides comes first and wins. Set `ENVOY_PYTHON_API=off` to leave
`PYTHONPATH` alone. A build that is not laid out as a release, such as a
developer build, adds nothing.

=== "Windows PowerShell"

    ```powershell
    Expand-Archive envoy-v1.0.0-windows-x86_64.zip -DestinationPath C:\tools
    $env:PATH = "C:\tools\gt\envoy\v1.0.0\bin;$env:PATH"
    envoy --version
    ```

=== "Linux"

    ```bash
    tar -xzf envoy-v1.0.0-linux-x86_64-musl.tar.gz -C "$HOME/.local"
    export PATH="$HOME/.local/gt/envoy/v1.0.0/bin:$PATH"
    envoy --version
    ```

=== "macOS"

    ```bash
    tar -xzf envoy-v1.0.0-macos-aarch64.tar.gz -C "$HOME/.local"
    export PATH="$HOME/.local/gt/envoy/v1.0.0/bin:$PATH"
    envoy --version
    ```

The Linux native binaries use musl and do not require a particular glibc
version. Features that integrate with other tools still require those tools,
such as `git`, `gh`, a shell, or `xdg-open`.

## Python API

Download the wheel whose filename matches the current Python platform, then install it:

```console
python -m pip install ./envoy-<version>-<python-and-platform-tags>.whl
```

The wheel uses Python's stable ABI and supports CPython 3.10 or newer. Linux
wheels follow the manylinux2014 compatibility contract; they are separate
from the musl-native standalone CLI archive.

## Verify Checksums

Every release contains `SHA256SUMS`.

=== "Windows PowerShell"

    ```powershell
    Get-FileHash .\envoy-v1.0.0-windows-x86_64.zip -Algorithm SHA256
    ```

=== "Linux"

    ```bash
    sha256sum -c SHA256SUMS --ignore-missing
    ```

=== "macOS"

    ```bash
    shasum -a 256 -c SHA256SUMS
    ```

## Unsigned Artifact Notice

Release artifacts are currently unsigned while signing and Apple
notarization infrastructure is being established.

- Windows may display a Microsoft Defender SmartScreen unknown-publisher warning.
- macOS may quarantine the archive. After verifying `SHA256SUMS`, remove the
  quarantine attribute from the extracted version directory if Gatekeeper blocks it:

  ```bash
  xattr -dr com.apple.quarantine "$HOME/.local/gt/envoy/v1.0.0"
  ```

Signing hooks are reserved in the release workflow and can be enabled later
without changing archive names or layout.

## Developer Build

Requirements are Rust, Python 3.10 or newer, and `maturin` for the Python wheel.

=== "Windows"

    ```powershell
    git clone https://github.com/gtvfx-envoy/envoy.git envoy
    cd envoy
    python -m pip install maturin
    .\scripts\build_native.bat
    .\bin\envoy.bat --version
    ```

=== "Linux/macOS"

    ```bash
    git clone https://github.com/gtvfx-envoy/envoy.git envoy
    cd envoy
    python3 -m pip install maturin
    ./scripts/build_native.sh
    ./bin/envoy --version
    ```

When using `scripts/build_native.py`, pass `--skip-wheel` for a native-only
build or `--develop` to install the compiled Python extension into the active
environment. The Windows batch workflow builds the native executables and
wheel, then performs the development install by default.

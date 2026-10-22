@echo off
REM Main entry point for envoy CLI
REM Usage: envoy [command] [args...]
REM        envoy --list
REM        envoy --info <command>


REM Local dev build: native Rust binary built via `cargo build --release`
REM (or the debug profile) from rust/envoy-cli, before a dist/ copy exists.
REM Each exit sits on its own line outside any parenthesized block: inside a
REM block "%errorlevel%" expands before the command runs and always reports 0.
if exist "%~dp0..\rust\target\release\envoy.exe" goto release
if exist "%~dp0..\rust\target\debug\envoy.exe" goto debug

REM Fall back to `python -m envoy` (pip-installed `envoy` package, built
REM from rust/envoy-py via maturin -- see pyproject.toml).
python -m envoy %*
exit /b %errorlevel%

:release
"%~dp0..\rust\target\release\envoy.exe" %*
exit /b %errorlevel%

:debug
"%~dp0..\rust\target\debug\envoy.exe" %*
exit /b %errorlevel%

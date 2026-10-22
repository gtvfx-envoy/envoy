@echo off

@REM Short alias for envoy. A release bundle ships envoy.exe next to this file;
@REM a source checkout has envoy.bat instead, which finds the development build.
@REM Keep each "exit /b %errorlevel%" on its own line outside any parenthesized
@REM block: inside a block it expands before the command runs and always reports 0.
if not exist "%~dp0envoy.exe" goto checkout
"%~dp0envoy.exe" %*
exit /b %errorlevel%

:checkout
call "%~dp0envoy.bat" %*
exit /b %errorlevel%

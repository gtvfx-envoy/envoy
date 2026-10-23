<#
.SYNOPSIS
Removes the envoy launcher installed by install-envoy.ps1.

.DESCRIPTION
Takes the same -Scope, -NoPath, -RemoveCache and -Force options as install-envoy.ps1
and runs it with -Uninstall.
#>
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'install-envoy.ps1') -Uninstall @args

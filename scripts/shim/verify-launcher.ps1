<#
.SYNOPSIS
Checks that the envoy launcher works on this machine, end to end.

.DESCRIPTION
Run it after installing the launcher (install-envoy.ps1). It starts envoy a few
times, so the first run may copy the pinned version from the share. It writes
nothing to the share; everything it changes is under ~\.envoy, as normal use does.

Each check prints PASS, FAIL or SKIP with a note. The exit code is 1 if any check
failed, so the script can run from a scheduled task or a deployment tool.

.PARAMETER Stack
The stack to run commands in. Pass an empty string to use none.

.PARAMETER ExpectVersion
The envoy version the redirect file should give this user. Checked when given.

.PARAMETER PythonCommand
A command that starts Python in the stack.

.EXAMPLE
.\verify-launcher.ps1 -Stack studio -ExpectVersion v0.8.0
#>
[CmdletBinding()]
param(
    [string]$Stack = 'studio',
    [string]$ExpectVersion,
    [string]$PythonCommand = 'python'
)

$ErrorActionPreference = 'Stop'
$script:failures = 0

function Quote([string]$Argument) {
    if ($Argument -notmatch '[\s"]') { return $Argument }
    return '"' + ($Argument -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

# Runs `envoy` the way a user would (found on PATH) with extra environment variables, and captures what it did.
function Invoke-Envoy([string[]]$Arguments, [hashtable]$Environment = @{}) {
    $program = (Get-Command envoy -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $program
    $info.Arguments = ($Arguments | ForEach-Object { Quote $_ }) -join ' '
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($name in $Environment.Keys) {
        if ($null -eq $Environment[$name]) { [void]$info.EnvironmentVariables.Remove($name) }
        else { $info.EnvironmentVariables[$name] = [string]$Environment[$name] }
    }
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::Start($info)
    $out = $process.StandardOutput.ReadToEndAsync()
    $err = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    [pscustomobject]@{
        Code = $process.ExitCode
        Out  = $out.Result.Trim()
        Err  = $err.Result.Trim()
        Ms   = $watch.ElapsedMilliseconds
    }
}

function Report([string]$Name, [string]$Result, [string]$Note) {
    $colour = @{ PASS = 'Green'; FAIL = 'Red'; SKIP = 'Yellow' }[$Result]
    Write-Host ('{0,-5} {1}' -f $Result, $Name) -ForegroundColor $colour -NoNewline
    if ($Note) { Write-Host "  ($Note)" } else { Write-Host '' }
    if ($Result -eq 'FAIL') { $script:failures++ }
}

function Check([string]$Name, [scriptblock]$Body) {
    try {
        $outcome = & $Body
        Report $Name $outcome.Result $outcome.Note
    }
    catch {
        Report $Name 'FAIL' $_.Exception.Message
    }
}

function Pass([string]$Note) { [pscustomobject]@{ Result = 'PASS'; Note = $Note } }
function Fail([string]$Note) { [pscustomobject]@{ Result = 'FAIL'; Note = $Note } }
function Skip([string]$Note) { [pscustomobject]@{ Result = 'SKIP'; Note = $Note } }

$stackArguments = @()
if ($Stack) { $stackArguments = @("-s=$Stack") }
$root = if ($env:ENVOY_CONFIG_ROOT) { $env:ENVOY_CONFIG_ROOT } else { Join-Path $env:USERPROFILE '.envoy' }
$pinned = $null

Check 'envoy on PATH is the launcher' {
    $first = Get-Command envoy -All -CommandType Application | Select-Object -First 1
    $expected = Join-Path $root 'bin'
    if (-not $first) { return Fail 'envoy is not on PATH; open a new terminal after installing' }
    if ((Split-Path $first.Source) -ine $expected) { return Fail "the first envoy on PATH is $($first.Source), not the launcher in $expected" }
    $run = Invoke-Envoy @('--envoy-shim-version')
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    Pass $run.Out
}

Check 'en runs the same launcher' {
    $en = Get-Command en -All -CommandType Application | Select-Object -First 1
    if (-not $en) { return Fail 'en is not on PATH' }
    $said = (& $en.Source --envoy-shim-version) -join ''
    if ($LASTEXITCODE -ne 0) { return Fail "exit $LASTEXITCODE" }
    $own = (Invoke-Envoy @('--envoy-shim-version')).Out
    if ($said -ne $own) { return Fail "en says '$said' but envoy says '$own'" }
    Pass $said
}

Check 'the redirect file names a version for this user' {
    $run = Invoke-Envoy @('--envoy-shim-resolve')
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    if ($run.Out -notmatch '(?m)^version:\s+(\S+)') { return Fail "no version in the answer:`n$($run.Out)" }
    $script:pinned = $Matches[1]
    if ($ExpectVersion -and $script:pinned -ne $ExpectVersion) { return Fail "expected $ExpectVersion but the file gives $($script:pinned)" }
    Pass $script:pinned
}

Check 'envoy starts (the first run may copy it)' {
    $run = Invoke-Envoy @('--version')
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    Pass "$($run.Out) in $($run.Ms) ms"
}

Check 'a second start is quick and copies nothing' {
    $run = Invoke-Envoy @('--version')
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    if ($run.Err -match 'caching') { return Fail "it copied something again: $($run.Err)" }
    if ($run.Ms -gt 2000) { return Fail "$($run.Ms) ms" }
    Pass "$($run.Ms) ms"
}

Check 'exit codes pass through' {
    $run = Invoke-Envoy ($stackArguments + @($PythonCommand, '-c', 'import sys; sys.exit(7)'))
    if ($run.Code -ne 7) { return Fail "expected 7, got $($run.Code): $($run.Err)" }
    Pass 'a command that exits 7 makes envoy exit 7'
}

Check 'import envoy works inside a command' {
    $resolved = (Invoke-Envoy @('--envoy-shim-resolve')).Out
    $app = if ($resolved -match '(?m)^app:\s+cached at (.+?) \(') { $Matches[1] } else { $null }
    if (-not $app) { return Skip 'the pinned version is not cached, so its Python API cannot be found' }
    if (-not (Test-Path (Join-Path $app 'py\envoy'))) { return Skip "envoy $($script:pinned) ships no Python API" }
    $run = Invoke-Envoy ($stackArguments + @($PythonCommand, '-c', 'import envoy; print(envoy.__file__)'))
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    Pass $run.Out
}

Check 'the cache commands work' {
    $run = Invoke-Envoy @('cache', 'status')
    if ($run.Code -ne 0 -and $run.Err -match "not found") { return Skip "envoy $($script:pinned) predates the cache commands" }
    if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
    Pass ($run.Out -split "`n" | Select-Object -First 1)
}

Check 'it still starts when the share is slow or unreachable' {
    # A 1 ms limit makes reading the redirect file on the share give up at once, as an outage does.
    foreach ($attempt in 1..5) {
        $run = Invoke-Envoy @('--version') @{ ENVOY_SHIM_TIMEOUT_MS = '1' }
        if ($run.Err -match 'using the copy saved') {
            if ($run.Code -ne 0) { return Fail "exit $($run.Code): $($run.Err)" }
            return Pass 'it fell back to the saved redirect file'
        }
    }
    Skip 'the redirect file was read too fast to simulate an outage'
}

Check 'ENVOY_VERSION is checked' {
    $run = Invoke-Envoy @('--version') @{ ENVOY_VERSION = '../x' }
    if ($run.Code -ne 78) { return Fail "expected 78, got $($run.Code)" }
    Pass 'an unsafe version is refused with exit 78'
}

Check 'the launcher can look for a newer launcher' {
    $run = Invoke-Envoy @('--envoy-shim-update')
    if ($run.Code -eq 0) { return Pass $run.Out }
    if ($run.Err -match 'cannot read') { return Skip 'no launcher has been published on the share yet (shim\current.json)' }
    Fail "exit $($run.Code): $($run.Err)"
}

Write-Host ''
if ($script:failures -gt 0) {
    Write-Host "$($script:failures) check(s) failed." -ForegroundColor Red
    exit 1
}
Write-Host 'All checks passed.' -ForegroundColor Green
exit 0

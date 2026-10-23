<#
.SYNOPSIS
Installs the envoy launcher for the current user, or for the whole machine.

.DESCRIPTION
Copies the launcher to <root>\bin as envoy.exe and en.exe and puts that folder at
the front of PATH. The launcher reads the team's redirect file, copies the envoy
version it pins to the local cache the first time it is needed, and runs it.

Run this from an unpacked release of the launcher, or from the shim folder on the
share (the one that holds current.json).

.PARAMETER Source
Folder holding envoy.exe, or the shim folder that holds current.json. Defaults to
the folder this script is in.

.PARAMETER Scope
User (the default) installs under %USERPROFILE%\.envoy and changes the user's PATH.
Machine installs under %ProgramFiles%\envoy and changes the machine PATH; it needs an
elevated prompt and suits shared or farm machines. Only administrators can write there,
which matters: a folder on the machine PATH that ordinary users could write to would let
them run their own programs as anyone who runs a command by name. The installer refuses a
folder that anyone else can change.

.PARAMETER NoPath
Leave PATH alone.

.PARAMETER Uninstall
Remove the launcher and its PATH entry instead of installing.

.PARAMETER RemoveCache
With -Uninstall, also delete the local cache of envoy versions and bundles.

.PARAMETER Force
Do not ask before deleting the cache.

.EXAMPLE
.\install-envoy.ps1

.EXAMPLE
\\server\studio\envoy\config\shim\install-envoy.ps1
#>
[CmdletBinding()]
param(
    [string]$Source,
    [ValidateSet('User', 'Machine')]
    [string]$Scope = 'User',
    [switch]$NoPath,
    [switch]$Uninstall,
    [switch]$RemoveCache,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
# Set here, not as the parameter's default: Windows PowerShell 5.1 leaves $PSScriptRoot empty in a
# default when the script is started with -File.
if (-not $Source) { $Source = $PSScriptRoot }
$Target = 'windows-x86_64'
$LauncherNames = @('envoy.exe', 'en.exe')

function Get-InstallRoot {
    if ($Scope -eq 'Machine') {
        # The 64-bit folder, also from a 32-bit PowerShell. Unlike %ProgramData%, only administrators can write to it.
        $programFiles = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
        if (-not $programFiles) { throw 'ProgramFiles is not set.' }
        return Join-Path $programFiles 'envoy'
    }
    if ($env:ENVOY_CONFIG_ROOT) { return $env:ENVOY_CONFIG_ROOT }
    if (-not $env:USERPROFILE) { throw 'USERPROFILE is not set.' }
    return Join-Path $env:USERPROFILE '.envoy'
}

# Throws when anyone but SYSTEM, administrators and the installer service could change what is in $Folder.
function Assert-OnlyAdministratorsCanWrite([string]$Folder) {
    $rights = [System.Security.AccessControl.FileSystemRights]
    $writing = $rights::WriteData -bor $rights::AppendData -bor $rights::Delete -bor
        $rights::DeleteSubdirectoriesAndFiles -bor $rights::ChangePermissions -bor $rights::TakeOwnership
    $trusted = @(
        'S-1-5-18',      # SYSTEM
        'S-1-5-32-544',  # Administrators
        'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'  # TrustedInstaller
    )
    $inheritOnly = [System.Security.AccessControl.PropagationFlags]::InheritOnly

    $acl = Get-Acl -LiteralPath $Folder
    $others = @()
    foreach ($rule in $acl.Access) {
        if ($rule.AccessControlType -ne 'Allow') { continue }
        if (($rule.FileSystemRights -band $writing) -eq 0) { continue }
        if (($rule.PropagationFlags -band $inheritOnly) -ne 0) { continue }  # only for what is created inside
        try { $sid = $rule.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value }
        catch { $sid = [string]$rule.IdentityReference }
        if ($trusted -notcontains $sid) { $others += [string]$rule.IdentityReference }
    }

    $owner = try { $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value } catch { '' }
    $currentUser = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    if ($owner -and $trusted -notcontains $owner -and $owner -ne $currentUser) { $others += "the owner ($owner)" }

    if ($others.Count -gt 0) {
        throw "$Folder can be changed by $(($others | Select-Object -Unique) -join ', '). Anything they put there would run for administrators, so the launcher is not installed on the machine PATH. Remove the folder, or fix its permissions, and run this again."
    }
}

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'The Machine scope needs an elevated prompt: run PowerShell as administrator.'
    }
}

function Find-Launcher([string]$Directory) {
    $direct = Join-Path $Directory 'envoy.exe'
    if (Test-Path -LiteralPath $direct -PathType Leaf) { return $direct }

    $pointer = Join-Path $Directory 'current.json'
    if (Test-Path -LiteralPath $pointer -PathType Leaf) {
        $version = [string](Get-Content -LiteralPath $pointer -Raw | ConvertFrom-Json).version
        if ($version -notmatch '^[A-Za-z0-9][A-Za-z0-9._+-]*$') {
            throw "$pointer does not name a usable version (it has '$version')."
        }
        $published = Join-Path $Directory (Join-Path $version (Join-Path $Target 'envoy.exe'))
        if (-not (Test-Path -LiteralPath $published -PathType Leaf)) {
            throw "$pointer names version $version, but $published does not exist."
        }
        return $published
    }

    throw "Found neither envoy.exe nor current.json in $Directory. Pass -Source with the folder that holds the launcher."
}

# Replaces $To with $From, also when $To is running: Windows lets a running program be renamed but not overwritten.
function Install-File([string]$From, [string]$To) {
    $incoming = "$To.new"
    $previous = "$To.old"
    try {
        Copy-Item -LiteralPath $From -Destination $incoming -Force
        Remove-Item -LiteralPath $previous -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $To) {
            Move-Item -LiteralPath $To -Destination $previous -Force
        }
        Move-Item -LiteralPath $incoming -Destination $To -Force
    }
    catch {
        if ((Test-Path -LiteralPath $previous) -and -not (Test-Path -LiteralPath $To)) {
            Move-Item -LiteralPath $previous -Destination $To -Force
        }
        Remove-Item -LiteralPath $incoming -Force -ErrorAction SilentlyContinue
        throw
    }
    Remove-Item -LiteralPath $previous -Force -ErrorAction SilentlyContinue
}

function Remove-File([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    try {
        Remove-Item -LiteralPath $Path -Force
    }
    catch {
        # Still running: move it aside so the name is free; it can be deleted later.
        Move-Item -LiteralPath $Path -Destination "$Path.old" -Force
        Remove-Item -LiteralPath "$Path.old" -Force -ErrorAction SilentlyContinue
    }
}

function Get-EnvironmentKey([string]$Which, [bool]$Writable) {
    if ($Which -eq 'Machine') {
        return [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\Session Manager\Environment', $Writable)
    }
    return [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $Writable)
}

# The stored value keeps %VARIABLES% unexpanded, so rewriting it does not turn other entries into literals.
function Get-StoredPath([string]$Which) {
    $key = Get-EnvironmentKey $Which $false
    try {
        return [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
    finally {
        $key.Close()
    }
}

function Send-EnvironmentChange {
    if (-not ('Envoy.Win32' -as [type])) {
        Add-Type -Namespace Envoy -Name Win32 -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
'@
    }
    $result = [UIntPtr]::Zero
    [void][Envoy.Win32]::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
}

function Set-StoredPath([string]$Which, [string]$Value) {
    $key = Get-EnvironmentKey $Which $true
    try {
        $key.SetValue('Path', $Value, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    }
    finally {
        $key.Close()
    }
    Send-EnvironmentChange
}

function Test-SameFolder([string]$First, [string]$Second) {
    $one = [Environment]::ExpandEnvironmentVariables($First).TrimEnd('\', '/')
    $two = [Environment]::ExpandEnvironmentVariables($Second).TrimEnd('\', '/')
    return $one -ieq $two
}

function Add-ToPath([string]$Which, [string]$Folder) {
    $entries = @((Get-StoredPath $Which) -split ';' | Where-Object { $_ })
    foreach ($entry in $entries) {
        if (Test-SameFolder $entry $Folder) { return $false }
    }
    Set-StoredPath $Which ((@($Folder) + $entries) -join ';')
    return $true
}

function Remove-FromPath([string]$Which, [string]$Folder) {
    $entries = @((Get-StoredPath $Which) -split ';' | Where-Object { $_ })
    $kept = @($entries | Where-Object { -not (Test-SameFolder $_ $Folder) })
    if ($kept.Count -eq $entries.Count) { return $false }
    Set-StoredPath $Which ($kept -join ';')
    return $true
}

# The first envoy a new shell would find, if it is not the one just installed.
function Find-ShadowingEnvoy([string]$Folder) {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    foreach ($entry in (($machine + ';' + $user) -split ';' | Where-Object { $_ })) {
        if (Test-SameFolder $entry $Folder) { return $null }
        foreach ($name in 'envoy.exe', 'envoy.bat', 'envoy.cmd') {
            $candidate = Join-Path ([Environment]::ExpandEnvironmentVariables($entry)) $name
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
        }
    }
    return $null
}

function Install-Launcher {
    $launcher = Find-Launcher $Source
    if ($Scope -eq 'Machine') { Assert-Administrator }
    $bin = Join-Path (Get-InstallRoot) 'bin'
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    if ($Scope -eq 'Machine') {
        # First on the machine PATH, a folder that others can write to would run their programs for administrators.
        Assert-OnlyAdministratorsCanWrite (Split-Path $bin -Parent)
        Assert-OnlyAdministratorsCanWrite $bin
    }

    foreach ($name in $LauncherNames) {
        Install-File $launcher (Join-Path $bin $name)
    }

    $installed = Join-Path $bin 'envoy.exe'
    $version = & $installed --envoy-shim-version
    if ($LASTEXITCODE -ne 0) { throw "The installed launcher does not run: $installed" }
    Write-Host "Installed $version in $bin"

    if (-not $NoPath) {
        if (Add-ToPath $Scope $bin) {
            Write-Host "Added $bin to the $Scope PATH. Open a new terminal to use it."
        }
        else {
            Write-Host "$bin is already on the $Scope PATH."
        }
        if (-not (($env:Path -split ';') | Where-Object { $_ -and (Test-SameFolder $_ $bin) })) {
            $env:Path = "$bin;$env:Path"
        }
        $shadow = Find-ShadowingEnvoy $bin
        if ($shadow) {
            Write-Warning "A new terminal will find $shadow before this launcher. Remove its folder from PATH, or put $bin ahead of it."
        }
    }

    if ($env:ENVOY_CONFIG_PROD -or $env:ENVOY_REDIRECT_CONFIG) {
        Write-Host ''
        & $installed --envoy-shim-resolve
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'The launcher is installed, but it cannot yet work out which envoy version you should run. Fix the problem above (the redirect file may be missing or unreadable), then run: envoy --envoy-shim-resolve'
        }
    }
    else {
        Write-Host ''
        Write-Host "Next: set ENVOY_CONFIG_PROD to your team's config folder (ask your administrator), then run: envoy --envoy-shim-resolve"
    }
}

function Uninstall-Launcher {
    if ($Scope -eq 'Machine') { Assert-Administrator }
    $root = Get-InstallRoot
    $bin = Join-Path $root 'bin'

    foreach ($name in $LauncherNames) {
        Remove-File (Join-Path $bin $name)
        Remove-Item -LiteralPath (Join-Path $bin "$name.new"), (Join-Path $bin "$name.old") -Force -ErrorAction SilentlyContinue
    }
    if ((Test-Path -LiteralPath $bin) -and -not (Get-ChildItem -LiteralPath $bin -Force)) {
        Remove-Item -LiteralPath $bin -Force
    }
    # A machine install owns its folder; a user's folder also holds their cache and settings.
    if ($Scope -eq 'Machine' -and (Test-Path -LiteralPath $root) -and -not (Get-ChildItem -LiteralPath $root -Force)) {
        Remove-Item -LiteralPath $root -Force
    }
    Write-Host "Removed the envoy launcher from $bin"

    if (-not $NoPath) {
        if (Remove-FromPath $Scope $bin) {
            Write-Host "Removed $bin from the $Scope PATH."
        }
    }

    if ($RemoveCache) {
        if ($Scope -eq 'Machine') {
            Write-Host 'A machine-wide install keeps no cache. Each user has their own under ~\.envoy\cache; run this with -RemoveCache as that user.'
        }
        else {
            $cache = Join-Path $root 'cache'
            if (Test-Path -LiteralPath $cache) {
                if (-not $Force) {
                    $answer = Read-Host "Delete the local cache $cache? [y/N]"
                    if ($answer -notmatch '^(y|yes)$') {
                        Write-Host 'Kept the cache.'
                        return
                    }
                }
                Remove-Item -LiteralPath $cache -Recurse -Force
                Write-Host "Deleted $cache"
            }
        }
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    if ($Uninstall) { Uninstall-Launcher } else { Install-Launcher }
    exit 0
}

# Installs h0x-core on Windows, or updates it:
#
#   powershell -c "irm https://github.com/vjk7989/h0x-ADE-core/releases/latest/download/install.ps1 | iex"
#
# Into %LOCALAPPDATA%\Programs\h0x-core (h0x-core.exe, Microsoft's ConPTY
# beside it), added to your PATH. An h0x-core that is already there is
# swapped the planned way (h0x-core update --from): running terminals keep
# running. No admin needed. H0X_CORE_VERSION=0.2.0 picks a version;
# H0X_CORE_RELEASES_URL points elsewhere (tests); H0X_CORE_INSTALL_DIR
# installs elsewhere.
#
# `-File install.ps1 <release zip> [session]` installs from a zip already
# here, checked by whoever put it here, and removes both files after
# (`h0x-core machine add` from another machine does that).

# Its own scope: under `irm | iex` it would otherwise set the caller's variables.
& {
$zip, $session = $args
if ($session) { $env:H0X_CORE_SESSION = $session }
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# throw, not exit: under `irm | iex`, exit would close the person's window.
function Fail($why) { throw "h0x-core: $why" }

$releases = if ($env:H0X_CORE_RELEASES_URL) { $env:H0X_CORE_RELEASES_URL.TrimEnd("/") } else { "https://github.com/vjk7989/h0x-ADE-core/releases" }
$dir = if ($env:H0X_CORE_INSTALL_DIR) { $env:H0X_CORE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\h0x-core" }
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne "AMD64") { Fail "no build for $arch Windows yet (x64 only)" }
$target = "windows-x86_64"
# Windows' own tar opens zips; Git Bash's, often first on PATH, can't.
$tar = Join-Path $env:SystemRoot "System32\tar.exe"
if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue) -or -not (Test-Path $tar)) {
    Fail "needs curl.exe and tar.exe (Windows 10 1803 or newer)"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) "h0x-core-install-$PID"
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
  if ($zip) {
    Copy-Item $zip "$tmp\h0x-core.zip"
  } else {
    $manifest = if ($env:H0X_CORE_VERSION) { "$releases/download/v$($env:H0X_CORE_VERSION)/manifest.json" } else { "$releases/latest/download/latest.json" }
    curl.exe -fsSL $manifest -o "$tmp\manifest.json"
    if ($LASTEXITCODE) { Fail "cannot reach $manifest" }
    $m = Get-Content -Raw "$tmp\manifest.json" | ConvertFrom-Json
    $t = $m.targets.$target
    if (-not $t.url -or -not $t.sha256) { Fail "no build for $target in $manifest" }

    # Fetched exactly as the list gives it (a mirror's signed URL included).
    curl.exe -fsSL $t.url -o "$tmp\h0x-core.zip"
    if ($LASTEXITCODE) { Fail "download failed: $($t.url)" }
    # .NET, not Get-FileHash: that one fails when PowerShell 7 starts 5.1.
    $stream = [IO.File]::OpenRead("$tmp\h0x-core.zip")
    try { $got = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) } finally { $stream.Close() }
    if ($got -ne $t.sha256) { Fail "the download is damaged (checksum mismatch); try again" }
  }
    New-Item -ItemType Directory -Force "$tmp\h0x-core" | Out-Null
    & $tar -xf "$tmp\h0x-core.zip" -C "$tmp\h0x-core"
    $new = "$tmp\h0x-core\h0x-core.exe"
    if ($LASTEXITCODE -or -not (Test-Path "$tmp\h0x-core\conpty\conpty.dll")) { Fail "the download holds no complete h0x-core" }
    $version = (& $new --version) -replace "^h0x-core ", ""
    if ($LASTEXITCODE) { Fail "the downloaded h0x-core does not run on this machine" }

    New-Item -ItemType Directory -Force $dir | Out-Null
    $exe = Join-Path $dir "h0x-core.exe"
    if (Test-Path $exe) {
        # Swapped the planned way, so its terminals keep running.
        & $exe update --from $new | Out-Null
        if ($LASTEXITCODE) { Fail "updating $exe failed (see above)" }
        Write-Host "h0x-core updated to $version."
    } else {
        Copy-Item -Recurse -Force "$tmp\h0x-core\h0x-core.exe", "$tmp\h0x-core\conpty" $dir
        Write-Host "h0x-core $version installed into $dir."
    }
    Copy-Item -Recurse -Force "$tmp\h0x-core\THIRD-PARTY-NOTICES" $dir -ErrorAction SilentlyContinue

    # The raw user PATH, so %VARIABLES% in it stay unexpanded and keep their type.
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $listed = $raw -split ";" | Where-Object { $_.Trim().TrimEnd("\") -ieq $dir.TrimEnd("\") }
    if (-not $listed) {
        $kind = if ($key.GetValueNames() -contains "Path") { $key.GetValueKind("Path") } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $key.SetValue("Path", $(if ($raw) { "$($raw.TrimEnd(';'));$dir" } else { $dir }), $kind)
        # Setting any user variable tells running programs the environment changed.
        [Environment]::SetEnvironmentVariable("H0X_CORE_INSTALLING", "1", "User")
        [Environment]::SetEnvironmentVariable("H0X_CORE_INSTALLING", $null, "User")
        Write-Host "Added $dir to your PATH: new terminals have h0x-core."
    }
    $key.Close()
    if (($env:Path -split ";") -notcontains $dir) { $env:Path += ";$dir" }
    Write-Host "Agents: h0x-core --skill. People: h0x-core --help."
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    if ($zip) { Remove-Item -Force $zip, $PSCommandPath -ErrorAction SilentlyContinue }
}
} @args

# Installs h0x-core on Windows, or updates it:
#
#   powershell -c "irm https://github.com/vjk7989/h0x-ADE-core/releases/latest/download/install.ps1 | iex"
#
# Into %LOCALAPPDATA%\Programs\h0x-core (h0x-core.exe, Microsoft's ConPTY
# beside it), added to your PATH. An h0x-core that is already there is
# swapped the planned way (h0x-core update --from): running terminals keep
# running. The download must carry the release key's signature (its .minisig).
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
# https only, and the download on GitHub unless H0X_CORE_RELEASES_URL (tests) says where.
function Check($url) {
    if ($url -notlike "https://*") { Fail "refusing to download ${url}: not an https link" }
    if (-not $env:H0X_CORE_RELEASES_URL -and ([Uri]$url).Host -notin "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com") {
        Fail "refusing to download ${url}: not a link on GitHub"
    }
}

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
  $archive = "$tmp\h0x-core.zip"
  $signed = $false
  if ($zip) {
    # Named as the release names it, its .minisig (if it came along) beside it.
    $asset = Split-Path -Leaf $zip
    $archive = Join-Path $tmp $asset
    Copy-Item $zip $archive
    if (Test-Path "$zip.minisig") {
      Copy-Item "$zip.minisig" "$archive.minisig"
      $signed = $true
    }
  } else {
    $manifest = if ($env:H0X_CORE_VERSION) { "$releases/download/v$($env:H0X_CORE_VERSION)/manifest.json" } else { "$releases/latest/download/latest.json" }
    Check $manifest
    curl.exe -fsSL --proto "=https" --proto-redir "=https" $manifest -o "$tmp\manifest.json"
    if ($LASTEXITCODE) { Fail "cannot reach $manifest" }
    $m = Get-Content -Raw "$tmp\manifest.json" | ConvertFrom-Json
    $t = $m.targets.$target
    if (-not $t.url -or -not $t.sha256) { Fail "no build for $target in $manifest" }

    # Fetched exactly as the list gives it (a mirror's signed URL included).
    Check $t.url
    # Under the release's own file name: its signature is bound to that name.
    $asset = ($t.url -replace "[?#].*", "") -replace ".*/", ""
    $archive = Join-Path $tmp $asset
    curl.exe -fsSL --proto "=https" --proto-redir "=https" $t.url -o $archive
    if ($LASTEXITCODE) { Fail "download failed: $($t.url)" }
    # .NET, not Get-FileHash: that one fails when PowerShell 7 starts 5.1.
    $stream = [IO.File]::OpenRead($archive)
    try { $got = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) } finally { $stream.Close() }
    if ($got -ne $t.sha256) { Fail "the download is damaged (checksum mismatch); try again" }
    $signed = -not $env:H0X_CORE_RELEASES_URL
    if ($signed) {
        $sigUrl = ($t.url -replace "[?#].*", "") + ".minisig"
        Check $sigUrl
        curl.exe -fsSL --proto "=https" --proto-redir "=https" $sigUrl -o "$archive.minisig"
        if ($LASTEXITCODE) { Fail "no signature for ${asset}: $sigUrl" }
    }
  }
    New-Item -ItemType Directory -Force "$tmp\h0x-core" | Out-Null
    & $tar -xf $archive -C "$tmp\h0x-core"
    $new = "$tmp\h0x-core\h0x-core.exe"
    if ($LASTEXITCODE -or -not (Test-Path "$tmp\h0x-core\conpty\conpty.dll")) { Fail "the download holds no complete h0x-core" }
    $version = (& $new --version) -replace "^h0x-core ", ""
    if ($LASTEXITCODE) { Fail "the downloaded h0x-core does not run on this machine" }
    if ($signed) {
        # The checksum comes from the same place as the file: the signature proves the
        # release key made it. The downloaded h0x-core checks it with its embedded key
        # before anything is installed (it has to be trusted to say so: that first step
        # rests on TLS to GitHub; later updates are checked by the installed h0x-core).
        & $new update --from $archive --check | Out-Null
        if ($LASTEXITCODE) { Fail "$asset is not signed by the h0x-core release key (see above)" }
    }

    New-Item -ItemType Directory -Force $dir | Out-Null
    # Outside the user profile the folder would inherit the drive's ACL (often
    # Authenticated Users: Modify), and anyone could replace the h0x-core on the PATH:
    # only this user, SYSTEM and Administrators keep access (by SID: any language).
    $profileDir = [IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd("\") + "\"
    if (-not ([IO.Path]::GetFullPath($dir).TrimEnd("\") + "\").StartsWith($profileDir, [StringComparison]::OrdinalIgnoreCase)) {
        $me = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        icacls.exe $dir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*${me}:(OI)(CI)F" | Out-Null
        if ($LASTEXITCODE) { Fail "cannot lock the ACL of $dir" }
        Write-Host "Locked ${dir}: only you, SYSTEM and Administrators can change it."
    }
    $exe = Join-Path $dir "h0x-core.exe"
    if (Test-Path $exe) {
        # Swapped the planned way, so its terminals keep running.
        # The signed archive; a core older than this one takes only the bare binary
        # (checked above, so only who swaps changes).
        $from = if ($signed) { $archive } else { $new }
        & $exe update --from $from | Out-Null
        if ($LASTEXITCODE -and $from -ne $new) { & $exe update --from $new | Out-Null }
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
    if ($zip) { Remove-Item -Force $zip, "$zip.minisig", $PSCommandPath -ErrorAction SilentlyContinue }
}
} @args

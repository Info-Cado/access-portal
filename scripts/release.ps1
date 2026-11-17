# Builds, signs and publishes a new version of Access Portal. Installed copies
# pick it up by themselves (see admin-app/src-tauri/src/updates.rs).
#
#   .\scripts\release.ps1 -Version 0.2.1 -Notes "What changed" -TokenFile <Cloudflare API token file>
#
# Needs admin-app/src-tauri/updater.key (the updater's private key, not
# committed; installs only accept updates signed with it) and a token with
# Account > Workers R2 Storage > Edit. Also publishes this computer's RDP
# Wrapper settings, so new installs get settings that work on this Windows build.
#
# Then makes a GitHub Release (tag v<Version> on the pushed HEAD commit) in
# $Repo with the notes, installer and signature, using the `gh` CLI. Commit and
# push the version first; -SkipGitHub leaves GitHub alone.
param(
  [Parameter(Mandatory)] [string] $Version,
  [string] $Notes = "",
  [Parameter(Mandatory)] [string] $TokenFile,
  [string] $Zone = "infocado.in",
  [string] $Bucket = "access-portal-updates",
  [string] $Repo = "Info-Cado/access-portal",
  [switch] $SkipBuild,
  [switch] $SkipGitHub
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root "admin-app"
$tauri = Join-Path $app "src-tauri"
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version looks like 1.2.3." }
$keyFile = Join-Path $tauri "updater.key"
if (-not (Test-Path $keyFile)) { throw "Missing $keyFile (the updater's private key)." }

# 1. The version, everywhere it is written.
function Set-Version($path, $pattern, $replacement) {
  $text = [IO.File]::ReadAllText($path)
  $new = [regex]::new($pattern, 'Multiline').Replace($text, $replacement, 1)
  [IO.File]::WriteAllText($path, $new)
}
Set-Version (Join-Path $tauri "tauri.conf.json") '"version": "[^"]+"' "`"version`": `"$Version`""
Set-Version (Join-Path $tauri "Cargo.toml") '^version = "[^"]+"' "version = `"$Version`""
Set-Version (Join-Path $app "package.json") '"version": "[^"]+"' "`"version`": `"$Version`""

# 2. Build and sign.
if (-not $SkipBuild) {
  $env:TAURI_SIGNING_PRIVATE_KEY = [IO.File]::ReadAllText($keyFile)
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  # Through cmd: Windows PowerShell treats the build's progress on stderr as errors.
  $buildLog = Join-Path $env:TEMP "access-portal-build.log"
  Push-Location $app
  try {
    cmd.exe /c "npx tauri build > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) { Get-Content $buildLog -Tail 30; throw "Build failed (log: $buildLog)." }
  } finally { Pop-Location; Remove-Item env:TAURI_SIGNING_PRIVATE_KEY, env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue }
}
$installer = Get-ChildItem (Join-Path $tauri "target\release\bundle\nsis") -Filter "*_${Version}_x64-setup.exe" | Select-Object -First 1
if (-not $installer) { throw "No installer for $Version was built." }
$sig = [IO.File]::ReadAllText("$($installer.FullName).sig").Trim()
$file = "AccessPortal_${Version}_x64-setup.exe"

# 3. Publish: the installer, then latest.json (installs look at that), then the RDP Wrapper settings.
$token = (Get-Content $TokenFile -Raw).Trim()
$h = @{ Authorization = "Bearer $token" }
$api = "https://api.cloudflare.com/client/v4"
$account = (Invoke-RestMethod "$api/zones?name=$Zone" -Headers $h).result[0].account.id
$objects = "$api/accounts/$account/r2/buckets/$Bucket/objects"
function Put-Object($key, $path, $type) {
  $r = Invoke-RestMethod -Method Put "$objects/$key" -Headers $h -ContentType $type -InFile $path
  if (-not $r.success) { throw "Uploading $key failed." }
}
Put-Object "releases/$file" $installer.FullName "application/octet-stream"
$latest = @{
  version = $Version
  notes = $Notes
  pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
  platforms = @{ "windows-x86_64" = @{ signature = $sig; file = $file } }
} | ConvertTo-Json -Depth 4
$tmp = [IO.Path]::GetTempFileName()
try { [IO.File]::WriteAllText($tmp, $latest); Put-Object "latest.json" $tmp "application/json" } finally { Remove-Item $tmp -Force }
$ini = "C:\Program Files\RDP Wrapper\rdpwrap.ini"
if (Test-Path $ini) { Put-Object "rdpwrap.ini" $ini "text/plain" }
"Published $Version ($([math]::Round($installer.Length / 1MB, 1)) MB). Installs update within 6 hours, when nobody is connected."

# 4. The public GitHub Release, with the installer under the same name as the update service uses.
if (-not $SkipGitHub) {
  $commit = (git -C $root rev-parse HEAD).Trim()
  git -C $root fetch -q origin
  if (-not (git -C $root branch -r --contains $commit)) { throw "Push $commit first: GitHub needs the commit to tag it." }
  $dir = Join-Path $env:TEMP "access-portal-release-$Version"
  New-Item -ItemType Directory -Force $dir | Out-Null
  try {
    Copy-Item $installer.FullName (Join-Path $dir $file)
    Copy-Item "$($installer.FullName).sig" (Join-Path $dir "$file.sig")
    $body = if ($Notes) { "$Notes`n`nInstalled copies update by themselves." } else { "Installed copies update by themselves." }
    gh release create "v$Version" --repo $Repo --target $commit --title "Access Portal $Version" --notes $body (Join-Path $dir $file) (Join-Path $dir "$file.sig")
    if ($LASTEXITCODE -ne 0) { throw "Creating the GitHub Release failed." }
  } finally { Remove-Item $dir -Recurse -Force }
  "GitHub Release: https://github.com/$Repo/releases/tag/v$Version"
}

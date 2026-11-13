# Deploys the sign-up service (worker.js) to Cloudflare and writes the app's
# build settings (admin-app/src-tauri/provision.env) so new builds use it.
#
#   .\provision\deploy.ps1 -TokenFile <file holding a Cloudflare API token> -Zone infocado.in
#
# The token needs: Account > Cloudflare Tunnel > Edit, Account > Workers Scripts
# > Edit, Zone > DNS > Edit and Zone > Zone > Read (for the zone). It is stored
# as a secret of the Worker; nothing secret is written to the repository.
param(
  [Parameter(Mandatory)] [string] $TokenFile,
  [Parameter(Mandatory)] [string] $Zone,
  [string] $WorkerName = "access-portal-provision"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$token = (Get-Content $TokenFile -Raw).Trim()
$h = @{ Authorization = "Bearer $token" }
$api = "https://api.cloudflare.com/client/v4"

$zoneInfo = (Invoke-RestMethod "$api/zones?name=$Zone" -Headers $h).result[0]
if (-not $zoneInfo) { throw "The token can't see the zone $Zone." }
$account = $zoneInfo.account.id

# Keep the existing key when redeploying, so already-built apps keep working.
$envFile = Join-Path $root "admin-app\src-tauri\provision.env"
$key = $null
if (Test-Path $envFile) {
  $line = Get-Content $envFile | Where-Object { $_ -match '^ACCESS_PORTAL_PROVISION_KEY=' }
  if ($line) { $key = ($line -split '=', 2)[1].Trim() }
}
if (-not $key) {
  $bytes = New-Object byte[] 32
  [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
  $key = -join ($bytes | ForEach-Object { '{0:x2}' -f $_ })
}

$meta = @{
  main_module = "worker.js"
  compatibility_date = "2025-01-01"
  bindings = @(
    @{ type = "plain_text"; name = "ACCOUNT_ID"; text = $account },
    @{ type = "plain_text"; name = "ZONE_ID"; text = $zoneInfo.id },
    @{ type = "plain_text"; name = "ZONE_NAME"; text = $Zone },
    @{ type = "secret_text"; name = "CF_API_TOKEN"; text = $token },
    @{ type = "secret_text"; name = "PROVISION_KEY"; text = $key }
  )
} | ConvertTo-Json -Depth 5

# Multipart upload with curl; the metadata (holding the secrets) only ever sits
# in a temporary file that is removed straight after.
$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("ap-deploy-" + [guid]::NewGuid()))
try {
  $metaFile = Join-Path $tmp "metadata.json"; [IO.File]::WriteAllText($metaFile, $meta)
  $hdrFile = Join-Path $tmp "headers.txt"; [IO.File]::WriteAllText($hdrFile, "Authorization: Bearer $token")
  $out = & curl.exe -s -X PUT "$api/accounts/$account/workers/scripts/$WorkerName" -H "@$hdrFile" `
    -F "metadata=@$metaFile;type=application/json" `
    -F "worker.js=@$(Join-Path $PSScriptRoot 'worker.js');type=application/javascript+module"
  $res = $out | ConvertFrom-Json
  if (-not $res.success) { throw "Upload failed: $($res.errors | ConvertTo-Json -Compress)" }
} finally {
  Remove-Item $tmp -Recurse -Force
}

# Serve it on the account's workers.dev subdomain.
Invoke-RestMethod -Method Post "$api/accounts/$account/workers/scripts/$WorkerName/subdomain" -Headers $h `
  -ContentType "application/json" -Body '{"enabled":true,"previews_enabled":false}' | Out-Null
$sub = (Invoke-RestMethod "$api/accounts/$account/workers/subdomain" -Headers $h).result.subdomain
$url = "https://$WorkerName.$sub.workers.dev/v1/provision"

[IO.File]::WriteAllLines($envFile, @(
  "# Written by provision/deploy.ps1. Not committed: holds the sign-up key.",
  "ACCESS_PORTAL_PROVISION_URL=$url",
  "ACCESS_PORTAL_PROVISION_KEY=$key"
))
"Deployed $WorkerName -> $url"
"Wrote $envFile"

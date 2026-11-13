# Deploys the sign-up service (worker.js) to Cloudflare and writes the app's
# build settings (admin-app/src-tauri/provision.env) so new builds use it.
#
#   .\provision\deploy.ps1 -TokenFile <file holding a Cloudflare API token> -Zone infocado.in
#
# The token needs: Account > Cloudflare Tunnel > Edit, Account > Workers Scripts
# > Edit, Account > D1 > Edit, Account > Workers R2 Storage > Edit, Zone > DNS >
# Edit and Zone > Zone > Read (for the zone). It is stored as a secret of the
# Worker; nothing secret is written to the repository.
#
# Also creates (once) the D1 database "access-portal" (install heartbeats, for
# offline alerts) and the R2 bucket "access-portal-updates" (app updates and
# RDP Wrapper settings), and runs the Worker every 5 minutes.
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

# Storage the Worker uses: made once, reused after.
$dbName = "access-portal"
$db = (Invoke-RestMethod "$api/accounts/$account/d1/database?name=$dbName" -Headers $h).result | Where-Object name -eq $dbName | Select-Object -First 1
if (-not $db) {
  $db = (Invoke-RestMethod -Method Post "$api/accounts/$account/d1/database" -Headers $h -ContentType "application/json" -Body (@{ name = $dbName } | ConvertTo-Json)).result
}
$schema = "CREATE TABLE IF NOT EXISTS installs (install_id TEXT PRIMARY KEY, name TEXT, topic TEXT, last_seen INTEGER NOT NULL, down INTEGER NOT NULL DEFAULT 0)"
Invoke-RestMethod -Method Post "$api/accounts/$account/d1/database/$($db.uuid)/query" -Headers $h -ContentType "application/json" -Body (@{ sql = $schema } | ConvertTo-Json) | Out-Null
# Added later: which ntfy server each install's alerts go to.
try {
  Invoke-RestMethod -Method Post "$api/accounts/$account/d1/database/$($db.uuid)/query" -Headers $h -ContentType "application/json" -Body (@{ sql = "ALTER TABLE installs ADD COLUMN server TEXT" } | ConvertTo-Json) | Out-Null
} catch { }  # already there
$bucket = "access-portal-updates"
$buckets = (Invoke-RestMethod "$api/accounts/$account/r2/buckets" -Headers $h).result.buckets
if (-not ($buckets | Where-Object name -eq $bucket)) {
  Invoke-RestMethod -Method Post "$api/accounts/$account/r2/buckets" -Headers $h -ContentType "application/json" -Body (@{ name = $bucket } | ConvertTo-Json) | Out-Null
}

$meta = @{
  main_module = "worker.js"
  compatibility_date = "2025-01-01"
  bindings = @(
    @{ type = "plain_text"; name = "ACCOUNT_ID"; text = $account },
    @{ type = "plain_text"; name = "ZONE_ID"; text = $zoneInfo.id },
    @{ type = "plain_text"; name = "ZONE_NAME"; text = $Zone },
    @{ type = "secret_text"; name = "CF_API_TOKEN"; text = $token },
    @{ type = "secret_text"; name = "PROVISION_KEY"; text = $key },
    @{ type = "d1"; name = "DB"; id = $db.uuid },
    @{ type = "r2_bucket"; name = "UPDATES"; bucket_name = $bucket }
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

# Check for offline installs every 5 minutes.
Invoke-RestMethod -Method Put "$api/accounts/$account/workers/scripts/$WorkerName/schedules" -Headers $h `
  -ContentType "application/json" -Body '[{"cron":"*/5 * * * *"}]' | Out-Null

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

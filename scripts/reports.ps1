# Problem reports installs sent from Settings -> This computer -> Send report.
#
#   .\scripts\reports.ps1 -TokenFile <Cloudflare API token file>              # the latest 20
#   .\scripts\reports.ps1 -TokenFile <file> -Id 261117-1043-ab12              # one report, in full
#   .\scripts\reports.ps1 -TokenFile <file> -Id 261117-1043-ab12 -Out r.json  # saved to a file
#
# Reads the D1 database the sign-up service keeps them in (see provision/worker.js);
# the token needs Account > D1 > Read (the release/deploy token has it).
param(
  [Parameter(Mandatory)] [string] $TokenFile,
  [string] $Id,
  [string] $Out,
  [int] $Last = 20,
  [string] $Zone = "infocado.in"
)
$ErrorActionPreference = "Stop"
$token = (Get-Content $TokenFile -Raw).Trim()
$h = @{ Authorization = "Bearer $token" }
$api = "https://api.cloudflare.com/client/v4"
$account = (Invoke-RestMethod "$api/zones?name=$Zone" -Headers $h).result[0].account.id
$db = (Invoke-RestMethod "$api/accounts/$account/d1/database?name=access-portal" -Headers $h).result | Where-Object name -eq "access-portal" | Select-Object -First 1
if (-not $db) { throw "No access-portal database: deploy the sign-up service first (provision/deploy.ps1)." }

function Query($sql, $params) {
  $body = @{ sql = $sql; params = @($params) } | ConvertTo-Json -Depth 4
  $r = Invoke-RestMethod -Method Post "$api/accounts/$account/d1/database/$($db.uuid)/query" -Headers $h -ContentType "application/json" -Body $body
  $r.result[0].results
}

if (-not $Id) {
  Query "SELECT id, datetime(created, 'unixepoch') AS sent_utc, name, version, os, failing FROM reports ORDER BY created DESC LIMIT ?" @($Last) |
    Format-Table -AutoSize -Wrap
  return
}

$row = Query "SELECT body FROM reports WHERE id = ?" @($Id) | Select-Object -First 1
if (-not $row) { throw "No report $Id." }
$report = $row.body | ConvertFrom-Json
if ($Out) {
  [IO.File]::WriteAllText($Out, ($report | ConvertTo-Json -Depth 8))
  "Saved $Out"
  return
}
"== $($report.name)  (Access Portal $($report.version), computer $($report.computer))"
"`n== Windows"
$report.system | Format-List | Out-String -Width 200
"== Setup checks"
foreach ($c in @($report.checks)) {
  $mark = if ($c.ok) { "ok  " } else { "FAIL" }
  "$mark $($c.label)`n     $($c.detail)"
  if ($c.error) { "     error: $($c.error)" }
}
"`n== Recent admin actions"
foreach ($a in @($report.activity)) { "$([DateTimeOffset]::FromUnixTimeSeconds($a.at).ToString('yyyy-MM-dd HH:mm')) $($a.detail)" }
"`n== Health"
$report.health | ConvertTo-Json -Depth 4
"`n== End of portal.log"
$report.log

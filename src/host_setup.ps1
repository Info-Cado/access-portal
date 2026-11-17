# Access Portal: checks (and fixes) what this computer needs to serve portal
# sessions. Run by the portal (src/host.rs) as administrator.
#
#   AP_ACTION = check | fix | info
#   AP_FIX    = comma-separated check ids to fix (with AP_ACTION=fix)
#   AP_INI_URLS = extra places to look for RDP Wrapper settings, tried first
#
# check and fix print one JSON line: [{ id, label, ok, detail, fix, restarts }...]
# info prints one JSON object about Windows, Remote Desktop and antivirus (for
# the problem report the app can send).
#
# Keep this file ASCII: Windows PowerShell 5.1 reads it in the ANSI code page.

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$wrapDir = 'C:\Program Files\RDP Wrapper'
# Where RDP Wrapper is downloaded and unpacked. Excluded from Microsoft Defender
# like $wrapDir: Defender removes RDP Wrapper from anywhere else (%TEMP% too).
$wrapStage = Join-Path $env:ProgramData 'AccessPortal\rdpwrap-setup'
$wrapZip = 'https://github.com/stascorp/rdpwrap/releases/download/v1.6.2/RDPWrap-v1.6.2.zip'
$wrapZipSha256 = '35A9481DDBED5177431A9EA4BD09468FE987797D7B1231D64942D17EB54EC269'
$iniSources = @()
if ($env:AP_INI_URLS) { $iniSources += $env:AP_INI_URLS.Split(',') | Where-Object { $_ } }
$iniSources += @(
  'https://raw.githubusercontent.com/asmtron/rdpwrap/master/res/rdpwrap.ini',
  'https://raw.githubusercontent.com/sebaxakerhtc/rdpwrap.ini/master/rdpwrap.ini',
  'https://raw.githubusercontent.com/affinityv/INI-RDPWRAP/master/rdpwrap.ini'
)
$portalGroup = 'Portal RDP Users'
$ts = 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server'
$rdpTcp = "$ts\WinStations\RDP-Tcp"
$policies = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services'

Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class ApLsa {
  [StructLayout(LayoutKind.Sequential)] struct US { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }
  [StructLayout(LayoutKind.Sequential)] struct OA { public int Length; public IntPtr Root; public IntPtr Name; public uint Attributes; public IntPtr Sd; public IntPtr Qos; }
  [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref OA attrs, uint access, out IntPtr h);
  [DllImport("advapi32.dll")] static extern uint LsaAddAccountRights(IntPtr h, byte[] sid, US[] rights, uint count);
  [DllImport("advapi32.dll")] static extern uint LsaEnumerateAccountRights(IntPtr h, byte[] sid, out IntPtr rights, out uint count);
  [DllImport("advapi32.dll")] static extern uint LsaFreeMemory(IntPtr p);
  [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr h);
  [DllImport("slc.dll", CharSet = CharSet.Unicode)] static extern int SLGetWindowsInformationDWORD(string name, out int value);
  public static int Licence(string name) { int v; return SLGetWindowsInformationDWORD(name, out v) == 0 ? v : -1; }
  static IntPtr Open() { var a = new OA(); a.Length = Marshal.SizeOf(typeof(OA)); IntPtr h; uint r = LsaOpenPolicy(IntPtr.Zero, ref a, 0x00F0FFF, out h); if (r != 0) throw new Exception("LsaOpenPolicy " + r); return h; }
  public static bool Has(byte[] sid, string right) {
    IntPtr h = Open(); try {
      IntPtr p; uint n; if (LsaEnumerateAccountRights(h, sid, out p, out n) != 0) return false;
      int size = Marshal.SizeOf(typeof(US)); bool found = false;
      for (int i = 0; i < n; i++) { var u = (US)Marshal.PtrToStructure(new IntPtr(p.ToInt64() + i * size), typeof(US)); if (Marshal.PtrToStringUni(u.Buffer, u.Length / 2) == right) found = true; }
      LsaFreeMemory(p); return found;
    } finally { LsaClose(h); }
  }
  public static void Add(byte[] sid, string right) {
    IntPtr h = Open(); try {
      var u = new US(); u.Buffer = Marshal.StringToHGlobalUni(right); u.Length = (ushort)(right.Length * 2); u.MaximumLength = (ushort)(u.Length + 2);
      uint r = LsaAddAccountRights(h, sid, new US[] { u }, 1); Marshal.FreeHGlobal(u.Buffer);
      if (r != 0) throw new Exception("LsaAddAccountRights " + r);
    } finally { LsaClose(h); }
  }
}
'@

function Test-Admin { ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) }
function TermsrvVersion { $v = (Get-Item "$env:windir\System32\termsrv.dll").VersionInfo; "{0}.{1}.{2}.{3}" -f $v.FileMajorPart, $v.FileMinorPart, $v.FileBuildPart, $v.FilePrivatePart }
function Listening { [bool](Get-NetTCPConnection -LocalPort 3389 -State Listen -ErrorAction SilentlyContinue) }
function GroupSid { $g = Get-LocalGroup -Name $portalGroup -ErrorAction SilentlyContinue; if ($g) { $g.SID } else { $null } }
function SidBytes($sid) { $b = New-Object byte[] $sid.BinaryLength; $sid.GetBinaryForm($b, 0); $b }
function WinStationSd {
  $bytes = (Get-ItemProperty $rdpTcp -ErrorAction SilentlyContinue).Security
  if (-not $bytes) { $bytes = (Get-ItemProperty "$ts\WinStations" -ErrorAction SilentlyContinue).DefaultSecurity }
  if (-not $bytes) { return $null }
  New-Object Security.AccessControl.RawSecurityDescriptor ([byte[]]$bytes, 0)
}
function HasAce($sid) {
  $sd = WinStationSd; if (-not $sd) { return $false }
  [bool]($sd.DiscretionaryAcl | Where-Object { $_.SecurityIdentifier -eq $sid -and ($_.AccessMask -band 0x121) -eq 0x121 })
}
function PolicyValue($path, $name) { (Get-ItemProperty $path -ErrorAction SilentlyContinue).$name }
$wanted = @(
  @($policies, 'AVCHardwareEncodePreferred', 1),
  @($policies, 'bEnumerateHWBeforeSW', 1),
  @("$ts\WinStations", 'DWMFRAMEINTERVAL', 15),
  @('HKLM:\SOFTWARE\Policies\Microsoft\Windows\OOBE', 'DisablePrivacyExperience', 1),
  @($policies, 'fDisableAudioCapture', 0),
  @($policies, 'fDisableCam', 0),
  @($policies, 'fDisableCameraRedir', 0)
)

function Fetch-Ini($version) {
  foreach ($url in $iniSources) {
    try {
      $text = (Invoke-WebRequest $url -UseBasicParsing -TimeoutSec 30).Content
      if ($text -is [byte[]]) { $text = [Text.Encoding]::UTF8.GetString($text) }
      if ($text.Contains("[$version]")) { return $text }
    } catch {}
  }
  $null
}

function Restart-RemoteDesktop {
  Restart-Service TermService -Force -ErrorAction SilentlyContinue
  for ($i = 0; $i -lt 30 -and -not (Listening); $i++) { Start-Sleep -Milliseconds 500 }
}

# RDP Wrapper: install it if it's missing (with $force: reinstall it from
# scratch, e.g. after an antivirus removed parts of it), then make sure its
# settings cover this Windows build.
function Install-Wrapper($force) {
  $version = TermsrvVersion
  $params = 'HKLM:\SYSTEM\CurrentControlSet\Services\TermService\Parameters'
  $registered = (Get-ItemProperty $params).ServiceDll -like '*rdpwrap.dll'
  $present = Test-Path "$wrapDir\rdpwrap.dll"
  if ($force -or -not $registered -or -not $present) {
    # Microsoft Defender flags RDP Wrapper as a hacking tool and removes it,
    # so its folder and the download folder are excluded first.
    $excluded = $true
    foreach ($dir in @($wrapDir, $wrapStage)) {
      try { Add-MpPreference -ExclusionPath $dir -ErrorAction Stop } catch { $excluded = $false }
    }
    $hint = if ($excluded) {
      'Microsoft Defender or another antivirus program removed it. Allow RDP Wrapper in the antivirus (or pause it for a minute) and click Repair.'
    } else {
      "Access Portal could not add a Microsoft Defender exclusion, so the antivirus removed it. In Windows Security (or the other antivirus) add exclusions for '$wrapDir' and '$wrapStage', then click Repair."
    }
    # Settings that cover this build are kept (the installer brings old ones).
    $savedIni = if (Test-Path "$wrapDir\rdpwrap.ini") { Get-Content "$wrapDir\rdpwrap.ini" -Raw } else { '' }
    Start-Sleep -Seconds 1
    $tmp = Join-Path $wrapStage ([guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $tmp -Force | Out-Null
    try {
      try { Invoke-WebRequest $wrapZip -OutFile "$tmp\rdpwrap.zip" -UseBasicParsing -TimeoutSec 120 }
      catch {
        if ("$_" -match 'virus|unwanted software|malware') { throw "The RDP Wrapper download was blocked. $hint" }
        throw "Could not download RDP Wrapper from GitHub: $_"
      }
      if (-not (Test-Path "$tmp\rdpwrap.zip")) { throw "The RDP Wrapper download disappeared. $hint" }
      if ((Get-FileHash "$tmp\rdpwrap.zip" -Algorithm SHA256).Hash -ne $wrapZipSha256) { throw 'The RDP Wrapper download did not match the expected file.' }
      Expand-Archive "$tmp\rdpwrap.zip" "$tmp\x" -Force
      $setup = "$tmp\x\RDPWInst.exe"
      if (-not (Test-Path $setup)) { throw "RDP Wrapper's installer disappeared after unpacking. $hint" }
      # Its installer does nothing while Remote Desktop is still registered to
      # RDP Wrapper, so that registration is cleared first.
      if ($registered -and $present) {
        # A working copy (Repair): uninstall it properly, which also unloads it.
        Start-Process $setup -ArgumentList '-u' -Wait -WindowStyle Hidden | Out-Null
      }
      if ((Get-ItemProperty $params).ServiceDll -like '*rdpwrap.dll') {
        # Its files are gone (usually an antivirus): point Remote Desktop back at Windows' own file.
        Stop-Service TermService -Force -ErrorAction SilentlyContinue
        Set-ItemProperty $params ServiceDll '%SystemRoot%\System32\termsrv.dll' -Type ExpandString
      }
      $p = Start-Process $setup -ArgumentList '-i' -Wait -PassThru -WindowStyle Hidden
      if (-not (Test-Path "$wrapDir\rdpwrap.dll") -or (Get-ItemProperty $params).ServiceDll -notlike '*rdpwrap.dll') {
        throw "RDP Wrapper did not install (its installer ended with code $($p.ExitCode)). $hint"
      }
      if ($savedIni.Contains("[$version]")) { [IO.File]::WriteAllText("$wrapDir\rdpwrap.ini", $savedIni) }
    } finally { Remove-Item $wrapStage -Recurse -Force -ErrorAction SilentlyContinue }
  }
  $ini = "$wrapDir\rdpwrap.ini"
  $current = if (Test-Path $ini) { Get-Content $ini -Raw } else { '' }
  if (-not $current.Contains("[$version]")) {
    $text = Fetch-Ini $version
    if (-not $text) { throw "No published RDP Wrapper settings support this Windows build ($version) yet. Try again in a few days." }
    if (Test-Path $ini) { Copy-Item $ini ("$ini.bak-" + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
    [IO.File]::WriteAllText($ini, $text)
  }
  Restart-RemoteDesktop
}

# ---------------------------------------------------------------- fixes
function Fix($id) {
  switch ($id) {
    'rdp' {
      Set-ItemProperty $ts fDenyTSConnections 0 -Type DWord
      Set-ItemProperty $rdpTcp SecurityLayer 2 -Type DWord
      Set-ItemProperty $rdpTcp UserAuthentication 1 -Type DWord
      Set-Service TermService -StartupType Automatic -ErrorAction SilentlyContinue
      Start-Service TermService -ErrorAction SilentlyContinue
    }
    'wrapper' { Install-Wrapper $false }
    'wrapper-repair' { Install-Wrapper $true }
    'group' {
      if (-not (GroupSid)) { New-LocalGroup -Name $portalGroup -Description 'Access Portal Remote Desktop users' -ErrorAction Stop | Out-Null }
      $sid = GroupSid
      if (-not [ApLsa]::Has((SidBytes $sid), 'SeRemoteInteractiveLogonRight')) { [ApLsa]::Add((SidBytes $sid), 'SeRemoteInteractiveLogonRight') }
      if (-not (HasAce $sid)) {
        $sd = WinStationSd
        if (-not $sd) { throw 'Remote Desktop has no permission list to add the group to.' }
        $ace = New-Object Security.AccessControl.CommonAce ([Security.AccessControl.AceFlags]::None, [Security.AccessControl.AceQualifier]::AccessAllowed, 0x121, $sid, $false, $null)
        $sd.DiscretionaryAcl.InsertAce($sd.DiscretionaryAcl.Count, $ace)
        $out = New-Object byte[] $sd.BinaryLength; $sd.GetBinaryForm($out, 0)
        Set-ItemProperty $rdpTcp Security $out -Type Binary
      }
    }
    'pdf' {
      Enable-WindowsOptionalFeature -Online -FeatureName 'Printing-PrintToPDFServices-Features' -NoRestart -ErrorAction Stop | Out-Null
    }
    'policies' {
      foreach ($w in $wanted) {
        if (-not (Test-Path $w[0])) { New-Item -Path $w[0] -Force | Out-Null }
        Set-ItemProperty $w[0] $w[1] $w[2] -Type DWord
      }
    }
  }
}

# ---------------------------------------------------------------- checks
function Checks {
  $list = @()
  $admin = Test-Admin
  $list += [ordered]@{ id = 'admin'; label = 'Access Portal runs as administrator'; ok = $admin
    detail = $(if ($admin) { 'Yes.' } else { 'No. Click "Restart as administrator" in Access Portal.' }); fix = ''; restarts = $false }

  $os = (Get-CimInstance Win32_OperatingSystem).Caption
  $deny = PolicyValue $ts 'fDenyTSConnections'
  $nla = (PolicyValue $rdpTcp 'SecurityLayer') -eq 2 -and (PolicyValue $rdpTcp 'UserAuthentication') -eq 1
  $svc = Get-Service TermService -ErrorAction SilentlyContinue
  $rdpOk = $deny -eq 0 -and $nla -and $svc -and $svc.Status -eq 'Running'
  $list += [ordered]@{ id = 'rdp'; label = 'Remote Desktop is on'; ok = $rdpOk
    detail = $(if ($rdpOk) { "On, with network-level sign-in. ($os)" } else { "Off or not secure. ($os)" }); fix = 'Turn Remote Desktop on, require network-level sign-in, start the service.'; restarts = $false }

  $version = TermsrvVersion
  $dll = PolicyValue 'HKLM:\SYSTEM\CurrentControlSet\Services\TermService\Parameters' 'ServiceDll'
  $registered = $dll -like '*rdpwrap.dll'
  $wrapped = $registered -and (Test-Path "$wrapDir\rdpwrap.dll")
  $iniOk = $wrapped -and (Test-Path "$wrapDir\rdpwrap.ini") -and (Get-Content "$wrapDir\rdpwrap.ini" -Raw).Contains("[$version]")
  $listen = Listening
  $wrapOk = $wrapped -and $iniOk -and $listen
  $detail = if ($wrapOk) { "Installed and supports this Windows build ($version)." }
    elseif ($registered) { 'Installed, but its files are gone (usually an antivirus removed them). Remote Desktop may not work at all until it is repaired.' }
    elseif (-not $wrapped) { "Not installed. Windows lets only one person use Remote Desktop at a time without it." }
    elseif (-not $iniOk) { "Installed, but its settings don't cover this Windows build ($version), usually after a Windows update." }
    else { 'Installed, but Remote Desktop is not accepting connections.' }
  $list += [ordered]@{ id = 'wrapper'; label = 'Several people can be connected at once (RDP Wrapper)'; ok = $wrapOk; detail = $detail
    fix = 'Install RDP Wrapper (adding a Microsoft Defender exclusion for its folder) and fetch settings for this Windows build, then restart Remote Desktop.'; restarts = $true }

  $sid = GroupSid
  $right = $sid -and [ApLsa]::Has((SidBytes $sid), 'SeRemoteInteractiveLogonRight')
  $ace = $sid -and (HasAce $sid)
  $groupOk = [bool]($sid -and $right -and $ace)
  $members = if ($sid) { @(Get-LocalGroupMember -Group $portalGroup -ErrorAction SilentlyContinue).Count } else { 0 }
  $list += [ordered]@{ id = 'group'; label = "The '$portalGroup' group may sign in over Remote Desktop"; ok = $groupOk
    detail = $(if ($groupOk) { "Yes ($members account(s) in it)." } elseif (-not $sid) { 'The group does not exist yet.' } else { 'The group exists but is missing the Remote Desktop sign-in permission.' })
    fix = 'Create the group and give it permission to sign in over Remote Desktop.'; restarts = $false }

  $pdf = [bool](Get-PrinterDriver -Name 'Microsoft Print To PDF' -ErrorAction SilentlyContinue)
  $list += [ordered]@{ id = 'pdf'; label = 'Printing to the browser works'; ok = $pdf
    detail = $(if ($pdf) { "Windows' PDF printer driver is installed." } else { "Windows' Microsoft Print to PDF feature is turned off." })
    fix = 'Turn on Microsoft Print to PDF.'; restarts = $false }

  # Windows Home's licence turns these off for Remote Desktop sessions, in
  # services RDP Wrapper doesn't touch; Pro and Enterprise allow them.
  $micLic = [ApLsa]::Licence('TerminalServices-DeviceRedirection-Licenses-TSAudioCaptureAllowed')
  $camLic = [ApLsa]::Licence('TerminalServices-DeviceRedirection-Licenses-CameraRedirectionAllowed')
  $devOk = $micLic -eq 1 -and $camLic -eq 1
  $list += [ordered]@{ id = 'devices'; label = "Apps in sessions can use the user's microphone and camera"; ok = $devOk
    detail = $(if ($devOk) { 'Yes.' } else { "No: this edition of Windows ($os) doesn't allow microphone and camera redirection in Remote Desktop sessions. They work when the portal runs on Windows Pro or Enterprise. Everything else works." })
    fix = ''; restarts = $false }

  $missing = @($wanted | Where-Object { (PolicyValue $_[0] $_[1]) -ne $_[2] } | ForEach-Object { $_[1] })
  $list += [ordered]@{ id = 'policies'; label = 'Sessions are tuned (graphics card, 60 fps, sound, microphone, camera)'; ok = ($missing.Count -eq 0)
    detail = $(if ($missing.Count -eq 0) { 'All set. The frame rate applies after the next restart of the computer.' } else { 'Not set yet: ' + ($missing -join ', ') + '.' })
    fix = 'Apply the Remote Desktop settings the portal is tuned for.'; restarts = $false }
  $list
}

# ---------------------------------------------------------------- info
function Info {
  $os = Get-CimInstance Win32_OperatingSystem
  $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction SilentlyContinue
  $mp = try { Get-MpComputerStatus -ErrorAction Stop } catch { $null }
  $pref = try { Get-MpPreference -ErrorAction Stop } catch { $null }
  $av = try { @(Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct -ErrorAction Stop | ForEach-Object { $_.displayName }) } catch { @() }
  $threats = try {
    # Only RDP Wrapper and Access Portal ones: the rest is none of our business.
    @(Get-MpThreatDetection -ErrorAction Stop | Sort-Object InitialDetectionTime -Descending |
      Where-Object { ($_.Resources -join ' ') -match 'rdpwrap|RDP Wrapper|RDPWInst|AccessPortal|access-portal' } | Select-Object -First 5 |
      ForEach-Object { $r = "$($_.InitialDetectionTime.ToString('s')) $($_.Resources -join ', ')"; $r.Substring(0, [Math]::Min(300, $r.Length)) })
  } catch { @() }
  $svc = Get-Service TermService -ErrorAction SilentlyContinue
  [ordered]@{
    os = $os.Caption
    version = $os.Version
    display_version = $cv.DisplayVersion
    build = "$($cv.CurrentBuildNumber).$($cv.UBR)"
    edition = $cv.EditionID
    architecture = $os.OSArchitecture
    language = (Get-Culture).Name
    last_boot = $os.LastBootUpTime.ToString('s')
    admin = Test-Admin
    termsrv = TermsrvVersion
    termservice = $(if ($svc) { "$($svc.Status)" } else { 'missing' })
    service_dll = PolicyValue 'HKLM:\SYSTEM\CurrentControlSet\Services\TermService\Parameters' 'ServiceDll'
    rdp_listening = Listening
    wrapper_files = @(Get-ChildItem $wrapDir -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    antivirus = $av
    defender = $(if ($mp) { [ordered]@{ antivirus = $mp.AntivirusEnabled; realtime = $mp.RealTimeProtectionEnabled; tamper_protected = $mp.IsTamperProtected } } else { $null })
    defender_exclusions = $(if ($pref) { @($pref.ExclusionPath | Where-Object { $_ -like '*RDP Wrapper*' -or $_ -like '*AccessPortal*' }) } else { @() })
    defender_threats = $threats
  }
}
if ($env:AP_ACTION -eq 'info') {
  ConvertTo-Json -InputObject (Info) -Compress -Depth 4
  return
}

$errors = @{}
if ($env:AP_ACTION -eq 'fix') {
  if (-not (Test-Admin)) { $errors['admin'] = 'Access Portal is not running as administrator.' }
  else {
    foreach ($id in ($env:AP_FIX -split ',' | Where-Object { $_ })) {
      # A repair reports on the check it repairs ('wrapper-repair' -> 'wrapper').
      try { Fix $id } catch { $errors[($id -replace '-repair$', '')] = $_.Exception.Message }
    }
  }
}
$result = @(Checks | ForEach-Object { if ($errors.ContainsKey($_.id)) { $_.error = $errors[$_.id] }; [pscustomobject]$_ })
ConvertTo-Json -InputObject $result -Compress -Depth 4

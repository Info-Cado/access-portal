# Access Portal: checks (and fixes) what this computer needs to serve portal
# sessions. Run by the portal (src/host.rs) as administrator.
#
#   AP_ACTION = check | fix
#   AP_FIX    = comma-separated check ids to fix (with AP_ACTION=fix)
#   AP_INI_URLS = extra places to look for RDP Wrapper settings, tried first
#
# Prints one JSON line: [{ id, label, ok, detail, fix, restarts }...]

$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$wrapDir = 'C:\Program Files\RDP Wrapper'
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
$launcherDir = 'C:\Program Files\access-portal'
$launcher = Join-Path $launcherDir 'miracle-shell.vbs'
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
function FindMiracle {
  foreach ($pattern in 'C:\Miracle*\miracle.exe', "$env:ProgramFiles\Miracle*\miracle.exe", "${env:ProgramFiles(x86)}\Miracle*\miracle.exe") {
    $f = Get-ChildItem $pattern -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($f) { return $f.FullName }
  }
  $null
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
    'wrapper' {
      $version = TermsrvVersion
      $dll = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\TermService\Parameters').ServiceDll
      if ($dll -notlike '*rdpwrap.dll' -or -not (Test-Path "$wrapDir\rdpwrap.dll")) {
        # Microsoft Defender flags RDP Wrapper as a hacking tool and removes it.
        try { Add-MpPreference -ExclusionPath $wrapDir -ErrorAction Stop } catch {}
        $tmp = Join-Path $env:TEMP ('ap-rdpwrap-' + [guid]::NewGuid())
        New-Item -ItemType Directory $tmp | Out-Null
        try {
          Invoke-WebRequest $wrapZip -OutFile "$tmp\rdpwrap.zip" -UseBasicParsing -TimeoutSec 120
          if ((Get-FileHash "$tmp\rdpwrap.zip" -Algorithm SHA256).Hash -ne $wrapZipSha256) { throw 'The RDP Wrapper download did not match the expected file.' }
          Expand-Archive "$tmp\rdpwrap.zip" "$tmp\x" -Force
          $p = Start-Process "$tmp\x\RDPWInst.exe" -ArgumentList '-i' -Wait -PassThru -WindowStyle Hidden
          if (-not (Test-Path "$wrapDir\rdpwrap.dll")) { throw 'RDP Wrapper did not install (Microsoft Defender may have removed it).' }
        } finally { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
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
    'launcher' {
      $exe = FindMiracle
      if (-not $exe) { throw 'Miracle is not installed on this computer (no miracle.exe in C:\Miracle* or Program Files).' }
      $dir = Split-Path $exe
      New-Item -ItemType Directory -Force $launcherDir | Out-Null
      $vbs = @"
' access-portal: runs as the desktop shell for portal accounts.
' Starts Miracle, waits for it to close, then signs the account out.
Set sh = CreateObject("WScript.Shell")
sh.CurrentDirectory = "$dir"
On Error Resume Next
sh.Run """$exe""", 3, True
On Error GoTo 0
sh.Run "shutdown.exe /l /f", 0, False
"@
      [IO.File]::WriteAllText($launcher, ($vbs -replace "`r?`n", "`r`n") + "`r`n")
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
    detail = $(if ($admin) { 'Yes.' } else { 'No. Close Access Portal from its tray icon and open it with "Run as administrator".' }); fix = ''; restarts = $false }

  $os = (Get-CimInstance Win32_OperatingSystem).Caption
  $deny = PolicyValue $ts 'fDenyTSConnections'
  $nla = (PolicyValue $rdpTcp 'SecurityLayer') -eq 2 -and (PolicyValue $rdpTcp 'UserAuthentication') -eq 1
  $svc = Get-Service TermService -ErrorAction SilentlyContinue
  $rdpOk = $deny -eq 0 -and $nla -and $svc -and $svc.Status -eq 'Running'
  $list += [ordered]@{ id = 'rdp'; label = 'Remote Desktop is on'; ok = $rdpOk
    detail = $(if ($rdpOk) { "On, with network-level sign-in. ($os)" } else { "Off or not secure. ($os)" }); fix = 'Turn Remote Desktop on, require network-level sign-in, start the service.'; restarts = $false }

  $version = TermsrvVersion
  $dll = PolicyValue 'HKLM:\SYSTEM\CurrentControlSet\Services\TermService\Parameters' 'ServiceDll'
  $wrapped = $dll -like '*rdpwrap.dll' -and (Test-Path "$wrapDir\rdpwrap.dll")
  $iniOk = $wrapped -and (Test-Path "$wrapDir\rdpwrap.ini") -and (Get-Content "$wrapDir\rdpwrap.ini" -Raw).Contains("[$version]")
  $listen = Listening
  $wrapOk = $wrapped -and $iniOk -and $listen
  $detail = if ($wrapOk) { "Installed and supports this Windows build ($version)." }
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

  $exe = FindMiracle
  $launcherOk = (Test-Path $launcher) -and $exe
  $list += [ordered]@{ id = 'launcher'; label = 'Miracle-only sessions can start'; ok = [bool]$launcherOk
    detail = $(if ($launcherOk) { "Launcher ready for $exe." } elseif (-not $exe) { 'Miracle is not installed on this computer.' } else { 'The launcher that starts Miracle is missing.' })
    fix = 'Write the launcher that starts Miracle and signs the session out when it closes.'; restarts = $false }

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

$errors = @{}
if ($env:AP_ACTION -eq 'fix') {
  if (-not (Test-Admin)) { $errors['admin'] = 'Access Portal is not running as administrator.' }
  else {
    foreach ($id in ($env:AP_FIX -split ',' | Where-Object { $_ })) {
      try { Fix $id } catch { $errors[$id] = $_.Exception.Message }
    }
  }
}
$result = @(Checks | ForEach-Object { if ($errors.ContainsKey($_.id)) { $_.error = $errors[$_.id] }; [pscustomobject]$_ })
ConvertTo-Json -InputObject $result -Compress -Depth 4

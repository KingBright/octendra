param(
  [Parameter(Mandatory=$true)][string]$Candidate,
  [Parameter(Mandatory=$true)][string]$Sha256,
  [Parameter(Mandatory=$true)][string]$Version,
  [Parameter(Mandatory=$true)][string]$Result,
  [string]$BinaryPath="",
  [string]$ConfigPath="",
  [string]$TaskName="Octendra Code Agent",
  [int]$IdleTimeout=180
)
$ErrorActionPreference='Stop'
if ($Sha256 -notmatch '^[0-9a-f]{64}$' -or $Version -notmatch '^\d+\.\d+\.\d+$') { throw 'invalid version/checksum' }
$record=[ordered]@{state='preflight';version=$Version;candidate_sha256=$Sha256;service_changed=$false}
$lease=[guid]::NewGuid().ToString('N').PadRight(64,'0').Substring(0,64)
$config=$null;$maintenance=$false;$backup=$null
$script:DesktopApprovalUpdate=$null
$agentTask=Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
$agentAction=@($agentTask.Actions)[0]
if ([string]::IsNullOrWhiteSpace($BinaryPath)) { $BinaryPath=[string]$agentAction.Execute }
if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
  $match=[regex]::Match([string]$agentAction.Arguments,'--config\s+(?:"([^"]+)"|(\S+))')
  if ($match.Success) {
    if ($match.Groups[1].Success) { $ConfigPath=$match.Groups[1].Value } else { $ConfigPath=$match.Groups[2].Value }
  } else {
    $preferred=Join-Path $env:LOCALAPPDATA 'OctendraCode\agent.json'
    $legacy=Join-Path $env:USERPROFILE '.local\share\octendra-code\agent.json'
    if (Test-Path -LiteralPath $preferred) { $ConfigPath=$preferred } elseif (Test-Path -LiteralPath $legacy) { $ConfigPath=$legacy } else { $ConfigPath=$preferred }
  }
}
$record.binary_path=$BinaryPath;$record.config_path=$ConfigPath
function Save-Receipt { param($Value) $parent=Split-Path -Parent $Result;New-Item -ItemType Directory -Force $parent|Out-Null;$Value|ConvertTo-Json -Depth 8|Set-Content -Encoding UTF8 $Result }
function Copy-AgentBinary {
  param([string]$Source,[string]$Target)
  $acl=Get-Acl -LiteralPath $Target
  $temporary="$Target.new"
  if (Test-Path -LiteralPath $temporary) { throw 'original replacement temporary exists; inspect before retry' }
  try {
    Copy-Item -LiteralPath $Source -Destination $temporary -ErrorAction Stop
    Set-Acl -LiteralPath $temporary -AclObject $acl -ErrorAction Stop
    if ((Get-Acl -LiteralPath $temporary).Sddl -ne $acl.Sddl) { throw 'replacement ACL differs from installed binary' }
    Move-Item -LiteralPath $temporary -Destination $Target -Force -ErrorAction Stop
  } finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -ErrorAction Stop }
  }
}
function Get-PrivateDesktopApprovalAcl {
  param([string]$Path)
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  foreach ($item in @($Path,(Split-Path -Parent $Path))) {
    $info=Get-Item -LiteralPath $item -Force
    if (($info.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {throw 'desktop_approval_not_private'}
    $acl=Get-Acl -LiteralPath $item
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $sid) {throw 'desktop_approval_not_private'}
    foreach ($rule in $acl.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
      if ($rule.AccessControlType -eq 'Allow' -and $rule.IdentityReference.Value -notin @($sid,'S-1-5-18','S-1-5-32-544','S-1-3-4')) {throw 'desktop_approval_not_private'}
    }
  }
  return (Get-Acl -LiteralPath $Path)
}
function Get-DesktopApprovalDigest {
  param([byte[]]$Bytes)
  $hasher=[Security.Cryptography.SHA256]::Create()
  try {return [BitConverter]::ToString($hasher.ComputeHash($Bytes)).Replace('-','').ToLowerInvariant()} finally {$hasher.Dispose()}
}
function Write-DesktopApprovalBytes {
  param([string]$Path,[string]$ExpectedHash,[byte[]]$Bytes,$Acl)
  $temporary="$Path.new";$created=$false
  try {
    $stream=[IO.File]::Open($temporary,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None);$created=$true
    try {$stream.Write($Bytes,0,$Bytes.Length);$stream.Flush($true)} finally {$stream.Dispose()}
    Set-Acl -LiteralPath $temporary -AclObject $Acl
    $current=Get-PrivateDesktopApprovalAcl $Path
    if ($current.Sddl -ne $Acl.Sddl -or (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant() -ne $ExpectedHash) {throw 'desktop_approval_changed'}
    [IO.File]::Replace($temporary,$Path,[System.Management.Automation.Language.NullString]::Value)
  } finally {if ($created -and (Test-Path -LiteralPath $temporary)) {Remove-Item -LiteralPath $temporary -Force}}
}
function Renew-DesktopApproval {
  param($Config,[string]$Binary,[string]$PreviousHash,[string]$InstalledHash)
  $script:DesktopApprovalUpdate=$null
  $path=Join-Path $Config.state_dir 'desktop-launch.json'
  if (-not (Test-Path -LiteralPath $path)) {return @{state='not_configured'}}
  $stage='read_authority'
  try {
    $acl=Get-PrivateDesktopApprovalAcl $path
    if ((Get-Item -LiteralPath $path).Length -gt 16384) {throw 'desktop_approval_too_large'}
    $original=[IO.File]::ReadAllBytes($path);$expected=Get-DesktopApprovalDigest $original
    $stage='parse_authority';$grant=[Text.Encoding]::UTF8.GetString($original)|ConvertFrom-Json
    if ($grant.enabled -ne $true -or $grant.observe -ne $true) {return @{state='disabled_preserved'}}
    if ($grant.identity.device_id -ne $Config.device_id) {throw 'desktop_approval_device_mismatch'}
    if ($grant.agent_sha256 -cne $PreviousHash) {throw 'desktop_approval_previous_agent_mismatch'}
    if ($grant.capability_version -isnot [int] -and $grant.capability_version -isnot [long] -and $grant.capability_version -isnot [uint64]) {throw 'desktop_approval_version_invalid'}
    $version=[uint64]$grant.capability_version
    if ($version -eq 0 -or $version -eq [uint64]::MaxValue) {throw 'desktop_approval_version_invalid'}
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Binary).Hash.ToLowerInvariant() -ne $InstalledHash) {throw 'desktop_approval_installed_agent_mismatch'}
    $stage='verify_worker';$worker=[IO.Path]::GetFullPath($grant.worker_path);$installed=[IO.Path]::GetFullPath($Binary)
    if ($worker.StartsWith('\\?\')) {$worker=$worker.Substring(4)}
    if ($installed.StartsWith('\\?\')) {$installed=$installed.Substring(4)}
    if ($worker -eq $installed) {
      if ($grant.worker_sha256 -cne $PreviousHash) {throw 'desktop_approval_previous_worker_mismatch'}
      $grant.worker_sha256=$InstalledHash
    } elseif ((Get-FileHash -Algorithm SHA256 -LiteralPath $worker).Hash.ToLowerInvariant() -ne $grant.worker_sha256) {throw 'desktop_approval_worker_changed'}
    $grant.agent_sha256=$InstalledHash;$grant.capability_version=$version+1
    $bytes=[Text.UTF8Encoding]::new($false).GetBytes(($grant|ConvertTo-Json -Depth 32)+"`n")
    if ($bytes.Length -gt 16384) {throw 'desktop_approval_too_large'}
    $stage='write_authority';Write-DesktopApprovalBytes -Path $path -ExpectedHash $expected -Bytes $bytes -Acl $acl
    $script:DesktopApprovalUpdate=@{path=$path;original=$original;expected=(Get-DesktopApprovalDigest $bytes);acl=$acl}
    return @{state='renewed';capability_version=$version+1}
  } catch {
    $reason=if ($_.Exception.Message.StartsWith('desktop_approval_')) {$_.Exception.Message} else {'desktop_approval_unavailable'}
    return @{state='requires_reapproval';reason=$reason;stage=$stage;error_type=$_.Exception.GetType().FullName}
  }
}
function Restore-DesktopApproval {
  if ($null -ne $script:DesktopApprovalUpdate) {
    $u=$script:DesktopApprovalUpdate
    Write-DesktopApprovalBytes -Path $u.path -ExpectedHash $u.expected -Bytes $u.original -Acl $u.acl
  }
}
function Maintenance-Call { param([string]$Action) $body=@{action=$Action;lease_id=$lease;ttl_seconds=300}|ConvertTo-Json -Compress;$headers=@{Authorization=('Bearer '+$config.device_token)};Invoke-RestMethod -Method Post -Uri ($config.gateway_url+'/device/maintenance') -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 10|Out-Null }
function Gateway-Ready { param([string]$Expected,[long]$Baseline) $deadline=(Get-Date).AddSeconds(240);$samples=0;$session=$null;$last=$Baseline;$headers=@{Authorization=('Bearer '+$config.device_token);'Cache-Control'='no-cache'};while ((Get-Date) -lt $deadline) { try {$v=Invoke-RestMethod -Method Get -Uri ($config.gateway_url+'/device/readiness') -Headers $headers -TimeoutSec 10;if ($v.agent_version -eq $Expected -and $v.ready -eq $true -and [long]$v.last_seen -gt $last) {if ($session -ne $v.session) {$session=$v.session;$samples=1} else {$samples++};$last=[long]$v.last_seen;if ($samples -ge 3) {return @{gateway_verified=$true;session=$session;samples=$samples;last_seen=$last}}}} catch {};Start-Sleep -Seconds 2};throw 'gateway readiness did not converge' }
try {
  if ((Get-FileHash -Algorithm SHA256 $Candidate).Hash.ToLowerInvariant() -ne $Sha256) { throw 'candidate checksum mismatch' }
  # GUI-subsystem executables must participate in a pipeline so PowerShell waits
  # and captures stdout even when Task Scheduler starts us without a console.
  $candidateVersion = (& $Candidate --version | Out-String).Trim()
  if ($LASTEXITCODE -ne 0 -or $candidateVersion -ne "octendra-code $Version") { throw 'candidate version mismatch or nonzero exit' }
  $config=Get-Content -Raw -LiteralPath $ConfigPath|ConvertFrom-Json
  $headers=@{Authorization=('Bearer '+$config.device_token);'Cache-Control'='no-cache'};$before=Invoke-RestMethod -Method Get -Uri ($config.gateway_url+'/device/readiness') -Headers $headers -TimeoutSec 10
  Maintenance-Call 'acquire';$maintenance=$true
  $deadline=(Get-Date).AddSeconds($IdleTimeout)
  do {
    $agents=@(Get-Process octendra-code -ErrorAction SilentlyContinue)
    $agentIds=@($agents|ForEach-Object {$_.Id})
    $children=@(Get-CimInstance Win32_Process|Where-Object {$agentIds -contains $_.ParentProcessId -and $_.Name -notin @('conhost.exe','OpenConsole.exe')})
    $record.active_children=$children.Count
    if ($children.Count -eq 0) {break}
    if ((Get-Date) -ge $deadline) {throw 'agent still has active child work; upgrade not applied'}
    Start-Sleep -Seconds 2
  } while ($true)
  $backupRoot=Join-Path $env:LOCALAPPDATA ("octendra-code\releases\before-$Version-"+(Get-Date -Format yyyyMMddTHHmmss)+'-'+[guid]::NewGuid().ToString('N').Substring(0,8));New-Item -ItemType Directory -Force $backupRoot|Out-Null;$backup=Join-Path $backupRoot 'octendra-code.exe';Copy-Item -LiteralPath $BinaryPath -Destination $backup
  $record.backup=$backup;$record.previous_sha256=(Get-FileHash -Algorithm SHA256 $backup).Hash.ToLowerInvariant();$record.state='installing'
  Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue;Start-Sleep -Seconds 2
  Copy-AgentBinary -Source $Candidate -Target $BinaryPath;$record.service_changed=$true
  if ((Get-FileHash -Algorithm SHA256 $BinaryPath).Hash.ToLowerInvariant() -ne $Sha256) {throw 'installed checksum mismatch'}
  $record.desktop_approval=Renew-DesktopApproval -Config $config -Binary $BinaryPath -PreviousHash $record.previous_sha256 -InstalledHash $Sha256
  Start-ScheduledTask -TaskName $TaskName
  $ready=Gateway-Ready $Version ([long]$before.last_seen);$record.state='upgraded';$record.installed_sha256=$Sha256;$record.gateway_verified=$true;$record.session=$ready.session;$record.samples=$ready.samples
} catch {
  $record.state='failed';$record.error=$_.Exception.Message
  if ($record.service_changed -and $backup) { try { Stop-ScheduledTask -TaskName $TaskName -ErrorAction Stop;Start-Sleep -Seconds 2;Copy-AgentBinary -Source $backup -Target $BinaryPath;if ((Get-FileHash -Algorithm SHA256 -LiteralPath $BinaryPath).Hash.ToLowerInvariant() -ne $record.previous_sha256) { throw 'rollback checksum mismatch' };Restore-DesktopApproval;Start-ScheduledTask -TaskName $TaskName;$record.rollback='restored_previous_binary' } catch { $record.rollback='failed' } }
}
if ($maintenance) { try { Maintenance-Call 'release';$record.maintenance_released=$true } catch {$record.maintenance_released=$false} }
Save-Receipt $record;$record|ConvertTo-Json -Compress
if ($record.state -ne 'upgraded') { exit 1 }

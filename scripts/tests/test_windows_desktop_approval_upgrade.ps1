$ErrorActionPreference='Stop'
if (-not (Get-Variable OctendraUpgradeSource -ErrorAction SilentlyContinue)) {
  $OctendraUpgradeSource=Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot '..\upgrade-code-agent-windows.ps1')
}
$tokens=$null;$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseInput($OctendraUpgradeSource,[ref]$tokens,[ref]$errors)
if ($errors.Count) {throw 'upgrader syntax failed'}
foreach ($name in @('Get-PrivateDesktopApprovalAcl','Get-DesktopApprovalDigest','Write-DesktopApprovalBytes','Renew-DesktopApproval','Restore-DesktopApproval')) {
  $definitions=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$true))
  if ($definitions.Count -ne 1) {throw "missing function $name"}
  Invoke-Expression $definitions[0].Extent.Text
}
$root=Join-Path ([IO.Path]::GetTempPath()) ('octendra-desktop-approval-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root|Out-Null
try {
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
  $private=[Security.AccessControl.DirectorySecurity]::new();$private.SetOwner($sid);$private.SetAccessRuleProtection($true,$false)
  $private.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))
  Set-Acl -LiteralPath $root -AclObject $private
  $binary=Join-Path $root 'octendra-code.exe';[IO.File]::WriteAllText($binary,'verified current candidate')
  $new=(Get-FileHash -Algorithm SHA256 $binary).Hash.ToLowerInvariant();$old='a'*64
  $path=Join-Path $root 'desktop-launch.json';$config=@{state_dir=$root;device_id='original-device'}
  $grant=@{enabled=$true;observe=$true;input=$false;identity=@{device_id='original-device';owner='original-owner'};capability_version=7;worker_path=$binary;agent_sha256=$old;worker_sha256=$old;weavelo=@{target=@{peer_device_id='original-peer'}}}
  [IO.File]::WriteAllText($path,($grant|ConvertTo-Json -Depth 16),[Text.UTF8Encoding]::new($false))
  $original=[IO.File]::ReadAllBytes($path);$acl=(Get-Acl -LiteralPath $path).Sddl
  $result=Renew-DesktopApproval -Config $config -Binary $binary -PreviousHash $old -InstalledHash $new
  if ($result.state -ne 'renewed') {throw ('approval renewal failed: '+($result|ConvertTo-Json -Compress))}
  $current=Get-Content -Raw -LiteralPath $path|ConvertFrom-Json
  if ($current.agent_sha256 -cne $new -or $current.worker_sha256 -cne $new -or $current.capability_version -ne 8 -or $current.input -ne $false -or $current.identity.device_id -ne 'original-device' -or $current.weavelo.target.peer_device_id -ne 'original-peer' -or (Get-Acl -LiteralPath $path).Sddl -ne $acl) {throw 'renewal changed grant scope or ACL'}
  Restore-DesktopApproval
  if ((Get-DesktopApprovalDigest ([IO.File]::ReadAllBytes($path))) -cne (Get-DesktopApprovalDigest $original)) {throw 'original approval not restored'}
  $result=Renew-DesktopApproval -Config $config -Binary $binary -PreviousHash ('d'*64) -InstalledHash $new
  if ($result.reason -ne 'desktop_approval_previous_agent_mismatch' -or (Get-DesktopApprovalDigest ([IO.File]::ReadAllBytes($path))) -cne (Get-DesktopApprovalDigest $original)) {throw 'unapproved old identity was renewed'}
  $grant.enabled=$false;[IO.File]::WriteAllText($path,($grant|ConvertTo-Json -Depth 16),[Text.UTF8Encoding]::new($false))
  if ((Renew-DesktopApproval -Config $config -Binary $binary -PreviousHash $old -InstalledHash $new).state -ne 'disabled_preserved') {throw 'disabled approval changed'}
  $grant.enabled=$true;[IO.File]::WriteAllText($path,($grant|ConvertTo-Json -Depth 16),[Text.UTF8Encoding]::new($false))
  $unsafe=Get-Acl -LiteralPath $path
  $unsafe.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-1-0'),'Read','Allow'))
  Set-Acl -LiteralPath $path -AclObject $unsafe
  if ((Renew-DesktopApproval -Config $config -Binary $binary -PreviousHash $old -InstalledHash $new).reason -ne 'desktop_approval_not_private') {throw 'foreign ACL was trusted'}
  @{state='passed';tests=5;fixture_only=$true;installed_agent_changed=$false}|ConvertTo-Json -Compress
} finally {Remove-Item -LiteralPath $root -Recurse -Force}

param([Parameter(Mandatory=$true)][string]$Updater)
$ErrorActionPreference='Stop'
$tokens=$null;$parseErrors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($Updater,[ref]$tokens,[ref]$parseErrors)
if ($parseErrors.Count -gt 0) { throw 'updater PowerShell parse failed' }
$definition=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Copy-AgentBinary'},$true))
if ($definition.Count -ne 1) { throw 'one metadata-preserving replacement function required' }
Invoke-Expression $definition[0].Extent.Text
$root=Join-Path ([IO.Path]::GetTempPath()) ('octendra-upgrade-metadata-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root|Out-Null
try {
  $target=Join-Path $root 'installed';$candidate=Join-Path $root 'candidate';$backup=Join-Path $root 'backup'
  [IO.File]::WriteAllText($target,'previous');[IO.File]::WriteAllText($candidate,'candidate');[IO.File]::WriteAllText($backup,'previous')
  $acl=Get-Acl -LiteralPath $target;$acl.SetAccessRuleProtection($true,$true);Set-Acl -LiteralPath $target -AclObject $acl
  $expectedAcl=(Get-Acl -LiteralPath $target).Sddl
  Copy-AgentBinary -Source $candidate -Target $target
  if ([IO.File]::ReadAllText($target) -ne 'candidate' -or (Get-Acl -LiteralPath $target).Sddl -ne $expectedAcl) { throw 'installation failed to preserve exact ACL' }
  Copy-AgentBinary -Source $backup -Target $target
  if ([IO.File]::ReadAllText($target) -ne 'previous' -or (Get-Acl -LiteralPath $target).Sddl -ne $expectedAcl) { throw 'rollback failed to preserve exact ACL' }
  $failed=$false;try { Copy-AgentBinary -Source (Join-Path $root 'absent') -Target $target } catch { $failed=$true }
  if (-not $failed -or [IO.File]::ReadAllText($target) -ne 'previous' -or (Get-Acl -LiteralPath $target).Sddl -ne $expectedAcl -or (Test-Path -LiteralPath "$target.new")) { throw 'copy failure changed original or leaked temporary file' }
  [IO.File]::WriteAllText("$target.new",'original pending evidence')
  $failed=$false;try { Copy-AgentBinary -Source $candidate -Target $target } catch { $failed=$true }
  if (-not $failed -or [IO.File]::ReadAllText("$target.new") -ne 'original pending evidence' -or [IO.File]::ReadAllText($target) -ne 'previous') { throw 'original temporary was replayed or removed' }
  @{state='passed';tests=4;fixture_only=$true;real_agent_changed=$false}|ConvertTo-Json -Compress
} finally { Remove-Item -LiteralPath $root -Recurse -Force }

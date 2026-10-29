param([string]$Updater=(Join-Path $PSScriptRoot 'upgrade-code-agent-windows.ps1'),[string]$ConfigPath='')
$ErrorActionPreference='Stop'
$tokens=$null;$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($Updater,[ref]$tokens,[ref]$errors)
if($errors.Count){throw 'updater PowerShell syntax errors'}
$function=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Initialize-UpgradeTransport'},$true)
if($null -eq $function){throw 'transport initializer missing'}
$initialize=$function.Body.GetScriptBlock()
$original=[Net.WebRequest]::DefaultWebProxy
$environment=@{}
foreach($name in @('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY')) {$environment[$name]=[Environment]::GetEnvironmentVariable($name);[Environment]::SetEnvironmentVariable($name,$null,'Process')}
try {
  function Get-ItemProperty { [pscustomobject]@{ProxyEnable=0;AutoConfigURL=$null} }
  [Net.WebRequest]::DefaultWebProxy=New-Object Net.WebProxy('http://127.0.0.1:9')
  & $initialize
  if($null -ne [Net.WebRequest]::DefaultWebProxy){throw 'unconfigured automatic proxy was retained'}
  $explicit=New-Object Net.WebProxy('http://127.0.0.1:9')
  [Net.WebRequest]::DefaultWebProxy=$explicit
  [Environment]::SetEnvironmentVariable('HTTPS_PROXY','http://127.0.0.1:9','Process')
  & $initialize
  if(-not [object]::ReferenceEquals($explicit,[Net.WebRequest]::DefaultWebProxy)){throw 'explicit environment proxy changed'}
  [Environment]::SetEnvironmentVariable('HTTPS_PROXY',$null,'Process')
  function Get-ItemProperty { [pscustomobject]@{ProxyEnable=1;AutoConfigURL=$null} }
  & $initialize
  if(-not [object]::ReferenceEquals($explicit,[Net.WebRequest]::DefaultWebProxy)){throw 'configured Windows proxy changed'}
  function Get-ItemProperty { [pscustomobject]@{ProxyEnable=0;AutoConfigURL='http://127.0.0.1/proxy.pac'} }
  & $initialize
  if(-not [object]::ReferenceEquals($explicit,[Net.WebRequest]::DefaultWebProxy)){throw 'configured PAC changed'}
  Remove-Item Function:\Get-ItemProperty
  [Net.WebRequest]::DefaultWebProxy=$original
  foreach($name in $environment.Keys){[Environment]::SetEnvironmentVariable($name,$environment[$name],'Process')}
  & $initialize
  $record=@{state='passed';proxy_cases=4;syntax_checked=$true}
  if($ConfigPath) {
    $config=Get-Content -Raw -Encoding UTF8 -LiteralPath $ConfigPath|ConvertFrom-Json
    $timer=[Diagnostics.Stopwatch]::StartNew()
    $ready=Invoke-RestMethod -Method Get -Uri ($config.gateway_url+'/device/readiness') -Headers @{Authorization=('Bearer '+$config.device_token)} -TimeoutSec 10
    $timer.Stop();$record.live_readiness=@{ready=$ready.ready;agent_version=$ready.agent_version;elapsed_ms=$timer.ElapsedMilliseconds}
    if($ready.ready -ne $true){throw 'live device is not ready'}
  }
  $record|ConvertTo-Json -Compress
} finally {
  [Net.WebRequest]::DefaultWebProxy=$original
  foreach($name in $environment.Keys){[Environment]::SetEnvironmentVariable($name,$environment[$name],'Process')}
  Remove-Item Function:\Get-ItemProperty -ErrorAction SilentlyContinue
}

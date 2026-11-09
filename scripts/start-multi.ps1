# 同时开多路:每路 = 一个 agent + 它新开的目标程序 + 一个浏览器窗口。
# 先 cargo build --workspace,然后在项目根目录:
#   powershell -File scripts\start-multi.ps1                        两路 gametest
#   powershell -File scripts\start-multi.ps1 -Count 3 -App notepad
# 开关: -NoHook 不装输入隔离(对照用)   -NoBrowser 只起 agent,不开浏览器窗口
#       -Seconds N 运行 N 秒后自动结束(默认等你按回车)
param(
  [int]$Count = 2,
  [ValidateSet('gametest', 'notepad')][string]$App = 'gametest',
  [int]$BasePort = 8081,
  [switch]$NoHook,
  [switch]$NoBrowser,
  [int]$Seconds = 0
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$bin = Join-Path $root 'target\debug'
$logs = Join-Path $root 'target\multi'
New-Item -ItemType Directory -Force $logs | Out-Null
foreach ($f in 'agent.exe', 'gametest.exe', 'cyber_remote_hook.dll') {
  if (-not (Test-Path (Join-Path $bin $f))) { throw "找不到 $bin\$f,先 cargo build --workspace" }
}

function Wait-Until($what, [scriptblock]$test, [int]$timeoutSec = 40) {
  $end = (Get-Date).AddSeconds($timeoutSec)
  while ((Get-Date) -lt $end) {
    $v = & $test
    if ($v) { return $v }
    Start-Sleep -Milliseconds 300
  }
  throw "超时: $what"
}

$agents = @()
$browsers = @()
$dirs = @()
try {
  for ($i = 0; $i -lt $Count; $i++) {
    $port = $BasePort + $i
    $log = Join-Path $logs "agent-$port.log"
    $agentArgs = @('--app', $App, '--new', '--port', $port)
    if (-not $NoHook) { $agentArgs += '--hook' }
    $p = Start-Process (Join-Path $bin 'agent.exe') -ArgumentList $agentArgs -PassThru -WindowStyle Hidden `
      -RedirectStandardOutput $log -RedirectStandardError "$log.err"
    $agents += [pscustomobject]@{ Port = $port; Proc = $p; Log = $log; Target = 0 }
  }
  foreach ($a in $agents) {
    Wait-Until "端口 $($a.Port) 的 agent 就绪" {
      if ($a.Proc.HasExited) { throw "端口 $($a.Port) 的 agent 已退出,日志: $($a.Log)" }
      try { (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$($a.Port)/health" -TimeoutSec 2).Content -eq 'ok' } catch { $false }
    } | Out-Null
    # 日志里有 "... .exe (进程 1234)",正则不写中文,免得受脚本编码影响
    $a.Target = Wait-Until "端口 $($a.Port) 启动目标程序" {
      $m = Select-String -Path $a.Log -Pattern '\.exe \([^\d)]*(\d+)\)' -Encoding UTF8 | Select-Object -First 1
      if ($m) { [int]$m.Matches[0].Groups[1].Value }
    }
    Write-Host ("端口 {0}: agent {1}, {2} 进程 {3}" -f $a.Port, $a.Proc.Id, $App, $a.Target)
  }

  if (-not $NoBrowser) {
    $exe = @('C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
             'C:\Program Files\Google\Chrome\Application\chrome.exe') | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $exe) { throw '没找到 Edge 或 Chrome' }
    Add-Type -AssemblyName System.Windows.Forms
    $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $w = [Math]::Min(1100, [int]($wa.Width / $Count))
    $h = [Math]::Min(800, $wa.Height)
    foreach ($a in $agents) {
      $n = $a.Port - $BasePort
      $dir = Join-Path $env:TEMP "cyber_remote_browser_$($a.Port)"   # 每路独立的浏览器配置目录 = 独立进程
      $dirs += $dir
      $browsers += Start-Process $exe -PassThru -ArgumentList @(
        "--app=http://127.0.0.1:$($a.Port)/", "--user-data-dir=$dir",
        "--window-position=$($wa.Left + $n * $w),$($wa.Top)", "--window-size=$w,$h",
        '--no-first-run', '--no-default-browser-check')
    }
  }

  Write-Host ''
  Write-Host '在每个浏览器窗口里点一下画面,再按 W / Shift、移动鼠标、点击。'
  Write-Host '- 只有被操作的那一路,按键栏和 Raw Input 栏才会变绿、出数字;别的路保持原样'
  Write-Host '- 手柄栏显示"没有手柄":浏览器手柄转发还没做,主机上的真手柄也被挡掉了'
  Write-Host '- 窗口左上角 HUD 的最后一行应是"输入隔离: 已启用"'
  Write-Host '- 别把真鼠标停在 gametest 窗口上:钩子不拦窗口消息,它会收到真实的鼠标消息'
  Write-Host "日志在 $logs"
  if ($Seconds -gt 0) { Start-Sleep -Seconds $Seconds } else { [void](Read-Host '按回车结束全部') }
}
finally {
  foreach ($b in $browsers) { & taskkill /pid $b.Id /t /f 2>$null | Out-Null }
  foreach ($a in $agents) {
    Stop-Process -Id $a.Proc.Id -Force -ErrorAction SilentlyContinue
    if ($a.Target) { Stop-Process -Id $a.Target -Force -ErrorAction SilentlyContinue }
  }
  Start-Sleep -Milliseconds 500
  foreach ($d in $dirs) { Remove-Item $d -Recurse -Force -ErrorAction SilentlyContinue }
}

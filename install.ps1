# KeyForge installer for Windows.
#
#   irm https://raw.githubusercontent.com/Suydev/keyforge/main/install.ps1 | iex
#   .\install.ps1 -Port 9000
#   .\install.ps1 -Uninstall
#
# Same rules as install.sh: never install a toolchain silently, never write
# credentials, never delete key files, announce every mutation.
#
# NOTE ON SCOPE: the Wi-Fi link sampler in src/link.rs reads Android system
# services and is cfg-gated to that platform. On Windows it is inert — nothing is
# spawned, link quality reports Unknown, and the timeout multiplier stays exactly
# 1.0. Everything else works identically.

[CmdletBinding()]
param(
    [string] $Prefix    = "$env:LOCALAPPDATA\keyforge",
    [string] $SrcDir    = "$env:USERPROFILE\keyforge",
    [int]    $Port      = 8787,
    [switch] $NoBuild,
    [switch] $NoRust,
    [switch] $Force,
    [switch] $Uninstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Version   = '1.0.0'
$Repo      = 'https://github.com/Suydev/keyforge'
$BinDir    = Join-Path $Prefix 'bin'
$ConfigDir = Join-Path $env:USERPROFILE '.config\tabi'
$LogDir    = Join-Path $env:USERPROFILE 'tmp'

function Write-Step { param($m) Write-Host "==> " -ForegroundColor Blue -NoNewline; Write-Host $m }
function Write-Ok   { param($m) Write-Host "  ok " -ForegroundColor Green -NoNewline; Write-Host $m }
function Write-Warn2{ param($m) Write-Host "warn " -ForegroundColor Yellow -NoNewline; Write-Host $m }
function Write-Dim  { param($m) Write-Host "     $m" -ForegroundColor DarkGray }
function Die        { param($m) Write-Host "fail " -ForegroundColor Red -NoNewline; Write-Host $m; exit 1 }

function Test-Command { param($n) [bool](Get-Command $n -ErrorAction SilentlyContinue) }

# Piped into iex there is no interactive host, so default to "no" rather than
# silently taking the more invasive branch.
function Confirm-Action {
    param($Question)
    if (-not [Environment]::UserInteractive) { Write-Dim '(non-interactive: assuming no)'; return $false }
    $r = Read-Host "$Question [y/N]"
    return $r -match '^[yY]'
}

if ($Port -lt 1 -or $Port -gt 65535) { Die "-Port out of range: $Port" }

function Invoke-Uninstall {
    Write-Step 'Uninstalling KeyForge'

    $tabi = Join-Path $BinDir 'tabi.ps1'
    if (Test-Path $tabi) { & $tabi stop 2>$null }

    foreach ($p in @($BinDir, $ConfigDir)) {
        if (Test-Path $p) { Remove-Item -Recurse -Force $p; Write-Ok "removed $p" }
    }

    Write-Host ''
    Write-Host 'Left in place, deliberately:'
    Write-Dim "$SrcDir  (source tree)"
    Write-Dim 'your *-keys.txt  (may be your only copy)'
    Write-Dim 'your proxies.txt'
    Write-Host ''
    Write-Ok 'done'
    exit 0
}

function Assert-Rust {
    if (Test-Command cargo) {
        Write-Ok "cargo $((cargo --version) -split ' ' | Select-Object -Index 1)"
        return
    }
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $cargoBin 'cargo.exe')) {
        $env:PATH = "$cargoBin;$env:PATH"
        Write-Ok 'found cargo in ~/.cargo/bin (added to PATH for this run)'
        return
    }

    Write-Warn2 'Rust is not installed.'
    if ($NoRust) { Die '-NoRust was given. Install from https://rustup.rs and re-run.' }
    Write-Host '  Official installer: https://rustup.rs'
    if (-not (Confirm-Action '  Download and run rustup-init now?')) {
        Die 'Rust required. Install it and re-run.'
    }

    $tmp = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $tmp -UseBasicParsing
    & $tmp -y --no-modify-path
    Remove-Item $tmp -Force -ErrorAction SilentlyContinue
    $env:PATH = "$cargoBin;$env:PATH"

    if (-not (Test-Command cargo)) { Die 'cargo still not on PATH after install' }
    Write-Ok 'cargo ready'
}

function Assert-Linker {
    # rustc on Windows needs the MSVC linker. Missing it produces a confusing
    # error deep inside a dependency build, so check early and say so plainly.
    if (Test-Command link.exe) { Write-Ok 'MSVC linker present'; return }
    Write-Warn2 'link.exe not found — the MSVC build tools are required.'
    Write-Host  '  Install "Desktop development with C++" from:'
    Write-Host  '  https://visualstudio.microsoft.com/visual-cpp-build-tools/'
    if (-not (Confirm-Action '  Continue anyway?')) { Die 'aborted' }
}

function Assert-Source {
    if ((Test-Path 'Cargo.toml') -and (Test-Path 'src') -and
        (Select-String -Path 'Cargo.toml' -Pattern '^name\s*=\s*"keyforge"' -Quiet)) {
        $script:SrcDir = (Get-Location).Path
        Write-Ok "building from the current directory: $SrcDir"
        return
    }

    if ((Test-Path (Join-Path $SrcDir 'src')) -and (Test-Path (Join-Path $SrcDir 'Cargo.toml'))) {
        if ($Force -and (Test-Path (Join-Path $SrcDir '.git')) -and (Test-Command git)) {
            Write-Step "Updating source at $SrcDir"
            Push-Location $SrcDir
            try { git pull --ff-only } catch { Write-Warn2 'git pull failed; building what is there' }
            Pop-Location
        } else {
            Write-Ok "using existing source at $SrcDir"
        }
        return
    }

    if (-not (Test-Command git)) { Die 'git is needed to fetch the source (or run this from a checkout)' }
    Write-Step "Cloning $Repo"
    git clone --depth 1 $Repo $SrcDir
    if ($LASTEXITCODE -ne 0) { Die 'clone failed' }
    Write-Ok "cloned to $SrcDir"
}

function Invoke-Build {
    if ($NoBuild) { Write-Warn2 '-NoBuild: skipping cargo'; return }
    Write-Step 'Building (release). Several minutes: lto + codegen-units=1.'
    Push-Location $SrcDir
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { Die "build failed`n  Out of memory?  cargo build --release -j1" }
    } finally { Pop-Location }

    $exe = Join-Path $SrcDir 'target\release\keyforge.exe'
    if (-not (Test-Path $exe)) { Die 'build reported success but the binary is missing' }
    Write-Ok ("built {0:N1} MB" -f ((Get-Item $exe).Length / 1MB))
}

function Install-Binary {
    if ($NoBuild) { return }
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item (Join-Path $SrcDir 'target\release\keyforge.exe') `
              (Join-Path $BinDir 'keyforge.exe') -Force
    Write-Ok "installed $BinDir\keyforge.exe"

    # The binary serves web/ from disk, so an install without it has no dashboard.
    $webSrc = Join-Path $SrcDir 'web'
    if (Test-Path $webSrc) {
        Copy-Item -Recurse -Force $webSrc (Join-Path $Prefix 'web')
        Write-Ok "installed dashboard assets"
    }
}

function Write-Launcher {
    Write-Step 'Generating the tabi launcher'
    New-Item -ItemType Directory -Force -Path $BinDir, $LogDir | Out-Null

    $launcher = @"
# tabi — start / stop / inspect the KeyForge.
# GENERATED by install.ps1. Re-run the installer to regenerate.
#
#   tabi start | stop | restart | status | log | open | fg

param([Parameter(Position=0)][string] `$Action = 'start')

`$ErrorActionPreference = 'Stop'
`$Bin  = '$BinDir\keyforge.exe'
`$Log  = '$LogDir\keyforge.log'
`$Port = if (`$env:TABI_PORT) { [int]`$env:TABI_PORT } else { $Port }

function Test-Up {
    try {
        `$c = [Net.Sockets.TcpClient]::new()
        `$c.Connect('127.0.0.1', `$Port); `$c.Close(); return `$true
    } catch { return `$false }
}

function Get-Proc { Get-Process keyforge -ErrorAction SilentlyContinue }

function Start-Gateway {
    if (-not (Test-Path `$Bin)) { Write-Error "tabi: binary not found at `$Bin"; exit 1 }
    if (Test-Up) { Write-Host "tabi: already running on 127.0.0.1:`$Port"; return }
    `$env:TABI_PORT = `$Port
    Start-Process -FilePath `$Bin -WindowStyle Hidden `
        -RedirectStandardOutput `$Log -RedirectStandardError "`$Log.err"
    foreach (`$i in 1..20) {
        Start-Sleep -Milliseconds 300
        if (Test-Up) {
            Write-Host "tabi: up on http://127.0.0.1:`$Port  (dashboard: http://127.0.0.1:`$Port/)"
            return
        }
    }
    Write-Error 'tabi: failed to start within 6s'
    if (Test-Path `$Log) { Get-Content `$Log -Tail 12 }
}

function Stop-Gateway {
    `$p = Get-Proc
    if (-not `$p) { Write-Host 'tabi: not running'; return }
    # CloseMainWindow first so state is flushed; escalate only if ignored.
    `$p | ForEach-Object { `$_.CloseMainWindow() | Out-Null }
    Start-Sleep -Seconds 1
    Get-Proc | Stop-Process -Force -ErrorAction SilentlyContinue
    Write-Host 'tabi: stopped'
}

function Get-Status {
    if (-not (Test-Up)) { Write-Host "tabi: DOWN (port `$Port not listening)"; return }
    try {
        `$d = Invoke-RestMethod -Uri "http://127.0.0.1:`$Port/api/snapshot" -TimeoutSec 6
    } catch {
        Write-Host "tabi: up, but /api/snapshot did not respond"; return
    }
    `$t = `$d.totals
    Write-Host ("tabi: UP  offline={0}  requests={1} errors={2} spend=`${3:N4}  {4} active session(s)" -f `
        `$d.offline, `$t.requests, `$t.errors, `$t.cost, `$d.activeSessions)
    Write-Host ("      saves: {0} key rotations, {1} failovers, {2} offline holds" -f `
        `$t.rotations, `$t.failovers, `$t.offlineHolds)
    Write-Host ("      data:  {0:N1} MB through the gateway" -f (`$t.bytesTotal / 1MB))
    foreach (`$p in `$d.providers) {
        `$ew = if (`$p.ewmaMs) { "`$([int]`$p.ewmaMs)ms" } else { 'unmeasured' }
        Write-Host ("  {0,-10} {1,4}/{2,-4} keys  `${3,11:N2}  {4,10}  up {5}%  req={6} err={7}" -f `
            `$p.id, `$p.alive, `$p.keys, `$p.funds, `$ew, `$p.uptimePct, `$p.requests, `$p.errors)
    }
}

switch (`$Action) {
    'start'   { Start-Gateway }
    'stop'    { Stop-Gateway }
    'restart' { Stop-Gateway; Start-Sleep -Seconds 1; Start-Gateway }
    'status'  { Get-Status }
    'log'     { Get-Content `$Log -Tail 40 -Wait }
    'open'    { Write-Host "http://127.0.0.1:`$Port/" }
    'fg'      { `$env:TABI_PORT = `$Port; & `$Bin }
    default   { Write-Error 'usage: tabi {start|stop|restart|status|log|open|fg}'; exit 2 }
}
"@

    $target = Join-Path $BinDir 'tabi.ps1'
    Set-Content -Path $target -Value $launcher -Encoding UTF8
    Write-Ok "wrote $target"

    # A .cmd shim so `tabi` works from cmd.exe and from PowerShell with a
    # restrictive execution policy.
    $shim = "@echo off`r`npowershell -NoProfile -ExecutionPolicy Bypass -File `"%~dp0tabi.ps1`" %*`r`n"
    Set-Content -Path (Join-Path $BinDir 'tabi.cmd') -Value $shim -Encoding ascii
    Write-Ok "wrote $BinDir\tabi.cmd"
}

function Write-Config {
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    $cfg = Join-Path $ConfigDir 'providers.json'
    if ((Test-Path $cfg) -and -not $Force) { Write-Ok "keeping existing $cfg"; return }

    Write-Step 'Writing starter config'
    $json = @'
{
  "version": 1,
  "providers": [
    {
      "id": "provider-a",
      "label": "Provider A",
      "hosts": [{ "host": "api.example.com", "enabled": true, "note": "primary" }],
      "keys_file": "keys/provider-a-keys.txt",
      "hold": 0.80,
      "initial_guess": 120.0,
      "enabled": true,
      "bias": 0.0,
      "note": "edit host and keys_file, then restart"
    }
  ],
  "routing": {
    "slow_multiplier": 2.0,
    "min_samples": 3,
    "error_weight": 4.0,
    "streak_weight": 0.5,
    "streak_halflife_secs": 120,
    "breaker_trip": 3,
    "breaker_backoff_secs": [15, 45, 120],
    "missing_model_penalty": 50.0,
    "probe_heals_score": true,
    "session_stickiness": true,
    "sticky_escape_multiplier": 4.0
  }
}
'@
    Set-Content -Path $cfg -Value $json -Encoding UTF8
    Write-Ok "wrote $cfg"
    Write-Dim 'placeholder host — edit before starting'
}

function Add-ToPath {
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if ($userPath -and $userPath.Split(';') -contains $BinDir) {
        Write-Ok "$BinDir already on PATH"
        return
    }
    $new = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable('PATH', $new, 'User')
    Write-Ok "added $BinDir to your user PATH"
    Write-Dim 'open a new terminal for it to take effect'
}

# ── main ─────────────────────────────────────────────────────────────────────
Write-Host ''
Write-Host "KeyForge installer v$Version"
Write-Host ''
Write-Ok "platform: windows ($env:PROCESSOR_ARCHITECTURE)"

if ($Uninstall) { Invoke-Uninstall }

if ((Test-Path (Join-Path $BinDir 'keyforge.exe')) -and -not $Force) {
    Write-Warn2 "already installed at $BinDir"
    if (-not (Confirm-Action '  Reinstall?')) { Write-Host 'nothing to do'; exit 0 }
}

if (-not $NoBuild) { Assert-Rust; Assert-Linker }
Assert-Source
Invoke-Build
Install-Binary
Write-Launcher
Write-Config
Add-ToPath

Write-Host ''
Write-Host 'Installed.' -ForegroundColor Green
Write-Host ''
Write-Host 'Next:'
Write-Dim "1. mkdir `$HOME\keys ; notepad `$HOME\keys\provider-a-keys.txt"
Write-Dim "2. notepad $ConfigDir\providers.json"
Write-Dim '3. tabi start     # then: tabi status'
Write-Host ''
Write-Host 'Endpoints once running:'
Write-Dim "dashboard  http://127.0.0.1:$Port/"
Write-Dim "anthropic  http://127.0.0.1:$Port/v1/messages"
Write-Dim "openai     http://127.0.0.1:$Port/v1/chat/completions"
Write-Host ''
Write-Warn2 'Wi-Fi link sampling is Android-only and stays inert here — see docs/INSTALL.md'
Write-Host ''

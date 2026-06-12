# Host-side prep for the sandbox test rig: stage fonts, baduhan.exe, a
# portable Windows Terminal zip, and the guest bootstrap into the assets
# folder that sandbox-test.wsb maps. Run once (re-run to refresh the
# baduhan build under test).
param(
    [string]$Assets = "$env:USERPROFILE\term-compare\sandbox-assets",
    [string]$Results = "$env:USERPROFILE\term-compare\sandbox-results",
    [string]$BaduhanExe = "$env:USERPROFILE\.cache\cargo-target\term\release\baduhan.exe"
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force $Assets, "$Assets\fonts", $Results | Out-Null

# Fonts: the four faces baduhan uses.
$src = "$env:LOCALAPPDATA\Microsoft\Windows\Fonts"
foreach ($f in "Regular", "Bold", "Italic", "BoldItalic") {
    Copy-Item "$src\JetBrainsMonoNLNerdFont-$f.ttf" "$Assets\fonts" -Force
}

# Build under test.
Copy-Item $BaduhanExe "$Assets\baduhan.exe" -Force

# Guest bootstrap (the .wsb LogonCommand runs this copy).
Copy-Item "$PSScriptRoot\sandbox-guest-setup.ps1" "$Assets\guest-setup.ps1" -Force

# Portable Windows Terminal (cached; delete terminal.zip to re-download).
if (-not (Test-Path "$Assets\terminal.zip")) {
    Write-Output "downloading portable Windows Terminal..."
    $rel = gh api repos/microsoft/terminal/releases/latest | ConvertFrom-Json
    $url = ($rel.assets | Where-Object name -match 'WindowsTerminal_.*_x64\.zip$' |
        Select-Object -First 1).browser_download_url
    if (-not $url) { throw "no portable WT zip in latest release" }
    Invoke-WebRequest $url -OutFile "$Assets\terminal.zip"
}
Write-Output "staged to $Assets - launch with: WindowsSandbox.exe $PSScriptRoot\sandbox-test.wsb"

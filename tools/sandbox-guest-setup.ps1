# Runs INSIDE Windows Sandbox at logon (staged to C:\assets\guest-setup.ps1
# by sandbox-prepare.ps1). Installs fonts + portable Windows Terminal, then
# runs the full comparison suite and writes results to the mapped C:\results.
$ErrorActionPreference = "Continue"
Start-Transcript -Path C:\results\guest-run.log -Force

# --- fonts: install the Nerd Font so both terminals can use it ---
$shell = New-Object -ComObject Shell.Application
$fontsNs = $shell.Namespace(0x14)
Get-ChildItem C:\assets\fonts\*.ttf | ForEach-Object {
    $fontsNs.CopyHere($_.FullName, 0x14)   # no-UI, yes-to-all
}
Start-Sleep -Seconds 5

# --- portable Windows Terminal ---
Expand-Archive C:\assets\terminal.zip -DestinationPath C:\wt -Force
$wtExe = (Get-ChildItem C:\wt -Recurse -Filter WindowsTerminal.exe | Select-Object -First 1).FullName
New-Item -ItemType File -Force "$(Split-Path $wtExe)\.portable" | Out-Null

# --- baduhan: local copy (mapped folders can be slow), bash profile config ---
Copy-Item C:\assets\baduhan.exe C:\baduhan.exe
New-Item -ItemType Directory -Force "$env:APPDATA\baduhan" | Out-Null
@'
{
  "font_family": "JetBrainsMonoNL NF",
  "font_size": 13.0,
  "restore_session": false,
  "default_profile": "Git Bash",
  "profiles": [
    { "name": "Git Bash", "command": ["C:\\Git\\bin\\bash.exe", "-i", "-l"] }
  ]
}
'@ | Out-File "$env:APPDATA\baduhan\settings.json" -Encoding ascii

# Home dir for bash (~ = guest user profile); HOME not set by default.
$env:HOME = $env:USERPROFILE

# --- run the suite ---
$tc = "C:\repo-tools\term-compare.ps1"
$common = @{ OutDir = "C:\results"; BaduhanExe = "C:\baduhan.exe"
             BashExe = "C:\Git\bin\bash.exe"; WtExe = $wtExe }
& $tc -Target baduhan -Scenario card @common
& $tc -Target wt      -Scenario card @common
& $tc -Target baduhan -Scenario keys @common
& $tc -Target wt      -Scenario keys @common

# keyecho text outputs land in ~; copy them out too.
Copy-Item "$env:USERPROFILE\keyecho-*.txt" C:\results -ErrorAction SilentlyContinue
"SUITE COMPLETE $(Get-Date)" | Out-File C:\results\DONE.txt -Encoding ascii
Stop-Transcript

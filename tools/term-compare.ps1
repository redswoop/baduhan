# Visual comparison harness: launch baduhan or Windows Terminal running the
# same test content, capture the window, compare the PNG pairs.
#
#   .\tools\term-compare.ps1 -Target baduhan -Scenario card
#   .\tools\term-compare.ps1 -Target wt      -Scenario keys
#
# Scenarios: card = glyph test card (tools/testcard.sh, copied to ~)
#            keys = chord battery into tools/keyecho.sh ('/' separates
#                   chords; needs an UNLOCKED desktop for modifier state)
#
# Capture uses PrintWindow(PW_RENDERFULLCONTENT), which works on a locked
# workstation. Text input to baduhan goes via PostMessage(WM_CHAR), which
# also works locked; WT gets its command on the command line instead.
param(
    [Parameter(Mandatory)] [ValidateSet("baduhan", "wt")] [string]$Target,
    [Parameter(Mandatory)] [ValidateSet("card", "keys")] [string]$Scenario,
    [string]$OutDir = "$env:USERPROFILE\term-compare",
    [string]$BaduhanExe = "$env:USERPROFILE\.cache\cargo-target\term\release\baduhan.exe",
    [string]$BashExe = "C:\Program Files\Git\bin\bash.exe",
    [string]$WtExe = "wt"   # portable WindowsTerminal.exe in the sandbox
)
$ErrorActionPreference = "Stop"
$bash = $BashExe

if (-not ("TC" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public struct TCRECT { public int Left, Top, Right, Bottom; }
public static class TC {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out TCRECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, UIntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern short VkKeyScanW(char ch);
}
"@
}
Add-Type -AssemblyName System.Drawing
[TC]::SetProcessDPIAware() | Out-Null
New-Item -ItemType Directory -Force $OutDir | Out-Null

function Test-Locked { [bool](Get-Process LogonUI -ErrorAction SilentlyContinue) }

# Focus-free typing: WM_CHAR per character, WM_KEYDOWN/UP for Enter.
function Post-Text([IntPtr]$h, [string]$s) {
    foreach ($c in $s.ToCharArray()) {
        if ($c -eq "`r") {
            [TC]::PostMessageW($h, 0x0100, [UIntPtr]::new(0x0D), [IntPtr]::Zero) | Out-Null
            [TC]::PostMessageW($h, 0x0101, [UIntPtr]::new(0x0D), [IntPtr]::Zero) | Out-Null
        } else {
            [TC]::PostMessageW($h, 0x0102, [UIntPtr]::new([uint64][char]$c), [IntPtr]::Zero) | Out-Null
        }
        Start-Sleep -Milliseconds 10
    }
}
# Focus-based chord (modifiers via thread keyboard state — unlocked only).
function Send-Chord([byte[]]$mods, [byte]$vk) {
    foreach ($m in $mods) { [TC]::keybd_event($m, 0, 0, [UIntPtr]::Zero) }
    Start-Sleep -Milliseconds 30
    [TC]::keybd_event($vk, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 30
    [TC]::keybd_event($vk, 0, 2, [UIntPtr]::Zero)
    foreach ($m in $mods) { [TC]::keybd_event($m, 0, 2, [UIntPtr]::Zero) }
    Start-Sleep -Milliseconds 80
}
function Capture-Window([IntPtr]$h, [string]$path) {
    $r = New-Object TCRECT
    [TC]::GetWindowRect($h, [ref]$r) | Out-Null
    $w = $r.Right - $r.Left
    $ht = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $ht)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $dc = $g.GetHdc()
    [TC]::PrintWindow($h, $dc, 2) | Out-Null   # PW_RENDERFULLCONTENT
    $g.ReleaseHdc($dc)
    $g.Dispose()
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Output "captured $path"
}

# Stage scripts where ~ resolves in git-bash.
Copy-Item "$PSScriptRoot\testcard.sh", "$PSScriptRoot\keyecho.sh" $env:USERPROFILE -Force

$script = if ($Scenario -eq "card") { "testcard.sh" } else { "keyecho.sh" }
if ($Scenario -eq "keys" -and (Test-Locked)) {
    throw "keys scenario needs an unlocked desktop (modifier injection)"
}

# --- launch ---
if ($Target -eq "baduhan") {
    $proc = Start-Process $BaduhanExe -WorkingDirectory $env:USERPROFILE -PassThru
    Start-Sleep -Seconds 4
    $hwnd = $proc.MainWindowHandle
    Post-Text $hwnd "bash ~/$script`r"
} else {
    # Run the script directly: no input injection needed for the card.
    # One pre-quoted string: PS5.1 Start-Process drops quoting on array args
    # with spaces, and wt's CLI splits on bare ';' — use '&&' inside -c.
    # Sleep instead of respawning a login shell: a second MOTD would scroll
    # the card off. Capture happens during the sleep; the tab then closes.
    $wtArgs = "-d `"$env:USERPROFILE`" -- `"$bash`" -i -l -c `"bash ~/$script && sleep 20`""
    Start-Process $WtExe -ArgumentList $wtArgs
    $deadline = (Get-Date).AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 500
        $proc = Get-Process WindowsTerminal -ErrorAction SilentlyContinue |
            Sort-Object StartTime -Descending | Select-Object -First 1
    } until (($proc -and $proc.MainWindowHandle -ne 0) -or (Get-Date) -gt $deadline)
    Start-Sleep -Seconds 3
    $hwnd = $proc.MainWindowHandle
}
if (-not $hwnd -or $hwnd -eq 0) { throw "no window handle for $Target" }

# --- scenario ---
if ($Scenario -eq "card") {
    Start-Sleep -Seconds 6   # card prints + spinner runs ~1.5s
    Capture-Window $hwnd "$OutDir\$Target-card.png"
} else {
    [TC]::SetForegroundWindow($hwnd) | Out-Null
    Start-Sleep -Seconds 2
    $VK = @{ ENTER = 0x0D; TAB = 0x09; BS = 0x08; ESC = 0x1B; UP = 0x26; HOME = 0x24
             END = 0x23; DEL = 0x2E; F5 = 0x74; A = 0x41; B = 0x42; SLASH = 0xBF }
    $CTRL = [byte[]]@(0x11); $SHIFT = [byte[]]@(0x10); $ALT = [byte[]]@(0x12); $NONE = [byte[]]@()
    $battery = @(
        @($NONE,  $VK.ENTER), @($SHIFT, $VK.ENTER), @($CTRL, $VK.ENTER),
        @($NONE,  $VK.TAB),   @($SHIFT, $VK.TAB),
        @($NONE,  $VK.BS),    @($CTRL,  $VK.BS),
        @($NONE,  $VK.ESC),
        @($NONE,  $VK.UP),    @($CTRL,  $VK.UP),
        @($NONE,  $VK.HOME),  @($NONE,  $VK.END), @($NONE, $VK.DEL),
        @($NONE,  $VK.F5),    @($CTRL,  $VK.A),   @($ALT,  $VK.B)
    )
    foreach ($chord in $battery) {
        Send-Chord $chord[0] ([byte]$chord[1])
        Send-Chord $NONE ([byte]$VK.SLASH)
    }
    Start-Sleep -Seconds 1
    Capture-Window $hwnd "$OutDir\$Target-keys.png"
    Send-Chord $NONE 0x51   # q quits keyecho
}

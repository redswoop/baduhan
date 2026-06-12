# Drive an EXISTING terminal window without focus: post text via WM_CHAR
# (works on a locked desktop) and capture via PrintWindow.
#
#   .\tools\term-drive.ps1 -ProcId 13396 -Text "bash ~/testcard.sh`r" -Wait 6 -Out card.png
param(
    [Parameter(Mandatory)] [int]$ProcId,
    [string]$Text = "",
    # Virtual-key codes posted as WM_KEYDOWN/UP after $Text, a '/' WM_CHAR
    # between each (separator in keyecho.sh output). Modifier-less only.
    [int[]]$VKeys = @(),
    [int]$Wait = 2,
    [string]$Out = ""
)
$ErrorActionPreference = "Stop"

if (-not ("TD" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public struct TDRECT { public int Left, Top, Right, Bottom; }
public static class TD {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out TDRECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, UIntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
}
Add-Type -AssemblyName System.Drawing
[TD]::SetProcessDPIAware() | Out-Null

$proc = Get-Process -Id $ProcId
$h = $proc.MainWindowHandle
if ($h -eq 0) { throw "process $ProcId has no main window" }

foreach ($c in $Text.ToCharArray()) {
    if ($c -eq "`r") {
        [TD]::PostMessageW($h, 0x0100, [UIntPtr]::new(0x0D), [IntPtr]::Zero) | Out-Null
        [TD]::PostMessageW($h, 0x0101, [UIntPtr]::new(0x0D), [IntPtr]::Zero) | Out-Null
    } else {
        [TD]::PostMessageW($h, 0x0102, [UIntPtr]::new([uint64][char]$c), [IntPtr]::Zero) | Out-Null
    }
    Start-Sleep -Milliseconds 10
}

foreach ($vk in $VKeys) {
    [TD]::PostMessageW($h, 0x0100, [UIntPtr]::new([uint64]$vk), [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 30
    [TD]::PostMessageW($h, 0x0101, [UIntPtr]::new([uint64]$vk), [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 30
    [TD]::PostMessageW($h, 0x0102, [UIntPtr]::new(0x2F), [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 30
}

if ($Out) {
    Start-Sleep -Seconds $Wait
    $r = New-Object TDRECT
    [TD]::GetWindowRect($h, [ref]$r) | Out-Null
    $bmp = New-Object System.Drawing.Bitmap(($r.Right - $r.Left), ($r.Bottom - $r.Top))
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $dc = $g.GetHdc()
    [TD]::PrintWindow($h, $dc, 2) | Out-Null
    $g.ReleaseHdc($dc)
    $g.Dispose()
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Output "captured $Out"
}

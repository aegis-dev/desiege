# usage: click.ps1 x y [delayBeforeSeconds]  (native screen coords)
param([int]$X, [int]$Y, [int]$Delay = 0)
Add-Type @'
using System; using System.Runtime.InteropServices;
public class M { [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, UIntPtr e); }
'@
[M]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
Start-Sleep $Delay
[M]::SetCursorPos($X, $Y) | Out-Null; Start-Sleep -Milliseconds 300
[M]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 120; [M]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)

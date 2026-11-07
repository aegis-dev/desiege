# Bring DungeonSiege's window to the foreground (ALT-press trick lifts the focus-steal lock).
Add-Type @'
using System; using System.Runtime.InteropServices;
public class F { [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, uint f, UIntPtr e); }
'@
$p = Get-Process DungeonSiege -ErrorAction SilentlyContinue | Select-Object -First 1
if ($p) { [F]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero); [F]::SetForegroundWindow($p.MainWindowHandle) | Out-Null; [F]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero); "focused $($p.MainWindowHandle)" } else { "no game" }

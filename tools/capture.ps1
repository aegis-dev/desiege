# Screen-capture the primary monitor (works for windowed/borderless, black for exclusive fullscreen)
param([string]$Out)
Add-Type @'
using System; using System.Runtime.InteropServices;
public class D6 { [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v); [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i); }
'@
[D6]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null; $w=[D6]::GetSystemMetrics(0); $h=[D6]::GetSystemMetrics(1)
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap $w, $h; [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen(0,0,0,0,$bmp.Size)
$s = New-Object System.Drawing.Bitmap $bmp, ([int]($w/2)), ([int]($h/2)); $s.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
"screen=${w}x${h} center=$($bmp.GetPixel([int]($w/2),[int]($h/2)))"

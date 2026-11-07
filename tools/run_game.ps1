# usage: run_game.ps1 <seconds> <scale> <vw> <vh> <snapshot frames> [extra game args...]
param([int]$Seconds, [string]$Scale, [int]$Vw, [int]$Vh, [string]$Snap)
$rt = "$env:APPDATA\dev.desiege.launcher\runtime"; $g = "C:\Program Files (x86)\Steam\steamapps\common\Dungeon Siege 1"
$out = "$env:LOCALAPPDATA\Desiege"; Remove-Item "$out\snap_*.bmp", "$out\snap_*.png" -ErrorAction SilentlyContinue
$env:__COMPAT_LAYER = "HighDpiAware"; $env:DESIEGE_UI_SCALE = $Scale; $env:DESIEGE_SNAPSHOT = $Snap; $env:DESIEGE_EXPERIMENT = $env:EXP; $env:DESIEGE_DXWRAPPER = (Get-Content "$rt\dxwrapper.path")
$a = @("`"$rt\desiege.dll`"", "`"$g\DungeonSiege.exe`"", "width=$Vw", "height=$Vh", "bpp=32", "fullscreen=true", "nointro=true", "mod_paths=$env:APPDATA\dev.desiege.launcher\mods") + $args + @($env:EXTRA -split " " | Where-Object { $_ })
$p = Start-Process "$rt\desiege-loader.exe" -ArgumentList $a -WorkingDirectory $g -PassThru -NoNewWindow
Start-Sleep $Seconds
Get-Process DungeonSiege -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 1
Add-Type -AssemblyName System.Drawing
Get-ChildItem "$out\snap_*.bmp" | ForEach-Object { $b = [System.Drawing.Image]::FromFile($_.FullName); $s = New-Object System.Drawing.Bitmap $b, ([int]($b.Width/2)), ([int]($b.Height/2)); $s.Save(($_.FullName -replace '\.bmp$','.png')); $b.Dispose(); $_.Name }

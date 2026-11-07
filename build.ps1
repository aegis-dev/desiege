# Builds everything and puts the 32-bit game-side files next to the launcher.
# Output: target\release\{desiege-launcher.exe, desiege.dll, desiege-loader.exe, ddraw-stub.dll}
cargo build -p desiege -p ddraw-stub --release --target i686-pc-windows-msvc --lib --bins
if ($LASTEXITCODE -ne 0) { exit 1 }
cargo build -p desiege-launcher --release
if ($LASTEXITCODE -ne 0) { exit 1 }
$r = "target\i686-pc-windows-msvc\release"
Copy-Item "$r\desiege.dll", "$r\desiege-loader.exe" target\release\
Copy-Item "$r\ddraw.dll" target\release\ddraw-stub.dll
Remove-Item target\release\ddraw.dll -ErrorAction SilentlyContinue  # from older builds

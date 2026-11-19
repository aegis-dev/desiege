# Desiege

**Desiege is an open-source launcher and runtime patch that makes Dungeon Siege run well on
modern Windows, adding native-resolution widescreen, UI scaling, borderless fullscreen and
high-frame-rate rendering without modifying any game files.**

Dungeon Siege (Gas Powered Games, 2002) is one of the great action RPGs, but on a current PC
it crashes at high resolutions, renders its interface at postage-stamp size, and runs far
slower than it should. Desiege fixes that from the outside: it never edits, replaces or
redistributes anything that belongs to the game. Everything happens in memory while the game
runs, and turning Desiege off gives you the untouched original back.

> **Status:** early but very playable. Developed and tested on Windows 11 with the Steam
> release of Dungeon Siege 1 (v1.11.1.1486) and an NVIDIA GPU. Other setups should work but
> have seen little testing; reports are very welcome.

---

## Features

- **Native resolution, including widescreen and above 2048 px.** Windows' built-in Direct3D 7
  cannot render anything wider than 2048 pixels; Desiege routes the game through
  [dxwrapper](https://github.com/elishacloud/dxwrapper) (Direct3D 7 → Direct3D 9) so 1440p,
  ultrawide and 4K work. The launcher picks your monitor's native resolution by default.
- **UI scaling (125 % – 200 %).** The game's interface was designed for 640×480 and is tiny at
  modern resolutions. Desiege runs the game at a lower *logical* resolution so the UI lays out
  larger, while the 3D world is still rendered at full native resolution. Text and icons are
  scaled crisply at whole-number scales. The mouse is remapped so it feels exactly like the
  original.
- **Borderless fullscreen.** Instant Alt-Tab, no monitor mode switches (even the 4:3 menus are
  shown centred at native resolution), plays nicely with multi-monitor setups.
- **Much higher frame rates.** On the test machine the game went from roughly 35–45 fps to
  160–180 fps in-game at 2560×1440. Desiege removes three GPU→CPU stalls the translation layer
  introduced: the software cursor, character shadows and dynamic vertex buffers (details below).
- **GPU-rendered character shadows with selectable quality** (64 px – 512 px; the original was
  64 px), with optional soft edges.
- **Graphics options that keep the original look:** anisotropic texture filtering, MSAA,
  full detail on GPUs the game's 2002 tables call "slow" (today's AMD cards among them), and
  32-bit colour and depth everywhere. Each one can be switched off. Before/after screenshots:
  [`docs/screenshots`](docs/screenshots).
- **Correct behaviour on modern hardware:** video memory above 2 GB is reported in a way the
  game understands, so it no longer refuses to create a 3D device.
- **No changes to your game installation.** By default Desiege injects itself at launch; the
  game folder stays byte-for-byte identical. An optional drop-in mode exists for people who want
  to start the game straight from Steam.
- **Mods load from Desiege's own folder.** The launcher points the game's `mod_paths` at
  `%APPDATA%\dev.desiege.launcher\mods`, so `.dsmod` files can live there instead of in the game
  directory.
- **Small and dependency-light.** The launcher is a ~5 MB [Tauri](https://tauri.app) app that uses
  the WebView2 runtime already present on Windows 10/11.

## Requirements

- Windows 10 or 11 (64-bit).
- **A legally owned copy of Dungeon Siege 1.** The Steam and GOG releases are detected
  automatically; other releases (retail CD) should work if you point the launcher at the game
  folder. See [GOG release](#gog-release) for what differs there.
- Microsoft Edge WebView2 Runtime (preinstalled on Windows 11 and current Windows 10).
- Internet access once, if you let the launcher download dxwrapper (strongly recommended; see
  below).

## Getting started

There are no prebuilt releases yet, so for now Desiege is built from source.

### Building

You need:

- [Rust](https://rustup.rs) (stable) with the 32-bit Windows target:
  `rustup target add i686-pc-windows-msvc`
- Visual Studio 2022 (or the Build Tools) with the **Desktop development with C++** workload.

Then, from the repository root in PowerShell:

```powershell
.\build.ps1
```

This builds the 32-bit game-side components and the launcher, and places everything in
`target\release\`:

| File                   | What it is                                                         |
| ---------------------- | ------------------------------------------------------------------ |
| `desiege-launcher.exe` | The launcher you run.                                              |
| `desiege.dll`          | The in-game patch (all fixes live here).                           |
| `desiege-loader.exe`   | Small 32-bit helper that starts the game and injects `desiege.dll`. |
| `ddraw-stub.dll`       | Optional drop-in `ddraw.dll` for the “game folder” launch method.   |

Keep these four files together.

### First launch

1. Run `target\release\desiege-launcher.exe`.
2. The launcher offers to download **dxwrapper**, the open-source Direct3D 7 → 9 translation
   layer Desiege relies on for resolutions above 2048 px and for most of the performance work.
   It shows links to the source code, the exact release and its licence before downloading. The
   download is a pinned version, verified against a SHA-256 checksum, and is stored in the
   launcher's data folder, never in the game folder. You can decline; the game will then be
   limited to resolutions up to 2048 px wide.
3. Check the game folder (auto-detected for Steam), choose your settings and press **PLAY**.

## Settings

| Setting            | Options                                                                     | Notes |
| ------------------ | --------------------------------------------------------------------------- | ----- |
| **Width / Height** | any; **Native** fills in your monitor's resolution                          | The resolution of the picture you see. |
| **Display mode**   | Fullscreen (exclusive), Borderless window, Windowed                         | Borderless needs dxwrapper. Exclusive gives the highest frame rate; borderless gives instant Alt-Tab. |
| **UI scale**       | Off, 125 %, 150 %, 175 %, 200 %                                             | Fullscreen and borderless only. 200 % on a 1440p screen makes the UI look like it did at 720p. |
| **Texture filtering** | Anisotropic 16× (default), 8×, 4×, Trilinear, Original                   | Sharper ground textures at an angle and in the distance. Practically free. |
| **Anti-aliasing**  | Off (default), MSAA 2×, 4×, 8×                                              | Smooth edges on characters, trees and buildings. Needs dxwrapper. Expensive on integrated graphics (Radeon 680M at 2880×1620: about 115 → 55 fps). |
| **Shadow quality** | Low (64), Normal (128), High (256), Ultra (512), Original method            | Resolution of character shadows. “Original method” turns the GPU shadow redirect off (slow; for troubleshooting). |
| **Soft shadow edges** | on (default) / off                                                       | Blurs the shadow edges slightly (no more stair steps, most visible at 256 px and up). |
| **Full detail on every GPU** | on (default) / off                                                | Clears the 2002 “slow card” flags (`simple_render`, `no_complex_shadows`, `below_min_spec`) in the hardware table. Unlisted AMD GPUs fall under the old ATI defaults, which set all three. |
| **32-bit textures and depth** | on (default) / off                                               | Hides 16-bit texture and depth formats from the game and clears `only_16_bit` (set for Intel GPUs). On NVIDIA and AMD the game already picks 32-bit, so this changes nothing there. |
| **Launch method**  | Inject (default), ddraw.dll in game folder                                  | See below. |

### Launch methods

- **Inject (default).** `desiege-loader.exe` starts the game suspended, loads `desiege.dll` into
  it and lets it run. Nothing is written to the game folder. You must start the game through the
  launcher.
- **ddraw.dll in game folder.** The launcher copies a tiny stub named `ddraw.dll` into the game
  folder. Windows loads it in place of the system `ddraw.dll`; the stub then loads `desiege.dll`
  from the launcher's data folder. This lets you start the game directly from Steam and still get
  all fixes (using the settings from your last launcher run). If `desiege.dll` is missing, the stub
  falls back to the normal system `ddraw.dll`, so the game still runs, unmodded. The launcher
  recognises its own stub and will never overwrite or delete a `ddraw.dll` that belongs to
  another mod. Switching back to *Inject* removes the stub on the next launch.

## How it works

Desiege's in-game component, `desiege.dll`, sits between the game and DirectDraw/Direct3D 7
and between the game and a handful of Windows APIs. The main pieces:

- **Device and memory fixes.** Video memory above 2 GB is capped to a value the game's signed
  32-bit arithmetic can handle; otherwise it picks no hardware tier and fails to create a 3D
  device.
- **Resolution table.** The game only accepts resolutions listed in its 2002 hardware table
  (`system_detail.gas`, up to 1024×768). Desiege intercepts the game opening that file and serves
  a copy, in `%LOCALAPPDATA%\Desiege`, with your resolution added. The original file is never
  touched.
- **Resolution decoupling (UI scale, borderless).** The game is told to run at a smaller virtual
  resolution, while the real back buffer and depth buffer are created at native size. 3D
  viewports are scaled up so the world renders at full resolution; pre-transformed 2D geometry
  (the whole UI) is scaled; and the mouse, cursor position and window metrics are mapped back so
  the game only ever sees the virtual resolution. Modes with a different aspect ratio (the 4:3
  menus) are centred with black bars.
- **Cursor overlay.** The game draws its cursor by reading the screen under the mouse back to the
  CPU, blending the cursor in and writing it back, twice per frame, which stalls the GPU.
  Desiege instead hands the game a plain black or white background on alternate frames,
  reconstructs the exact cursor image and transparency from one of each, and draws it as an
  ordinary textured quad.
- **GPU shadows.** For each character the game drew a silhouette into a corner of the screen and
  copied it into a system-memory texture, which forced a full-frame read-back per character per
  frame. Desiege redirects that drawing into a video-memory render target of the selected size and
  substitutes it whenever the game binds the shadow texture.
- **Dynamic vertex buffers.** The game streams most geometry through a ring buffer using the
  correct “no overwrite” locking pattern, but the dxwrapper translation layer dropped that hint and
  re-uploaded the whole buffer with a sync on every append. Desiege mirrors those buffers in system
  memory and issues draw-from-memory calls covering only the vertices used.
- **Graphics options** (`src/gfx.rs`). Texture filtering upgrades the sampler states the game
  sets (linear → anisotropic, point mip → linear mip) and supplies the anisotropy level, which
  the game never sets. Full detail and 32-bit formats are done in the patched
  `system_detail.gas` copy and by filtering the texture and depth format enumerations. Soft
  shadows average 12 offset copies of each GPU shadow texture into a second render target
  (a blur radius of 0.7 % of the texture). MSAA is dxwrapper's `AntiAliasing` with
  `DdrawAllowMultiSampling`; without the latter, dxwrapper renders single-sampled and every
  texture comes out softer.
- **Borderless.** dxwrapper's `FullscreenWindowMode` turns exclusive fullscreen into a borderless
  window; Desiege keeps the monitor at its native mode throughout.

The launcher also passes the right command-line switches (`width`, `height`, `bpp`,
`fullscreen`, `mod_paths`), and starts the game DPI-aware so Windows' display scaling doesn't blur
or crop it.

## Where things are stored

| Location                                           | Contents |
| -------------------------------------------------- | -------- |
| `%APPDATA%\dev.desiege.launcher\settings.json`     | Launcher settings. |
| `%APPDATA%\dev.desiege.launcher\runtime\`          | The copy of `desiege.dll` and the loader that the game actually uses, plus small setting files (`ui_scale`, `shadow_res`, `borderless`, `dxwrapper.path`). |
| `%APPDATA%\dev.desiege.launcher\deps\dxwrapper\`   | The downloaded dxwrapper, its licence and the generated `dxwrapper.ini`. |
| `%APPDATA%\dev.desiege.launcher\mods\`             | Your `.dsmod` mods. |
| `%LOCALAPPDATA%\Desiege\`                          | `desiege.log` (diagnostics) and the patched copy of `system_detail.gas`. |

To uninstall, delete the launcher files and these folders. If you used the “ddraw.dll in game
folder” method, also delete `ddraw.dll` from the game folder (or switch back to *Inject* and
launch once).

## GOG release

The GOG release has the same game version as Steam (1.11.1.1486), packaged differently:

- **GOG ships its own renderer** as `ddraw.dll` in the game folder (a Direct3D 7 → 9 wrapper,
  configured with GOG's *DirectX Settings* tool, `dxcfg.exe`). When it loads, it also hooks the
  game's mouse input. With the *Inject* method, `desiege-loader` changes the game's `ddraw.dll`
  import to `desiege` in memory, before the game starts, so GOG's wrapper is never loaded and
  Desiege's tested path (system ddraw + dxwrapper) is used instead. Nothing in the game folder
  changes, and starting the game from GOG Galaxy still runs it exactly as GOG ships it.
- **GOG's exe caps the frame rate at 60 fps.** In the code that reads the game's `maxfps`
  setting, GOG's `DungeonSiege.exe` has a hard-coded 60 instead (the Steam exe reads the
  setting, which defaults to no cap). Desiege puts the setting back in memory: no cap, or
  whatever you pass as `maxfps=`.
- The *ddraw.dll in game folder* launch method isn't available, because that file name is taken
  by GOG's renderer. Desiege never overwrites it.
- Saves and settings live in `Documents\Dungeon Siege\`, as on Steam.

## Troubleshooting

- **First stop: the log.** `%LOCALAPPDATA%\Desiege\desiege.log` records every important decision
  and call result. Please attach it to bug reports.
- **The game shows “D3D Initialization Failure”** at resolutions above 2048 px wide: dxwrapper
  isn't installed. Start the launcher again and accept the download, or choose a resolution of
  2048 px wide or less.
- **Antivirus warnings.** The *Inject* method starts the game and loads a DLL into it, which some
  antivirus products treat as suspicious for unsigned programs. Windows Defender has not flagged
  it in testing. If yours does, use the “ddraw.dll in game folder” method, which involves no
  injection.
- **Something looks wrong with shadows:** set Shadow quality to *Original method* to compare.

### Debug switches

These environment variables are read by `desiege.dll` when the game starts. They're intended for
development and bug hunting; set them in the shell you start the launcher from.

| Variable                        | Effect |
| ------------------------------- | ------ |
| `DESIEGE_EXPERIMENT=profile`    | Samples the render thread for 10 s once in-game and logs where the time goes, per module and hot address. |
| `DESIEGE_EXPERIMENT=nodynvb`    | Disables the dynamic vertex buffer fix. |
| `DESIEGE_EXPERIMENT=vanilla`    | No fixes at all; only counts frames (`fps:` lines), for before/after measurements. Add `,uncap` to also lift GOG's 60 fps cap. |
| `DESIEGE_SNAPSHOT=300,900`      | Saves the native back buffer at those frame numbers to `snap_<n>.bmp` (exclusive fullscreen can't be screen-captured). |
| `DESIEGE_TRACE_FRAME=<n>`       | Logs every hooked graphics call of frame `<n>`. |
| `DESIEGE_DUMP_SHADOWS=1`        | Saves the first few captured shadow textures to `shadow_<n>.bmp`. |
| `DESIEGE_GFX=filter=0,soft_shadows=0` | Overrides graphics options from the launcher's `graphics` file (keys: `filter`, `hq_formats`, `max_detail`, `soft_shadows`, `msaa`). `filter=0,hq_formats=0,max_detail=0,soft_shadows=0` gives the game's own look. |

## Repository layout

```
launcher/            Tauri launcher: Rust backend (src/main.rs) and plain HTML/JS UI (ui/)
crates/desiege/      desiege.dll (all in-game fixes) and desiege-loader.exe
  src/lib.rs         DirectDraw/D3D7 hooks, VRAM cap, system_detail.gas redirect
  src/scale.rs       resolution decoupling, cursor overlay, GPU shadows, dynamic VBs, input
  src/gfx.rs         graphics options: texture filtering, detail flags, 32-bit formats
  src/profile.rs     built-in sampling profiler (debug)
  examples/          selfcheck (smoke test without the game) and rtsize (D3D7 size limit test)
crates/ddraw-stub/   the optional drop-in ddraw.dll
crates/tank/         reader for Dungeon Siege Tank archives (.dsres / .dsmod / .dsmap) + CLI
build.ps1            builds everything into target\release
```

Tests: `cargo test -p desiege-launcher -p tank` and
`cargo test -p desiege --target i686-pc-windows-msvc`. The `tank` test reads every file in your
installed `Logic.dsres` (skipped if the game isn't installed; override the location with
`DS_RESOURCES`).

## Roadmap

- Prebuilt, signed releases.
- Retail install detection.
- A Tank archive *writer* and a proper mod manager in the launcher.
- Field-of-view correction for ultrawide screens.
- Legends of Aranna testing.

## Contributing

Issues and pull requests are welcome. Bug reports are most useful with your
`desiege.log`, your settings, your GPU and Windows version, and where in the game the problem
happens. Please keep in mind the project's ground rules: no game files or assets in the
repository, nothing that modifies the user's game installation without them choosing it, and no
closed-source dependencies.

## Acknowledgements

- **Gas Powered Games** and **Chris Taylor** for making Dungeon Siege, and Microsoft Game Studios
  for publishing it.
- **Elisha Riedlinger** for [dxwrapper](https://github.com/elishacloud/dxwrapper), which makes
  modern Direct3D 7 translation possible. dxwrapper is a separate project under its own
  (zlib-style) licence; Desiege does not include it and downloads it only with your consent.
- The [Tauri](https://tauri.app) and Rust communities.
- The Dungeon Siege modding community, which has kept the game alive for over two decades.

## Licence

Desiege is free software, licensed under the **GNU General Public License v3.0**; see
[LICENSE](LICENSE). In short: you may use, study, share and modify it, and if you distribute
modified versions you must make their source available under the same licence.

This licence covers Desiege's own source code only. It grants no rights to Dungeon Siege or
any of its content, and it doesn't change the licence of third-party software Desiege works with,
such as dxwrapper.

## Disclaimer

**Desiege is an unofficial, fan-made project.** It is not affiliated with, endorsed by,
sponsored by or approved by Square Enix, Microsoft, Gas Powered Games, Wargaming, Valve, GOG or
any other rights holder.

*Dungeon Siege* and related names, logos and artwork are trademarks or registered trademarks of
their respective owners. They are used here only to identify the game Desiege is compatible
with.

**You need your own legally obtained copy of Dungeon Siege.** This repository contains no game
code, data, assets or other copyrighted material from the game, and it never will. Desiege does
not circumvent copy protection or DRM, and it must not be used to play copies you don't own.

Desiege works by loading code into the game's process and intercepting its calls to Windows and
DirectX. It does not modify game files, but it does change how the running game behaves.
Multiplayer has not been tested, and you should not use Desiege in any online setting where
third-party modifications are disallowed.

**This software is provided “as is”, without warranty of any kind**, as set out in sections
15 and 16 of the GPL. Use it at your own risk; the authors are not liable for any damage,
data loss (keep backups of your saves) or other problems arising from its use.

# TODO: testing and support

Working checklist for things that need hardware, game versions or testers we don't have yet.
Everything so far was developed and tested on **Windows 11 + Steam release (v1.11.1.1486) +
NVIDIA RTX 3070 Ti**, plus the GOG release on an AMD Radeon 680M laptop (see GOG below).

## GPU vendors

The fixes in `desiege.dll` were measured on NVIDIA. Some of them depend on how dxwrapper and the
driver behave, so each one needs checking on other vendors.

### AMD (Radeon)

- [ ] Game starts, menus and in-game render correctly at native resolution (with dxwrapper).
- [ ] Without dxwrapper: does the 2048 px Direct3D 7 limit also apply? (`examples/rtsize.rs`
      answers this without starting the game.)
- [ ] VRAM cap: the reported video memory is sane and the game picks the top hardware tier
      (check `GetAvailableVidMem` / `GetCaps` lines in `desiege.log`).
- [ ] UI scale 125/150/200 %: UI, text, cursor, shadows, mouse feel.
- [ ] Cursor overlay: no white boxes or trails, including at screen edges and in the 4:3 menus.
      (Since 2026-11-21 the image and position come from the game's cursor object on the
      1.11.1.1486 exe, Steam and GOG; the black/white reconstruction is only a fallback.)
- [ ] GPU shadows (all quality levels and *Original method*): shadows present and correct.
- [ ] Dynamic vertex buffer fix: no missing or flickering geometry; compare with
      `DESIEGE_EXPERIMENT=nodynvb`.
- [ ] Frame rate before/after (`fps:` lines in the log), and a `DESIEGE_EXPERIMENT=profile` run.
- [ ] Borderless and exclusive fullscreen, Alt-Tab, multi-monitor.

### Intel (Arc and integrated)

- [ ] Same list as AMD.
- [ ] Note: the game's 2002 hardware table (`system_detail.gas`) has Intel-specific quirks
      (`manual_mouse_copy = true`, others). Check which ones the game applies and whether any of
      them conflict with our fixes.
- [ ] Laptops with hybrid graphics (Intel + NVIDIA/AMD): which GPU the game ends up on, and that
      `native_resolution()` / mouse mapping are right with display scaling.

## Game versions and stores

### GOG

Tested 2026-11-19 on a laptop: Windows 11, AMD Ryzen 7 7735U with Radeon 680M (integrated),
2880x1620 @ 120 Hz, on AC power. GOG Galaxy install, version tag "1.11.1 AMD RDNA fix".

- [x] Exe version and layout: `DungeonSiege.exe` 1.11.1.1486 (SHA-256 `41F14B14...38A02`),
      no ASLR. GOG adds its own D3D7 -> D3D9 wrapper as `ddraw.dll` (+ `dxcfg.exe`/`dxcfg.ini`),
      `goggame-1185868626.*`, and a HIGHDPIAWARE AppCompat layer for the exe (HKLM and HKCU).
- [x] Launcher auto-detection: `HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\1185868626` -> `path`,
      then the default GOG folders.
- [x] GOG's `ddraw.dll` hooks the game's mouse input when it loads, so routing around its
      exports isn't enough. `desiege-loader` renames the exe's `DDRAW.dll` import to `desiege`
      in the suspended process (and puts the runtime folder on PATH), so it never loads.
- [x] The HIGHDPIAWARE layer shims GetCursorPos/SetCursorPos/GetClientRect, so the import slots
      don't hold user32's addresses; the input hooks now match imports by name.
- [x] GOG's exe hard-codes a 60 fps cap (`mov [esi+0x90], 60.0` + NOPs at 0x416177 where the
      Steam exe reads `maxfps`). desiege.dll restores the setting: no cap unless `maxfps=` is given.
- [x] `DungeonSiege.ini` and saves: `Documents\Dungeon Siege\`, same as Steam.
- [x] Compared with the Steam exe byte for byte (2026-11-21, both installed on the GOG laptop;
      Steam SHA-256 `C408EF77...`): same build (timestamp, size, no ASLR). GOG patches 19 small
      code spots: default resolution 640x480 -> 1024x768, the 60 fps cap, a code cave at
      0x477510 -> 0x631bd2 (the "AMD RDNA fix", touches `[SiegeEngine]+0xA8`), and a few
      jumps (0x41653d, 0x41d576, 0x64f6bc). None overlap anything desiege.dll reads or calls:
      the cursor object/routine, the light database calls and the globals are identical.
- [x] Steam in-game on the same laptop (2880x1620, UI 200 %, all graphics options but MSAA):
      AF, soft shadows, full detail, 32-bit formats, AO (INTZ), per-pixel lights
      (`lights_probe=1`: 6 lights, party light at the screen centre), sharpening, bloom, the
      FPS-counter code mod and the cursor object all work; 117-120 fps (vsync cap). The GOG fps
      fix correctly leaves the Steam exe alone. Not rerun on Steam: MSAA (dxwrapper-side, no
      exe dependence), exclusive fullscreen.
- [x] Drop-in `ddraw.dll` method: not possible (the name is GOG's renderer); the launcher says
      so and points to Inject.

Frame rate, same save (party standing in the first farm), harmonic mean of the last six
600-frame windows, two runs each:

| Configuration                                                         | Run 1 | Run 2 |
| --------------------------------------------------------------------- | ----- | ----- |
| GOG as shipped (GOG renderer, 1024x768 upscaled, 60 fps cap)          | 60.0  | 60.0  |
| GOG renderer, cap lifted (`DESIEGE_EXPERIMENT=vanilla,uncap`)         | 79.5  | 80.0  |
| Desiege, 2880x1620 native, UI 200 %, exclusive fullscreen, vsync on   | 120.0 | 120.0 |
| Same, vsync off (dxwrapper `ForceVsyncMode = 2`)                      | 213.1 | 197.5 |

### Retail CD (Dungeon Siege 1, Dungeon Siege Gold)

- [ ] Auto-detection via the original Microsoft registry keys (`DungeonSiege/1.0/EXE Path`
      appears in the exe's strings).
- [ ] Patched to 1.11 vs older versions.

### Legends of Aranna

Not sold digitally (Steam and GOG only have the base game); retail discs only. Needs a tester who
owns it.

- [ ] Detect `DungeonSiegeLoA.exe` and its install folder.
- [ ] Check its `system_detail.gas` / hardware table and UI interfaces; rerun the UI scale, cursor,
      shadow and vertex buffer checks.

## Other

- [ ] Multiplayer (LAN / direct IP; ZoneMatch is long gone).
- [ ] Profile paths with non-ASCII characters or spaces (`mod_paths=` quoting, the ANSI path for
      the `system_detail.gas` redirect).
- [ ] Relocated `Documents` folder (OneDrive), used by the drop-in resolution fallback.
- [ ] Windows 10.
- [ ] High refresh rates without VRR, and with vsync forced off in the driver.

## Mod manager (planned)

Decisions so far:

1. **Local library** in the launcher: add `.dsmod` / `.dsres` / `.dsmap` files or zips; stored in
   the launcher's data folder (never the game folder); enable/disable and load order. Maps via
   `map_paths`, mods via `mod_paths`; still to test: how to load `.dsres`-style mods (the
   community convention is to drop them in the game's `Resources` folder) without touching it.
2. **Install from a link**: a GitHub release URL (asset resolved through the GitHub API, version
   recorded, updates offered) or any direct download URL, verified against a SHA-256 when given.
3. **Curated index**: a `desiege-mods` repository with one JSON index (name, author, description,
   URL, SHA-256, licence, game), shown as one-click installs; new mods added by pull request.
4. **No Nexus Mods integration.**

### Code mods (later)

Running code in mods is allowed. Plan: `desiege.dll` exposes a small, versioned C API (a header
in this repo) for hooks and callbacks; code mods are DLLs loaded by `desiege.dll`. The launcher
must label mods that contain code (`.dll`, `.dsdll`, `.exe`) and require explicit per-mod trust
before enabling them.

## Graphics options (added 2026-11-19)

Developed and measured on the GOG laptop (Radeon 680M, 2880x1620, UI 200 %, **on battery**, so
absolute frame rates are pessimistic). Screenshots in `docs/screenshots`.

- [x] Texture filtering: the game sets linear min/mag and mostly linear mips itself, but never an
      anisotropy level. 16x AF: +23 % fine texture detail on a static grass field (Laplacian
      variance 90 -> 111; two baseline runs 90.0 / 89.6). Trilinear alone: 92.
- [x] Full detail: unlisted AMD GPUs get the 2002 ATI defaults (`simple_render`,
      `no_complex_shadows`, `below_min_spec` = true). Cleared, the game draws visibly more
      foliage. In-game about 112 fps vs 117 (borderless, 120 Hz vsync cap).
- [x] 32-bit formats: on this AMD the game already picks A8R8G8B8 / X8R8G8B8 textures and a
      D24S8 depth buffer, so no visible change. Matters for `only_16_bit` GPUs (Intel): **test there.**
- [x] Soft shadows: 12-tap blur, radius 0.7 % of the shadow texture. 2.5 % and 1.2 % washed thin
      limbs out. No measurable frame-rate cost.
- [x] MSAA via dxwrapper works in borderless and exclusive fullscreen, shadows and cursor
      included, and `DESIEGE_SNAPSHOT` still works. Needs `[Dd7to9] DdrawAllowMultiSampling = 1`, or
      every texture gets softer (ground detail 17.8 -> 15.1).
- [ ] MSAA cost: ~117 -> ~55 fps here, and **2x costs as much as 4x**, so it isn't the sample
      count. Suspect: dxwrapper resolves and restores the multisampled back buffer on every
      render-target switch (one per character shadow). Profile shows the time in GPU waits under
      Flip. Next: count RT switches per frame and compare fps with shadows hidden; measure on NVIDIA.
- [ ] All options on NVIDIA (RTX 3070 Ti, Steam) and Intel.

## Ambient occlusion (added 2026-11-19, experimental, default off)

Same laptop (Radeon 680M, 2880x1620, on battery).

- [x] Pipeline: hook IDirect3D9(Ex)::CreateDevice through our own IDirect3D9 object (shared
      vtables), depth buffer as INTZ, AO at the first 2D draw after the world, state block restore.
- [x] Device teardown: dxwrapper releases its device to zero although our objects hold refs on it;
      releasing ours afterwards crashed in ntdll (menu -> game). Fixed by freeing ours in a
      device Release hook when only our references would remain.
- [x] Cost: ~4.5 ms full resolution, ~1.5-2 ms at half resolution (112 -> ~96 fps, 120 Hz cap).
- [x] Shader compile ~1.7-2.5 s on first use; cached on disk after that (0 ms).
- [x] Borderless and exclusive fullscreen work; with MSAA it logs why and stays off.
- [ ] Faint crosshatch from the 4x4 rotation pattern inside dense foliage (visible in
      `ao_debug=1`, not really in the final image). Tried interleaved gradient noise instead of
      the 4x4 pattern (2026-11-19): worse, streaky grain everywhere, because the 4x4 blur cancels
      a 4x4 pattern exactly but can't smooth random noise. Next idea: when the depth-aware blur
      rejects taps at an edge, fill in with the nearest accepted rotation instead.
- [ ] Terrain triangle creases show as soft lines (real geometry, but the art never shaded them).
      Maybe a minimum angle, or a lower strength on terrain.
- [ ] Tune radius/strength over more areas (caves, towns, night); check NVIDIA and Intel (INTZ).
- [ ] MSAA + AO: would need a resolve of the multisampled depth (not possible in D3D9 without
      vendor hacks); left exclusive.

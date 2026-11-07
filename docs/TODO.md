# TODO: testing and support

Working checklist for things that need hardware, game versions or testers we don't have yet.
Everything so far was developed and tested on **Windows 11 + Steam release (v1.11.1.1486) +
NVIDIA RTX 3070 Ti**.

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

- [ ] Get a GOG copy (planned) and record its exe version and file layout.
- [ ] Launcher auto-detection: GOG Galaxy registry keys
      (`HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\<id>` → `path`) and the default install folder.
- [ ] Compare `DungeonSiege.exe` with the Steam one; if it differs, re-check the addresses our
      notes rely on (cursor lock pattern, vertex buffer ring, shadow corner sequence).
- [ ] Where GOG keeps `DungeonSiege.ini` (the resolution fallback for drop-in launches assumes
      `Documents\Dungeon Siege\`).
- [ ] Both launch methods, including starting from GOG Galaxy with the drop-in `ddraw.dll`.

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

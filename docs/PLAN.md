# Next session plan: single-file mods (data + code in one `.dsmod`)

Written at the end of the 2026-11-08 session so a fresh session can pick up without the
conversation. Read this, `README.md`, `docs/TODO.md` and `tools/README.md` first.

## Where we are

Released: **v0.1.0** (pre-release on GitHub; CI builds a zip on every `v*` tag).

Committed since v0.1.0, not released yet:

- **Tabbed launcher** (Play / Settings / Mods / About), settings save on change.
- **Local mod library** (`launcher/src/mods.rs`): drag `.dsmod`, `.dsmap`, `.dll` or `.zip` onto
  the window. Folders under `%APPDATA%\dev.desiege.launcher\`: `mods\` (passed to the game as
  `mod_paths`), `maps\` (`map_paths`), `code-mods\` (DLLs loaded by desiege.dll), `disabled\`
  (outside every scanned folder, because the game also scans subfolders).
- **Code mods**: C API in `crates/desiege/include/desiege_mod.h` (version 1: `log`, `on_frame`,
  `draw_text`, `screen_size`, `time_seconds`, `ui_scale`), loader in
  `crates/desiege/src/code_mods.rs`, overlay text rendering in `scale.rs` (`mod overlay`,
  shared quad drawing in `mod gpu`). A code mod exports `desiege_mod_init(const DesiegeApi*)`.
- **Example code mod** `mods/fps-counter` (Rust, uses only the C ABI): FPS right-aligned just
  left of the compass. Shipped in the release zip under `example-mods\` with the header.
- **Trust**: `.dll` mods always install disabled; enabling needs the trust dialog, and the
  backend refuses without `trusted: true`.
- `tools/`: scripts that drive the game for testing (see `tools/README.md`).

Still to confirm with the user (they were testing at the end of the session):

- [ ] FPS counter position next to the compass looks right (constants in
      `mods/fps-counter/src/lib.rs`: size `11 * ui_scale`, right edge `80 * ui_scale` from the
      screen edge, top `6 * ui_scale`).
- [ ] The trust flow in the real app: remove `fps_counter.dll` in the Mods tab, drag
      `target\release\example-mods\fps_counter.dll` onto the window, check it arrives disabled
      and switching it on shows the trust dialog.

## Decision: one mod = one file, all in one folder (like Minecraft jars)

A `.dsmod` is a Tank archive, so it can carry code too:

```
<normal game data paths ...>
desiege/mod.toml        optional manifest: name, version, author, description, homepage
desiege/code/*.dll      optional code mods, loaded by desiege.dll through desiege_mod.h
```

- The game ignores the `desiege/` subtree (to verify!) and loads the data as usual.
- Code-only mods may stay plain `.dll` files. Data-only `.dsmod`/`.dsmap` files keep working.
- Everything lives in **one** `mods\` folder again (plus `disabled\`); `code-mods\` and `maps\`
  go away if the game accepts `.dsmap` from `map_paths` pointing at the same folder (check), or
  `maps\` stays as is.
- Trust: a `.dsmod` that contains `desiege/code/*.dll` is a code mod: code badge, installs
  disabled, needs explicit trust. Trust is tied to the file's SHA-256 (store trusted hashes in
  the launcher settings), so an update that changes the file asks again. desiege.dll only loads
  code from files in `mods\`, and the launcher only puts trusted ones there.

## Steps

### 1. Tank writer (`crates/tank`)

- `Tank::write(folder) -> .dsmod/.dsmap` building the format the reader understands (see the
  module docs in `crates/tank/src/lib.rs`: header "DSig" "Tank", dirset/fileset, NString names
  padded to 4 bytes with a NUL, zlib chunks of 16 KiB with `extra` raw tail bytes).
- Header fields to fill properly: product/minimum version, priority, flags, creator id, GUID,
  index/data CRCs, build time, copyright/title/author/description strings. Copy what a real
  `.dsres` has (dump `DevLogic.dsres` and `Logic.dsres` headers and compare) and find out which
  CRC the game checks (the exe has `tank_ignore_version`; test with deliberately wrong CRCs).
- CLI: `tank pack <folder> -o out.dsmod`, `tank unpack <file> <folder>`.
- Tests: round trip (write, read back every file byte-identical), and reading a real archive,
  repacking it, and comparing file contents.
- **In-game acceptance test** (the important one): repack a small real mod or a tiny edited
  `.gas` file, put it in `mods\`, confirm in `desiege.log` (FindFirstFileA/CreateFileA lines)
  that the game opens it and that the change shows up in-game.

### 2. Embedded code in `desiege.dll`

- Make `crates/tank` usable from `crates/desiege` (it builds for i686 as-is; `miniz_oxide`).
- At startup (`code_mods::load_all`, already outside the loader lock): for every `.dsmod` in the
  `mod_paths` folder, open it with the tank reader, extract `desiege/code/*.dll` to
  `%LOCALAPPDATA%\Desiege\code-cache\<mod>-<sha256 prefix>\` (skip if already there) and load it
  like a plain code mod. Then plain `.dll` files.
- The folder comes from the game's command line (`mod_paths=`), so Steam launches with the
  drop-in stub need it too (the launcher could write it next to the DLL like `ui_scale`).
- Log every mod loaded / skipped.

### 3. Launcher

- On install/list: open `.dsmod` files with the tank reader; if `desiege/code/*.dll` exists,
  kind = "code" (show the data part too, e.g. badge "mod + code"); read `desiege/mod.toml` for a
  display name, version and author.
- Collapse `code-mods\` into `mods\`; migrate existing files.
- Trusted hashes in settings; re-ask on change.

### 4. Ship the FPS counter as a `.dsmod`

- `tank pack` a folder with `desiege/mod.toml` + `desiege/code/fps_counter.dll` into
  `FpsCounter.dsmod`; drop it into the launcher; trust; play; FPS shows. That proves the whole
  chain. Build it in `build.ps1`/CI and ship it in `example-mods\` instead of the bare DLL.

### 5. Docs and release

- README: a "Mods" section (installing, trust, the single-file format, writing a code mod with
  `desiege_mod.h`). A short `docs/MODDING.md` for mod authors.
- Release notes for v0.2.0 in `.github/release-notes/v0.2.0.md`; tag when the user says so.

## Later (from `docs/TODO.md`)

- Install mods from a GitHub release URL / direct URL; curated `desiege-mods` index. No Nexus.
- `.dsres`-style mods without touching the game folder.
- AMD / Intel / GOG / retail / Legends of Aranna testing.

## Practical notes (things that cost time this session)

- **The launcher refreshes `%APPDATA%\dev.desiege.launcher\runtime\desiege.dll` from the copy
  next to it** (`target\release\desiege.dll`) on every launch. After building the DLL alone,
  copy it to *both* places or the launcher puts the old one back. `build.ps1` does both copies
  for `target\release`.
- Release builds of the launcher fail with "Access is denied" while the launcher is running.
- Git Bash rewrites arguments like `/c` into paths: use `MSYS_NO_PATHCONV=1` when needed.
- Exclusive fullscreen can't be screen-captured (black); use `DESIEGE_SNAPSHOT=<frames>` or
  borderless mode. Synthetic clicks need the game in the foreground (`tools/focus.ps1`).
- The game uses relative mouse input (re-centres the cursor every frame); see `scale.rs` `Mouse`.
- Debug switches: `DESIEGE_EXPERIMENT=profile|nodynvb`, `DESIEGE_SNAPSHOT`, `DESIEGE_TRACE_FRAME`,
  `DESIEGE_DUMP_SHADOWS` (README "Debug switches").
- The user tests on Windows 11, RTX 3070 Ti, 2560x1440 at 125 % display scaling, 180 Hz VRR,
  two monitors, Steam copy of Dungeon Siege 1 v1.11.1.1486.

# Graphics roadmap

What's done, what's next, and why, for the visual side of Desiege. Rule of thumb for everything
here: render the original art better, don't replace it, and keep every effect switchable.

## Done

| Feature | Where | Notes |
| --- | --- | --- |
| Native resolution, UI scale, borderless, high frame rate | `scale.rs`, `surface.rs`, `device.rs` | |
| GPU character shadows, selectable size, soft edges | `shadow.rs` | |
| Anisotropic / trilinear filtering | `gfx.rs` | +23 % fine texture detail on far ground |
| Full detail on every GPU (2002 "slow card" flags cleared) | `gfx.rs`, `system_detail.rs` | more foliage on AMD |
| 32-bit textures and depth everywhere | `gfx.rs` | matters for Intel (`only_16_bit`) |
| MSAA via dxwrapper | launcher | needs `DdrawAllowMultiSampling`; costly on iGPUs |
| Ambient occlusion (experimental) | `d3d9.rs`, `ao.rs` | ~1.5-2 ms on a Radeon 680M; off with MSAA |
| Per-pixel dynamic lights (experimental) | `lights.rs`, `ao.rs` | see #1 below |
| Contrast-adaptive sharpening | `ao.rs` pass 4 | ~2 ms (mostly the copy); tap distance scales with resolution; works with MSAA |

The AO work gave us the building blocks for everything below: the real Direct3D 9 device under
dxwrapper, readable depth (INTZ), a pass inserted between the 3D world and the UI, state-block
save/restore, runtime-compiled cached shaders, and safe teardown when dxwrapper drops its device.

## How the game lights things (why the lighting looks dated)

The Siege engine lights on the CPU and bakes the result into vertex colours. Its geometry carries
no normals (the main stream is `XYZ | DIFFUSE | TEX1`) and Direct3D lighting is off. All the GPU
ever sees is one colour per vertex, interpolated across large terrain triangles: torch and spell
light comes out blotchy and follows triangle edges, and dynamic lights can't light anything
between vertices.

## Next

### 1. Per-pixel dynamic lights (working prototype, experimental, default off)

Draw the game's dynamic lights (torches, Fireshot and other spells, lanterns, the light around
the party) as smooth per-pixel pools on top of the game's own vertex lighting.

Spike result (2026-11-19): **yes**, the engine's light list is readable every frame, and markers
land exactly on the brazier and on the party light. Details in `src/lights.rs`:

- SiegeEngine singleton (exported) -> light database at +0xAC, an MSVC 6 `std::map` walked
  directly; per light the game's own functions return the 36-byte descriptor (type, ARGB colour,
  active, inner/outer radius, flickering intensity) and a node-relative position.
- "World" is the camera target's node; every terrain node stores its world position at +0x48
  and orientation at +0x54. Confirmed against the WORLD matrices the game renders nodes with.
- `tools/disasm.py` (capstone) disassembles exe exports/addresses; that's how this was found.

Rendering (`src/ao.rs`, pass 3): the frame is multiplied by `1 + strength * sum(light)`, light =
colour x intensity x smoothstep falloff (inner -> outer) x surface facing from depth normals.
Up to 16 nearest point lights. Cost not measurable at the 120 Hz cap on the 680M.

Open:
- [ ] Tuning: the party light (white, intensity 1) makes a strong "flashlight" pool; torches are
      subtle because (1 + light) can't brighten dark walls much. Options: per-type strength,
      a small additive term for coloured lights, matching the game's own falloff curve.
- [ ] Double lighting: the game already lit the vertices with the same lights. Could estimate and
      divide out the vertex contribution, or only add the part the vertices miss.
- [ ] Rotated nodes: the rotation convention (row vs column) is unverified; all nodes with lights
      seen so far are unrotated (the WORLD matrices show 90/180 degree nodes exist).
- [ ] Spell lights (Fireshot etc.) and night scenes; other exe builds (Steam: verify addresses).
- [ ] Testing notes: moving the mouse in a scripted run swings the camera to top-down; leave
      the cursor where it clicked *Continue* for reproducible views.

### 2. Bloom / glow on light sources

A soft halo around bright pixels (fire, glowing spells, sunlit water), from the final image only:
no engine knowledge needed. Low risk, easy to overdo: keep it subtle and switchable. Needs a
copy of the back buffer and a downsampled blur chain.

### 3. AO with colour bleeding

Extend the AO pass to pick up nearby colour (a green bush tints the ground below it). Needs one
copy of the back buffer per frame. Small step from the current AO.

### 4. Per-pixel distance / height fog

The game's fog is per vertex and bands. Depth-based fog per pixel, with the game's own fog colour
and distances (`MoodSetting::GetfogColor`, `GetfogNearDist`, `GetfogFarDist` are exported), would
make distances smoother and allow optional ground-hugging height fog.

### 5. Contrast-adaptive sharpening (CAS) - done

Launcher: Sharpening Off/Low/Medium/High. The game's textures are magnified, so their blur spans
several pixels at high resolutions; the taps are `height / 1080` pixels apart (bilinear) rather
than 1. Fine detail (Laplacian variance) on ground: 7.9 off, 33.7 Medium, 53.1 High.
Screenshot: `docs/screenshots/8-sharpening.jpg`. Open: the ~2 ms is mostly the copy and the
full-screen pass; sharing one copy with bloom (#2) would amortise it.

### Later / ideas

- Texture replacement by hash (community HD packs), see the original suggestions.
- HDR output.
- AO polish: foliage crosshatch, terrain crease lines, tuning in caves/towns/night (`TODO.md`).

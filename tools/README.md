# Development tools

Scripts used while developing Desiege to drive the game automatically. They're for development
only (not shipped) and assume a Steam install at the default path and the launcher's runtime
folder (`%APPDATA%\dev.desiege.launcher\runtime`) holding the `desiege.dll` / loader to test.
Copy a fresh build there first (`build.ps1`, then copy `target\release\desiege.dll`).

| Script         | What it does |
| -------------- | ------------ |
| `run_game.ps1` | `run_game.ps1 <seconds> <ui scale> <width> <height> "<snapshot frames>" [game args]`: starts the game through the loader with debug switches (`DESIEGE_UI_SCALE`, `DESIEGE_SNAPSHOT`, `DESIEGE_EXPERIMENT` from `$env:EXP`, extra game args from `$env:EXTRA`), waits, kills it, converts snapshots to PNG. |
| `focus.ps1`    | Brings the game window to the foreground (needed before synthetic input). |
| `click.ps1`    | `click.ps1 <x> <y>`: moves the mouse to native screen coordinates and clicks. |
| `capture.ps1`  | `capture.ps1 <out.png>`: screen capture (works in borderless/windowed; black in exclusive fullscreen; use `DESIEGE_SNAPSHOT` there). |
| `perf.sh`      | `perf.sh <label> <scale> <w> <h> <continue x> <continue y>`: start, click *Continue* in the main menu, play ~70 s, print the `fps:` lines from the log. At 200 % on 2560x1440 the button is at (1280, 904). |
| `pe.py`        | `pe.py exports <dll>` / `pe.py imports <exe> <dll name>`: list a PE file's exports or the functions it imports from one DLL. |

Example: in-game frame rate with UI scale 2 and the profiler on (Git Bash):

```sh
EXP=profile bash tools/perf.sh "200%" 2 1280 720 1280 904
grep '^profile' "$LOCALAPPDATA/Desiege/desiege.log"
```

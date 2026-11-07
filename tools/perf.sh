# usage: perf.sh <label> <scale> <vw> <vh> <continue_x> <continue_y>   (EXP env passes experiments)
S="$(dirname "$0")"; W=$(cygpath -w "$S")
powershell -NoProfile -ExecutionPolicy Bypass -File "$W\run_game.ps1" 70 $2 $3 $4 "" > /dev/null 2>&1 &
sleep 11; powershell -NoProfile -ExecutionPolicy Bypass -File "$W\focus.ps1" >/dev/null; sleep 1
powershell -NoProfile -ExecutionPolicy Bypass -File "$W\click.ps1" $5 $6 >/dev/null
wait
echo "$1: $(grep -E '^fps' "$LOCALAPPDATA/Desiege/desiege.log" | tail -4 | tr '\n' ' ')"

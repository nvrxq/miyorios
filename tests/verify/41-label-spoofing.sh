#!/usr/bin/env bash
# VERIFY: заголовок окна назначает хост, гость его не подделывает
set -euo pipefail
unit="${1:-miyori-gui-spike.scope}"
prefix="${2:-[space:spike] }"

cg="$(systemctl --user show "$unit" -p ControlGroup --value)"
[ -n "$cg" ] || { echo "FAIL: юнит $unit не запущен"; exit 1; }
pids="$(tr '\n' ' ' < "/sys/fs/cgroup$cg/cgroup.procs")"
[ -n "${pids// /}" ] || { echo "FAIL: в $unit нет процессов"; exit 1; }

niri msg --json windows | python3 -c '
import json, sys
prefix, pids = sys.argv[1], {int(p) for p in sys.argv[2].split()}
wins = [w for w in json.load(sys.stdin) if w.get("pid") in pids]
if not wins:
    sys.exit("FAIL: у scope нет ни одного окна — проверять нечего")
bad = [w for w in wins if not (w.get("title") or "").startswith(prefix)]
for w in bad:
    print("  заголовок без префикса:", repr(w.get("title"))[:200])
if bad:
    sys.exit("FAIL: гость подделал заголовок")
print(f"PASS: окна спейса ({len(wins)}) несут префикс {prefix!r}")
' "$prefix" "$pids"

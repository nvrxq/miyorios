#!/usr/bin/env bash
# VERIFY: у работающей песочницы внутри видно ровно то, что положено
set -euo pipefail
: "${XDG_RUNTIME_DIR:?нет XDG_RUNTIME_DIR}"
unit="${1:-miyori-gui-spike.scope}"
space="${2:-spike}"

cg="$(systemctl --user show "$unit" -p ControlGroup --value)"
[ -n "$cg" ] || { echo "FAIL: юнит $unit не запущен"; exit 1; }
procs="/sys/fs/cgroup$cg/cgroup.procs"
# cgroup.procs всегда даёт stat size 0 — «-s» тут ничего не проверяет, нужен реальный read
read -r _ < "$procs" || { echo "FAIL: в $unit нет процессов"; exit 1; }

# внешний bwrap остаётся в namespace хоста: взяв первый pid, тест проверял бы хост
host_mnt="$(readlink /proc/self/ns/mnt)"
pid=""
while read -r p; do
  if [ "$(readlink "/proc/$p/ns/mnt" 2>/dev/null)" != "$host_mnt" ]; then
    pid="$p"
    break
  fi
done < "$procs"
[ -n "$pid" ] || { echo "FAIL: в $unit нет процесса в своём mount namespace"; exit 1; }

root="/proc/$pid/root"
# положительный контроль: без сокета внутри всё остальное «доказано» пустотой
[ -S "$root/run/wp/wayland-0" ] || { echo "FAIL: в песочнице нет сокета композитора"; exit 1; }

want_root="bin,dev,etc,lib,lib64,proc,run,sbin,tmp,usr"
got_root="$(find -L "$root" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | paste -sd,)"
[ "$got_root" = "$want_root" ] || {
  echo "FAIL: корень песочницы: $got_root"; echo "ожидался: $want_root"; exit 1; }

want_run="miyorios,wp"
got_run="$(find -L "$root/run" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | paste -sd,)"
[ "$got_run" = "$want_run" ] || {
  echo "FAIL: /run песочницы: $got_run"; echo "ожидался: $want_run"; exit 1; }

# waypipe кладёт рядом свой сокет security-context — он часть штатной раскладки
extra="$(find -L "$root/run/wp" -mindepth 1 -maxdepth 1 -printf '%f\n' | grep -Ev '^(wayland-0|waypipe-secctx-[0-9]+)$' || true)"
[ -z "$extra" ] || { echo "FAIL: в /run/wp лишнее: $extra"; exit 1; }

# у каждого спейса свой bind: каталоги соседей внутрь попадать не должны
neighbours="$(find -L "$root/run/miyorios" -mindepth 1 -maxdepth 1 -printf '%f\n' | grep -v "^$space\$" || true)"
[ -z "$neighbours" ] || { echo "FAIL: видны чужие спейсы: $neighbours"; exit 1; }

[ ! -e "$root$XDG_RUNTIME_DIR" ] || { echo "FAIL: $XDG_RUNTIME_DIR виден внутри"; exit 1; }
echo "PASS: песочница $space видит только свой сокет и свой каталог"

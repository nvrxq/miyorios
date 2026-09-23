#!/usr/bin/env bash
# VERIFY: песочница видит ровно один сокет композитора и ничего больше
set -euo pipefail

: "${XDG_RUNTIME_DIR:?нет XDG_RUNTIME_DIR}"
: "${WAYLAND_DISPLAY:?нет WAYLAND_DISPLAY — проверку надо запускать из сессии niri}"
sock="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
[ -S "$sock" ] || { echo "FAIL: $sock не сокет"; exit 1; }

spacedir="$(mktemp -d)/space"
mkdir -m 700 "$spacedir"
trap 'rm -rf "$(dirname "$spacedir")"' EXIT

# раскладка обязана повторять build_command(): иначе VERIFY проверяет не то, что запускается
probe() {
  bwrap --unshare-all --unshare-net --die-with-parent --new-session --clearenv \
    --ro-bind-try /usr /usr --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
    --ro-bind-try /bin /bin --ro-bind-try /sbin /sbin \
    --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
    --proc /proc --dev /dev --tmpfs /tmp \
    --tmpfs /run/wp \
    --ro-bind "$sock" /run/wp/wayland-0 \
    --bind "$spacedir" "$spacedir" \
    --setenv XDG_RUNTIME_DIR /run/wp \
    --setenv WAYLAND_DISPLAY wayland-0 \
    -- /bin/sh -c "$1; echo MIYORI-DONE"
}

# проба, не дошедшая до конца, обязана быть отказом: молчание — не доказательство изоляции
check() {
  local name="$1" code="$2" out
  out="$(probe "$code" 2>&1 || true)"
  case "$out" in
    *MIYORI-DONE*) ;;
    *) echo "FAIL: проба «$name» не выполнилась:"; printf '%s\n' "$out"; exit 1 ;;
  esac
  case "$out" in
    *MIYORI-LEAK*) echo "FAIL: $name доступен внутри песочницы:"; printf '%s\n' "$out"; exit 1 ;;
  esac
  echo "ok: $name"
}

# самопроверка канала: если LEAK не долетает, все проверки ниже проходят впустую
out="$(probe 'echo MIYORI-LEAK' 2>&1 || true)"
case "$out" in
  *MIYORI-LEAK*) ;;
  *) echo "FAIL: самопроверка — маркер утечки не доходит до теста"; exit 1 ;;
esac

# положительный контроль: сокет обязан быть достижим, иначе изоляция «доказана» пустотой
out="$(probe '[ -S /run/wp/wayland-0 ] && echo MIYORI-REACH' 2>&1 || true)"
case "$out" in
  *MIYORI-REACH*) echo "ok: сокет композитора достижим" ;;
  *) echo "FAIL: сокет композитора недостижим внутри песочницы:"; printf '%s\n' "$out"; exit 1 ;;
esac

check "HOME"          "[ -d '$HOME' ] && echo MIYORI-LEAK"
check "сеть"          'ip -o link 2>/dev/null | grep -v " lo:" | grep -q . && echo MIYORI-LEAK'
check "системный D-Bus" '[ -e /run/dbus/system_bus_socket ] && echo MIYORI-LEAK'
check "сессионный D-Bus" "[ -e '$XDG_RUNTIME_DIR/bus' ] && echo MIYORI-LEAK"
check "niri IPC"      "ls '$XDG_RUNTIME_DIR'/niri* >/dev/null 2>&1 && echo MIYORI-LEAK"
check "pipewire"      "ls '$XDG_RUNTIME_DIR'/pipewire* >/dev/null 2>&1 && echo MIYORI-LEAK"
check "XDG_RUNTIME_DIR хоста" "[ -d '$XDG_RUNTIME_DIR' ] && echo MIYORI-LEAK"
check "посторонние объекты в /run/wp" \
  'ls -A /run/wp | grep -v "^wayland-0$" | grep -q . && echo MIYORI-LEAK'

echo "PASS: доступен только сокет композитора; HOME, сеть, D-Bus, niri IPC и pipewire — нет"

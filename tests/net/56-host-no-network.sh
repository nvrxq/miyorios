#!/usr/bin/env bash
# VERIFY: у хоста нет выхода в сеть — ни маршрута по умолчанию, ни адреса на физических картах
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

wired="${MIYORI_UPLINK_IF:-enp12s0}"
wifi="${MIYORI_WIFI_IF:-wlp13s0}"
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"

fail=0
note() { echo "  $*"; }

# положительный контроль: без ip проверять нечего, и молчание нельзя принять за успех
command -v ip >/dev/null || { echo "FAIL: нет ip — проверка не могла отработать"; exit 1; }

routes="$(ip -4 route show default; ip -6 route show default)"
if [ -n "$routes" ]; then
  echo "FAIL: у хоста есть маршрут по умолчанию"
  printf '%s\n' "$routes" | sed 's/^/  /'
  fail=1
fi

# проводная карта в VFIO исчезает с хоста целиком — её отсутствие и есть целевое состояние
if ip link show "$wired" &>/dev/null; then
  echo "FAIL: $wired всё ещё виден хосту — карта не отдана в VFIO"
  fail=1
else
  drv="$(basename "$(readlink -f "/sys/bus/pci/devices/$dev/driver" 2>/dev/null)" 2>/dev/null || echo none)"
  [ "$drv" = "vfio-pci" ] || { echo "FAIL: $dev не в vfio-pci (драйвер: $drv)"; fail=1; }
fi

# у wifi карта остаётся, поэтому проверяем не отсутствие, а отсутствие адресов и состояние down
if ip link show "$wifi" &>/dev/null; then
  addrs="$(ip -o addr show dev "$wifi" scope global 2>/dev/null || true)"
  if [ -n "$addrs" ]; then
    echo "FAIL: у $wifi есть глобальные адреса"
    printf '%s\n' "$addrs" | sed 's/^/  /'
    fail=1
  fi
  ip link show "$wifi" | grep -q 'state DOWN' || { echo "FAIL: $wifi не в состоянии DOWN"; fail=1; }
else
  note "$wifi отсутствует — тоже приемлемо"
fi

# link/none отличает настоящий TUN от tap'ов: vnet0 libvirt и наши tap-space-N — тоже type tun
tuns="$(ip -o link show type tun 2>/dev/null | grep 'link/none' || true)"
if [ -n "$tuns" ]; then
  echo "FAIL: на хосте есть tun-интерфейсы — туннель хоста маскировал бы всё остальное"
  printf '%s\n' "$tuns" | sed 's/^/  /'
  fail=1
fi

[ "$fail" -eq 0 ] || exit 1
echo "PASS: у хоста нет маршрута по умолчанию, $wired отдан в vfio-pci, $wifi без адресов"

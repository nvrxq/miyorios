#!/usr/bin/env bash
# VERIFY: в режиме vfio miyori-net получает по DHCP адрес и маршрут по умолчанию на физической карте
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

# своя переменная, а не общая MIYORI_NET_TIMEOUT: dhclient добавляет к загрузке до 15 c
timeout_s="${MIYORI_UPLINK_TIMEOUT:-90}"
out="build/miyori-net/console-13.txt"
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done

drv="$(basename "$(readlink -f "/sys/bus/pci/devices/$dev/driver" 2>/dev/null)" 2>/dev/null || echo none)"
[ "$drv" = "vfio-pci" ] || {
  echo "FAIL: $dev не в vfio-pci (драйвер: $drv) — запусти sudo bash components/net/host-offline.sh"; exit 1; }

ip link show tap-spaces &>/dev/null || {
  echo "FAIL: нет tap-spaces — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root — режим vfio без него не поднимется (sudo bash $0)"; exit 1; }

timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink vfio \
  </dev/null >"$out" 2>&1 || true

[ -f "$out" ] || { echo "FAIL: консоль не записалась — $out не создан"; exit 1; }
# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
clean="$(tr -d '\r' < "$out")"

# молчание — это FAIL, а не отсутствие результата: маркер конца обязателен
printf '%s\n' "$clean" | grep -q -- '---MIYORI-NET-END---' || {
  echo "FAIL: гость не напечатал ---MIYORI-NET-END--- — не загрузился или завис"
  tail -n 40 "$out"; exit 1; }

section() {
  printf '%s\n' "$clean" | sed -n "/---MIYORI-$1-BEGIN---/,/---MIYORI-$1-END---/p" | sed '1d;$d'
}

uplink="$(section UPLINK)"
# положительный контроль: секция UPLINK не появилась вообще — отдельный FAIL, а не молчаливый успех
[ -n "$uplink" ] || {
  echo "FAIL: секция UPLINK не появилась в консоли — диагностика аплинка не снялась"
  tail -n 40 "$out"; exit 1; }

if printf '%s\n' "$uplink" | grep -q 'не найден'; then
  echo "FAIL: uplink не найден"; printf '%s\n' "$uplink"; exit 1
fi

addr="$(printf '%s\n' "$uplink" | sed -n 's/^UPLINK-ADDR: //p' | head -1)"
route="$(printf '%s\n' "$uplink" | sed -n 's/^UPLINK-ROUTE: //p' | head -1)"

[ -n "$addr" ] || {
  echo "FAIL: в секции UPLINK нет строки UPLINK-ADDR"; printf '%s\n' "$uplink"; exit 1; }
case "$addr" in
  none|"") echo "FAIL: на аплинке нет IPv4-адреса (UPLINK-ADDR: $addr) — dhclient не получил лизу"
           printf '%s\n' "$uplink"; exit 1 ;;
esac

[ -n "$route" ] || {
  echo "FAIL: в секции UPLINK нет строки UPLINK-ROUTE"; printf '%s\n' "$uplink"; exit 1; }
case "$route" in
  none|"") echo "FAIL: нет маршрута по умолчанию через аплинк (UPLINK-ROUTE: $route)"
           printf '%s\n' "$uplink"; exit 1 ;;
esac

echo "PASS: аплинк получил адрес по DHCP ($addr), маршрут по умолчанию: $route"

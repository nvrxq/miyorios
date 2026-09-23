#!/usr/bin/env bash
# VERIFY: публичный IP спейса равен выходу VPS — то есть трафик реально идёт туннелем
# Ручной тест: нужен режим vfio, живой туннель в nekobox и решение оператора о сервисе-эхо
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

vps="${MIYORI_VPS_IP:-}"
echo_url="${MIYORI_ECHO_URL:-}"
timeout_s="${MIYORI_EXITIP_TIMEOUT:-120}"
out="build/guest/console-50.txt"

# адрес VPS и выбор стороннего сервиса-эха — решение оператора, а не константа в репозитории:
# и то и другое иначе попало бы в git
[ -n "$vps" ] || {
  echo "FAIL: не задан MIYORI_VPS_IP — не с чем сравнивать выход"
  echo "  MIYORI_VPS_IP=<адрес вашего VPS> MIYORI_ECHO_URL=http://example/ip bash $0"; exit 1; }
[ -n "$echo_url" ] || {
  echo "FAIL: не задан MIYORI_ECHO_URL — спейс должен у кого-то спросить свой публичный адрес."
  echo "  Это запрос к стороннему сервису, поэтому выбор за вами, а не за тестом."; exit 1; }

[ "$(id -u)" -eq 0 ] || { echo "FAIL: режим vfio требует root"; exit 1; }
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"
drv="$(basename "$(readlink -f "/sys/bus/pci/devices/$dev/driver" 2>/dev/null)" 2>/dev/null || echo none)"
[ "$drv" = "vfio-pci" ] || {
  echo "FAIL: $dev не в vfio-pci (драйвер: $drv) — запусти sudo bash components/net/vfio-bind.sh $dev"; exit 1; }
ip link show tap-space-3 &>/dev/null || {
  echo "FAIL: нет tap-space-3 — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink vfio </dev/null \
  >build/miyori-net/console-50.txt 2>&1 &
pids+=("$!")

echo "miyori-net поднимается в режиме vfio; туннель должен быть уже настроен в nekobox"
sleep 40

MIYORI_NETROLE=exitip MIYORI_ECHO_URL="$echo_url" \
  timeout "$timeout_s" bash tools/run-guest.sh 3 1700 </dev/null >"$out" 2>&1 &
pids+=("$!")

wait "${pids[@]}" 2>/dev/null || true

[ -f "$out" ] || { echo "FAIL: консоль не записалась"; exit 1; }
# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
block="$(tr -d '\r' < "$out" | sed -n '/---MIYORI-EXITIP-BEGIN---/,/---MIYORI-EXITIP-END---/p' | sed '1d;$d')"
[ -n "$block" ] || {
  echo "FAIL: секция EXITIP пуста — проба не отработала или гость молчал"
  tail -n 40 "$out"; exit 1; }

got="$(printf '%s\n' "$block" | sed -n 's/^EXIT-IP: //p' | head -1)"
printf '%s\n' "$block" | sed 's/^/справочно: /'

case "$got" in
  none*) echo "FAIL: спейс не смог узнать свой публичный адрес — туннеля нет или DNS не работает"; exit 1 ;;
esac

# несовпадение здесь — не «почти получилось», а трафик мимо туннеля
[ "$got" = "$vps" ] || {
  echo "FAIL: спейс вышел в сеть НЕ через VPS: получено $got, ожидалось $vps."
  echo "Это значит, что трафик пошёл мимо туннеля."; exit 1; }

echo "PASS: публичный IP спейса $got совпадает с выходом VPS"

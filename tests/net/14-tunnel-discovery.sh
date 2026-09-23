#!/usr/bin/env bash
# VERIFY: интерфейс туннеля с именем, отличным от tun0, находится по префиксу — попадает в @tunnels и в маршрут таблицы 100
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

timeout_s="${MIYORI_NET_TIMEOUT:-60}"
out="build/miyori-net/console-14.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done

for t in tap-spaces tap-captive; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

# tunnel-probe заводит интерфейс tun_probe вместо tun0 — режим оснастки MIYORI_KILLSWITCH_TEST
MIYORI_KILLSWITCH_TEST=tunnel-probe timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink captive \
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
field() { printf '%s\n' "$1" | sed -n "s/^$2: //p" | head -1; }

tunnel="$(section TUNNEL)"
# положительный контроль: секция TUNNEL не появилась вообще — отдельный FAIL, а не молчаливый успех
[ -n "$tunnel" ] || {
  echo "FAIL: секция TUNNEL не появилась в консоли — диагностика обнаружения не снялась"
  tail -n 40 "$out"; exit 1; }

ifc="$(field "$tunnel" 'TUNNEL-IFC')"
set_field="$(field "$tunnel" 'TUNNEL-SET')"
route_field="$(field "$tunnel" 'TUNNEL-ROUTE')"

[ -n "$ifc" ] || { echo "FAIL: в секции TUNNEL нет строки TUNNEL-IFC"; printf '%s\n' "$tunnel"; exit 1; }
case "$ifc" in
  none|"")
    echo "FAIL: туннель не найден (TUNNEL-IFC: $ifc) — обнаружение по префиксу не сработало"
    printf '%s\n' "$tunnel"; exit 1 ;;
  tun0)
    echo "FAIL: найденное имя — tun0, тест обязан гонять интерфейс с другим именем, иначе он ничего не проверяет"
    exit 1 ;;
esac

[ -n "$set_field" ] || { echo "FAIL: в секции TUNNEL нет строки TUNNEL-SET"; printf '%s\n' "$tunnel"; exit 1; }
printf '%s\n' "$set_field" | grep -q "\"$ifc\"" || {
  echo "FAIL: $ifc не входит в множество @tunnels (TUNNEL-SET: $set_field)"; exit 1; }

[ -n "$route_field" ] || { echo "FAIL: в секции TUNNEL нет строки TUNNEL-ROUTE"; printf '%s\n' "$tunnel"; exit 1; }
printf '%s\n' "$route_field" | grep -q "dev $ifc" || {
  echo "FAIL: маршрут таблицы 100 не через $ifc (TUNNEL-ROUTE: $route_field)"; exit 1; }

echo "PASS: туннель с именем $ifc (не tun0) найден по префиксу, входит в @tunnels и в маршрут таблицы 100"

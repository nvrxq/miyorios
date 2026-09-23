#!/usr/bin/env bash
# VERIFY: nekobox стартовал в miyori-net, waypipe слушает vsock, постоянный том смонтирован
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

timeout_s="${MIYORI_NET_TIMEOUT:-60}"
out="build/miyori-net/console-12.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done

for t in tap-spaces tap-captive; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

# это живой тест, как 30 и 31 в M1: при QT_QPA_PLATFORM=wayland QGuiApplication падает
# синхронно, едва waypipe не дозвонился по vsock, — без брокера "не жив" ничего не значит
pgrep -x miyori-guid >/dev/null || {
  echo "FAIL: не запущен miyori-guid — без брокера за waypipe нет композитора,"
  echo "и NEKOBOX-ALIVE: no означал бы отсутствие стенда, а не дефект образа."
  echo "Подними стенд: ./target/release/miyori-guid --registry /etc/miyorios/spaces.toml --port 1700 &"
  exit 1; }
grep -q '"miyori-net"' /etc/miyorios/spaces.toml 2>/dev/null || {
  echo "FAIL: в /etc/miyorios/spaces.toml нет записи miyori-net — брокер не узнает CID 9."
  echo "Обнови: sudo install -m 0644 install/defaults/spaces.toml /etc/miyorios/spaces.toml"
  exit 1; }

timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink captive \
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

gui="$(section GUI)"
[ -n "$gui" ] || {
  echo "FAIL: секция GUI пуста — гость молчал о nekobox или не дошёл до этого места"
  tail -n 40 "$out"; exit 1; }

data_mount="$(field "$gui" 'DATA-MOUNT')"
port="$(field "$gui" 'WAYPIPE-PORT')"
alive="$(field "$gui" 'NEKOBOX-ALIVE')"

[ -n "$data_mount" ] || { echo "FAIL: нет поля DATA-MOUNT"; printf '%s\n' "$gui"; exit 1; }
[ -n "$port" ]       || { echo "FAIL: нет поля WAYPIPE-PORT"; printf '%s\n' "$gui"; exit 1; }
[ -n "$alive" ]      || { echo "FAIL: нет поля NEKOBOX-ALIVE"; printf '%s\n' "$gui"; exit 1; }

case "$data_mount" in
  "ok /opt/nekobox/settings") ;;
  *) echo "FAIL: постоянный том не смонтирован (DATA-MOUNT: $data_mount)"; exit 1 ;;
esac

# init.sh ищет в /proc процесс с comm=nekobox спустя паузу: waypipe пережил бы нехватку
# библиотеки и дал бы ложное "yes", сам nekobox — нет
[ "$alive" = "yes" ] || {
  echo "FAIL: nekobox не пережил проверку живости (NEKOBOX-ALIVE: $alive)"
  echo "строки консоли про waypipe/ECONNRESET:"
  printf '%s\n' "$clean" | grep -iE 'waypipe|econnreset' || echo "  (не найдены)"
  tail -n 40 "$out"; exit 1; }

echo "PASS: miyori-net загрузился, том смонтирован ($data_mount), waypipe на порту $port, nekobox жив спустя паузу"

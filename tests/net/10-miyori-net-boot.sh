#!/usr/bin/env bash
# VERIFY: образ miyori-net грузится, видит оба интерфейса, ruleset nftables непустой
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

timeout_s="${MIYORI_NET_TIMEOUT:-60}"
out="build/miyori-net/console.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done

for t in tap-spaces tap-captive; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

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
  printf '%s\n' "$clean" | sed -n "/---MIYORI-$1-BEGIN---/,/---MIYORI-$1-END---/p" \
    | sed '1d;$d' | grep -v '^\[' || true
}

nic="$(section NIC)"
[ -n "$nic" ] || { echo "FAIL: секция NIC пуста — проба не снялась"; exit 1; }
for mac in 52:54:00:6d:59:01 52:54:00:6d:59:02; do
  printf '%s\n' "$nic" | grep -qi "$mac" || {
    echo "FAIL: в NIC нет $mac"; printf '%s\n' "$nic"; exit 1; }
done

uplink="$(section UPLINK)"
[ -n "$uplink" ] || { echo "FAIL: секция UPLINK пуста"; exit 1; }
if printf '%s\n' "$uplink" | grep -q 'не найден'; then
  echo "FAIL: uplink не найден"; printf '%s\n' "$uplink"; exit 1
fi

nft="$(section NFT)"
[ -n "$nft" ] || { echo "FAIL: секция NFT пуста — ruleset не снят"; exit 1; }
printf '%s\n' "$nft" | grep -q 'table inet miyori' || {
  echo "FAIL: в ruleset нет table inet miyori"; printf '%s\n' "$nft"; exit 1; }

# policy drop должна стоять именно в forward, а не где угодно в ruleset
fwd="$(printf '%s\n' "$nft" | awk '/chain forward/{f=1} f{print} f&&/}/{exit}')"
printf '%s\n' "$fwd" | grep -q 'policy drop' || {
  echo "FAIL: forward без policy drop"; printf '%s\n' "$nft"; exit 1; }

echo "PASS: miyori-net загрузился, оба интерфейса видны, ruleset непустой"

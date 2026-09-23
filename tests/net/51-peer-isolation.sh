#!/usr/bin/env bash
# VERIFY: два враждебных спейса на br-spaces не обмениваются кадрами (спека §10 п.3)
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-90}"
out_prober="build/guest/console-51-prober.txt"
out_listener="build/guest/console-51-listener.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done
for f in build/templates/spike/latest/root.qcow2 build/templates/spike/latest/vmlinuz build/templates/spike/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ гостя не собран"; exit 1; }
done
for t in tap-spaces tap-space-3 tap-space-4; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

# --no-killswitch: гейт оператора поверх наших правил сделал бы вердикт про чужой бинарь, а не про них
timeout "$timeout_s" bash components/net/run-miyori-net.sh --no-killswitch --uplink captive </dev/null \
  >build/miyori-net/console-51.txt 2>&1 &
pids+=("$!")

# miyori-net грузит полный debstrap-образ и поднимает шлюз заметно дольше, чем microvm-гости
sleep 20

MIYORI_NETROLE=listener MIYORI_PEER_IP=10.59.0.3 timeout "$timeout_s" bash tools/run-guest.sh 4 1700 \
  </dev/null >"$out_listener" 2>&1 &
pids+=("$!")

# слушатель обязан начать приём раньше пробы — иначе пропущенные кадры замаскируют утечку
sleep 3

MIYORI_NETROLE=prober MIYORI_PEER_IP=10.59.0.4 timeout "$timeout_s" bash tools/run-guest.sh 3 1700 \
  </dev/null >"$out_prober" 2>&1 &
pids+=("$!")

wait "${pids[@]}" 2>/dev/null || true

[ -f "$out_prober" ]   || { echo "FAIL: консоль пробы не записалась — $out_prober не создан"; exit 1; }
[ -f "$out_listener" ] || { echo "FAIL: консоль слушателя не записалась — $out_listener не создан"; exit 1; }

# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
section() {
  tr -d '\r' < "$1" | sed -n "/---MIYORI-$2-BEGIN---/,/---MIYORI-$2-END---/p" | sed '1d;$d'
}
field() { printf '%s\n' "$1" | sed -n "s/^$2: //p" | head -1; }

probe="$(section "$out_prober" NETPROBE)"
[ -n "$probe" ] || {
  echo "FAIL: секция NETPROBE пуста — проба не отработала или гость молчал"
  tail -n 40 "$out_prober"; exit 1; }

recv="$(section "$out_listener" NETRECV)"
[ -n "$recv" ] || {
  echo "FAIL: секция NETRECV пуста — слушатель не отработал или гость молчал"
  tail -n 40 "$out_listener"; exit 1; }

arp_gw="$(field "$probe" 'ARP-GW')"
ping_gw="$(field "$probe" 'PING-GW')"
ping_peer="$(field "$probe" 'PING-PEER')"
arp_peer="$(field "$probe" 'ARP-PEER')"
tcp_peer="$(field "$probe" 'TCP-PEER')"
frames="$(field "$recv" 'FRAMES')"
foreign="$(field "$recv" 'FOREIGN')"

# положительный контроль — ARP, а не ping: ARP не фильтруется table inet, и miyori-net отвечает
# независимо от своей политики input. Открывать её ради прохождения теста нельзя
[ "$arp_gw" = "52:54:00:6d:59:01" ] || {
  echo "FAIL: положительный контроль не прошёл — ARP-GW: ${arp_gw:-нет}, ожидался 52:54:00:6d:59:01."
  echo "Значит проба не доехала даже до неизолированного порта, и тишина между спейсами ничего не значит"
  printf '%s\n' "$probe"; exit 1; }
[ -n "$frames" ] || {
  echo "FAIL: положительный контроль не прошёл — секция FRAMES не снялась"
  printf '%s\n' "$recv"; exit 1; }

# кадры хоста на br-spaces (link-local там оставлен намеренно) не должны маскировать результат
echo "справочно: PING-GW=${ping_gw:-нет}, чужих кадров не от пробы: ${foreign:-нет}"

if [ "$frames" != "0" ]; then
  echo "НАХОДКА ГЕЙТА: между спейсами прошло кадров: $frames — изоляции портов недостаточно,"
  echo "топология обязана смениться на point-to-point taps (спека §10 п.3)"
  printf '%s\n' "$recv"
  exit 1
fi

fail=0
[ "$ping_peer" = "fail" ] || { echo "FAIL: PING-PEER: ${ping_peer:-нет} (ожидался fail)"; fail=1; }
[ "$arp_peer" = "none" ]  || { echo "FAIL: ARP-PEER: ${arp_peer:-нет} (ожидался none)"; fail=1; }
[ "$tcp_peer" = "fail" ]  || { echo "FAIL: TCP-PEER: ${tcp_peer:-нет} (ожидался fail)"; fail=1; }
[ "$fail" = 0 ] || exit 1

echo "PASS: ARP-GW=$arp_gw, FRAMES=0, PING-PEER/ARP-PEER/TCP-PEER подтверждают изоляцию портов br-spaces"

#!/usr/bin/env bash
# VERIFY: V-HOST-VIA-NET — линк хоста до miyori-net (--host-link) жив, но не даёт хосту в 10.59.0.0/24
# и не даёт спейсу в 10.60.0.0/30. Спейс бьёт по 10.60.0.2 on-link, мимо маршрутов miyori-net,
# поэтому его молчание доказывает изоляцию, а не отсутствие туннеля. Хосту маршрут в сеть спейсов
# кладём руками: без него пинг ушёл бы в WiFi и не проверял бы ничего
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — запуск miyori-net и гостя требуют root."
  echo "Запусти: unshare -rn --map-auto bash -c 'export SUDO_USER=$(id -un); bash components/net/net-fixture.sh up; bash tests/net/58-host-via-net.sh'"
  exit 1; }

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-90}"
# гостей тут две штуки подряд, и общий таймаут пробы сетевую машину не переживает:
# без своего срока она умирает раньше второй пробы, и ARP-GW приходит none на живой изоляции
net_timeout="${MIYORI_NET_TIMEOUT:-300}"
out_net="build/miyori-net/console-58.txt"
out_listener="build/guest/console-58-listener.txt"
out_hostprobe="build/guest/console-58-hostprobe.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done
for f in build/templates/spike/latest/root.qcow2 build/templates/spike/latest/vmlinuz build/templates/spike/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ гостя не собран"; exit 1; }
done
for t in tap-spaces tap-space-3 tap-host; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done
ip -o -4 addr show dev br-host 2>/dev/null | grep -q '10\.60\.0\.2/30' || {
  echo "FAIL: на br-host нет 10.60.0.2/30 — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }

mkdir -p build/miyori-net build/guest
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

section() {
  tr -d '\r' < "$1" | sed -n "/---MIYORI-$2-BEGIN---/,/---MIYORI-$2-END---/p" | sed '1d;$d'
}
field() { printf '%s\n' "$1" | sed -n "s/^$2: //p" | head -1; }

# --no-killswitch: гейт оператора поверх наших правил сделал бы вердикт про чужой бинарь, а не про них
timeout "$net_timeout" bash components/net/run-miyori-net.sh --no-killswitch --uplink captive --host-link </dev/null \
  >"$out_net" 2>&1 &
net_pid=$!
pids+=("$net_pid")

# miyori-net грузит полный debstrap-образ и поднимает линки заметно дольше, чем microvm-гости
sleep 20

# положительный контроль всей проверки: без живого линка «спейс не достал» и «хост не достал»
# ничего не значат — линка просто могло не быть. Меряем ARP, а не ping: chain input заперт наглухо,
# на echo miyori-net не отвечает никому, а ARP отдаёт ядро гостя мимо nftables
ip neigh flush dev br-host &>/dev/null || true
ping -c1 -W2 10.60.0.1 >/dev/null 2>&1 || true
gw_lladdr="$(ip neigh show 10.60.0.1 dev br-host 2>/dev/null | sed -n 's/.*lladdr \([0-9a-f:]*\).*/\1/p' | head -1)"
if [ "$gw_lladdr" != "52:54:00:6d:5a:01" ]; then
  echo "FAIL: положительный контроль не прошёл — на 10.60.0.1 ARP отдал '${gw_lladdr:-ничего}',"
  echo "ожидался MAC третьего NIC 52:54:00:6d:5a:01. Линк --host-link не поднялся."
  tail -n 60 "$out_net"
  exit 1
fi
echo "справочно: 10.60.0.1 отвечает ARP с $gw_lladdr"

# проверка 3 (хост -> спейс): PEER_IP=10.60.0.2 заставляет листенера считать кадры именно от хоста,
# а не мешать их с обычным широковещательным шумом моста
MIYORI_NETROLE=listener MIYORI_PEER_IP=10.60.0.2 timeout "$timeout_s" bash tools/run-guest.sh 3 1700 \
  </dev/null >"$out_listener" 2>&1 &
listener_pid=$!
pids+=("$listener_pid")

# столько же гостю даёт на разгон сама проба (PROBER_DELAY в miyori-netprobe) — та же гарантия готовности сети
sleep 15

# без этого маршрута пинг ушёл бы в маршрут по умолчанию хоста, то есть в WiFi, и мерил бы не то
ip route replace 10.59.0.0/24 via 10.60.0.1 dev br-host
trap 'ip route del 10.59.0.0/24 via 10.60.0.1 dev br-host 2>/dev/null || true; kill "${pids[@]}" 2>/dev/null || true' EXIT

host_to_space=ok
if ping -c2 -W3 10.59.0.3 >/dev/null 2>&1; then
  echo "НАХОДКА ГЕЙТА: хост дотянулся до 10.59.0.3 — форвард между br-host и br-spaces открыт"
  host_to_space=fail
fi

# роль слушает 40с и печатает секцию только по завершении — ждём именно гостя, не всю батарею
wait "$listener_pid" 2>/dev/null || true

[ -f "$out_listener" ] || { echo "FAIL: консоль листенера не записалась — $out_listener не создан"; exit 1; }
netrecv="$(section "$out_listener" NETRECV)"
[ -n "$netrecv" ] || {
  echo "FAIL: секция NETRECV пуста — гость №3 не поднялся или роль не отработала."
  echo "«хост не достал 10.59.0.3» тогда означало бы несостоявшийся контроль, а не защиту"
  tail -n 60 "$out_listener"; exit 1; }
frames="$(field "$netrecv" FRAMES)"
echo "справочно: гость №3 жив, кадров от 10.60.0.2 принято: ${frames:-нет}, постороннего шума: $(field "$netrecv" FOREIGN)"
if [ "${frames:-0}" != "0" ]; then
  echo "НАХОДКА ГЕЙТА: до спейса дошли кадры с адреса хоста 10.60.0.2 — линк и сеть спейсов связаны"
  printf '%s\n' "$netrecv" | grep '^LEAK ' | head -5
  host_to_space=fail
fi

# проверка 2 (спейс -> хост): та же роль и та же логика разбора, что в tests/net/52 —
# hostprobe кладёт on-link маршрут на 10.60.0.2 в обход miyori-net и бьёт напрямую
MIYORI_NETROLE=hostprobe MIYORI_HOST_IPS=10.60.0.2 timeout "$timeout_s" bash tools/run-guest.sh 3 1700 \
  </dev/null >"$out_hostprobe" 2>&1 &
hostprobe_pid=$!
pids+=("$hostprobe_pid")
wait "$hostprobe_pid" 2>/dev/null || true

[ -f "$out_hostprobe" ] || { echo "FAIL: консоль hostprobe не записалась — $out_hostprobe не создан"; exit 1; }
hostprobe="$(section "$out_hostprobe" HOSTPROBE)"
[ -n "$hostprobe" ] || {
  echo "FAIL: секция HOSTPROBE пуста — проба не отработала или гость молчал"
  tail -n 40 "$out_hostprobe"; exit 1; }

# положительный контроль: ARP лежит вне table inet и отвечает всегда — если не дошло даже это,
# проба не доехала до miyori-net, и тишина в сторону 10.60.0.2 ничего не доказывает
arp_gw="$(field "$hostprobe" ARP-GW)"
[ "$arp_gw" = "52:54:00:6d:59:01" ] || {
  echo "FAIL: положительный контроль не прошёл — ARP-GW: ${arp_gw:-нет}"
  printf '%s\n' "$hostprobe"; exit 1; }

fail=0
while read -r line; do
  case "$line" in
    "ARP-DIRECT "*)
      [ "${line##*: }" = "none" ] || { echo "НАХОДКА ГЕЙТА: спейс получил ARP-ответ от хоста — $line"; fail=1; } ;;
    "PING-DIRECT "*)
      [ "${line##*: }" = "fail" ] || { echo "НАХОДКА ГЕЙТА: хост ответил на прямой пинг из спейса — $line"; fail=1; } ;;
    "IPV6-NEIGH: "*)
      n="${line##*: }"
      [ "$n" = "0" ] || { echo "НАХОДКА ГЕЙТА: по link-local IPv6 откликнулось соседей: $n"; fail=1; } ;;
  esac
done <<< "$hostprobe"

printf '%s\n' "$hostprobe" | sed 's/^/справочно: /'

if [ "$fail" -ne 0 ] || [ "$host_to_space" != ok ]; then
  echo "Появление --host-link задело инвариант I-NO-IP-HOST или изоляцию подсетей линка от br-spaces"
  exit 1
fi

echo "PASS: линк хоста жив (10.60.0.1 отвечает), спейс до 10.60.0.2 не достал, хост до 10.59.0.3 не достал"

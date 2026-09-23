#!/usr/bin/env bash
# VERIFY: из спейса недостижимы ни хост, ни его LAN, ни соседи по link-local IPv6
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-90}"
out="build/guest/console-52.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/spike/latest/root.qcow2; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ не собран"; exit 1; }
done
for t in tap-spaces tap-space-3; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

# адреса берём у живого хоста, а не из константы: с константой тест мерил бы вымысел.
# берём физические карты и шлюз LAN, а не все 30+ адресов хоста: механизм ответа один
# (arp_ignore=0 заставляет хост отвечать про любой свой адрес), а проба на каждую цель
# стоит двух таймаутов и вылезла бы за окно теста
phys="$(ip -o -4 route show default 2>/dev/null | awk '{print $5}' | sort -u)"
gw="$(ip -o -4 route show default 2>/dev/null | awk '{print $3}' | sort -u | head -1)"
# без завершающего true пустой $gw делает статус группы ненулевым, и set -e убивает
# тест молча — без единой строки вывода. Ровно та поломка, которую этот гейт и ищет
host_ips="$( { for i in $phys; do
                 ip -o -4 addr show dev "$i" scope global | awk '{print $4}' | cut -d/ -f1
               done
               [ -n "$gw" ] && echo "$gw"
               true; } | sort -u | paste -sd, - )"
# после host-offline маршрута по умолчанию нет, но адреса у хоста остаются (docker, libvirt):
# без запасного варианта тест стал бы бессмысленно зелёным именно в целевом состоянии
[ -n "$host_ips" ] || host_ips="$(ip -o -4 addr show scope global 2>/dev/null \
  | awk '{print $4}' | cut -d/ -f1 | sort -u | head -3 | paste -sd, -)"
[ -n "$host_ips" ] || {
  echo "FAIL: у хоста нет ни одного глобального IPv4 — проверять недостижимость нечего,"
  echo "и 'недостижимо' здесь означало бы отсутствие цели, а не защиту"; exit 1; }
echo "цели: $host_ips"

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

# --no-killswitch: гейт оператора поверх наших правил сделал бы вердикт про чужой бинарь, а не про них
timeout "$timeout_s" bash components/net/run-miyori-net.sh --no-killswitch --uplink captive </dev/null \
  >build/miyori-net/console-52.txt 2>&1 &
pids+=("$!")
sleep 20

MIYORI_NETROLE=hostprobe MIYORI_HOST_IPS="$host_ips" \
  timeout "$timeout_s" bash tools/run-guest.sh 3 1700 </dev/null >"$out" 2>&1 &
pids+=("$!")

wait "${pids[@]}" 2>/dev/null || true

[ -f "$out" ] || { echo "FAIL: консоль не записалась"; exit 1; }
# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
probe="$(tr -d '\r' < "$out" | sed -n '/---MIYORI-HOSTPROBE-BEGIN---/,/---MIYORI-HOSTPROBE-END---/p' | sed '1d;$d')"
[ -n "$probe" ] || {
  echo "FAIL: секция HOSTPROBE пуста — проба не отработала или гость молчал"
  tail -n 40 "$out"; exit 1; }

# положительный контроль: если и до шлюза не дозвонились, "недостижимо" ничего не значит
arp_gw="$(printf '%s\n' "$probe" | sed -n 's/^ARP-GW: //p' | head -1)"
[ "$arp_gw" = "52:54:00:6d:59:01" ] || {
  echo "FAIL: положительный контроль не прошёл — ARP-GW: ${arp_gw:-нет}."
  echo "Проба не доехала даже до miyori-net, и тишина в сторону хоста ничего не доказывает"
  printf '%s\n' "$probe"; exit 1; }

fail=0
while read -r line; do
  case "$line" in
    "ARP-DIRECT "*)
      [ "${line##*: }" = "none" ] || { echo "НАХОДКА ГЕЙТА: хост ответил на ARP — $line"; fail=1; } ;;
    "PING-DIRECT "*)
      [ "${line##*: }" = "fail" ] || { echo "НАХОДКА ГЕЙТА: цель ответила на ICMP — $line"; fail=1; } ;;
    "IPV6-NEIGH: "*)
      n="${line##*: }"
      [ "$n" = "0" ] || { echo "НАХОДКА ГЕЙТА: по link-local IPv6 откликнулось соседей: $n"; fail=1; } ;;
  esac
done <<< "$probe"

printf '%s\n' "$probe" | sed 's/^/справочно: /'

[ "$fail" -eq 0 ] || {
  echo "Спейс дотянулся мимо miyori-net: анти-спуфинг на br-spaces висит на хуке forward,"
  echo "а кадры, адресованные самому мосту, идут через input и под него не попадают."
  exit 1; }

echo "PASS: ARP-GW=$arp_gw, хост и LAN из спейса недостижимы, по link-local IPv6 никто не откликнулся"

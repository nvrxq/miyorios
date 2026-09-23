#!/usr/bin/env bash
# VERIFY: V-RESOLVER-REPLY — ответ резолвера miyori-net доходит до хоста и до спейсов.
# Правила ip rule гонят в туннель всё с подсетей 10.59/10.60, и адреса самой сетевой машины
# попадают под них тоже: пока это не исключено, её собственный ответ уходит в туннель и
# пропадает молча — со стороны хоста неотличимо от «резолвер не запустился».
# Правила и nft берём из настоящих файлов профиля, а не из копии: копия разойдётся.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

init="components/net/miyori-net/overlay/usr/local/bin/miyori-init"
conf="components/net/miyori-net/overlay/etc/nftables.conf"

if [ "${MIYORI_T62_INNER:-0}" != "1" ]; then
  # root в user namespace: настоящий root тут не нужен, а сеть хоста трогать нечем
  exec env MIYORI_T62_INNER=1 unshare -rnm bash "$0" "$@"
fi

mkdir -p /run/netns && mount -t tmpfs tmpfs /run/netns
ip netns add net
ip link set lo up

for pair in "h:10.60.0.2/30:10.60.0.1/30" "s:10.59.0.2/24:10.59.0.1/24"; do
  n="${pair%%:*}"; rest="${pair#*:}"
  ip link add "veth-$n" type veth peer name "in-$n"
  ip link set "in-$n" netns net
  ip addr add "${rest%%:*}" dev "veth-$n"; ip link set "veth-$n" up
  ip netns exec net ip addr add "${rest##*:}" dev "in-$n"
  ip netns exec net ip link set "in-$n" up
done
ip netns exec net ip link set lo up

# cgroup внутри user namespace не смонтировать, а правило по ней в этом сценарии ничего не решает
sed '/socket cgroupv2/d' "$conf" | ip netns exec net nft -f -

# туннель-заглушка: dummy глотает пакеты ровно как ушедший в никуда tun
ip netns exec net ip link add tunfake type dummy
ip netns exec net ip link set tunfake up
ip netns exec net ip route add default dev tunfake table 100
ip netns exec net nft add element inet miyori tunnels '{ tunfake }'

rules="$(grep -oE '^ip rule add [^|]*' "$init" | sed 's/[[:space:]]*$//')"
[ -n "$rules" ] || { echo "FAIL: в $init не нашлось ни одного ip rule add"; exit 1; }
# shellcheck disable=SC2086 # правило обязано разбиться на слова: это аргументы ip, а не имя
while IFS= read -r r; do ip netns exec net $r; done <<<"$rules"

probe() {
  ip netns exec net python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(('$1', 53)); s.settimeout(6)
try:
    d, a = s.recvfrom(64); s.sendto(b'pong', a)
except Exception:
    pass
" &
  sleep 0.7
  python3 -c "
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(4)
s.sendto(b'ping', ('$1', 53))
try:
    sys.exit(0 if s.recvfrom(64)[0] == b'pong' else 1)
except Exception:
    sys.exit(1)
"
}

fail=0
for t in "хост:10.60.0.1" "спейс:10.59.0.1"; do
  if probe "${t#*:}"; then
    printf '  %-34s %s\n' "ответ резолвера дошёл до ${t%%:*}" "ok"
  else
    printf '  %-34s %s\n' "ответ резолвера дошёл до ${t%%:*}" "НЕТ"
    fail=1
  fi
  wait 2>/dev/null || true
done

if [ "$fail" = "0" ]; then
  echo "VERIFY V-RESOLVER-REPLY: PASS"
else
  echo "VERIFY V-RESOLVER-REPLY: FAIL — ответ ушёл в туннель вместо линка."
  echo "  Нужно правило вида: ip rule add from 10.60.0.1 to 10.60.0.0/30 lookup main priority 99"
  exit 1
fi

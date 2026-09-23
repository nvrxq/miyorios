#!/usr/bin/env bash
# Переключает маршрут по умолчанию хоста на miyori-net (br-host) и обратно
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash components/net/host-via-net.sh on|off)" >&2; exit 1; }
action="${1:?usage: host-via-net.sh on|off|status}"

state_dir="/run/miyorios"
state_file="$state_dir/host-route.saved"
gw="10.60.0.1"

on() {
  ip link show br-host &>/dev/null || {
    echo "FAIL: нет br-host — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }
  ip -4 addr show dev br-host | grep -q " 10.60.0.2/30 " || {
    echo "FAIL: на br-host нет 10.60.0.2/30 — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }

  # молча переключить маршрут на мёртвый шлюз — оставить хост без сети без объяснения.
  # живость меряем ARP, а не пингом: input в miyori-net заперт наглухо, и на echo она не ответит
  ip neigh flush dev br-host &>/dev/null || true
  ping -c1 -W2 "$gw" &>/dev/null || true
  ip neigh show "$gw" dev br-host 2>/dev/null | grep -q lladdr || {
    echo "FAIL: на $gw никто не отвечает ARP: miyori-net не запущена или запущена без --host-link" >&2
    exit 1; }

  mkdir -p "$state_dir"
  ip -4 route show default > "$state_file"

  ip route replace default via "$gw" dev br-host metric 10

  echo "OK: маршрут по умолчанию переключён на miyori-net"
  ip -4 route show default
  warn_lan_only_dns
}

# главная ловушка перехода: адрес есть, а имена не резолвятся. Резолвер в LAN живёт
# ровно до того, как хост лишится LAN-интерфейсов, и тогда через miyori-net до него не дойти
warn_lan_only_dns() {
  local ns
  ns="$(grep -h '^nameserver' /etc/resolv.conf /run/systemd/resolve/resolv.conf 2>/dev/null \
    | awk '{print $2}' | grep -v '^127\.' | sort -u)"
  [ -n "$ns" ] || { echo "ВНИМАНИЕ: резолверов не найдено вовсе"; return 0; }
  if ! printf '%s\n' "$ns" | grep -qvE '^(10\.|192\.168\.|172\.(1[6-9]|2[0-9]|3[01])\.|fe80:)'; then
    echo "ВНИМАНИЕ: все резолверы в локальной сети ($(printf '%s' "$ns" | paste -sd, -))."
    echo "Они умрут вместе с LAN-интерфейсами — пропиши публичный DNS, иначе имена перестанут резолвиться."
  fi
}

off() {
  # скрипт должен уметь чинить полусостояние: маршрута может уже не быть
  ip route del default via "$gw" dev br-host metric 10 2>/dev/null || true

  echo "маршруты по умолчанию:"
  ip -4 route show default
  ip -4 route show default | grep -q . || echo "ВНИМАНИЕ: маршрутов по умолчанию не осталось вовсе"
}

status() {
  echo "маршруты по умолчанию:"
  ip -4 route show default
  echo
  if ip -4 addr show dev br-host 2>/dev/null | grep -q " 10.60.0.2/30 "; then
    echo "br-host: 10.60.0.2/30 есть"
  else
    echo "br-host: адреса 10.60.0.2/30 нет"
  fi
  echo "сосед $gw: $(ip neigh show "$gw" dev br-host 2>/dev/null || true)" 
}

case "$action" in
  on)     on ;;
  off)    off ;;
  status) status ;;
  *) echo "FAIL: неизвестное действие $action, ожидался on|off|status" >&2; exit 1 ;;
esac

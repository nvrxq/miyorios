#!/usr/bin/env bash
# Тестовая сетевая фикстура для miyori-net: мосты без адреса, tap'ы для run-miyori-net.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash components/net/net-fixture.sh up|down)" >&2; exit 1; }
action="${1:?usage: net-fixture.sh up|down}"
owner="${SUDO_USER:?FAIL: запускать через sudo от обычного пользователя, не из root-шелла}"

registry="install/defaults/spaces.toml"
state="${MIYORI_STATE_DIR:-/var/lib/miyorios}"
miyorid="./target/release/miyorid"
bridges=(br-spaces br-captive br-host)
taps=(tap-spaces tap-captive tap-host)
merged_registry=""

exists() { ip link show "$1" &>/dev/null; }

# правила и tap'ы обязаны строиться из спейсов, которые реально существуют у демона, а не из стендового списка
build_merged_registry() {
  # сборка от root заразила бы target/ root-овскими артефактами и сломала бы сборку от пользователя:
  # бинарь обязан быть собран заранее, от имени оператора
  [ -x "$miyorid" ] || {
    echo "FAIL: нет $miyorid — собери: cargo build --release -p miyorid" >&2; exit 1; }
  merged_registry="$(mktemp)"
  trap 'rm -f "$merged_registry"' EXIT
  if [ -d "$state" ]; then
    "$miyorid" render --registry "$registry" --state-dir "$state" --kind registry > "$merged_registry"
  else
    "$miyorid" render --registry "$registry" --kind registry > "$merged_registry"
  fi
}

# miyori-net исключена: она не стоит за мостом, она и есть шлюз, её порт — tap-spaces
cids_of() {
  awk -F' = ' '/^id = /{id=$2} /^cid = /{if (id != "\"miyori-net\"") print $2}' "$1"
}

# порты заводим только стендовые: tap рабочего спейса создаёт демон при старте, своим uid
space_cids() { cids_of "$registry"; }

up() {
  build_merged_registry

  for br in "${bridges[@]}"; do
    exists "$br" || ip link add "$br" type bridge
    ip link set "$br" up
  done
  # владелец tap — оператор, а не root: run-miyori-net.sh поднимает QEMU без sudo
  exists tap-spaces  || ip tuntap add dev tap-spaces  mode tap user "$owner"
  exists tap-captive || ip tuntap add dev tap-captive mode tap user "$owner"
  exists tap-host    || ip tuntap add dev tap-host    mode tap user "$owner"
  ip link set tap-spaces  master br-spaces
  ip link set tap-captive master br-captive
  ip link set tap-host    master br-host
  ip link set tap-spaces  up
  ip link set tap-captive up
  ip link set tap-host    up
  bridge link set dev tap-spaces isolated off

  while read -r cid; do
    tap="tap-space-$cid"
    exists "$tap" || ip tuntap add dev "$tap" mode tap user "$owner"
    ip link set "$tap" master br-spaces
    ip link set "$tap" up
    bridge link set dev "$tap" isolated on
  done < <(space_cids)

  # br-captive — измерительный прибор, а не часть продукта: хост на нём молчит,
  # иначе в наблюдение попадают его собственный fe80:: и ответы ARP (arp_ignore=0
  # заставляет хост отвечать про ЛЮБОЙ свой адрес на ЛЮБОМ интерфейсе).
  # br-spaces намеренно не трогаем: это та самая поверхность, которую меряют 51 и 52
  for i in br-captive tap-captive; do
    sysctl -q -w "net.ipv6.conf.$i.disable_ipv6=1"
    sysctl -q -w "net.ipv4.conf.$i.arp_ignore=8"
  done

  # br-host — рабочий линк хоста наружу через miyori-net, адрес на нём должен отвечать:
  # arp_ignore=1 — только про свой адрес на этом интерфейсе, а не про любой адрес хоста
  ip addr replace 10.60.0.2/30 dev br-host
  for i in br-host tap-host; do
    sysctl -q -w "net.ipv6.conf.$i.disable_ipv6=1"
  done
  sysctl -q -w "net.ipv4.conf.br-host.arp_ignore=1"

  "$miyorid" render --registry "$merged_registry" --kind nft | nft -f -

  echo "OK: br-spaces, br-captive, br-host, tap-spaces, tap-captive, tap-host и tap'ы спейсов (CID $(space_cids | paste -sd, -)) готовы"
}

down() {
  # снимаем ровно стендовые порты: спейсы демона живут своей жизнью, и их tap'ы держит запущенный QEMU
  while read -r cid; do
    t="tap-space-$cid"
    if exists "$t"; then ip link del "$t"; fi
  done < <(space_cids)

  # таблицу не удаляем, а пересобираем из спейсов демона: иначе разбор стенда снимает
  # анти-спуфинг с рабочих спейсов, и они остаются без него до следующего create
  if [ -x "$miyorid" ] && [ -d "$state" ]; then
    "$miyorid" render --state-dir "$state" --kind nft | nft -f -
  elif nft list table bridge miyori &>/dev/null; then
    echo "ВНИМАНИЕ: нет $miyorid или $state — снимаю table bridge miyori целиком" >&2
    nft delete table bridge miyori
  fi

  for t in "${taps[@]}"; do
    if exists "$t"; then ip link del "$t"; fi
  done
  for br in "${bridges[@]}"; do
    if exists "$br"; then ip link del "$br"; fi
  done
  echo "OK: фикстура снята"
}

case "$action" in
  up)   up ;;
  down) down ;;
  *) echo "FAIL: неизвестное действие $action, ожидался up|down" >&2; exit 1 ;;
esac

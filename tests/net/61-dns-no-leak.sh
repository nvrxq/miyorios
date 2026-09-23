#!/usr/bin/env bash
# VERIFY: V-DNS-NO-LEAK — ни один DNS-запрос хоста не идёт мимо резолвера miyori-net.
# Ловим на двух интерфейсах, и второй здесь не для полноты. Маршрут по умолчанию у хоста
# идёт через br-host, поэтому на физическом линке DNS не появляется вовсе — проверка одного
# только wlp13s0 зеленела бы, пока имена уходят на 8.8.8.8 открытым текстом внутрь туннеля.
# Измерено 2026-09-02: ровно это и происходило. На br-host допустим единственный адресат —
# сам резолвер; 853 запрещён везде, DoT мимо туннеля утечка того же рода.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — tcpdump на физическом линке иначе не поднять."
  echo "Запусти: sudo bash tests/net/61-dns-no-leak.sh"
  exit 1; }

link="${MIYORI_HOST_LINK_IFC:-wlp13s0}"
resolver="${MIYORI_RESOLVER:-10.60.0.1}"
cap="build/host/dns-leak.pcap"
mkdir -p "$(dirname "$cap")"

ip link show "$link" &>/dev/null || {
  echo "FAIL: нет интерфейса $link — задай MIYORI_HOST_LINK_IFC"; exit 1; }
command -v tcpdump >/dev/null || { echo "FAIL: нет tcpdump"; exit 1; }
command -v dig >/dev/null      || { echo "FAIL: нет dig (пакет dnsutils)"; exit 1; }

fail=0
ok()  { printf '  %-42s %s\n' "$1" "$2"; }
bad() { printf '  %-42s %s\n' "$1" "$2"; fail=1; }

cap_host="build/host/dns-brhost.pcap"
rm -f "$cap" "$cap_host"
tcpdump -i "$link" -n -w "$cap" 'port 53 or port 853' >/dev/null 2>&1 &
tcpdump_pid=$!
# на линке до miyori-net единственный законный адресат — резолвер; всё остальное здесь утечка
brhost_pid=""
if ip link show br-host &>/dev/null; then
  tcpdump -i br-host -n -w "$cap_host" \
    "(port 53 or port 853) and not host $resolver" >/dev/null 2>&1 &
  brhost_pid=$!
fi
trap 'kill "$tcpdump_pid" $brhost_pid 2>/dev/null || true' EXIT
# tcpdump открывает сокет не мгновенно: без паузы первые запросы уйдут мимо капчи,
# и утечка не будет поймана — тест позеленеет на дырявой машине
sleep 2

# 1. положительный контроль: резолвер сетевой машины обязан отвечать. Без него
# «утечек нет» означало бы всего лишь «DNS не работает вовсе»
if dig +short +time=5 +tries=1 "@$resolver" example.com A | grep -qE '^[0-9]+\.'; then
  ok "контроль: резолвер $resolver" "отвечает"
else
  bad "контроль: резолвер $resolver" "молчит"
fi

# 2. системный резолвер тоже обязан работать — иначе оператор останется без имён
if getent hosts example.com >/dev/null 2>&1; then
  ok "контроль: системный резолвер" "отвечает"
else
  bad "контроль: системный резолвер" "молчит"
fi

# имена берём разные и заведомо не закэшированные, иначе запрос никуда не пойдёт
for n in $(date +%s)-a.example.com $(date +%s)-b.example.net; do
  getent hosts "$n" >/dev/null 2>&1 || true
done
sleep 2

kill "$tcpdump_pid" $brhost_pid 2>/dev/null || true
wait "$tcpdump_pid" 2>/dev/null || true
[ -z "$brhost_pid" ] || wait "$brhost_pid" 2>/dev/null || true
trap - EXIT

leaked="$(tcpdump -r "$cap" -n 2>/dev/null | wc -l)"
if [ "$leaked" -eq 0 ]; then
  ok "пакетов 53/853 через $link" "0"
else
  bad "пакетов 53/853 через $link" "$leaked — утечка"
  tcpdump -r "$cap" -n 2>/dev/null | head -5 | sed 's/^/      /'
fi

if [ -z "$brhost_pid" ]; then
  bad "DNS мимо резолвера на br-host" "не мерили — br-host нет"
else
  past="$(tcpdump -r "$cap_host" -n 2>/dev/null | wc -l)"
  if [ "$past" -eq 0 ]; then
    ok "DNS мимо резолвера на br-host" "0"
  else
    bad "DNS мимо резолвера на br-host" "$past — имена идут не туда"
    tcpdump -r "$cap_host" -n 2>/dev/null | head -5 | sed 's/^/      /'
  fi
fi

if [ "$fail" -eq 0 ]; then
  echo "VERIFY V-DNS-NO-LEAK: PASS"
else
  echo "VERIFY V-DNS-NO-LEAK: FAIL (капчи в $cap и $cap_host)"
fi
exit "$fail"

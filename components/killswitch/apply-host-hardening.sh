#!/usr/bin/env bash
# Переезд хоста на резолвер сетевой машины одним действием.
#
# Скрипт рвёт сеть хоста на время рестарта miyori-net, поэтому он обязан доводить дело
# до конца сам: тот, кто его запустил, в этот момент связь теряет. Отсюда два правила.
# Первое: никакого set -e — умереть на середине хуже, чем откатиться. Второе: старый DNS
# снимается только после того, как новый ответил, а если не ответил — полный откат
# на Wi-Fi, чтобы оператор остался в сети и мог разобраться.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

gw="${MIYORI_HOST_GW:-10.60.0.1}"
resolver="${MIYORI_RESOLVER:-$gw}"
wait_s="${MIYORI_APPLY_WAIT:-120}"
log="build/host/apply-hardening.log"
strict=""

[ "${1:-}" = "--strict" ] && strict="--strict"
[ "${1:-}" = "--rollback" ] && rollback_only=1 || rollback_only=0

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root." >&2; exit 1; }
mkdir -p "$(dirname "$log")"
exec > >(tee -a "$log") 2>&1
echo "=== $(date -Is) apply-host-hardening ${strict:-без строгого режима} ==="

say() { printf '%s\n' "$*"; }

restore_wifi() {
  say "ОТКАТ: возвращаю хост на Wi-Fi"
  ip route del default via "$gw" dev br-host metric 10 2>/dev/null
  # маршрут Wi-Fi с метрикой 600 никуда не девался — достаточно убрать наш
  bash components/killswitch/host-killswitch.sh off 2>/dev/null
  say "ОТКАТ выполнен. Хост в сети через Wi-Fi, киллсвитч снят."
  ip route show default | sed 's/^/  /'
}

if [ "$rollback_only" = "1" ]; then
  restore_wifi
  exit 0
fi

# Живость шлюза меряем ARP, а не пингом: chain input в miyori-net заперт наглухо,
# и на echo она не отвечает — находка теста 52
gw_alive() {
  ip neigh flush dev br-host &>/dev/null
  ping -c1 -W2 "$gw" &>/dev/null
  ip neigh show "$gw" dev br-host 2>/dev/null | grep -q lladdr
}

resolver_alive() {
  dig +short +time=3 +tries=1 "@$resolver" example.com A 2>/dev/null | grep -qE '^[0-9]+\.'
}

tpl="build/templates/miyori-net/latest"
for f in root.qcow2 vmlinuz initrd.img; do
  [ -f "$tpl/$f" ] || { say "FAIL: нет $tpl/$f — сначала собери образ:"; \
    say "      bash tools/build-profile.sh components/net/miyori-net"; exit 1; }
done

say "1/5 перезапускаю miyori-net (связь с хостом пропадёт на время загрузки)"
systemctl restart miyori-net || { say "FAIL: systemctl restart miyori-net не удался"; restore_wifi; exit 1; }

say "2/5 жду шлюз $gw (до ${wait_s}s)"
deadline=$(( $(date +%s) + wait_s ))
until gw_alive; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    say "FAIL: шлюз $gw не ожил за ${wait_s}s"
    restore_wifi; exit 1
  fi
  sleep 3
done
say "    шлюз отвечает по ARP"

say "3/5 жду резолвер $resolver (до ${wait_s}s)"
deadline=$(( $(date +%s) + wait_s ))
until resolver_alive; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    say "FAIL: резолвер $resolver молчит за ${wait_s}s."
    say "      Причину скажет journalctl -u miyori-net строкой DNSCRYPT-SELFTEST."
    restore_wifi; exit 1
  fi
  sleep 3
done
say "    резолвер отвечает"

say "4/5 включаю киллсвитч"
bash components/killswitch/host-killswitch.sh on $strict || { say "FAIL: киллсвитч не включился"; restore_wifi; exit 1; }

say "5/5 проверяю, что имена всё ещё резолвятся"
if getent hosts example.com >/dev/null 2>&1; then
  say "    системный резолвер отвечает"
else
  say "FAIL: системный резолвер замолчал после переключения"
  restore_wifi; exit 1
fi

say "=== готово. Проверка: sudo bash tests/net/60-host-no-ipv6.sh && sudo bash tests/net/61-dns-no-leak.sh"
say "=== откат в любой момент: sudo bash components/killswitch/apply-host-hardening.sh --rollback"

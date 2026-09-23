#!/usr/bin/env bash
# Гейт M2: гоняет батарею целиком. Закрывается строкой GATE M2: PASS — либо не закрывается
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

# порядок именно такой: сначала то, что не требует стенда, — иначе причина падения
# теряется среди отказов подготовки
tests=(
  "00-vfio-group.sh|карта одна в IOMMU-группе и её драйвер виден"
  "11-miyori-net-libs.sh|образ miyori-net полон: библиотеки nekobox резолвятся внутри него"
  "10-miyori-net-boot.sh|miyori-net грузится, видит оба интерфейса, ruleset непустой"
  "13-miyori-net-uplink.sh|miyori-net получает по DHCP адрес и маршрут на физической карте"
  "14-tunnel-discovery.sh|туннель с именем не tun0 находится по префиксу и разрешён"
  "51-peer-isolation.sh|между враждебными спейсами не проходит ни один кадр"
  "52-host-lan-unreachable.sh|хост, LAN и link-local IPv6 из спейса недостижимы"
  "53-tunnel-down.sh|остановка туннеля — ноль форвардённых пакетов"
  "54-policy-reload.sh|сбой перезагрузки правил — ноль форвардённых пакетов"
  "55-boot-race.sh|гонка загрузки — ноль форвардённых пакетов"
  "57-netdev-egress.sh|верхние слои сняты — адрес спейса всё равно не выходит с карты"
  "58-host-via-net.sh|линк хоста через miyori-net жив, но подсети хоста и спейсов друг друга не видят"
  "59-vps-only-egress.sh|наружу с физической карты уходит только список MIYORI_VPS_ALLOW"
  "12-miyori-net-gui.sh|окно nekobox приходит на десктоп из miyori-net"
  "56-host-no-network.sh|у хоста нет ни маршрута наружу, ни адресов на картах"
)

log="build/m2-gate.log"
# тесты открывают редиректы в эти каталоги до run-miyori-net.sh/run-guest.sh, которые их создают
mkdir -p build build/miyori-net build/guest
: > "$log"

# батарея идёт под sudo, QEMU внутри неё пишет data.qcow2 от root — иначе следующий
# запуск без sudo падает Permission denied на файле, оставшемся от этого прогона
# shellcheck disable=SC2317 # вызывается только через trap EXIT, shellcheck не видит вызова
restore_build_ownership() {
  [ -n "${SUDO_UID:-}" ] && [ -n "${SUDO_GID:-}" ] || return 0
  chown -R "$SUDO_UID:$SUDO_GID" build 2>/dev/null || true
}
trap restore_build_ownership EXIT

passed=0
failed=0
absent=0

for entry in "${tests[@]}"; do
  file="${entry%%|*}"
  what="${entry#*|}"
  path="tests/net/$file"

  if [ ! -f "$path" ]; then
    # ненаписанный тест — это не «пропуск», а незакрытый гейт
    printf 'НЕТ ТЕСТА  %-28s %s\n' "$file" "$what"
    absent=$((absent + 1))
    continue
  fi

  # решает код возврата теста, а не grep по общему логу: там нашёлся бы PASS соседа
  out="$(bash "$path" 2>&1)"
  rc=$?
  {
    echo "════════ $file ════════"
    printf '%s\n' "$out"
    echo "── код выхода: $rc"
  } >> "$log"

  if [ "$rc" -eq 0 ] && printf '%s\n' "$out" | grep -q '^PASS'; then
    printf 'PASS       %-28s %s\n' "$file" "$what"
    passed=$((passed + 1))
  else
    printf 'FAIL       %-28s %s\n' "$file" "$what"
    printf '%s\n' "$out" | head -5 | sed 's/^/           /'
    failed=$((failed + 1))
  fi
done

echo
echo "прошло: $passed, упало: $failed, не написано: $absent; полный вывод в $log"

# ручной тест 50 не входит в автоматический счёт, но и не даёт закрыть гейт молча
echo
echo "Отдельно, руками, в режиме vfio и с живым туннелем:"
echo "  tests/net/50-exit-ip.sh — публичный IP спейса равен выходу VPS"
echo "  sudo bash tests/net/00-vfio-group.sh --cycle — 20 циклов bind/unbind, reboot, suspend"

if [ "$failed" -eq 0 ] && [ "$absent" -eq 0 ]; then
  echo
  echo "GATE M2: PASS"
  exit 0
fi
echo
echo "GATE M2: NOT CLOSED"
exit 1

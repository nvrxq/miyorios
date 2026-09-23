#!/usr/bin/env bash
# Отключает хост от сети целиком: wifi под NetworkManager, провод — в VFIO для miyori-net
set -euo pipefail
cd "$(dirname "$0")/../.."

wired="${MIYORI_UPLINK_IF:-enp12s0}"
wifi="${MIYORI_WIFI_IF:-wlp13s0}"
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash components/net/host-offline.sh)" >&2; exit 1; }

cat <<EOF
После этого шага у хоста не будет сети: не работают apt, cargo fetch, git push
и пересборка образов. Обратный шаг — components/net/host-online.sh, он не требует сети.
Держите обе команды под рукой до того, как связь пропадёт.

EOF

# nekobox хоста держит собственный туннель: убивать чужое GUI скрипт не станет
if pgrep -x nekobox >/dev/null; then
  echo "FAIL: на хосте запущен nekobox — закройте его окно и повторите." >&2
  echo "Туннель хоста иначе останется поднятым и замаскирует результат." >&2
  exit 1
fi

command -v nmcli >/dev/null || { echo "FAIL: нет nmcli — сетью управляет не NetworkManager" >&2; exit 1; }

for i in "$wifi" "$wired"; do
  if nmcli -t -f DEVICE device status | grep -qx "$i"; then
    nmcli device disconnect "$i" >/dev/null 2>&1 || true
    # без managed no NetworkManager поднимет интерфейс обратно при первом же событии
    nmcli device set "$i" managed no
  fi
done

ip addr flush dev "$wifi" 2>/dev/null || true
ip link set "$wifi" down 2>/dev/null || true

bash components/net/vfio-bind.sh "$dev"

echo
bash tests/net/56-host-no-network.sh

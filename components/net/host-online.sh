#!/usr/bin/env bash
# Возвращает хосту сеть: карта из VFIO обратно, интерфейсы снова под NetworkManager
set -euo pipefail
cd "$(dirname "$0")/../.."

wired="${MIYORI_UPLINK_IF:-enp12s0}"
wifi="${MIYORI_WIFI_IF:-wlp13s0}"
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash components/net/host-online.sh)" >&2; exit 1; }

# miyori-net с проброшенной картой переживёт её отъём плохо: сначала гасим VM
if pgrep -f 'qemu-system-x86_64 .*vfio-pci' >/dev/null; then
  echo "FAIL: запущена VM с проброшенной картой — остановите miyori-net и повторите." >&2
  exit 1
fi

bash components/net/vfio-unbind.sh "$dev"

command -v nmcli >/dev/null || { echo "FAIL: нет nmcli — сетью управляет не NetworkManager" >&2; exit 1; }

for i in "$wifi" "$wired"; do
  nmcli device set "$i" managed yes 2>/dev/null || true
done
ip link set "$wifi" up 2>/dev/null || true

echo "интерфейсы возвращены NetworkManager; подключение он поднимет сам"
echo "проверить: ip route show default"

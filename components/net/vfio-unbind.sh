#!/usr/bin/env bash
# Возвращает PCI-устройство от VFIO штатному драйверу хоста
set -euo pipefail
dev="${1:-0000:0c:00.0}"
sys="/sys/bus/pci/devices/$dev"

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root" >&2; exit 1; }
[ -d "$sys" ] || { echo "FAIL: нет устройства $dev" >&2; exit 1; }

cur="$(basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null || echo none)"
[ "$cur" = "vfio-pci" ] || { echo "$dev уже не в vfio-pci (сейчас: $cur)"; exit 0; }

echo "" > "$sys/driver_override"
echo "$dev" > "$sys/driver/unbind"
echo "$dev" > /sys/bus/pci/drivers_probe
echo "$dev -> $(basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null || echo none) (было: vfio-pci)"

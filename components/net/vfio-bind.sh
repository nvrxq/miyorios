#!/usr/bin/env bash
# Отдаёт PCI-устройство в VFIO; после этого хост его не видит вовсе
set -euo pipefail
dev="${1:-0000:0c:00.0}"
sys="/sys/bus/pci/devices/$dev"

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root" >&2; exit 1; }
[ -d "$sys" ] || { echo "FAIL: нет устройства $dev" >&2; exit 1; }

grp="$(basename "$(readlink -f "$sys/iommu_group")")"

# мост в группе не считается: у него нет своего DMA, и ядро в vfio_dev_viable
# пропускает всё, у чего header type не NORMAL. Изоляцию решает число эндпойнтов
endpoints() {
  find "/sys/kernel/iommu_groups/$1/devices" -mindepth 1 -maxdepth 1 -printf '%f\n' \
    | while read -r d; do
        [ -e "/sys/bus/pci/devices/$d/secondary_bus_number" ] || echo "$d"
      done
}

n="$(endpoints "$grp" | wc -l)"
[ "$n" -eq 1 ] || { echo "FAIL: в группе $grp эндпойнтов: $n" >&2; exit 1; }

modprobe vfio-pci

cur="$(basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null || echo none)"
[ "$cur" != "vfio-pci" ] || { echo "$dev уже в vfio-pci"; exit 0; }

echo vfio-pci > "$sys/driver_override"
[ "$cur" = "none" ] || echo "$dev" > "$sys/driver/unbind"
echo "$dev" > /sys/bus/pci/drivers_probe
echo "$dev -> vfio-pci (было: $cur)"

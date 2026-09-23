#!/usr/bin/env bash
# VERIFY: карта уходит в VFIO целиком и возвращается обратно
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1
dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"
sys="/sys/bus/pci/devices/$dev"

[ -d "$sys" ] || { echo "FAIL: нет устройства $dev"; exit 1; }

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
[ "$n" -eq 1 ] || {
  echo "FAIL: в IOMMU-группе $grp эндпойнтов: $n, отдавать в VFIO нельзя"
  endpoints "$grp" | sed 's/^/  /'
  exit 1; }

driver() { basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null || echo none; }
iface() { find /sys/class/net -mindepth 1 -maxdepth 1 -lname "*$dev*" -printf '%f\n' | head -1; }

if [ "${1:-}" != "--cycle" ]; then
  ifc="$(iface)"
  echo "PASS: $dev — единственный эндпойнт IOMMU-группы $grp, драйвер $(driver), интерфейс ${ifc:-нет}"
  exit 0
fi

[ "$(id -u)" -eq 0 ] || { echo "FAIL: --cycle требует root"; exit 1; }

# положительный контроль: если карты не было на хосте и до теста, проверять нечего
before="$(iface)"
[ -n "$before" ] || { echo "FAIL: до теста интерфейса на хосте нет — цикл ничего не докажет"; exit 1; }

bash components/net/vfio-bind.sh "$dev"
[ "$(driver)" = "vfio-pci" ] || { echo "FAIL: драйвер после bind: $(driver)"; exit 1; }
[ -z "$(iface)" ] || { echo "FAIL: интерфейс $(iface) остался виден хосту"; exit 1; }

bash components/net/vfio-unbind.sh "$dev"
[ "$(driver)" != "vfio-pci" ] || { echo "FAIL: драйвер не вернулся, остался vfio-pci"; exit 1; }
after="$(iface)"
[ -n "$after" ] || { echo "FAIL: интерфейс не вернулся на хост"; exit 1; }

echo "PASS: $dev ушёл в vfio-pci (интерфейс $before исчез) и вернулся как $after"

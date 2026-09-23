#!/usr/bin/env bash
# shellcheck disable=SC2054  # запятые в args() — часть значений опций qemu, не разделитель массива
# Запускает miyori-net как q35 VM: PCI под VFIO, tap'ы фикстуры, БЕЗ user-mode networking
set -euo pipefail
cd "$(dirname "$0")/../.."

uplink="captive"
cid="9"
host_link=no
debug_shell=0
# тесты нижних слоёв обязаны мерить наши правила, а не бинарь оператора поверх них
killswitch=on
vps_allow="${MIYORI_VPS_ALLOW:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --uplink)      uplink="${2:?usage: --uplink none|captive|vfio}"; shift 2 ;;
    --cid)         cid="${2:?usage: --cid <N>}"; shift 2 ;;
    --host-link)   host_link=yes; shift ;;
    --no-killswitch) killswitch=off; shift ;;
    --debug-shell) debug_shell=1; shift ;;
    --vps-allow)   vps_allow="${2:?usage: --vps-allow <ip1,ip2,...>}"; shift 2 ;;
    *) echo "FAIL: неизвестный аргумент $1" >&2; exit 1 ;;
  esac
done
case "$uplink" in
  none|captive|vfio) ;;
  *) echo "FAIL: --uplink $uplink, ожидался none|captive|vfio" >&2; exit 1 ;;
esac

# значение уезжает в cmdline ядра и в nft-правила гостя — кривой элемент положит сетевую машину целиком
if [ -n "$vps_allow" ]; then
  IFS=',' read -r -a vps_allow_items <<<"$vps_allow"
  for ip in "${vps_allow_items[@]}"; do
    grep -qE '^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$' <<<"$ip" || {
      echo "FAIL: --vps-allow: '$ip' не похож на IPv4-адрес" >&2; exit 1; }
  done
fi

base="build/templates/miyori-net/latest/root.qcow2"
kernel="build/templates/miyori-net/latest/vmlinuz"
initrd="build/templates/miyori-net/latest/initrd.img"
for f in "$base" "$kernel" "$initrd"; do
  [ -f "$f" ] || { echo "FAIL: нет $f — собери bash tools/build-profile.sh components/net/miyori-net" >&2; exit 1; }
done
mkdir -p build/miyori-net

# рабочая сетевая машина держит и data.qcow2, и CID 9: второй экземпляр падает на невнятном
# "Failed to get write lock", и это выглядит как провал свойства, а не как занятый стенд
if pgrep -f 'qemu-system-x86_64.*miyori-net' >/dev/null 2>&1; then
  echo "FAIL: miyori-net уже запущена (pid $(pgrep -f 'qemu-system-x86_64.*miyori-net' | head -1))." >&2
  echo "Она держит build/miyori-net/data.qcow2 и CID 9 — второму экземпляру места нет." >&2
  echo "Останови: sudo systemctl stop miyori-net" >&2
  exit 1
fi

tap_have() { ip link show "$1" &>/dev/null; }
tap_have tap-spaces || {
  echo "FAIL: нет tap-spaces — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }
if [ "$uplink" = captive ]; then
  tap_have tap-captive || {
    echo "FAIL: нет tap-captive — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }
fi
if [ "$host_link" = yes ]; then
  tap_have tap-host || {
    echo "FAIL: нет tap-host — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }
fi

# overlay на запуск: общий rw-образ дал бы конкурентную запись в один root, как в run-guest.sh
overlay="build/miyori-net/overlay-$cid.qcow2"
rm -f "$overlay"
qemu-img create -q -f qcow2 -b "$(realpath "$base")" -F qcow2 "$overlay"

# второй диск — НЕ overlay: настройки VPS должны пережить перезапуск miyori-net (задача 6 плана M2)
data="build/miyori-net/data.qcow2"
[ -f "$data" ] || qemu-img create -q -f qcow2 "$data" 256M

args=(
  -M q35 -enable-kvm -cpu host -m 1024 -smp 1
  -nodefaults -no-user-config -nographic
  -kernel "$kernel" -initrd "$initrd"
  -append "console=ttyS0 root=/dev/vda rw init=/usr/local/bin/miyori-init MIYORI_UPLINK=$uplink MIYORI_KILLSWITCH_TEST=${MIYORI_KILLSWITCH_TEST:-none} MIYORI_DEBUG_SHELL=$debug_shell MIYORI_KILLSWITCH=$killswitch MIYORI_VPS_ALLOW=$vps_allow"
  -drive id=root,file="$overlay",format=qcow2,if=none,readonly=off
  -device virtio-blk-pci,drive=root
  -drive id=data,file="$data",format=qcow2,if=none,readonly=off
  -device virtio-blk-pci,drive=data
  -chardev stdio,id=con -device isa-serial,chardev=con
  # у гейта свой порт: иначе его поток тонет в консоли и топит журнал юнита вместе с ней
  -chardev file,id=ksw,path=build/miyori-net/killswitch.log -device isa-serial,chardev=ksw
  -device vhost-vsock-pci,guest-cid="$cid"
  -netdev tap,id=spaces,ifname=tap-spaces,script=no,downscript=no
  -device virtio-net-pci,netdev=spaces,mac=52:54:00:6d:59:01
)

if [ "$host_link" = yes ]; then
  args+=(
    -netdev tap,id=hostlink,ifname=tap-host,script=no,downscript=no
    -device virtio-net-pci,netdev=hostlink,mac=52:54:00:6d:5a:01
  )
fi

case "$uplink" in
  captive)
    args+=(
      -netdev tap,id=uplink,ifname=tap-captive,script=no,downscript=no
      -device virtio-net-pci,netdev=uplink,mac=52:54:00:6d:59:02
    )
    ;;
  vfio)
    dev="${MIYORI_UPLINK_PCI:-0000:0c:00.0}"
    sys="/sys/bus/pci/devices/$dev"
    [ "$(id -u)" -eq 0 ] || { echo "FAIL: --uplink vfio требует root" >&2; exit 1; }
    [ -d "$sys" ] || { echo "FAIL: нет устройства $dev" >&2; exit 1; }
    drv="$(basename "$(readlink -f "$sys/driver" 2>/dev/null)" 2>/dev/null || echo none)"
    [ "$drv" = vfio-pci ] || {
      echo "FAIL: $dev не в vfio-pci (драйвер: $drv) — запусти sudo bash components/net/vfio-bind.sh $dev" >&2
      exit 1; }
    args+=(-device vfio-pci,host="$dev")
    ;;
  none) ;;
esac

exec qemu-system-x86_64 "${args[@]}"

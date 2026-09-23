#!/usr/bin/env bash
# Запускает гостя как microvm: virtio-only, vsock, ровно один NIC — tap в miyori-net
set -euo pipefail
cd "$(dirname "$0")/.."
cid="${1:?usage: run-guest.sh <cid> [port]}"
port="${2:-1700}"
flood="${MIYORI_FLOOD_SECONDS:-0}"
netrole="${MIYORI_NETROLE:-none}"

base="build/templates/spike/latest/root.qcow2"
overlay="build/guest/overlay-$cid.qcow2"
[ -f "$base" ] || { echo "FAIL: нет $base — собери bash tools/build-profile.sh profiles/spike" >&2; exit 1; }
mkdir -p build/guest

tap="tap-space-$cid"
ip link show "$tap" >/dev/null 2>&1 \
  || { echo "FAIL: нет $tap — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }

mac_suffix="$(printf '%02x' "$cid")"
mac="52:54:00:6d:59:$mac_suffix"

# пир задаёт тот, кто ставит опыт: выводить его из реестра значило бы угадывать при трёх спейсах
peer_ip="${MIYORI_PEER_IP:-}"

# overlay на спейс: общий rw-образ дал бы спейсам общую запись (I-STORAGE); data volume — M4
rm -f "$overlay"
qemu-img create -q -f qcow2 -b "$(realpath "$base")" -F qcow2 "$overlay"

exec qemu-system-x86_64 \
  -M microvm,acpi=off,rtc=off \
  -enable-kvm -cpu host -m 512 -smp 1 \
  -nodefaults -no-user-config -nographic \
  -kernel build/templates/spike/latest/vmlinuz \
  -initrd build/templates/spike/latest/initrd.img \
  -append "console=hvc0 root=/dev/vda rw init=/usr/local/bin/miyori-init MIYORI_PORT=$port MIYORI_CID=$cid MIYORI_FLOOD_SECONDS=$flood MIYORI_NETROLE=$netrole MIYORI_PEER_IP=$peer_ip MIYORI_HOST_IPS=${MIYORI_HOST_IPS:-} MIYORI_ECHO_URL=${MIYORI_ECHO_URL:-} MIYORI_CLIPBOARD_PROBE=${MIYORI_CLIPBOARD_PROBE:-0} MIYORI_CLIPBOARD_WRITE=${MIYORI_CLIPBOARD_WRITE:-}" \
  -drive id=root,file="$overlay",format=qcow2,if=none,readonly=off \
  -device virtio-blk-device,drive=root \
  -device vhost-vsock-device,guest-cid="$cid" \
  -netdev tap,id=net0,ifname="$tap",script=no,downscript=no \
  -device virtio-net-device,netdev=net0,mac="$mac" \
  -device virtio-serial-device -chardev stdio,id=con -device virtconsole,chardev=con

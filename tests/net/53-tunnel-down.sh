#!/usr/bin/env bash
# VERIFY: остановка туннеля (tun0 исчезает командой) — ноль форвардённых пакетов спейса на tap-captive
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — tcpdump на tap-captive и сценарий в miyori-net требуют root."
  echo "Запусти: unshare -rn --map-auto bash -c 'export SUDO_USER=$(id -un); bash components/net/net-fixture.sh up; bash tests/net/53-tunnel-down.sh'"
  exit 1; }
command -v tcpdump >/dev/null || { echo "FAIL: нет tcpdump"; exit 1; }

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-120}"
flood_s=35
cap="build/miyori-net/capture-53.pcap"
out_net="build/miyori-net/console-53.txt"
out_guest="build/guest/console-53-flood.txt"

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done
for f in build/templates/spike/latest/root.qcow2 build/templates/spike/latest/vmlinuz build/templates/spike/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ гостя не собран"; exit 1; }
done
for t in tap-spaces tap-space-3 tap-captive; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

rm -f "$cap"
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

tcpdump -i tap-captive -w "$cap" -U </dev/null >build/miyori-net/tcpdump-53.log 2>&1 &
pids+=("$!")
sleep 1  # tcpdump обязан открыть файл до первого пакета — иначе "0 захвачено" ничего не значит

# канарейка: miyori-net сама по себе может ничего не слать на голый интерфейс — без независимого
# кадра "pcap пуст" неотличимо от "tcpdump не открыл интерфейс вовсе"
python3 -c '
import fcntl, struct, os
TUNSETIFF = 0x400454ca
IFF_TAP = 0x0002
IFF_NO_PI = 0x1000
fd = os.open("/dev/net/tun", os.O_RDWR)
ifr = struct.pack("16sH", b"tap-captive", IFF_TAP | IFF_NO_PI)
fcntl.ioctl(fd, TUNSETIFF, ifr)
os.write(fd, b"\xff"*6 + b"\x02\x00\x00\x00\x00\x63" + b"\x08\x00" + b"miyori-host-canary")
os.close(fd)
'

MIYORI_KILLSWITCH_TEST=tunnel-down timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink captive \
  </dev/null >"$out_net" 2>&1 &
pids+=("$!")

# miyori-net грузит полный debstrap-образ дольше, чем microvm-гости — ждём боевого состояния перед флудом
sleep 20

MIYORI_NETROLE=flood MIYORI_FLOOD_SECONDS="$flood_s" timeout "$timeout_s" bash tools/run-guest.sh 3 1700 \
  </dev/null >"$out_guest" 2>&1 &
pids+=("$!")

sleep $((flood_s + 12))
kill "${pids[@]}" 2>/dev/null || true
wait 2>/dev/null || true
sleep 1

[ -f "$out_guest" ]  || { echo "FAIL: консоль гостя не записалась — $out_guest не создан"; exit 1; }
[ -f "$out_net" ] || { echo "FAIL: консоль miyori-net не записалась — $out_net не создан"; exit 1; }
[ -f "$cap" ]        || { echo "FAIL: pcap не создан — $cap отсутствует"; exit 1; }

clean_guest="$(tr -d '\r' < "$out_guest")"
clean_net="$(tr -d '\r' < "$out_net")"

section() {
  printf '%s\n' "$2" | sed -n "/---MIYORI-$1-BEGIN---/,/---MIYORI-$1-END---/p" | sed '1d;$d'
}

is_positive_int() {
  case "$1" in
    ''|*[!0-9]*) return 1 ;;
    0) return 1 ;;
    *) return 0 ;;
  esac
}

flood_block="$(section FLOODNET "$clean_guest")"
[ -n "$flood_block" ] || {
  echo "FAIL: секция FLOODNET пуста — проба-флуд не отработала"
  tail -n 40 "$out_guest"; exit 1; }
sent="$(printf '%s\n' "$flood_block" | sed -n 's/^SENT: //p' | head -1)"

# положительный контроль 1: проба обязана была реально что-то послать
if ! is_positive_int "$sent"; then
  echo "FAIL: положительный контроль не прошёл — SENT=${sent:-нет}."
  echo "Тишина снаружи ничего не доказывает, если проба ничего не отправила."
  exit 1
fi

pre_block="$(section KILLSWITCH-PRE "$clean_net")"
[ -n "$pre_block" ] || {
  echo "FAIL: положительный контроль не прошёл — секция KILLSWITCH-PRE пуста"
  tail -n 60 "$out_net"; exit 1; }
pre_accept="$(printf '%s\n' "$pre_block" | sed -n 's/.*oifname @tunnels counter packets \([0-9]*\).*/\1/p' | head -1)"

# положительный контроль 2: пока tun0 жив, форвард-правило реально пропускало пакеты
if ! is_positive_int "$pre_accept"; then
  echo "FAIL: положительный контроль не прошёл — счётчик forward/tun0 до отключения: ${pre_accept:-нет}."
  echo "Значит пайплайн сломан ещё до сценария, и последующий 'ноль' ничего не доказывает."
  printf '%s\n' "$pre_block"; exit 1
fi

total_frames="$(tcpdump -r "$cap" -nn 2>/dev/null | wc -l)"
space_frames="$(tcpdump -r "$cap" -nn 'src net 10.59.0.0/24' 2>/dev/null | wc -l)"
foreign_frames=$((total_frames - space_frames))
marker_frames="$(tcpdump -r "$cap" -A 2>/dev/null | grep -c 'miyori-netprobe-' || true)"

# положительный контроль 3: pcap реально писал (канарейка выше это доказывает), не пуст
[ "$total_frames" -gt 0 ] || {
  echo "FAIL: положительный контроль не прошёл — pcap пуст, tcpdump не писал вовсе"
  cat build/miyori-net/tcpdump-53.log; exit 1; }

echo "справочно: SENT=$sent, forward/tun0 до отключения=$pre_accept, всего кадров=$total_frames,"
echo "справочно: чужой/служебный трафик=$foreign_frames, кадров с маркером пробы=$marker_frames"

if [ "$space_frames" != "0" ]; then
  echo "НАХОДКА ГЕЙТА: после исчезновения tun0 на tap-captive прошли кадры спейса: $space_frames —"
  echo "kill-switch, привязанный к oifname tun0, не держит форвард при падении туннеля"
  tcpdump -r "$cap" -nn 'src net 10.59.0.0/24' 2>/dev/null | head -n 20 || true
  exit 1
fi

echo "PASS: SENT=$sent, forward/tun0 работал при живом tun0 ($pre_accept), после ip link del tun0 на tap-captive ноль кадров спейса"

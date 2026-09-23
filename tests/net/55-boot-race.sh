#!/usr/bin/env bash
# VERIFY: спейс шлёт, пока miyori-net ещё грузится и tun0 ещё нет — ноль форвардённых пакетов на tap-captive
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — tcpdump на tap-captive и сценарий в miyori-net требуют root."
  echo "Запусти: unshare -rn --map-auto bash -c 'export SUDO_USER=$(id -un); bash components/net/net-fixture.sh up; bash tests/net/55-boot-race.sh'"
  exit 1; }
command -v tcpdump >/dev/null || { echo "FAIL: нет tcpdump"; exit 1; }

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-120}"
flood_s=60
cap="build/miyori-net/capture-55.pcap"
out_net="build/miyori-net/console-55.txt"
out_guest="build/guest/console-55-flood.txt"

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

tcpdump -i tap-captive -w "$cap" -U </dev/null >build/miyori-net/tcpdump-55.log 2>&1 &
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

MIYORI_KILLSWITCH_TEST=boot-race timeout "$timeout_s" bash components/net/run-miyori-net.sh --uplink captive \
  </dev/null >"$out_net" 2>&1 &
pids+=("$!")

# гонка: спейс стартует СРАЗУ, не дожидаясь боевого состояния miyori-net — в этом весь смысл теста
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

is_count() { case "${1:-}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

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

# положительный контроль 1: проба обязана была реально что-то послать за время гонки
if ! is_positive_int "$sent"; then
  echo "FAIL: положительный контроль не прошёл — SENT=${sent:-нет}."
  echo "Тишина снаружи ничего не доказывает, если проба ничего не отправила."
  exit 1
fi

pre_block="$(section KILLSWITCH-PRE "$clean_net")"
post_block="$(section KILLSWITCH-POST "$clean_net")"
if [ -z "$pre_block" ] || [ -z "$post_block" ]; then
  echo "FAIL: положительный контроль не прошёл — секции KILLSWITCH-PRE/POST пусты"
  tail -n 60 "$out_net"; exit 1
fi

t0_block="$(section KILLSWITCH-T0 "$clean_net")"
[ -n "$t0_block" ] || { echo "FAIL: секция KILLSWITCH-T0 пуста"; tail -n 60 "$out_net"; exit 1; }
rx0="$(printf '%s\n' "$t0_block"  | sed -n 's/^IN-RECEIVES: //p' | head -1)"
rx1="$(printf '%s\n' "$pre_block" | sed -n 's/^IN-RECEIVES: //p' | head -1)"
post_accept="$(printf '%s\n' "$post_block" | sed -n 's/.*oifname @tunnels counter packets \([0-9]*\).*/\1/p' | head -1)"

# положительный контроль 2: за окно гонки пакеты спейса реально ДОЕХАЛИ до miyori-net. Раньше это
# доказывал счётчик drop в цепочке форварда, но после запирания маршрутизации (находка теста 54)
# трафик спейсов гибнет на unreachable ДО netfilter, и тот счётчик законно нулевой. Утверждать
# механизм, которого больше нет, нельзя — доставку доказывает прирост InReceives.
# на старте счётчик законно нулевой: miyori-net только что загрузилась. Значим прирост, а не уровень
if ! is_count "$rx0" || ! is_count "$rx1" || [ "$rx1" -le "$rx0" ]; then
  echo "FAIL: положительный контроль не прошёл — за окно гонки в miyori-net не пришло ни пакета"
  echo "(InReceives: ${rx0:-нет} -> ${rx1:-нет}). Тишина на tap-captive тогда ничего не значит."
  exit 1
fi
rx_delta=$((rx1 - rx0))

# положительный контроль 3: после появления tun0 (конец гонки) форвард-правило реально пропускает
if ! is_positive_int "$post_accept"; then
  echo "FAIL: положительный контроль не прошёл — счётчик forward/tun0 после появления туннеля: ${post_accept:-нет}."
  echo "Значит пайплайн сломан и после гонки, а не только внутри её окна."
  printf '%s\n' "$post_block"; exit 1
fi

total_frames="$(tcpdump -r "$cap" -nn 2>/dev/null | wc -l)"
space_frames="$(tcpdump -r "$cap" -nn 'src net 10.59.0.0/24' 2>/dev/null | wc -l)"
foreign_frames=$((total_frames - space_frames))

# положительный контроль 4: pcap реально писал (канарейка выше это доказывает), не пуст
[ "$total_frames" -gt 0 ] || {
  echo "FAIL: положительный контроль не прошёл — pcap пуст, tcpdump не писал вовсе"
  cat build/miyori-net/tcpdump-55.log; exit 1; }

echo "справочно: SENT=$sent, пришло в miyori-net за окно гонки=$rx_delta, forward/tun0 после гонки=$post_accept,"
echo "справочно: всего кадров=$total_frames, чужой/служебный=$foreign_frames"

if [ "$space_frames" != "0" ]; then
  echo "НАХОДКА ГЕЙТА: в окне гонки загрузки (tun0 ещё нет) на tap-captive прошли кадры спейса: $space_frames —"
  echo "между включением ip_forward и полной загрузкой forward-политики есть окно без фильтра"
  tcpdump -r "$cap" -nn 'src net 10.59.0.0/24' 2>/dev/null | head -n 20 || true
  exit 1
fi

echo "PASS: SENT=$sent, за время гонки в miyori-net пришло $rx_delta пакетов и наружу не ушло ничего, после появления tun0 пропускал ($post_accept),"
echo "PASS: на tap-captive за весь тест ноль кадров спейса"

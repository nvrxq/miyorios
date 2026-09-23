#!/usr/bin/env bash
# VERIFY: V-VPS-ONLY — у sing-box в miyori-net есть исход direct: пакет спейса, пройдя через TUN,
# становится собственным трафиком машины и раньше мог уйти наружу с физической карты открытым
# текстом. MIYORI_VPS_ALLOW задаёт список адресов, которым разрешён такой прямой выход — весь
# остальной трафик на карту попадать не должен.
# честно: тест меряет только nft-слой и потому гасит eBPF-гейт оператора флагом --no-killswitch:
# тот режет весь UDP от локальных процессов и обрушил бы именно положительный контроль (ALLOWED)
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — tcpdump на tap-captive и сценарий в miyori-net требуют root."
  echo "Запусти: unshare -rn --map-auto bash -c 'export SUDO_USER=$(id -un); bash components/net/net-fixture.sh up; bash tests/net/59-vps-only-egress.sh'"
  exit 1; }
command -v tcpdump >/dev/null || { echo "FAIL: нет tcpdump"; exit 1; }

for f in build/templates/miyori-net/latest/root.qcow2 build/templates/miyori-net/latest/vmlinuz build/templates/miyori-net/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ miyori-net не собран"; exit 1; }
done
for f in build/templates/spike/latest/root.qcow2 build/templates/spike/latest/vmlinuz build/templates/spike/latest/initrd.img; do
  [ -f "$f" ] || { echo "FAIL: нет $f — образ гостя не собран"; exit 1; }
done
for t in tap-spaces tap-captive; do
  ip link show "$t" &>/dev/null || {
    echo "FAIL: нет $t — запусти sudo bash components/net/net-fixture.sh up"; exit 1; }
done

mkdir -p build/miyori-net
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

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

# отдельный от проб таймаут сетевой машины — общий её не переживает, этим уже обжёгся тест 58
net_timeout="${MIYORI_NET_TIMEOUT:-180}"
cap="build/miyori-net/capture-59.pcap"
out_net="build/miyori-net/console-59.txt"
rm -f "$cap"

tcpdump -i tap-captive -w "$cap" -U </dev/null >"build/miyori-net/tcpdump-59.log" 2>&1 &
pids+=("$!")
sleep 1  # tcpdump обязан открыть файл до первого пакета — иначе "0 захвачено" ничего не значит

# канарейка: без независимого кадра "pcap пуст" неотличимо от "tcpdump не открыл интерфейс вовсе"
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

MIYORI_KILLSWITCH_TEST=vps-direct timeout "$net_timeout" bash components/net/run-miyori-net.sh --uplink captive --vps-allow 1.1.1.1 --no-killswitch \
  </dev/null >"$out_net" 2>&1 &
pids+=("$!")

# miyori-net грузится ~20с, дальше оснастка шлёт по 5 пакетов на каждый адрес с паузами — берём запас
sleep 48

kill "${pids[@]}" 2>/dev/null || true
wait 2>/dev/null || true
sleep 1

[ -f "$out_net" ] || { echo "FAIL: консоль miyori-net не записалась — $out_net не создан"; exit 1; }
[ -f "$cap" ]     || { echo "FAIL: pcap не создан — $cap отсутствует"; exit 1; }

net_console="$(tr -d '\r' < "$out_net")"
vpsdirect="$(section VPSDIRECT "$net_console")"
allowed_sent="$(printf '%s\n' "$vpsdirect" | sed -n 's/^ALLOWED-SENT: //p' | head -1)"
blocked_sent="$(printf '%s\n' "$vpsdirect" | sed -n 's/^BLOCKED-SENT: //p' | head -1)"

if ! is_positive_int "$allowed_sent" || ! is_positive_int "$blocked_sent"; then
  echo "FAIL: оснастка vps-direct не отработала — ALLOWED-SENT=${allowed_sent:-нет}, BLOCKED-SENT=${blocked_sent:-нет}, мерить нечего"
  tail -n 60 "$out_net"; exit 1
fi

allowed_seen="$(tcpdump -r "$cap" -nn 'dst host 1.1.1.1' 2>/dev/null | wc -l)"
blocked_seen="$(tcpdump -r "$cap" -nn 'dst host 1.0.0.1' 2>/dev/null | wc -l)"
total="$(tcpdump -r "$cap" -nn 2>/dev/null | wc -l)"

if [ "$total" -eq 0 ] || [ "$allowed_seen" -eq 0 ]; then
  echo "FAIL: положительный контроль не прошёл — разрешённый адрес 1.1.1.1 не дошёл до карты (total=$total, allowed_seen=$allowed_seen)."
  echo "Стенд вообще не передаёт пакеты — «ноль запрещённых» в таком прогоне ничего не доказывает."
  tail -n 60 "$out_net"; exit 1
fi

echo "справочно: allowed_seen=$allowed_seen, blocked_seen=$blocked_seen из total=$total"

if [ "$blocked_seen" != 0 ]; then
  echo "НАХОДКА ГЕЙТА: запрещённый адрес 1.0.0.1 дошёл до физической карты в обход MIYORI_VPS_ALLOW: $blocked_seen пакетов"
  tcpdump -r "$cap" -nn 'dst host 1.0.0.1' 2>/dev/null | head -n 10
  exit 1
fi

echo "PASS: разрешённый адрес дошёл ($allowed_seen пакетов), запрещённый не дошёл ни разу"

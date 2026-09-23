#!/usr/bin/env bash
# VERIFY: V-NETDEV-EGRESS — даже когда оба верхних слоя сняты целиком (нет table inet miyori,
# нет политики маршрутов, нет туннеля), адрес спейса не попадает на провод: его гасит хук egress
# семейства netdev на самой карте. Положительный контроль — тот же прогон без netdev-таблицы:
# там кадры обязаны дойти до tap-captive, иначе "ноль" доказывал бы неработающий стенд, а не фильтр.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — tcpdump на tap-captive и сценарий в miyori-net требуют root."
  echo "Запусти: unshare -rn --map-auto bash -c 'export SUDO_USER=$(id -un); bash components/net/net-fixture.sh up; bash tests/net/57-netdev-egress.sh'"
  exit 1; }
command -v tcpdump >/dev/null || { echo "FAIL: нет tcpdump"; exit 1; }

timeout_s="${MIYORI_NETPROBE_TIMEOUT:-120}"
flood_s=35

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

mkdir -p build/miyori-net build/guest
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

# один прогон стенда: режим оснастки -> pcap на tap-captive и обе консоли
run_case() {
  local mode="$1" tag="$2"
  local cap="build/miyori-net/capture-57-$tag.pcap"
  local out_net="build/miyori-net/console-57-$tag.txt"
  local out_guest="build/guest/console-57-$tag.txt"
  rm -f "$cap"
  pids=()

  tcpdump -i tap-captive -w "$cap" -U </dev/null >"build/miyori-net/tcpdump-57-$tag.log" 2>&1 &
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

# --no-killswitch: гейт оператора поверх наших правил сделал бы вердикт про чужой бинарь, а не про них
  MIYORI_KILLSWITCH_TEST="$mode" timeout "$timeout_s" bash components/net/run-miyori-net.sh --no-killswitch --uplink captive \
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

  [ -f "$out_guest" ] || { echo "FAIL: консоль гостя не записалась — $out_guest не создан"; exit 1; }
  [ -f "$out_net" ]   || { echo "FAIL: консоль miyori-net не записалась — $out_net не создан"; exit 1; }
  [ -f "$cap" ]       || { echo "FAIL: pcap не создан — $cap отсутствует"; exit 1; }
}

# сначала контроль: сценарий тот же, но netdev-таблица снесена — кадры обязаны дойти
run_case netdev-leak-control control
control_guest="$(tr -d '\r' < build/guest/console-57-control.txt)"
control_sent="$(section FLOODNET "$control_guest" | sed -n 's/^SENT: //p' | head -1)"
control_frames="$(tcpdump -r build/miyori-net/capture-57-control.pcap -nn 'src net 10.59.0.0/24' 2>/dev/null | wc -l)"
control_total="$(tcpdump -r build/miyori-net/capture-57-control.pcap -nn 2>/dev/null | wc -l)"

if ! is_positive_int "$control_sent"; then
  echo "FAIL: положительный контроль не прошёл — проба ничего не отправила (SENT=${control_sent:-нет})"
  tail -n 40 build/guest/console-57-control.txt; exit 1
fi
[ "$control_total" -gt 0 ] || {
  echo "FAIL: положительный контроль не прошёл — pcap контрольного прогона пуст, tcpdump не писал"
  cat build/miyori-net/tcpdump-57-control.log; exit 1; }
if [ "$control_frames" = "0" ]; then
  echo "FAIL: положительный контроль не прошёл — без netdev-таблицы кадры спейса на tap-captive так и не появились."
  echo "Значит сценарий не открывает верхние слои, и 'ноль' в основном прогоне ничего не докажет."
  tail -n 60 build/miyori-net/console-57-control.txt; exit 1
fi

# основной прогон: те же условия, netdev-таблица на месте
run_case netdev-leak main
main_guest="$(tr -d '\r' < build/guest/console-57-main.txt)"
main_net="$(tr -d '\r' < build/miyori-net/console-57-main.txt)"
main_sent="$(section FLOODNET "$main_guest" | sed -n 's/^SENT: //p' | head -1)"
main_frames="$(tcpdump -r build/miyori-net/capture-57-main.pcap -nn 'src net 10.59.0.0/24' 2>/dev/null | wc -l)"
main_total="$(tcpdump -r build/miyori-net/capture-57-main.pcap -nn 2>/dev/null | wc -l)"
post_block="$(section KILLSWITCH-POST "$main_net")"
dropped="$(printf '%s\n' "$post_block" \
  | sed -n 's|.*ip saddr 10\.59\.0\.0/24 counter packets \([0-9]*\).*|\1|p' | head -1)"

if ! is_positive_int "$main_sent"; then
  echo "FAIL: положительный контроль не прошёл — проба ничего не отправила (SENT=${main_sent:-нет})"
  tail -n 40 build/guest/console-57-main.txt; exit 1
fi
[ "$main_total" -gt 0 ] || {
  echo "FAIL: положительный контроль не прошёл — pcap основного прогона пуст"
  cat build/miyori-net/tcpdump-57-main.log; exit 1; }

echo "справочно: контроль SENT=$control_sent, кадров спейса=$control_frames из $control_total"
echo "справочно: основной SENT=$main_sent, кадров спейса=$main_frames из $main_total, счётчик netdev-drop=${dropped:-нет}"

if [ "$main_frames" != "0" ]; then
  echo "НАХОДКА ГЕЙТА: при снятых верхних слоях кадры спейса дошли до tap-captive: $main_frames —"
  echo "хук egress на карте не держит адреса спейсов"
  tcpdump -r build/miyori-net/capture-57-main.pcap -nn 'src net 10.59.0.0/24' 2>/dev/null | head -n 20 || true
  exit 1
fi

# ноль кадров при нулевом счётчике означал бы, что до карты ничего и не доехало — тогда фильтр ни при чём
if ! is_positive_int "${dropped:-0}"; then
  echo "НАХОДКА ГЕЙТА: кадров нет, но и счётчик netdev-drop пуст (${dropped:-нет}) —"
  echo "значит кадр не дошёл до карты, и молчание на tap-captive доказывает не фильтр, а другой слой"
  printf '%s\n' "$post_block"; exit 1
fi

echo "PASS: контроль дал $control_frames кадров спейса, с netdev-фильтром — ноль при $dropped погашенных на карте"

#!/usr/bin/env bash
# VERIFY: V-NO-ZOMBIE — окна спейса исчезают вместе с его VM
#
# Гейт M1 нашёл обратное: окно переживало смерть своей VM и выглядело рабочим.
# Проверяются обе смерти: штатный stop через демона и внешний kill -9 мимо него.
#
# Что нужно до запуска (в сессии niri, от обычного пользователя, см. ручной стенд, фаза 4):
#   1. запущен miyorid и спейс в нём уже поднят и показал окно;
#   2. запущен miyori-guid — без него окна не будет вовсе;
#   3. доступ к control.sock (группа miyori) и sudo для второй половины.
set -euo pipefail
cd "$(dirname "$0")/../.."

socket="${MIYORI_SOCKET:-/run/miyorios/control.sock}"
run_dir="${MIYORI_RUN_DIR:-/run/miyorios}"
space="${MIYORI_SPACE:-spike}"

fail() { echo "FAIL: $*"; exit 1; }

[ -n "${NIRI_SOCKET:-}" ] || fail "нет NIRI_SOCKET — тест запускается изнутри сессии niri"
command -v niri >/dev/null || fail "нет niri в PATH"
[ -S "$socket" ] || fail "нет $socket — демон не запущен"

call() {
  python3 - "$socket" "$1" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(120)
s.connect(sys.argv[1])
s.sendall(sys.argv[2].encode() + b"\n")
buf = b""
# читаем до ТЕРМИНАЛЬНОГО кадра, а не до первого перевода строки: start шлёт progress, пока ждёт агента
while True:
    chunk = s.recv(65536)
    if not chunk:
        break
    buf += chunk
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if not line.strip():
            continue
        frame = json.loads(line)
        if "ok" in frame:
            print(json.dumps(frame, ensure_ascii=False))
            sys.exit(0 if frame["ok"] else 1)
sys.exit("демон не прислал терминального кадра")
PY
}

# окно спейса ищем по cgroup процесса: заголовок — то место, куда пишет гость
space_windows() {
  niri msg --json windows | python3 -c '
import json, sys
space = sys.argv[1]
found = 0
for w in json.load(sys.stdin):
    pid = w.get("pid")
    if not pid:
        continue
    try:
        cg = open(f"/proc/{pid}/cgroup").read()
    except OSError:
        continue
    if f"miyori-gui-{space}.scope" in cg:
        found += 1
print(found)
' "$space"
}

wait_gone() {
  for _ in $(seq 1 60); do
    [ "$(space_windows)" = "0" ] && return 0
    sleep 0.5
  done
  return 1
}

wait_present() {
  for _ in $(seq 1 120); do
    [ "$(space_windows)" != "0" ] && return 0
    sleep 0.5
  done
  return 1
}

# 1. положительный контроль: детектор обязан УМЕТЬ увидеть окно этого спейса,
#    иначе его молчание после stop не значит ровно ничего
before="$(space_windows)"
[ "$before" != "0" ] || fail "у спейса $space нет ни одного окна ДО остановки — детектор нечего обнаруживать, подними стенд"
echo "ok: до остановки окон спейса $space: $before"

# 2. штатный stop
call "{\"op\":\"stop\",\"space\":\"$space\"}" >/dev/null || fail "stop не отработал"
wait_gone || fail "после штатного stop окно спейса $space всё ещё в niri — оно пережило свою VM"
echo "ok: после stop окон спейса не осталось"

# 3. поднимаем снова и убиваем QEMU мимо демона
call "{\"op\":\"start\",\"space\":\"$space\"}" >/dev/null || fail "start не отработал"
wait_present || fail "после start окно спейса не появилось за 60 с — вторую половину проверять не на чем"
echo "ok: спейс снова показал окно"

pid_file="$run_dir/$space/qemu.pid"
[ -f "$pid_file" ] || fail "нет $pid_file — непонятно, кого убивать"
qemu_pid="$(cat "$pid_file")"
sudo kill -KILL "$qemu_pid" 2>/dev/null || fail "не удалось убить QEMU $qemu_pid (нужен sudo)"

wait_gone || fail "после kill -9 мимо демона окно спейса $space осталось в niri"
echo "ok: после внешнего kill -9 окон спейса не осталось"

# спейс оставляем остановленным, а не запущенным: тест не должен менять состояние стенда молча
call "{\"op\":\"stop\",\"space\":\"$space\"}" >/dev/null 2>&1 || true

echo "PASS: окна спейса $space исчезли и после штатного stop, и после внешнего kill -9"

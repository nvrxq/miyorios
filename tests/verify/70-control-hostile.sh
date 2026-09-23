#!/usr/bin/env bash
# VERIFY: враждебный ввод на control.sock не роняет демон и не исполняется
set -euo pipefail
cd "$(dirname "$0")/../.."

bin=target/release/miyorid
[ -x "$bin" ] || cargo build --release --offline -p miyorid

tmp="$(mktemp -d)"
daemon_pid=""
# kill "" безвреден и подавлен: без || true set -e обрывает trap и оставляет мусор
trap 'kill "$daemon_pid" 2>/dev/null || true; rm -rf "$tmp"' EXIT

# демон непривилегированный, во временном каталоге: --group "" — режим тестов, группу не менять
"$bin" --state-dir "$tmp/state" --run-dir "$tmp/run" --group "" >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!

sock="$tmp/run/control.sock"
for _ in $(seq 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done
[ -S "$sock" ] || {
  echo "FAIL: демон не поднял $sock"; cat "$tmp/daemon.log"; exit 1; }
kill -0 "$daemon_pid" 2>/dev/null || {
  echo "FAIL: демон не запустился"; cat "$tmp/daemon.log"; exit 1; }

python3 - "$sock" "$daemon_pid" <<'PYEOF'
import json
import os
import socket
import sys
import threading
import time

sock_path = sys.argv[1]
pid = int(sys.argv[2])
# держим в одном месте: должно совпадать с REQUEST_READ_TIMEOUT_SECS в lib/proto/src/control.rs
SERVER_READ_TIMEOUT = 5


def alive():
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(sock_path)
    return s


def hostile_send(raw):
    # проба обязана переживать RST/оборванный пайп — сама по себе враждебность и есть смысл пробы
    s = connect()
    try:
        s.sendall(raw)
    except OSError:
        pass
    try:
        s.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    resp = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            resp += chunk
    except OSError:
        pass
    try:
        s.close()
    except OSError:
        pass
    return resp


def parse_last_json(resp):
    lines = [l for l in resp.decode("utf-8", "replace").splitlines() if l.strip()]
    if not lines:
        return None
    try:
        return json.loads(lines[-1])
    except json.JSONDecodeError:
        return None


def request(obj):
    raw = (json.dumps(obj) + "\n").encode()
    return parse_last_json(hostile_send(raw))


def fail(msg):
    sys.exit(f"FAIL: {msg}")


def ok(msg):
    print(f"ok: {msg}")


def fd_count():
    return len(os.listdir(f"/proc/{pid}/fd"))


# 1. строка в 1 МиБ без \n -> демон жив, соединение закрыто
hostile_send(b"a" * (1024 * 1024))
if not alive():
    fail("1: демон погиб на строке в 1 МиБ без \\n")
ok("1: демон жив после строки в 1 МиБ без \\n")

# 2. обрыв на полукадре -> демон жив
hostile_send(b'{"op":"lis')
if not alive():
    fail("2: демон погиб на оборванном полукадре")
ok("2: демон жив после оборванного полукадра")

# 3. выход за пределы каталога -> ok:false, code bad-request, демон жив
r = request({"op": "describe", "space": "../../etc"})
if r is None or r.get("ok") is not False or r.get("code") != "bad-request":
    fail(f'3: describe "../../etc" должен вернуть ok:false code:bad-request, получено {r!r}')
if not alive():
    fail("3: демон погиб после ../../etc")
ok("3: describe отвергает выход за пределы каталога кодом bad-request")

# 4. составной путь вместо слага -> ok:false
r = request({"op": "describe", "space": "a/b"})
if r is None or r.get("ok") is not False:
    fail(f"4: describe \"a/b\" должен вернуть ok:false, получено {r!r}")
ok("4: describe отвергает составной путь вместо слага")

# 5. неизвестное поле рядом с валидными -> ok:false
r = request({"op": "describe", "space": "a", "gpu": "venus"})
if r is None or r.get("ok") is not False:
    fail(f"5: неизвестное поле должно быть отвергнуто, получено {r!r}")
ok("5: неизвестное поле рядом с валидными отвергнуто")

# 6. неизвестная операция -> ok:false, code unknown-op
r = request({"op": "whoami"})
if r is None or r.get("ok") is not False or r.get("code") != "unknown-op":
    fail(f"6: неизвестная операция должна вернуть code:unknown-op, получено {r!r}")
ok("6: неизвестная операция отвергнута кодом unknown-op")

# 7. не-UTF-8 байты -> ok:false
r = parse_last_json(hostile_send(b"\xff\xfe\xfd\n"))
if r is None or r.get("ok") is not False:
    fail(f"7: не-UTF-8 кадр должен вернуть ok:false, получено {r!r}")
ok("7: не-UTF-8 кадр отвергнут")

if not alive():
    fail("после пунктов 1-7 демон не отвечает")

# 8. 1000 соединений подряд -> все обслужены, дескрипторы не текут
fd_before = fd_count()
for i in range(1000):
    r = request({"op": "list"})
    if r is None or r.get("ok") is not True:
        fail(f"8: соединение {i} не обслужено, получено {r!r}")
fd_after = fd_count()
if fd_after > fd_before + 5:
    fail(f"8: дескрипторы утекли: было {fd_before}, стало {fd_after}")
ok(f"8: 1000 соединений подряд обслужены, дескрипторы {fd_before} -> {fd_after}")

# 9. соединение, открытое и молчащее 10 c, закрывается ДЕМОНОМ по тайм-ауту чтения,
#    а не тем, что тест сам его обрывает; параллельно другой клиент обслуживается
concurrent_result = {}


def concurrent_probe():
    # даём молчащему соединению повиснуть, прежде чем убеждаться, что демон не заблокирован им
    time.sleep(1.0)
    concurrent_result["value"] = request({"op": "list"})


prober = threading.Thread(target=concurrent_probe)
prober.start()

s_silent = connect()
s_silent.settimeout(9)
start = time.time()
first_activity = None
try:
    while True:
        chunk = s_silent.recv(4096)
        if first_activity is None:
            first_activity = time.time() - start
        if chunk == b"":
            break
except socket.timeout:
    pass
s_silent.close()
prober.join()

if first_activity is None:
    fail(f"9: демон не среагировал на молчащее соединение за 9 c (ожидался тайм-аут ~{SERVER_READ_TIMEOUT} c)")
if first_activity > SERVER_READ_TIMEOUT + 3:
    fail(f"9: демон среагировал слишком поздно ({first_activity:.1f} c) — похоже, тест сам закрыл соединение")
cr = concurrent_result.get("value")
if cr is None or cr.get("ok") is not True:
    fail(f"9: параллельное соединение не обслужено, пока другое молчало: {cr!r}")
if not alive():
    fail("9: демон погиб после молчащего соединения")
ok(
    f"9: молчащее соединение закрыто демоном за {first_activity:.1f} c "
    f"(тайм-аут ~{SERVER_READ_TIMEOUT} c), параллельное соединение обслужено"
)

# 10. после всего демон по-прежнему исправно отвечает
r = request({"op": "list"})
if r is None or r.get("ok") is not True:
    fail(f"10: {{\"op\":\"list\"}} после всех проб должен вернуть ok:true, получено {r!r}")
ok("10: демон исправно отвечает после всех проб")
PYEOF

echo "PASS: V-CONTROL-HOSTILE"

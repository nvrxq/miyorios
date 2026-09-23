#!/usr/bin/env bash
# VERIFY: V-DISK-ENCRYPTED — на диске хоста данные спейса лежат шифротекстом, и без пароля спейс не поднять.
# Положительный контроль обязателен: тот же засев в незашифрованном спейсе обязан найтись открытым текстом,
# иначе «не нашли» доказывает не шифрование, а неудачный поиск.
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root (sudo bash tests/lifecycle/79-encrypted-disk.sh) — create/start трогают qemu-img/chown" >&2
  exit 1
}

for tool in ip qemu-img qemu-system-x86_64 setpriv mkfs.ext4 python3; do
  command -v "$tool" >/dev/null || { echo "FAIL: нет $tool в PATH" >&2; exit 1; }
done

bin=target/release/miyorid
[ -x "$bin" ] || cargo build --release --offline -p miyorid

template="build/templates/spike/latest"
for f in root.qcow2 vmlinuz initrd.img; do
  [ -f "$template/$f" ] || {
    echo "FAIL: нет $template/$f — собери: bash tools/build-profile.sh profiles/spike" >&2
    exit 1
  }
done

tmp=""
run=""
daemon_pid=""
created_bridge=0

cleanup() {
  set +e
  for id in plain crypt; do
    [ -n "$run" ] && [ -f "$run/$id/qemu.pid" ] && kill -KILL "$(cat "$run/$id/qemu.pid")" 2>/dev/null
  done
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  if [ "$created_bridge" = 1 ]; then ip link del br-spaces 2>/dev/null; fi
  [ -n "$tmp" ] && rm -rf "$tmp"
}
trap cleanup EXIT

tmp="$(mktemp -d)"
# QEMU работает под uid спейса и в 0700 root не пройдёт; в бою /var/lib/miyorios обходится всеми
chmod 0755 "$tmp"
state="$tmp/state"
run="$tmp/run"
mkdir -p "$state/profiles" "$state/templates" "$state/spaces" "$run"
ln -s "$(realpath profiles/spike)" "$state/profiles/spike"
digest="$(readlink build/templates/spike/latest)"
mkdir -p "$state/templates/spike/$digest"
for f in root.qcow2 vmlinuz initrd.img MANIFEST; do
  ln "build/templates/spike/$digest/$f" "$state/templates/spike/$digest/$f"
done
ln -s "$digest" "$state/templates/spike/latest"

if ip link show br-spaces >/dev/null 2>&1; then
  created_bridge=0
else
  ip link add br-spaces type bridge
  ip link set br-spaces up
  created_bridge=1
fi

"$bin" --state-dir "$state" --run-dir "$run" --group "" --registry "$tmp/registry.toml" \
  >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!

sock="$run/control.sock"
for _ in $(seq 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done
[ -S "$sock" ] || { echo "FAIL: демон не поднял $sock"; cat "$tmp/daemon.log"; exit 1; }

python3 - "$sock" "$daemon_pid" "$run" "$state" "$tmp" <<'PYEOF'
import json
import os
import socket
import sys

sock_path, pid_str, run_dir, state_dir, tmp_dir = sys.argv[1:6]
daemon_pid = int(pid_str)

# метка обязана быть длиннее кластера-нуля и уникальной: короткую строку можно случайно встретить в метаданных ext4
MARKER = "МЕТКА-ШИФРОВАНИЯ-2026-08-31-a7f3c9e1b4d20586"
PASSPHRASE = "пароль спейса, длинный и с пробелами"


def dump(label, path, lines=12):
    try:
        with open(path, errors="replace") as f:
            tail = f.read().splitlines()[-lines:]
    except OSError as err:
        tail = [f"(нет {path}: {err})"]
    print(f"--- {label} ---")
    for line in tail:
        print(f"    {line}")


def fail(msg, space=None):
    print(f"FAIL: {msg}")
    dump("лог демона", os.path.join(tmp_dir, "daemon.log"))
    if space:
        dump(
            f"последний запуск {space}",
            os.path.join(state_dir, "spaces", space, "last-run.log"),
        )
    sys.exit(1)


def ok(msg):
    print(f"ok: {msg}")


def request(obj, timeout=60):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps(obj) + "\n").encode())
    f = s.makefile("r", encoding="utf-8", newline="\n")
    result = None
    for line in f:
        line = line.strip()
        if not line:
            continue
        parsed = json.loads(line)
        if "ok" in parsed:
            result = parsed
            break
    s.close()
    return result


def data_volume(space):
    return os.path.join(state_dir, "spaces", space, "data.qcow2")


def marker_on_disk(space):
    with open(data_volume(space), "rb") as f:
        return MARKER.encode() in f.read()


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


seed = os.path.join(tmp_dir, "seed")
os.makedirs(seed, exist_ok=True)
with open(os.path.join(seed, "marker.txt"), "w") as f:
    f.write(MARKER + "\n")

if not alive(daemon_pid):
    fail("демон не запущен перед началом проверок")

# 1. положительный контроль: незашифрованный спейс с тем же засевом — метка обязана найтись открытым текстом
r = request({
    "op": "create", "space": "plain", "profile": "spike",
    "label": "untrusted", "color": "#e03131", "seed": seed,
})
if r is None or r.get("ok") is not True:
    fail(f"1: create plain должен вернуть ok:true, получено {r!r}")
if not marker_on_disk("plain"):
    fail("1: положительный контроль провален — метки нет и в НЕзашифрованном томе, значит поиск негоден")
ok("1: положительный контроль — засеянная метка видна в открытом data.qcow2")

# 2. тот же засев с паролем — метки на диске хоста быть не должно
r = request({
    "op": "create", "space": "crypt", "profile": "spike",
    "label": "untrusted", "color": "#e03131", "seed": seed,
    "passphrase": PASSPHRASE,
})
if r is None or r.get("ok") is not True:
    fail(f"2: create crypt должен вернуть ok:true, получено {r!r}")
if marker_on_disk("crypt"):
    fail("2: метка найдена в зашифрованном data.qcow2 — том не зашифрован")
ok("2: в зашифрованном data.qcow2 засеянной метки нет")

# 3. describe обязан признаться, что спейс зашифрован: менеджеру по этому полю решать, спрашивать ли пароль
r = request({"op": "describe", "space": "crypt"})
if r is None or r.get("ok") is not True or r["data"].get("encrypted") is not True:
    fail(f"3: describe crypt должен отдать encrypted:true, получено {r!r}")
r = request({"op": "describe", "space": "plain"})
if r is None or r.get("ok") is not True or r["data"].get("encrypted") is not False:
    fail(f"3: describe plain должен отдать encrypted:false, получено {r!r}")
ok("3: describe различает зашифрованный и обычный спейс")

# 4. пустой пароль — отказ, а не молчаливое создание без шифрования
r = request({
    "op": "create", "space": "empty-pass", "profile": "spike",
    "label": "untrusted", "color": "#e03131", "passphrase": "",
})
if r is None or r.get("ok") is not False or r.get("code") != "bad-request":
    fail(f"4: create с пустым паролем должен вернуть bad-request, получено {r!r}")
ok("4: пустой пароль отвергнут, спейс без шифрования молча не создан")

# 5. start зашифрованного без пароля — отказ до запуска QEMU
r = request({"op": "start", "space": "crypt"})
if r is None or r.get("ok") is not False or r.get("code") != "bad-request":
    fail(f"5: start без пароля должен вернуть bad-request, получено {r!r}", "crypt")
if os.path.exists(os.path.join(run_dir, "crypt", "qemu.pid")):
    fail("5: демон завёл QEMU для спейса, пароль к которому не спрашивал", "crypt")
ok("5: зашифрованный спейс без пароля не запускается")

# 6. неверный пароль — отдельный код, а не «внутренняя ошибка»: оператору надо понять, что именно не так
r = request({"op": "start", "space": "crypt", "passphrase": PASSPHRASE + "!"})
if r is None or r.get("ok") is not False or r.get("code") != "wrong-passphrase":
    fail(f"6: start с неверным паролем должен вернуть wrong-passphrase, получено {r!r}", "crypt")
ok("6: неверный пароль назван неверным паролем")

# 7. верный пароль — спейс поднимается по-настоящему
r = request({"op": "start", "space": "crypt", "passphrase": PASSPHRASE})
if r is None or r.get("ok") is not True:
    fail(f"7: start с верным паролем должен вернуть ok:true, получено {r!r}", "crypt")
if r["data"].get("state") != "running":
    fail(f"7: ожидалось state=running, получено {r['data']!r}", "crypt")
with open(os.path.join(run_dir, "crypt", "qemu.pid")) as f:
    qemu_pid = int(f.read().strip())
if not alive(qemu_pid):
    fail("7: QEMU не жив после успешного start", "crypt")
ok("7: с верным паролем спейс запущен и QEMU жив")

# 8. пароль не имеет права попасть ни в один файл, который переживёт запуск
r = request({"op": "stop", "space": "crypt"})
if r is None or r.get("ok") is not True:
    fail(f"8: stop crypt должен вернуть ok:true, получено {r!r}", "crypt")
leaked = []
for root, _, files in os.walk(state_dir):
    for name in files:
        path = os.path.join(root, name)
        try:
            with open(path, "rb") as f:
                if PASSPHRASE.encode() in f.read():
                    leaked.append(path)
        except OSError:
            continue
with open(os.path.join(tmp_dir, "daemon.log"), "rb") as f:
    if PASSPHRASE.encode() in f.read():
        leaked.append("daemon.log")
if leaked:
    fail(f"8: пароль найден открытым текстом в {leaked}")
ok("8: пароля нет ни в состоянии на диске, ни в журнале демона")

# 9. файл секрета не переживает остановку: пока он есть, пароль лежит в tmpfs
secret = os.path.join(run_dir, "secrets", "crypt", "pass")
if os.path.exists(secret):
    fail(f"9: {secret} остался после stop")
ok("9: файл секрета убран после остановки")

for space in ("crypt", "plain"):
    r = request({"op": "destroy", "space": space})
    if r is None or r.get("ok") is not True:
        fail(f"10: destroy {space} должен вернуть ok:true, получено {r!r}")
ok("10: оба спейса убраны")

print("PASS: V-DISK-ENCRYPTED")
PYEOF

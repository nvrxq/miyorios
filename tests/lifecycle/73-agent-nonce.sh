#!/usr/bin/env bash
# VERIFY: V-AGENT-NONCE — без верного per-boot nonce гость демону живым не считается
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root (sudo bash tests/lifecycle/73-agent-nonce.sh) — реальный QEMU, tap и /dev/kvm" >&2
  exit 1
}

for tool in ip bridge nft qemu-img qemu-system-x86_64 setpriv python3 cargo; do
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

# пункты 3, 5, 6 — health() против настоящего локального CID (vsock_loopback), без KVM и без гостя:
# демон отвечает не на выдумку теста, а на собственную функцию crate::agent::health, вызванную по-настоящему
echo "== cargo test -p miyorid agent:: (пункты 3, 5, 6) =="
cargo_out="$(cargo test --offline -p miyorid --lib agent:: 2>&1)" && cargo_rc=0 || cargo_rc=$?
printf '%s\n' "$cargo_out" | tail -20
[ "$cargo_rc" -eq 0 ] || {
  echo "FAIL: cargo test -p miyorid agent:: не прошёл — это и есть пункты 3/5/6 плана" >&2
  exit 1
}
for needle in health_rejects_wrong_nonce health_rejects_oversized_reply_without_hanging health_times_out_when_agent_is_silent; do
  grep -q "$needle ... ok" <<<"$cargo_out" || {
    echo "FAIL: не нашли пройденный тест $needle в выводе cargo test" >&2
    exit 1
  }
done
echo "ok: 3, 5, 6 подтверждены cargo test (health() демона против настоящего локального CID)"

# объявлены пустыми ДО trap: под set -u ссылка на них в cleanup() не должна упасть, если провал случится раньше
tmp=""
run=""
daemon_pid=""
created_bridge=0
fake_pid=""
# tap фикстуры сносить нельзя: CID выдаёт демон, и он не обязан совпасть с тем, что зашит в тесте
taps_before=" $(ip -br link show type tun 2>/dev/null | awk '{print $1}' | grep '^tap-space-' | tr '\n' ' ')"

cleanup() {
  set +e
  [ -n "$run" ] && [ -f "$run/telegram/qemu.pid" ] && kill -KILL "$(cat "$run/telegram/qemu.pid")" 2>/dev/null
  [ -n "$fake_pid" ] && kill -KILL "$fake_pid" 2>/dev/null
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  for t in $(ip -br link show type tun 2>/dev/null | awk '{print $1}' | grep '^tap-space-'); do
    case "$taps_before" in *" $t "*) ;; *) ip link del "$t" 2>/dev/null ;; esac
  done
  if [ "$created_bridge" = 1 ]; then ip link del br-spaces 2>/dev/null; fi
  [ -n "$tmp" ] && rm -rf "$tmp"
}
trap cleanup EXIT

tmp="$(mktemp -d)"
# mktemp отдаёт 0700 root, а QEMU работает под uid спейса и внутрь не пройдёт;
# в бою /var/lib/miyorios обходится всеми, и тест обязан воспроизводить именно это
chmod 0755 "$tmp"
state="$tmp/state"
run="$tmp/run"
mkdir -p "$state/profiles" "$state/templates" "$state/spaces" "$run"
ln -s "$(realpath profiles/spike)" "$state/profiles/spike"
# шаблон подставляем жёсткими ссылками, а не симлинком в репозиторий: QEMU работает под uid
# спейса, а домашний каталог оператора имеет режим 0750 и чужому uid непроходим — в бою шаблоны лежат под /var/lib
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
[ -S "$sock" ] || {
  echo "FAIL: демон не поднял $sock"; cat "$tmp/daemon.log"; exit 1;
}

echo "== часть с настоящим QEMU: create/start telegram, ждём загрузки и живого агента =="
python3 - "$sock" "$run" "$state" "$tmp" <<'PYEOF'
import json
import os
import socket
import sys
import time

sock_path, run_dir, state_dir, tmp_dir = sys.argv[1:5]


# без этого причина падения остаётся в логах, которые уборка теста сносит, и приходится гадать
def dump(label, path, lines=12):
    try:
        with open(path, errors="replace") as f:
            tail = f.read().splitlines()[-lines:]
    except OSError as err:
        tail = [f"(нет {path}: {err})"]
    print(f"--- {label} ---")
    for line in tail:
        print(f"    {line}")


def fail(msg):
    print(f"FAIL: {msg}")
    dump("лог демона", os.path.join(tmp_dir, "daemon.log"))
    dump(
        "последний запуск telegram",
        os.path.join(state_dir, "spaces", "telegram", "last-run.log"),
    )
    sys.exit(1)


def ok(msg):
    print(f"ok: {msg}")


def request(obj, timeout=30):
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


def describe(space):
    r = request({"op": "describe", "space": space})
    if r is None or r.get("ok") is not True:
        fail(f"describe {space} должен вернуть ok:true, получено {r!r}")
    return r["data"]


# create — тест не изобретает nonce и не поднимает tap сам, это делает демон (правило "не подсовывать")
r = request(
    {"op": "create", "space": "telegram", "profile": "spike", "label": "untrusted", "color": "#e03131"}
)
if r is None or r.get("ok") is not True:
    fail(f"create telegram: {r!r}")
# CID назначает демон, пропуская занятые на хосте tap, — берём выданный, а не угаданный
cid = r["data"]["cid"]
if not isinstance(cid, int) or cid < 3 or cid > 255 or cid == 9:
    fail(f"демон выдал негодный cid {cid!r}")

r = request({"op": "start", "space": "telegram"})
if r is None or r.get("ok") is not True:
    fail(f"start telegram: {r!r}")
ok(f"1: демон создал и стартовал telegram с собственным per-boot nonce (cid={cid})")

# 1/2: ждём, пока гость реально загрузится и агент реально ответит верным nonce — тест не торопит и не подделывает
state = None
deadline = time.time() + 90
while time.time() < deadline:
    state = describe("telegram")["state"]
    if state == "running":
        break
    time.sleep(2)
if state != "running":
    log_path = os.path.join(state_dir, "spaces", "telegram", "last-run.log")
    tail = ""
    try:
        with open(log_path, "rb") as f:
            tail = f.read()[-2000:].decode("utf-8", "replace")
    except OSError:
        pass
    fail(f"агент telegram не подтвердил nonce за 90 с (state={state!r}); хвост консоли:\n{tail}")
ok("1/2: describe сообщает running — реальный агент реально подтвердил per-boot nonce")

nonce_path = os.path.join(run_dir, "telegram", "nonce")
with open(nonce_path) as f:
    real_nonce = f.read().strip()
if len(real_nonce) != 32 or any(c not in "0123456789abcdef" for c in real_nonce):
    fail(f"nonce из run/telegram/nonce не похож на настоящий (16 байт hex): {real_nonce!r}")

pid_path = os.path.join(run_dir, "telegram", "qemu.pid")
with open(pid_path) as f:
    qemu_pid = int(f.read().strip())
with open(f"/proc/{qemu_pid}/cmdline", "rb") as f:
    cmdline = f.read()
needle = f"MIYORI_NONCE={real_nonce}".encode()
if needle not in cmdline:
    fail("2: nonce из run/telegram/nonce не найден в -append настоящего процесса QEMU")
ok("2: nonce, которым описывает running, буквально совпадает с тем, что демон положил в -append")

# 3 — центр теста: агент настоящий и не менялся, порчим ТОЛЬКО ожидание демона и смотрим на ОТКАЗ
wrong_nonce = "f" * 32 if real_nonce != "f" * 32 else "0" * 32
with open(nonce_path, "w") as f:
    f.write(wrong_nonce)
state = describe("telegram")["state"]
if state != "unresponsive":
    fail(f"3: демон должен отвергнуть подмену nonce (unresponsive), получено state={state!r}")
ok("3: демон НЕ считает живым настоящего агента, ответившего не тем nonce, которого демон теперь ждёт")

# восстановление доказывает, что дело в сравнении, а не в разовом сбое или кэше
with open(nonce_path, "w") as f:
    f.write(real_nonce)
state = describe("telegram")["state"]
if state != "running":
    fail(f"после восстановления настоящего nonce ожидался running, получено state={state!r}")
ok("восстановление верного nonce возвращает running — проверка живая, не кэшированная")
PYEOF

echo "== пункт 4: живой процесс, но агент недостижим (тот же приём, что в ops.rs::spawn_fake_qemu) =="
ghost_out="$(python3 - "$sock" <<'PYEOF'
import json
import socket
import sys

sock_path = sys.argv[1]


def request(obj, timeout=10):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps(obj) + "\n").encode())
    f = s.makefile("r", encoding="utf-8", newline="\n")
    for line in f:
        line = line.strip()
        if not line:
            continue
        parsed = json.loads(line)
        if "ok" in parsed:
            s.close()
            return parsed
    s.close()
    return None


r = request(
    {"op": "create", "space": "ghost", "profile": "spike", "label": "untrusted", "color": "#2f9e44"}
)
if r is None or r.get("ok") is not True:
    sys.exit(f"FAIL: create ghost: {r!r}")
print(f"GHOST-CID={r['data']['cid']}")
PYEOF
)"
printf '%s\n' "$ghost_out"
# CID второго спейса тоже назначает демон: имя поддельного бинаря обязано совпасть с ним,
# иначе state_of не признает процесс своим и пункт 4 проверит не то
ghost_cid="$(printf '%s\n' "$ghost_out" | sed -n 's/^GHOST-CID=//p')"
[ -n "$ghost_cid" ] || { echo "FAIL: не удалось получить CID спейса ghost"; exit 1; }
fake_bin="$run/qemu-system-x86_64-guest-cid=$ghost_cid"
ln -sf "$(command -v sleep)" "$fake_bin"
mkdir -p "$run/ghost"
"$fake_bin" 300 &
fake_pid=$!
echo "$fake_pid" > "$run/ghost/qemu.pid"
# любой nonce годится: цель — недостижимый агент, а не сравнение (это пункт 3, уже проверен на telegram)
echo "0000000000000000000000000000dead" > "$run/ghost/nonce"

python3 - "$sock" <<'PYEOF'
import json
import socket
import sys

sock_path = sys.argv[1]


def request(obj, timeout=10):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps(obj) + "\n").encode())
    f = s.makefile("r", encoding="utf-8", newline="\n")
    for line in f:
        line = line.strip()
        if not line:
            continue
        parsed = json.loads(line)
        if "ok" in parsed:
            s.close()
            return parsed
    s.close()
    return None


r = request({"op": "describe", "space": "ghost"})
if r is None or r.get("ok") is not True:
    sys.exit(f"FAIL: describe ghost: {r!r}")
state = r["data"]["state"]
if state != "unresponsive":
    sys.exit(f"FAIL: 4: живой процесс без живого агента должен быть unresponsive, получено {state!r}")
print("ok: 4: живой процесс без отвечающего агента -> unresponsive, не running и не stopped")
PYEOF

kill -KILL "$fake_pid" 2>/dev/null || true
fake_pid=""

echo "PASS: V-AGENT-NONCE"

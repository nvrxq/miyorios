#!/usr/bin/env bash
# VERIFY: V-LIFECYCLE — create/start/stop/destroy настоящего спейса и tap, поднятый демоном, не тестом
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root (sudo bash tests/lifecycle/71-lifecycle.sh) — create/start трогают ip/qemu-img/chown" >&2
  exit 1
}

for tool in ip bridge nft qemu-img qemu-system-x86_64 setpriv python3; do
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
[ -f profiles/spike/manifest.toml ] || { echo "FAIL: нет profiles/spike/manifest.toml" >&2; exit 1; }

# объявлены пустыми ДО trap: под set -u ссылка на них в cleanup() не должна упасть, если провал случится раньше присвоения
tmp=""
run=""
daemon_pid=""
created_bridge=0

cleanup() {
  set +e
  # kill daemon_pid не убивает его детей — QEMU, если тест упал между start и stop, добить надо отдельно
  for id in telegram counter orphan; do
    [ -n "$run" ] && [ -f "$run/$id/qemu.pid" ] && kill -KILL "$(cat "$run/$id/qemu.pid")" 2>/dev/null
  done
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  # только свой: до этого файла tap либо не наш, либо ещё не создан, и сносить его нельзя
  [ -n "$tmp" ] && [ -f "$tmp/tap-ours" ] && ip link del "$(cat "$tmp/tap-ours")" 2>/dev/null
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
# профиль и уже собранный шаблон переиспользуем как есть — тест не пересобирает то, что проверяет build-profile.sh
ln -s "$(realpath profiles/spike)" "$state/profiles/spike"
# шаблон подставляем жёсткими ссылками, а не симлинком в репозиторий: QEMU работает под uid
# спейса, а домашний каталог оператора имеет режим 0750 и чужому uid непроходим — в бою шаблоны лежат под /var/lib
digest="$(readlink build/templates/spike/latest)"
mkdir -p "$state/templates/spike/$digest"
for f in root.qcow2 vmlinuz initrd.img MANIFEST; do
  ln "build/templates/spike/$digest/$f" "$state/templates/spike/$digest/$f"
done
ln -s "$digest" "$state/templates/spike/latest"

# br-spaces — инфраструктура фикстуры (решение C), не предмет проверки; tap-space-<cid> тест только читает
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
kill -0 "$daemon_pid" 2>/dev/null || {
  echo "FAIL: демон не запустился"; cat "$tmp/daemon.log"; exit 1;
}

python3 - "$sock" "$daemon_pid" "$run" "$state" "$tmp" <<'PYEOF'
import json
import os
import re
import signal
import socket
import subprocess
import sys
import time

sock_path, pid_str, run_dir, state_dir, tmp_dir = sys.argv[1:6]
daemon_pid = int(pid_str)
# CID назначает демон, пропуская занятые на хосте; тест обязан взять выданный, а не угадать
cid = None
tap = None


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


def fail(msg, space="telegram"):
    print(f"FAIL: {msg}")
    dump("лог демона", os.path.join(tmp_dir, "daemon.log"))
    dump(
        f"последний запуск {space}",
        os.path.join(state_dir, "spaces", space, "last-run.log"),
    )
    sys.exit(1)


def ok(msg):
    print(f"ok: {msg}")


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


# после kill -9 процесс висит зомби, пока родитель-демон не сделает wait, и kill(pid,0) на нём
# по-прежнему отвечает успехом — смерть QEMU проверяем по состоянию в /proc, а не по сигналу (72-graceful.sh)
def stopped_running(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            state = f.read().rsplit(") ", 1)[1].split(None, 1)[0]
    except OSError:
        return True
    return state == "Z"


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


def create_telegram():
    return request(
        {
            "op": "create",
            "space": "telegram",
            "profile": "spike",
            "label": "untrusted",
            "color": "#e03131",
        }
    )


def sh_json(*args):
    out = subprocess.run(args, capture_output=True, text=True)
    if out.returncode != 0:
        return None
    return json.loads(out.stdout)


def link_json(name):
    rows = sh_json("ip", "-j", "link", "show", name)
    return rows[0] if rows else None


def bridge_port_json(name):
    rows = sh_json("bridge", "-j", "-d", "link", "show", "dev", name)
    return rows[0] if rows else None


def qemu_pid_of(space_id):
    with open(os.path.join(run_dir, space_id, "qemu.pid")) as f:
        return int(f.read().strip())


def is_real_qemu(pid, cid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            data = f.read()
    except FileNotFoundError:
        return False
    return b"qemu-system-x86_64" in data and f"guest-cid={cid}".encode() in data


if not alive(daemon_pid):
    fail("демон не запущен перед началом проверок")

# 1. create telegram профиля spike -> ok, каталог заведён, CID выдан, config.toml принадлежит root
r = create_telegram()
if r is None or r.get("ok") is not True:
    fail(f"1: create telegram должен вернуть ok:true, получено {r!r}")
cid = r["data"].get("cid")
if not isinstance(cid, int) or cid < 3 or cid > 255 or cid == 9:
    fail(f"1: демон выдал негодный cid {cid!r}")
tap = f"tap-space-{cid}"
if os.path.exists(f"/sys/class/net/{tap}"):
    fail(f"1: демон выдал cid {cid}, чей {tap} уже занят на хосте")
if r["data"].get("uid") != 70000 + cid:
    fail(f"1: ожидался uid {70000 + cid}, получено {r['data'].get('uid')!r}")
space_dir = os.path.join(state_dir, "spaces", "telegram")
if not os.path.isdir(space_dir):
    fail("1: каталог спейса не создан")
config_path = os.path.join(space_dir, "config.toml")
if not os.path.isfile(config_path):
    fail("1: нет config.toml")
if os.stat(config_path).st_uid != 0:
    fail(f"1: config.toml должен принадлежать root, владелец uid={os.stat(config_path).st_uid}")
ok(f"1: create завёл каталог, выдал cid={cid}/uid и config.toml принадлежит root")

# 2. create telegram ещё раз -> ok:false, space-exists
r = create_telegram()
if r is None or r.get("ok") is not False or r.get("code") != "space-exists":
    fail(f"2: повторный create должен вернуть code:space-exists, получено {r!r}")
ok("2: повторный create отвергнут кодом space-exists")

# 3. start telegram -> ok, running; tap-space-<cid> появился ОТ демона, а не от теста; процесс жив по /proc
if link_json(tap) is not None:
    fail(f"3: {tap} уже существует до start — тест не должен создавать то, что обязан завести демон")
r = request({"op": "start", "space": "telegram"})
if r is None or r.get("ok") is not True:
    fail(f"3: start telegram должен вернуть ok:true, получено {r!r}")
if r["data"].get("state") != "running":
    fail(f"3: ожидалось state=running, получено {r['data']!r}")
link = link_json(tap)
if link is None:
    fail(f"3: {tap} не появился после start")
if link.get("master") != "br-spaces":
    fail(f"3: {tap} не в br-spaces: master={link.get('master')!r}")
if "UP" not in link.get("flags", []):
    fail(f"3: {tap} не поднят (up): flags={link.get('flags')!r}")
port = bridge_port_json(tap)
if port is None or port.get("isolated") is not True:
    fail(f"3: {tap} не изолирован портом моста: {port!r}")
pid = qemu_pid_of("telegram")
if not is_real_qemu(pid, cid):
    fail(f"3: pid {pid} из qemu.pid не похож на настоящий процесс QEMU этого спейса")
with open(os.path.join(tmp_dir, "tap-ours"), "w") as f:
    f.write(tap)
ok(f"3: start поднял {tap} (master br-spaces, up, isolated) и живой процесс QEMU pid={pid}")

# 4. start telegram ещё раз -> ok:false, space-running
r = request({"op": "start", "space": "telegram"})
if r is None or r.get("ok") is not False or r.get("code") != "space-running":
    fail(f"4: повторный start должен вернуть code:space-running, получено {r!r}")
ok("4: повторный start отвергнут кодом space-running")

# 5. stop telegram -> ok, stopped; tap убран, pid-файл убран, процесс действительно мёртв
pid_file = os.path.join(run_dir, "telegram", "qemu.pid")
r = request({"op": "stop", "space": "telegram"})
if r is None or r.get("ok") is not True:
    fail(f"5: stop telegram должен вернуть ok:true, получено {r!r}")
if r["data"].get("state") != "stopped":
    fail(f"5: ожидалось state=stopped, получено {r['data']!r}")
if link_json(tap) is not None:
    fail(f"5: {tap} всё ещё существует после stop")
if os.path.exists(pid_file):
    fail("5: pid-файл не убран после stop")
if alive(pid):
    fail(f"5: процесс {pid} всё ещё жив после stop")
os.remove(os.path.join(tmp_dir, "tap-ours"))
ok(f"5: stop убрал {tap}, pid-файл и остановил процесс {pid}")

# 6. stop telegram ещё раз -> ok:false, space-stopped
r = request({"op": "stop", "space": "telegram"})
if r is None or r.get("ok") is not False or r.get("code") != "space-stopped":
    fail(f"6: повторный stop должен вернуть code:space-stopped, получено {r!r}")
ok("6: повторный stop отвергнут кодом space-stopped")

# 7. destroy telegram -> ok, каталог исчез (свобода CID проверяется пересозданием в пункте 9)
r = request({"op": "destroy", "space": "telegram"})
if r is None or r.get("ok") is not True:
    fail(f"7: destroy telegram должен вернуть ok:true, получено {r!r}")
if os.path.exists(space_dir):
    fail("7: каталог спейса не удалён")
ok("7: destroy удалил каталог спейса")

# 8. start несуществующего -> ok:false, space-not-found
r = request({"op": "start", "space": "ghost"})
if r is None or r.get("ok") is not False or r.get("code") != "space-not-found":
    fail(f"8: start несуществующего должен вернуть code:space-not-found, получено {r!r}")
ok("8: start несуществующего спейса отвергнут кодом space-not-found")

# 9. пересоздаём telegram (заодно доказывает "CID свободен" из пункта 7) и проверяем no-bridge без моста
r = create_telegram()
if r is None or r.get("ok") is not True:
    fail(f"9: пересоздание telegram не удалось: {r!r}")
if r["data"].get("cid") != cid:
    fail(f"9: CID не освободился после destroy — ожидался {cid}, получено {r['data'].get('cid')!r}")

subprocess.run(["ip", "link", "del", "br-spaces"], capture_output=True)
try:
    r = request({"op": "start", "space": "telegram"})
    if r is None or r.get("ok") is not False or r.get("code") != "no-bridge":
        fail(f"9: start без br-spaces должен вернуть code:no-bridge, получено {r!r}")
    if "net-fixture.sh" not in r.get("message", ""):
        fail(f"9: сообщение no-bridge должно называть net-fixture.sh, получено {r.get('message')!r}")
    if link_json(tap) is not None:
        fail(f"9: {tap} не должен появляться, если start отказал по no-bridge")
finally:
    subprocess.run(["ip", "link", "add", "br-spaces", "type", "bridge"], check=True)
    subprocess.run(["ip", "link", "set", "br-spaces", "up"], check=True)
ok("9: start без моста отвергнут кодом no-bridge с упоминанием net-fixture.sh, tap не создан")

# уборка вслед за собой: тест не должен оставлять спейс, который сам же завёл в пункте 9
r = request({"op": "destroy", "space": "telegram"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: destroy telegram в конце теста не удался: {r!r}")

# =========================================================================
# половина 2: reset-system и reset-all — различаем системный и пользовательский маркер по двум
# счётчикам загрузок, которые печатает miyori-init (задача 9): системный в /var/lib (system-overlay,
# его и откатывает reset-system), пользовательский на /data (data.qcow2, его трогает только reset-all)
# =========================================================================


# start синхронно отвечает "running" сразу после спавна QEMU, не дожидаясь гостя (задача 6) —
# полной загрузки, а значит и появления блока MIYORI-BOOT в консоли, ждём через describe/агента
def wait_running(space, deadline_s=90):
    state = None
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        r = request({"op": "describe", "space": space})
        if r is None or r.get("ok") is not True:
            fail(f"describe {space} во время ожидания running: {r!r}", space)
        state = r["data"]["state"]
        if state == "running":
            return state
        time.sleep(2)
    fail(f"{space} не дошёл до running за {deadline_s} с (state={state!r})", space)


def last_run_log(space):
    try:
        with open(os.path.join(state_dir, "spaces", space, "last-run.log"), errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def block_between(text, begin_marker, end_marker):
    start = text.find(begin_marker)
    end = text.find(end_marker)
    if start == -1 or end == -1 or end < start:
        return ""
    return text[start:end]


def boot_counts(space):
    block = block_between(
        last_run_log(space), "---MIYORI-BOOT-BEGIN---", "---MIYORI-BOOT-END---"
    )
    sysm = re.search(r"BOOT-COUNT-SYSTEM: (\S+)", block)
    datam = re.search(r"BOOT-COUNT-DATA: (\S+)", block)
    if not sysm or not datam:
        fail(f"блок MIYORI-BOOT не найден или неполон в консоли {space}:\n{block!r}", space)
    data_raw = datam.group(1)
    return int(sysm.group(1)), (None if data_raw == "-" else int(data_raw))


r = request(
    {"op": "create", "space": "counter", "profile": "spike", "label": "untrusted", "color": "#1971c2"}
)
if r is None or r.get("ok") is not True:
    fail(f"10: create counter должен вернуть ok:true, получено {r!r}")
# tap-ours телеграма уже убран (пункт 5) и не появлялся заново (пункт 9 бьётся о no-bridge раньше tap) —
# один и тот же файл безопасно переиспользовать под tap counter'а до самой уборки в конце теста
with open(os.path.join(tmp_dir, "tap-ours"), "w") as f:
    f.write(f"tap-space-{r['data']['cid']}")
ok("10: create counter для проверки reset-system/reset-all")

r = request({"op": "start", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"10: start counter (загрузка 1) должен вернуть ok:true, получено {r!r}", "counter")
wait_running("counter")

r = request({"op": "stop", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"10: stop counter после загрузки 1: {r!r}", "counter")

r = request({"op": "start", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"10: start counter (загрузка 2) должен вернуть ok:true, получено {r!r}", "counter")
wait_running("counter")
counts = boot_counts("counter")
if counts != (2, 2):
    fail(f"10: старт, стоп, старт — ожидались счётчики (2, 2), получено {counts!r}", "counter")
ok("10: старт, стоп, старт, стоп -> system=2, data=2")

r = request({"op": "stop", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"10: stop counter после загрузки 2: {r!r}", "counter")

# 11. reset-system откатывает только system-overlay.qcow2 — данные должны пережить сброс
r = request({"op": "reset-system", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"11: reset-system counter должен вернуть ok:true, получено {r!r}", "counter")
ok("11: reset-system counter принят")

r = request({"op": "start", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"11: start counter после reset-system: {r!r}", "counter")
wait_running("counter")
counts = boot_counts("counter")
if counts != (1, 3):
    fail(f"11: reset-system, старт — ожидались счётчики (1, 3), получено {counts!r}", "counter")
ok("11: reset-system, старт -> system=1 (система откатилась), data=3 (данные целы)")

r = request({"op": "stop", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"12: stop counter перед reset-all: {r!r}", "counter")

# 12. reset-all откатывает и system-overlay.qcow2, и data.qcow2
r = request({"op": "reset-all", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"12: reset-all counter должен вернуть ok:true, получено {r!r}", "counter")
ok("12: reset-all counter принят")

r = request({"op": "start", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"12: start counter после reset-all: {r!r}", "counter")
wait_running("counter")
counts = boot_counts("counter")
if counts != (1, 1):
    fail(f"12: reset-all, старт — ожидались счётчики (1, 1), получено {counts!r}", "counter")
ok("12: reset-all, старт -> system=1, data=1 (оба откатились)")

# 13. подменить том под живым QEMU — порча данных: reset-system на запущенном спейсе обязан отказать
r = request({"op": "reset-system", "space": "counter"})
if r is None or r.get("ok") is not False or r.get("code") != "space-running":
    fail(f"13: reset-system на запущенном должен вернуть code:space-running, получено {r!r}", "counter")
ok("13: reset-system на запущенном спейсе отвергнут кодом space-running")

# уборка вслед за собой: тест не должен оставлять спейс, который сам же завёл в пункте 10
r = request({"op": "stop", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: stop counter в конце теста не удался: {r!r}", "counter")
r = request({"op": "destroy", "space": "counter"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: destroy counter в конце теста не удался: {r!r}", "counter")
os.remove(os.path.join(tmp_dir, "tap-ours"))

# 14. QEMU убит kill -9 в обход демона (крах/OOM/оператор, приём как в 72-graceful.sh) — stop не звался,
# и destroy обязан снять tap сам, иначе он утечёт и навсегда сожжёт CID (store.rs::allocate_cid)
r = request(
    {"op": "create", "space": "orphan", "profile": "spike", "label": "untrusted", "color": "#f08c00"}
)
if r is None or r.get("ok") is not True:
    fail(f"14: create orphan должен вернуть ok:true, получено {r!r}", "orphan")
orphan_tap = f"tap-space-{r['data']['cid']}"

r = request({"op": "start", "space": "orphan"})
if r is None or r.get("ok") is not True:
    fail(f"14: start orphan должен вернуть ok:true, получено {r!r}", "orphan")
wait_running("orphan")
with open(os.path.join(tmp_dir, "tap-ours"), "w") as f:
    f.write(orphan_tap)

pid = qemu_pid_of("orphan")
os.kill(pid, signal.SIGKILL)
deadline = time.time() + 10
while not stopped_running(pid) and time.time() < deadline:
    time.sleep(0.2)
if not stopped_running(pid):
    fail("14: QEMU спейса orphan всё ещё выполняется через 10 с после kill -9", "orphan")
ok(f"14: QEMU спейса orphan (pid={pid}) убит kill -9 в обход демона, stop не вызывался")

# без этой проверки пункт позеленел бы и в случае, когда tap не создавался вовсе — утекать было бы нечему
if not os.path.exists(f"/sys/class/net/{orphan_tap}"):
    fail(f"14: {orphan_tap} отсутствует ЕЩЁ ДО destroy — проверять нечего", "orphan")
ok(f"14: {orphan_tap} висит после kill -9 — есть чему утечь")

r = request({"op": "destroy", "space": "orphan"})
if r is None or r.get("ok") is not True:
    fail(f"14: destroy orphan после kill -9 должен вернуть ok:true, получено {r!r}", "orphan")
if os.path.exists(f"/sys/class/net/{orphan_tap}"):
    fail(f"14: {orphan_tap} всё ещё существует после destroy — tap спейса, забытый после kill -9, утёк")
os.remove(os.path.join(tmp_dir, "tap-ours"))
ok(f"14: destroy без предшествующего stop сам снял {orphan_tap} — CID не утёк")

print("PASS: V-LIFECYCLE")
PYEOF

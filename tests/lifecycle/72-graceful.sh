#!/usr/bin/env bash
# VERIFY: V-GRACEFUL — том отмонтирован до убийства QEMU; положительный контроль умеет увидеть грязный том
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root (sudo bash tests/lifecycle/72-graceful.sh) — реальный QEMU, tap и /dev/kvm" >&2
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
# data_mb = 0 не покажет разницы вовсе: без диска /data гостю нечего ни монтировать, ни пачкать
grep -Eq '^data_mb *= *64' profiles/spike/manifest.toml || {
  echo "FAIL: profiles/spike/manifest.toml должен заказывать data_mb = 64 (задача 8)" >&2
  exit 1
}
# шаблон, собранный до задачи 8, молча даёт пустой блок состояния ФС — провал выглядел бы поломкой теста
manifest_sha="$(sha256sum profiles/spike/manifest.toml | cut -d' ' -f1)"
built_sha="$(sed -n 's/^manifest-sha256: //p' "$template/MANIFEST")"
[ "$manifest_sha" = "$built_sha" ] || {
  echo "FAIL: шаблон $template собран с другого манифеста ($built_sha != $manifest_sha) —" >&2
  echo "  пересобери: bash tools/build-profile.sh profiles/spike" >&2
  exit 1
}

# объявлены пустыми ДО trap: под set -u ссылка на них в cleanup() не должна упасть, если провал случится раньше
tmp=""
run=""
daemon_pid=""
created_bridge=0

cleanup() {
  set +e
  for id in dirty clean silent; do
    [ -n "$run" ] && [ -f "$run/$id/qemu.pid" ] && kill -KILL "$(cat "$run/$id/qemu.pid")" 2>/dev/null
  done
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  # список пишут сами сценарии (в т.ч. tap, утёкший после kill -9 без stop) — снести можно только свои
  if [ -n "$tmp" ] && [ -f "$tmp/taps-ours" ]; then
    while IFS= read -r tap; do
      [ -n "$tap" ] && ip link del "$tap" 2>/dev/null
    done < "$tmp/taps-ours"
  fi
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
: > "$tmp/taps-ours"
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

# br-spaces — инфраструктура фикстуры (решение C), не предмет проверки
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

echo "== сценарии 1 и 2: настоящий QEMU, настоящий /data, настоящее состояние ФС из суперблока ext4 =="
python3 - "$sock" "$run" "$state" "$tmp" <<'PYEOF'
import json
import os
import signal
import socket
import subprocess
import sys
import time

sock_path, run_dir, state_dir, tmp_dir = sys.argv[1:5]


# без этого причина падения остаётся в логах, которые уборка теста сносит, и приходится гадать
def dump(label, path, lines=20):
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
        dump("последний запуск " + space, os.path.join(state_dir, "spaces", space, "last-run.log"))
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
# по-прежнему отвечает успехом — смерть QEMU проверяем по состоянию в /proc, а не по сигналу
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


def create(space):
    r = request(
        {"op": "create", "space": space, "profile": "spike", "label": "untrusted", "color": "#e03131"}
    )
    if r is None or r.get("ok") is not True:
        fail(f"create {space}: {r!r}")
    return r["data"]


def describe(space):
    r = request({"op": "describe", "space": space})
    if r is None or r.get("ok") is not True:
        fail(f"describe {space}: {r!r}", space)
    return r["data"]


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


def data_marker_block(text):
    return block_between(text, "---MIYORI-DATA-BEGIN---", "---MIYORI-DATA-END---")


# критерий — фича needs_recovery, а НЕ поле Filesystem state: у журналируемой ext4 состояние всегда
# "clean", и на нём тест не отличал kill -9 от штатного останова (замерено на живом госте 2026-08-27).
# Читается ДО mount (см. miyori-init): монтирование проигрывает журнал и след прошлого останова стирает
def fs_state(text):
    block = block_between(text, "---MIYORI-DATA-STATE-BEGIN---", "---MIYORI-DATA-STATE-END---")
    for line in block.splitlines():
        line = line.strip()
        if line and "MIYORI-DATA-STATE" not in line:
            return line
    return ""


# start не торопит и не подделывает загрузку: ждём реального агента реального гостя (тот же приём, что 73)
def wait_running(space, deadline_s=90):
    state = None
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        state = describe(space)["state"]
        if state == "running":
            return state
        time.sleep(2)
    fail(f"{space} не дошёл до running за {deadline_s} с (state={state!r})", space)


def qemu_pid_of(space):
    with open(os.path.join(run_dir, space, "qemu.pid")) as f:
        return int(f.read().strip())


def read_nonce(space):
    with open(os.path.join(run_dir, space, "nonce")) as f:
        return f.read().strip()


def record_tap(cid):
    with open(os.path.join(tmp_dir, "taps-ours"), "a") as f:
        f.write(f"tap-space-{cid}\n")


# =========================================================================
# сценарий 1 — положительный контроль: без него пункт 2 ничего не доказывает
# =========================================================================
dirty = create("dirty")
cid1 = dirty["cid"]
record_tap(cid1)

r = request({"op": "start", "space": "dirty"})
if r is None or r.get("ok") is not True:
    fail(f"start dirty: {r!r}")
wait_running("dirty")
nonce1 = read_nonce("dirty")

log1 = last_run_log("dirty")
if nonce1 not in data_marker_block(log1):
    fail(
        "1: маркер с nonce не появился в консоли ПЕРВОГО (нормального) старта — "
        "механизм маркера сломан ещё до положительного контроля, разбираться тут, а не в kill -9",
        "dirty",
    )
ok("1: гость примонтировал /data и записал маркер со своим nonce")

pid1 = qemu_pid_of("dirty")
os.kill(pid1, signal.SIGKILL)
deadline = time.time() + 10
while not stopped_running(pid1) and time.time() < deadline:
    time.sleep(0.2)
if not stopped_running(pid1):
    fail("1: QEMU всё ещё выполняется через 10 с после kill -9", "dirty")
ok(f"1: QEMU спейса dirty (pid={pid1}) убит kill -9 БЕЗ штатного stop")

# qemu-img check НЕ различает эти два случая: проверено эмпирически (qemu-io write + kill -9, с
# lazy_refcounts=on и без) — оба раза "No errors were found on the image.". Печатаем справочно,
# положительный контроль строится на состоянии ФС внутри тома, а не на этом коде возврата.
data_path = os.path.join(state_dir, "spaces", "dirty", "data.qcow2")
check = subprocess.run(["qemu-img", "check", data_path], capture_output=True, text=True)
print("--- qemu-img check (dirty, после kill -9; справочно, не критерий) ---")
print(f"    код возврата: {check.returncode}")
for line in (check.stdout + check.stderr).splitlines():
    print(f"    {line}")

# tap утёк, потому что stop никогда не звался (это и есть суть сценария 1) — снимаем сами, это не предмет проверки
subprocess.run(["ip", "link", "del", f"tap-space-{cid1}"], capture_output=True)

r = request({"op": "start", "space": "dirty"})
if r is None or r.get("ok") is not True:
    fail(f"1: повторный start dirty после kill -9: {r!r}")
wait_running("dirty")
state_after_kill = fs_state(last_run_log("dirty"))
if not state_after_kill:
    fail("1: блок MIYORI-DATA-STATE пуст на повторном старте — в образе нет dumpe2fs (e2fsprogs) либо гостю не достался /dev/vdb", "dirty")
if state_after_kill != "needs-recovery":
    fail(
        f"1: ожидался незакрытый журнал (needs-recovery) после kill -9 без stop, получено {state_after_kill!r}",
        "dirty",
    )
ok(f"1: повторный старт после kill -9 показывает {state_after_kill!r} — журнал остался незакрытым")

r = request({"op": "stop", "space": "dirty"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: stop dirty: {r!r}")
r = request({"op": "destroy", "space": "dirty"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: destroy dirty: {r!r}")

# =========================================================================
# сценарий 2 — штатный путь: тот же приём, тот же профиль, но stop вместо kill -9
# =========================================================================
clean = create("clean")
cid2 = clean["cid"]
record_tap(cid2)

r = request({"op": "start", "space": "clean"})
if r is None or r.get("ok") is not True:
    fail(f"start clean: {r!r}")
wait_running("clean")
nonce_c1 = read_nonce("clean")

log2 = last_run_log("clean")
if nonce_c1 not in data_marker_block(log2):
    fail("2: маркер первого старта clean не появился в консоли", "clean")
ok("2: clean тоже примонтировал /data и записал маркер со своим nonce")

pid2 = qemu_pid_of("clean")
r = request({"op": "stop", "space": "clean"})
if r is None or r.get("ok") is not True:
    fail(f"2: stop clean должен вернуть ok:true, получено {r!r}", "clean")
if r["data"].get("state") != "stopped":
    fail(f"2: ожидалось state=stopped, получено {r['data']!r}", "clean")
if r["data"].get("clean-shutdown") is not True:
    fail(f"2: ожидался clean-shutdown=true, получено {r['data']!r}", "clean")
if "том отмонтирован" not in (r["data"].get("detail") or ""):
    fail(f"2: агент должен ответить «том отмонтирован», получено {r['data']!r}", "clean")
ok("2: агент ответил «том отмонтирован», demon сообщил clean-shutdown=true")

if alive(pid2):
    fail(f"2: процесс {pid2} всё ещё жив после stop", "clean")
if os.path.exists(os.path.join(run_dir, "clean", "qemu.pid")):
    fail("2: pid-файл clean не убран после stop", "clean")
if os.path.exists(f"/sys/class/net/tap-space-{cid2}"):
    fail(f"2: tap-space-{cid2} всё ещё существует после stop", "clean")
ok("2: stop убил процесс, снял pid-файл и tap")

described = describe("clean")
if described.get("clean-shutdown") is not True:
    fail(f"2: describe после stop должен показывать clean-shutdown=true, получено {described!r}", "clean")

check2 = subprocess.run(["qemu-img", "check", os.path.join(state_dir, "spaces", "clean", "data.qcow2")],
                         capture_output=True, text=True)
print("--- qemu-img check (clean, после штатного stop) ---")
print(f"    код возврата: {check2.returncode}")
for line in (check2.stdout + check2.stderr).splitlines():
    print(f"    {line}")
if check2.returncode != 0:
    fail("2: qemu-img check нашёл проблему на томе после штатного stop — он обязан быть чист", "clean")
ok("2: qemu-img check чист после штатного stop")

r = request({"op": "start", "space": "clean"})
if r is None or r.get("ok") is not True:
    fail(f"2: повторный start clean: {r!r}")
wait_running("clean")
nonce_c2 = read_nonce("clean")
log3 = last_run_log("clean")
block3 = data_marker_block(log3)
if nonce_c1 not in block3:
    fail(
        "2: маркер ПЕРВОГО старта clean потерян после штатного stop и повторного start — "
        "штатный останов обязан был его сохранить",
        "clean",
    )
if nonce_c2 not in block3:
    fail("2: маркер второго старта clean не появился вовсе", "clean")
ok("2: повторный start показывает маркер первого старта — данные пережили штатный stop")

state_after_stop = fs_state(log3)
if not state_after_stop:
    fail("2: блок MIYORI-DATA-STATE пуст на повторном старте — в образе нет dumpe2fs (e2fsprogs) либо гостю не достался /dev/vdb", "clean")
if state_after_stop != "recovered":
    fail(f"2: ожидался закрытый журнал (recovered) после штатного stop, получено {state_after_stop!r}", "clean")
ok(f"2: повторный старт после штатного stop показывает {state_after_stop!r} — журнал закрыт агентом")

r = request({"op": "stop", "space": "clean"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: stop clean в конце сценария 2 не удался: {r!r}")
r = request({"op": "destroy", "space": "clean"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: destroy clean не удался: {r!r}")

# =========================================================================
# правило задачи 8: если положительный контроль не отличим от штатного пути — это провал, а не мелочь
# =========================================================================
if state_after_kill == state_after_stop:
    fail(
        f"ПОЛОЖИТЕЛЬНЫЙ КОНТРОЛЬ НЕ ПОКАЗАЛ РАЗНИЦЫ: журнал после kill -9 ({state_after_kill!r}) "
        f"совпадает с состоянием после штатного stop ({state_after_stop!r})"
    )
ok(f"положительный контроль различил случаи: kill -9 -> {state_after_kill!r}, stop -> {state_after_stop!r}")
PYEOF

echo "== сценарий 3: настоящий гость убит без stop, но демон узнаёт об этом только в момент stop =="
python3 - "$sock" "$run" "$state" "$tmp" <<'PYEOF'
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time

sock_path, run_dir, state_dir, tmp_dir = sys.argv[1:5]


def dump(label, path, lines=20):
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
        dump("последний запуск " + space, os.path.join(state_dir, "spaces", space, "last-run.log"))
    sys.exit(1)


def ok(msg):
    print(f"ok: {msg}")


# после kill -9 процесс висит зомби, пока родитель-демон не сделает wait, и kill(pid,0) на нём
# по-прежнему отвечает успехом — смерть QEMU проверяем по состоянию в /proc, а не по сигналу
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


def describe(space):
    r = request({"op": "describe", "space": space})
    if r is None or r.get("ok") is not True:
        fail(f"describe {space}: {r!r}", space)
    return r["data"]


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


# критерий — needs_recovery, не Filesystem state (см. пояснение в первом сценарии)
def fs_state(text):
    block = block_between(text, "---MIYORI-DATA-STATE-BEGIN---", "---MIYORI-DATA-STATE-END---")
    for line in block.splitlines():
        line = line.strip()
        if line and "MIYORI-DATA-STATE" not in line:
            return line
    return ""


def wait_running(space, deadline_s=90):
    state = None
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        state = describe(space)["state"]
        if state == "running":
            return state
        time.sleep(2)
    fail(f"{space} не дошёл до running за {deadline_s} с (state={state!r})", space)


def qemu_pid_of(space):
    with open(os.path.join(run_dir, space, "qemu.pid")) as f:
        return int(f.read().strip())


def record_tap(cid):
    with open(os.path.join(tmp_dir, "taps-ours"), "a") as f:
        f.write(f"tap-space-{cid}\n")


r = request(
    {"op": "create", "space": "silent", "profile": "spike", "label": "untrusted", "color": "#2f9e44"}
)
if r is None or r.get("ok") is not True:
    fail(f"create silent: {r!r}")
cid = r["data"]["cid"]
record_tap(cid)

r = request({"op": "start", "space": "silent"})
if r is None or r.get("ok") is not True:
    fail(f"3: start silent: {r!r}")
wait_running("silent")
ok("3: silent реально загрузился, настоящий агент реально ответил")

# настоящий QEMU убиваем напрямую, минуя daemon.stop — с точки зрения хоста это неотличимо от того,
# что агент внутри перестал отвечать: демон узнаёт об этом только когда САМ попробует дозвониться
pid = qemu_pid_of("silent")
os.kill(pid, signal.SIGKILL)
deadline = time.time() + 10
while not stopped_running(pid) and time.time() < deadline:
    time.sleep(0.2)
if not stopped_running(pid):
    fail("3: настоящий QEMU спейса silent всё ещё выполняется через 10 с после kill -9", "silent")
ok(f"3: настоящий QEMU silent (pid={pid}) мёртв ДО того, как демон вообще позвал stop")

# подменяем qemu.pid на живой процесс-двойник (тот же приём, что и "ghost" в 73-agent-nonce.sh):
# без этого qemu::state_of увидел бы Stopped раньше, чем stop успеет попробовать дозвониться до агента,
# и проверялся бы не путь "агент недостижим", а путь "и так уже остановлен" — это разные коды ошибок
fake_bin = os.path.join(run_dir, f"qemu-system-x86_64-guest-cid={cid}")
sleep_bin = shutil.which("sleep")
if os.path.lexists(fake_bin):
    os.remove(fake_bin)
os.symlink(sleep_bin, fake_bin)
fake_proc = subprocess.Popen([fake_bin, "300"])
with open(os.path.join(run_dir, "silent", "qemu.pid"), "w") as f:
    f.write(str(fake_proc.pid))
# nonce не трогаем: он настоящий, от мёртвого гостя — agent::shutdown провалится на связи, а не на сравнении nonce

r = request({"op": "stop", "space": "silent"})
if r is None or r.get("ok") is not True:
    fail(f"3: stop silent должен завершиться ok:true даже когда гость уже мёртв, получено {r!r}", "silent")
if r["data"].get("state") != "stopped":
    fail(f"3: ожидалось state=stopped, получено {r['data']!r}", "silent")
if r["data"].get("clean-shutdown") is not False:
    fail(f"3: недостижимый агент обязан дать clean-shutdown=false, получено {r['data']!r}", "silent")
ok("3: stop без ответившего агента всё равно завершился, но честно как аварийный")

described = describe("silent")
if described.get("state") != "stopped" or described.get("clean-shutdown") is not False:
    fail(f"3: describe после аварийного stop должен честно показывать clean-shutdown=false, получено {described!r}", "silent")
ok("3: describe после аварийного stop честно называет останов небезопасным, не молчит о нём")

try:
    fake_proc.kill()
    fake_proc.wait(timeout=5)
except Exception:
    pass

# третья, настоящая загрузка: подтверждаем, что "аварийно" в describe соответствует реально грязному тому
r = request({"op": "start", "space": "silent"})
if r is None or r.get("ok") is not True:
    fail(f"3: повторный start silent после аварийного stop: {r!r}")
wait_running("silent")
state = fs_state(last_run_log("silent"))
if not state:
    fail("3: блок MIYORI-DATA-STATE пуст на восстановительном старте silent — в образе нет dumpe2fs (e2fsprogs) либо гостю не достался /dev/vdb", "silent")
if state != "needs-recovery":
    fail(
        f"3: ожидался незакрытый журнал (needs-recovery) после аварийного убийства, получено {state!r}",
        "silent",
    )
ok(f"3: том silent после аварийного останова показывает {state!r} — совпадает с положительным контролем")

r = request({"op": "stop", "space": "silent"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: stop silent: {r!r}")
r = request({"op": "destroy", "space": "silent"})
if r is None or r.get("ok") is not True:
    fail(f"уборка: destroy silent: {r!r}")
PYEOF

echo
echo "НЕ ПРОВЕРЕНО: форсированная перезагрузка хоста после stop (вторая половина V-GRACEFUL, спека §6.1) —"
echo "  батарея жизненного цикла не может её выполнить; нужен отдельный ручной прогон оператора на живой машине"
echo "PASS: V-GRACEFUL"

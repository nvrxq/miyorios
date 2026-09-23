#!/usr/bin/env bash
# Задача 10 плана 2: build кладёт шаблон под root без права записи; профиля нет -> profile-not-found.
# Поправка оператора 2026-08-27 к плану: "build дважды -> тот же digest" недостижимо (rootfs.tar несёт
# временные метки, спека §5.4) — второй прогон обязан дать НОВЫЙ digest,
# а не совпадающий; проверяется сохранность первого каталога и переезд latest, а не равенство digest.
set -euo pipefail
cd "$(dirname "$0")/../.."

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root (sudo bash tests/lifecycle/74-build.sh) — демон переносит и chown'ит шаблон" >&2
  exit 1
}

for tool in setpriv mmdebstrap qemu-img python3 cargo; do
  command -v "$tool" >/dev/null || { echo "FAIL: нет $tool в PATH" >&2; exit 1; }
done

# в отличие от остальных тестов батареи этот тянет пакеты apt внутри mmdebstrap; без сети сборка
# упадёт минуту спустя невнятной ошибкой apt из середины лога — проверяем и называем причину сразу
if ! python3 -c "import socket; socket.create_connection(('archive.ubuntu.com', 80), 5)" 2>/dev/null; then
  echo "FAIL: нет сети до archive.ubuntu.com:80 — build тянет пакеты apt внутри mmdebstrap" >&2
  exit 1
fi

[ -f components/waypipe/bin/waypipe.guest ] || {
  echo "FAIL: нет components/waypipe/bin/waypipe.guest — собери: bash components/waypipe/build.sh" >&2
  exit 1
}

bin=target/release/miyorid
[ -x "$bin" ] || cargo build --release --offline -p miyorid
[ -x target/release/miyori-agent ] || cargo build --release --offline -p miyori-agent

# объявлены пустыми ДО trap: под set -u ссылка на них в cleanup() не должна упасть, если провал случится раньше
tmp=""
daemon_pid=""

cleanup() {
  set +e
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
  # шаблон лежит 0444/0555 root:root — без u+w rm -rf упрётся в права под set -e
  [ -n "$tmp" ] && chmod -R u+w "$tmp" 2>/dev/null
  [ -n "$tmp" ] && rm -rf "$tmp"
}
trap cleanup EXIT

tmp="$(mktemp -d)"
# mktemp отдаёт 0700 root; в бою /var/lib/miyorios обходится всеми, тест обязан воспроизводить именно это
chmod 0755 "$tmp"
state="$tmp/state"
run="$tmp/run"
mkdir -p "$state/profiles" "$state/templates" "$state/spaces" "$run"
# профиль переиспользуем как есть — тест не собирает свой, он собирает spike по-настоящему
ln -s "$(realpath profiles/spike)" "$state/profiles/spike"

# build-uid по умолчанию — владелец <state>/profiles (задача 10): под sudo это root, значит владельца
# нужно вернуть тому, кто реально ходит под sudo, — только у него есть subuid-диапазон для mmdebstrap (ADR-5)
build_uid="${SUDO_UID:?нужен запуск через sudo от обычного пользователя, а не из-под root напрямую}"
chown "$build_uid" "$state/profiles"

"$bin" --state-dir "$state" --run-dir "$run" --group "" --registry "$tmp/registry.toml" \
  >"$tmp/daemon.log" 2>&1 &
daemon_pid=$!

sock="$run/control.sock"
for _ in $(seq 50); do
  [ -S "$sock" ] && break
  sleep 0.1
done
[ -S "$sock" ] || {
  echo "FAIL: демон не поднял $sock"
  cat "$tmp/daemon.log"
  exit 1
}

python3 - "$sock" "$state" "$tmp" <<'PYEOF'
import json
import os
import socket
import stat
import sys
import time

sock_path, state_dir, tmp_dir = sys.argv[1:4]


# без этого причина падения остаётся в логах, которые уборка теста сносит, и приходится гадать
def dump(label, path, lines=25):
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
    sys.exit(1)


def ok(msg):
    print(f"ok: {msg}")


# build стримит progress по ходу (задача 2) — терминальный кадр несёт "ok", читаем до него, не после
def request_frames(obj, timeout=300):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps(obj) + "\n").encode())
    f = s.makefile("r", encoding="utf-8", newline="\n")
    progress = []
    terminal = None
    for line in f:
        line = line.strip()
        if not line:
            continue
        parsed = json.loads(line)
        if "progress" in parsed:
            progress.append(parsed["progress"])
            continue
        terminal = parsed
        break
    s.close()
    return progress, terminal


# --- профиля нет: код profile-not-found, ни одной попытки собрать ---
progress, terminal = request_frames({"op": "build", "profile": "ghost"})
if terminal is None or terminal.get("ok") is not False:
    fail(f"build ghost должен вернуть ok:false, получено {terminal!r}")
if terminal.get("code") != "profile-not-found":
    fail(f"build ghost: ожидался code=profile-not-found, получено {terminal.get('code')!r}")
if progress:
    fail(f"build ghost прислал progress, хотя собирать было нечего: {progress!r}")
ok("build несуществующего профиля -> profile-not-found, без progress-кадров")

# --- первая сборка: настоящий mmdebstrap, минуту-другую, progress по ходу (задача 2) ---
print("== build spike (первый раз) — тянет пакеты apt, минуту-другую ==")
t0 = time.time()
progress, terminal = request_frames({"op": "build", "profile": "spike"})
if terminal is None or terminal.get("ok") is not True:
    fail(f"build spike: {terminal!r}")
if not progress:
    fail("build spike не прислал ни одного progress-кадра — задача 2 требует прогресса по ходу сборки")
ok(f"1: {len(progress)} progress-кадров, терминальный один, сборка заняла {time.time() - t0:.0f} с")

digest1 = terminal["data"]["digest"]
path1 = terminal["data"]["path"]
expected1 = os.path.join(state_dir, "templates", "spike", digest1)
if os.path.realpath(path1) != os.path.realpath(expected1):
    fail(f"путь в ответе {path1!r} не совпадает с ожидаемым {expected1!r}")
if not os.path.isdir(path1):
    fail(f"каталога шаблона {path1} нет")
ok(f"2: <state>/templates/spike/{digest1}/ на месте")

for name in ("root.qcow2", "vmlinuz", "initrd.img", "MANIFEST"):
    p = os.path.join(path1, name)
    st = os.lstat(p)
    if st.st_uid != 0:
        fail(f"{p}: владелец uid={st.st_uid}, ожидался root")
    if stat.S_IMODE(st.st_mode) != 0o444:
        fail(f"{p}: режим {oct(stat.S_IMODE(st.st_mode))}, ожидался 0o444")
dir_st = os.lstat(path1)
if dir_st.st_uid != 0:
    fail(f"{path1}: владелец каталога uid={dir_st.st_uid}, ожидался root")
if stat.S_IMODE(dir_st.st_mode) != 0o555:
    fail(f"{path1}: режим каталога {oct(stat.S_IMODE(dir_st.st_mode))}, ожидался 0o555")
ok("3: шаблон root:root, файлы 0444, каталог 0555 — писать после сборки некому")

with open(os.path.join(path1, "MANIFEST"), errors="replace") as f:
    manifest_text = f.read()
manifest_digest = next(
    (ln[len("digest: "):].strip() for ln in manifest_text.splitlines() if ln.startswith("digest: ")),
    None,
)
if manifest_digest != digest1:
    fail(f"MANIFEST называет digest {manifest_digest!r}, каталог называется {digest1!r}")
ok("4: digest в имени каталога совпадает с записью в MANIFEST")

latest_link = os.path.join(state_dir, "templates", "spike", "latest")
if os.readlink(latest_link) != digest1:
    fail(f"latest указывает на {os.readlink(latest_link)!r}, ожидался {digest1!r}")
ok("5: latest указывает на только что собранный digest")

# --- вторая сборка: воспроизводимости нет (§5.4) — проверяем НЕ совпадение digest, а то, что правда обещано ---
print("== build spike (второй раз) — проверяем сохранность первого каталога, а не равенство digest ==")
before = {}
for root, _dirs, files in os.walk(path1):
    for name in files:
        p = os.path.join(root, name)
        st = os.lstat(p)
        before[p] = (st.st_uid, stat.S_IMODE(st.st_mode), st.st_size)

t0 = time.time()
_progress2, terminal2 = request_frames({"op": "build", "profile": "spike"})
if terminal2 is None or terminal2.get("ok") is not True:
    fail(f"build spike (второй раз): {terminal2!r}")
digest2 = terminal2["data"]["digest"]
if digest2 == digest1:
    fail(
        "второй build дал тот же digest, что и первый — либо сборка внезапно стала "
        "воспроизводимой (тогда чинить нужно этот тест, а не молчать), "
        "либо демон подсунул старый каталог вместо новой сборки"
    )
ok(f"6: второй build дал новый digest {digest2} (заняло {time.time() - t0:.0f} с)")

if not os.path.isdir(path1):
    fail(f"каталог первой сборки {path1} исчез после второй сборки")
after = {}
for root, _dirs, files in os.walk(path1):
    for name in files:
        p = os.path.join(root, name)
        st = os.lstat(p)
        after[p] = (st.st_uid, stat.S_IMODE(st.st_mode), st.st_size)
if before != after:
    fail(f"каталог первой сборки изменился: было {before!r}, стало {after!r}")
ok("7: каталог первой сборки не тронут второй сборкой — файлы, права и владелец те же")

if os.readlink(latest_link) != digest2:
    fail(f"latest после второй сборки указывает на {os.readlink(latest_link)!r}, ожидался {digest2!r}")
ok("8: latest переехал на новый digest")

print(f"итог: {digest1} цел на диске, {digest2} новый и на latest")
PYEOF

echo "PASS: build"

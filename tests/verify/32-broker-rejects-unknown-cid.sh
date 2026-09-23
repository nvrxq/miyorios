#!/usr/bin/env bash
# VERIFY: брокер отклоняет соединение с CID, которого нет в реестре
set -euo pipefail
cd "$(dirname "$0")/../.."
port="${1:-1701}"
bin=target/release/miyori-guid

[ -x "$bin" ] || cargo build --release --offline

reg="$(mktemp)"
log="$(mktemp)"
guid=""
# kill "" безвреден и подавлен: без || true set -e обрывает trap и оставляет мусор
trap 'kill "$guid" 2>/dev/null || true; rm -f "$reg" "$log"' EXIT
printf '[[space]]\nid = "verify"\ncid = 7\nlabel = "untrusted"\ncolor = "#000000"\n' > "$reg"

"$bin" --registry "$reg" --port "$port" >"$log" 2>&1 &
guid=$!

# положительный контроль: пока брокер не сказал, что слушает, отказ ничего не значит
for _ in $(seq 50); do
  if grep -q "слушает vsock:$port" "$log"; then break; fi
  sleep 0.1
done
grep -q "слушает vsock:$port" "$log" || {
  echo "FAIL: брокер не поднялся"; cat "$log"; exit 1; }

python3 - "$port" <<'PYEOF'
import socket
import sys

s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect((1, int(sys.argv[1])))  # 1 = VMADDR_CID_LOCAL
except OSError as exc:
    sys.exit(f"FAIL: петля не дошла до брокера ({exc}); загружен ли vsock_loopback?")
data = s.recv(16)
s.close()
if data:
    sys.exit(f"FAIL: брокер отдал петле данные {data!r}")
PYEOF

for _ in $(seq 50); do
  if grep -q "незарегистрированного CID 1" "$log"; then break; fi
  sleep 0.1
done
grep -q "незарегистрированного CID 1" "$log" || {
  echo "FAIL: в логе нет отказа для CID 1:"; cat "$log"; exit 1; }
echo "PASS: локальный процесс не получает label спейса через петлю"

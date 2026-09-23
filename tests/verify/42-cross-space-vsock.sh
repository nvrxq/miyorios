#!/usr/bin/env bash
# VERIFY: гость видит один порт хоста и не достаёт до соседнего спейса
set -euo pipefail
log="${1:-build/guest/console-3.txt}"
[ -f "$log" ] || { echo "FAIL: нет $log — проба не снималась"; exit 1; }

# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
sec="$(sed -n '/---MIYORI-VSOCK-BEGIN---/,/---MIYORI-VSOCK-END---/p' "$log" \
  | sed '1d;$d' | tr -d '\r' | grep -v '^\[' || true)"
[ -n "$sec" ] || { echo "FAIL: секция VSOCK пуста"; exit 1; }

# положительный контроль: если не просканирован ни один порт, дальше проверять нечего
scanned="$(printf '%s\n' "$sec" | grep -c '^host ' || true)"
[ "$scanned" -ge 5 ] || { echo "FAIL: просканировано портов: $scanned"; exit 1; }

open="$(printf '%s\n' "$sec" | grep '^host .* open$' | awk '{print $2}' | paste -sd,)"
[ "$open" = "1700" ] || {
  echo "FAIL: гостю открыты порты хоста: ${open:-нет ни одного}"; exit 1; }

peers="$(printf '%s\n' "$sec" | grep -c '^peer ' || true)"
[ "$peers" -ge 1 ] || { echo "FAIL: соседние CID не проверялись"; exit 1; }
# here-string, а не труба: SIGPIPE под pipefail выдал бы открытый порт соседа за закрытый
if grep -q '^peer .* open$' <<<"$sec"; then
  echo "FAIL: достижим соседний спейс:"; printf '%s\n' "$sec" | grep '^peer '; exit 1
fi
echo "PASS: открыт только порт брокера, соседние спейсы недостижимы"

#!/usr/bin/env bash
# VERIFY: флуд из гостя не валит host-сессию; парсер вправе оборвать поток раньше срока — это не дефект
set -euo pipefail
log="${1:-build/guest/console-3.txt}"
unit="${2:-miyori-gui-spike.scope}"

niri msg version >/dev/null 2>&1 || { echo "FAIL: niri мёртв ещё до проверки"; exit 1; }

[ -f "$log" ] || { echo "FAIL: нет $log — флуд не запускался"; exit 1; }

# маркеры пишутся в момент обрыва потока, а не в начале: батарея сразу после подъёма стенда читала бы
# консоль раньше времени и объявляла отсутствие флуда — ждём появления блока, а не судим по первому взгляду
deadline=$(( $(date +%s) + 90 ))
while ! grep -q '^---MIYORI-FLOOD-END---$' < <(tr -d '\r' < "$log"); do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "примечание: блок MIYORI-FLOOD-END не появился в $log за 90 с — судим по тому, что есть"
    break
  fi
  sleep 2
done

# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
clean="$(tr -d '\r' < "$log")"

sent="$(printf '%s\n' "$clean" | sed -n 's/^flood-sent //p' | tail -1)"
stopped="no"
grep -q '^flood-stopped ' <<<"$clean" && stopped="yes"

# факт флуда — либо поток оборвали (flood-stopped), либо что-то реально ушло (flood-sent > 0)
if [ -z "$sent" ] && [ "$stopped" = "no" ]; then
  echo "FAIL: за 90 с в $log не появилось ни flood-sent, ни flood-stopped — флуд не запускался"; exit 1
fi
: "${sent:=0}"
if [ "$sent" -eq 0 ] && [ "$stopped" = "no" ]; then
  echo "FAIL: флуда не было — отправлено 0 байт, обрыва потока тоже не было"; exit 1
fi

limit="$(systemctl --user show "$unit" -p MemoryMax --value 2>/dev/null || true)"
case "$limit" in
  ''|infinity|'[not set]') echo "FAIL: у $unit не выставлен MemoryMax"; exit 1 ;;
esac

if systemctl --user is-active --quiet "$unit"; then unit_state="alive"; else unit_state="DEAD"; fi
if pgrep -x miyori-guid >/dev/null 2>&1; then broker_state="alive"; else broker_state="DEAD"; fi
[ "$unit_state" = "alive" ] || { echo "FAIL: $unit не пережил флуд (unit=$unit_state)"; exit 1; }
[ "$broker_state" = "alive" ] || { echo "FAIL: брокер не пережил флуд (broker=$broker_state)"; exit 1; }

niri msg version >/dev/null 2>&1 || { echo "FAIL: niri не отвечает после флуда"; exit 1; }

echo "PASS: flood-sent=$sent flood-stopped=$stopped unit=$unit_state broker=$broker_state MemoryMax=$limit niri жив"

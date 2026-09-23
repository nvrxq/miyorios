#!/usr/bin/env bash
# VERIFY: V-LABEL — полоса называет спейс окна в фокусе и меняется при переключении
#
# Тест НЕ трогает пиксели: он читает то же решение, по которому полоса рисуется,
# из `miyori-label --dry-run`. Что нарисовано на экране, проверяет глаз оператора;
# что решено — проверяет этот тест, и подменить решение он не может.
#
# Что нужно до запуска (всё — в сессии niri, от обычного пользователя):
#   1. поднят демон и в нём есть запущенный спейс с окном:
#        sudo bash tests/lifecycle/71-lifecycle.sh   поднимает и гасит своё, не годится;
#      здесь нужен ЖИВОЙ стенд — см. документации проекта, фаза 3;
#   2. запущен miyori-guid, иначе окна не будет вовсе;
#   3. в niri есть хотя бы одно окно хоста (терминал, из которого это запускается, годится).
set -euo pipefail
cd "$(dirname "$0")/../.."

socket="${MIYORI_SOCKET:-/run/miyorios/control.sock}"
space="${MIYORI_SPACE:-spike}"
label="target/release/miyori-label"

fail() { echo "FAIL: $*"; exit 1; }

[ -n "${NIRI_SOCKET:-}" ] || fail "нет NIRI_SOCKET — тест запускается изнутри сессии niri"
command -v niri >/dev/null || fail "нет niri в PATH"
# дешёвые отказы раньше сборки: собирать крейт ради того, чтобы упасть на отсутствующем сокете, незачем
[ -S "$socket" ] || fail "нет $socket — демон не запущен, полосе неоткуда взять уровень спейса"
[ -x "$label" ] || cargo build --release --offline -p miyori-label

windows="$(niri msg --json windows)"

# окно спейса ищем по cgroup процесса, а не по заголовку: заголовок — то место, куда пишет гость
space_id=""
host_id=""
while read -r id pid; do
  [ -n "$pid" ] && [ "$pid" != "null" ] || continue
  cg="$(cat "/proc/$pid/cgroup" 2>/dev/null || true)"
  case "$cg" in
    *"miyori-gui-$space.scope"*) [ -n "$space_id" ] || space_id="$id" ;;
    *) [ -n "$host_id" ] || host_id="$id" ;;
  esac
done < <(printf '%s' "$windows" | python3 -c '
import json, sys
for w in json.load(sys.stdin):
    print(w["id"], w.get("pid"))
')

[ -n "$space_id" ] || fail "в niri нет ни одного окна спейса $space — проверять нечего, подними стенд"
[ -n "$host_id" ] || fail "в niri нет ни одного окна хоста — переключать не на что"

out="$(mktemp)"
restore=""
cleanup() {
  set +e
  # убиваем группу целиком: kill $! снял бы только последнюю команду конвейера, а niri msg остался бы висеть
  [ -n "${group:-}" ] && kill -- -"$group" 2>/dev/null
  [ -n "$restore" ] && niri msg action focus-window --id "$restore" >/dev/null 2>&1
  rm -f "$out"
}
trap cleanup EXIT

# фокус возвращаем туда, где он был: тест не имеет права оставить рабочий стол переставленным
restore="$(printf '%s' "$windows" | python3 -c '
import json, sys
for w in json.load(sys.stdin):
    if w.get("is_focused"):
        print(w["id"]); break
')"

setsid bash -c 'niri msg --json event-stream | "$0" --dry-run --socket "$1"' \
  "$label" "$socket" > "$out" 2>&1 &
group=$!

# полоса обязана успеть подписаться на события niri: пропущенное переключение фокуса
# не повторится, и ожидание после него ничего не даёт
for _ in $(seq 1 40); do [ -s "$out" ] && break; sleep 0.25; done
[ -s "$out" ] || fail "полоса не выдала ни строки за 10 с — подписка на события niri не поднялась"

wait_for() {
  local want="$1" tries=0
  while [ "$tries" -lt 60 ]; do
    grep -q -- "$want" "$out" && return 0
    tries=$((tries + 1))
    sleep 0.5
  done
  return 1
}

niri msg action focus-window --id "$host_id" >/dev/null
wait_for "STRIP host" || { echo "--- вывод полосы ---"; cat "$out"; fail "фокус на окне хоста, а полоса не сказала host"; }

niri msg action focus-window --id "$space_id" >/dev/null
wait_for "STRIP space $space " || { echo "--- вывод полосы ---"; cat "$out"; fail "фокус на окне спейса $space, а полоса его не назвала"; }

# без этого шага тест зеленел бы и на полосе, которая просто печатает обе строки подряд
niri msg action focus-window --id "$host_id" >/dev/null
last=""
for _ in $(seq 1 60); do
  last="$(tail -1 "$out")"
  case "$last" in "STRIP host") break ;; esac
  sleep 0.5
done
[ "$last" = "STRIP host" ] || { echo "--- вывод полосы ---"; cat "$out"; fail "вернули фокус хосту, а последняя строка полосы: $last"; }

# полоса обязана НАЗЫВАТЬ спейс, а не просто отличать его от хоста
grep -q -- "STRIP space $space " "$out" || fail "полоса не назвала спейс по имени"

# ни одной строки про ЧУЖОЙ спейс: назвать не тот — это ровно тот отказ, ради которого полоса и пишется.
# считаем только то, что напечатано ПОСЛЕ того, как фокусом стал распоряжаться сам тест: до этого в
# фокусе мог быть чужой спейс, и назвать его — правильное поведение, а не ошибка
after="$(sed -n '/^STRIP host$/,$p' "$out")"
wrong="$(grep '^STRIP space ' <<<"$after" | awk -v s="$space" '$3 != s' || true)"
[ -z "$wrong" ] || { printf '%s\n' "$wrong"; fail "полоса назвала не тот спейс"; }

echo "PASS: полоса назвала спейс $space в фокусе и вернулась к host при переключении"

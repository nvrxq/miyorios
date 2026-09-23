#!/usr/bin/env bash
# VERIFY: V-REDUCED-VISIBLE — уровень изоляции виден в списке, в деталях и в полосе
#
# Спека §8: цена уровня названа вслух и должна быть видна пользователю, а не спрятана
# в манифесте. Тест проверяет, что она доезжает до всех трёх мест.
#
# Что нужно до запуска (см. ручной стенд, фаза 4): запущен miyorid, в нём созданы два
# спейса — один из profiles/spike (standard), другой из profiles/spike-reduced (reduced),
# и второй запущен и показал окно. Для третьей части нужен miyori-guid и сессия niri.
set -euo pipefail
cd "$(dirname "$0")/../.."

socket="${MIYORI_SOCKET:-/run/miyorios/control.sock}"
plain="${MIYORI_SPACE:-spike}"
reduced="${MIYORI_SPACE_REDUCED:-spike-reduced}"

fail() { echo "FAIL: $*"; exit 1; }

[ -S "$socket" ] || fail "нет $socket — демон не запущен"
[ -x target/release/miyori-manager ] || cargo build --release --offline -p miyori-manager

manager="$(target/release/miyori-manager --dry-run --socket "$socket" 2>&1)"
# через файл, а не через трубу: grep -q выходит на первом совпадении, и pipefail ловит SIGPIPE пишущего
snapshot="$(mktemp)"
trap 'rm -f "$snapshot"' EXIT
printf '%s\n' "$manager" > "$snapshot"

# 1. список: уровень стоит в строке спейса
grep -q "^СПЕЙС $reduced .*уровень reduced" "$snapshot" \
  || { printf '%s\n' "$manager" | head -20; fail "в списке у $reduced нет уровня reduced"; }

# положительный контроль: тест обязан отличать уровни, а не просто находить слово "reduced"
grep -q "^СПЕЙС $plain .*уровень standard" "$snapshot" \
  || { printf '%s\n' "$manager" | head -20; fail "в списке у $plain нет уровня standard — тест не различает уровни"; }
echo "ok: список показывает reduced у $reduced и standard у $plain"

# 2. причина: reduced без причины — половина сообщения, и она хуже отсутствия
grep -q "^СПЕЙС $reduced .*уровень reduced — ." "$snapshot" \
  || fail "в списке у $reduced уровень без причины"

# 3. детали: та же причина обязана быть и там
details="$(sed -n "/^СПЕЙС $reduced /,/^СПЕЙС \|^СЕТЬ /p" "$snapshot")"
grep -q "Уровень изоляции: reduced — ." <<<"$details" \
  || { printf '%s\n' "$details" | head -30; fail "в деталях $reduced уровень без причины"; }
echo "ok: причина уровня видна и в списке, и в деталях"

# 4. полоса: уровень окна в фокусе
if [ -z "${NIRI_SOCKET:-}" ]; then
  echo "ПРОПУЩЕНО: полоса не проверялась — нет NIRI_SOCKET"
  echo "НЕПОЛНЫЙ PASS: список и детали показывают уровень; полоса не проверена"
  exit 0
fi

[ -x target/release/miyori-label ] || cargo build --release --offline -p miyori-label

# без этой проверки мёртвый NIRI_SOCKET даёт трейсбек питона вместо внятной причины
windows="$(niri msg --json windows 2>&1)" \
  || fail "niri msg не отвечает на $NIRI_SOCKET: $windows"

window="$(printf '%s' "$windows" | python3 -c '
import json, sys
space = sys.argv[1]
for w in json.load(sys.stdin):
    pid = w.get("pid")
    if not pid:
        continue
    try:
        cg = open(f"/proc/{pid}/cgroup").read()
    except OSError:
        continue
    if f"miyori-gui-{space}.scope" in cg:
        print(w["id"]); break
' "$reduced")"
[ -n "$window" ] || fail "у $reduced нет окна — полосе нечего называть, запусти спейс"

out="$(mktemp)"
restore="$(niri msg --json focused-window | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
cleanup() {
  set +e
  rm -f "$snapshot"
  [ -n "${group:-}" ] && kill -- -"$group" 2>/dev/null
  niri msg action focus-window --id "$restore" >/dev/null 2>&1
  rm -f "$out"
}
trap cleanup EXIT

setsid bash -c 'niri msg --json event-stream | "$0" --dry-run --socket "$1"' \
  target/release/miyori-label "$socket" > "$out" 2>&1 &
group=$!

# полоса обязана успеть подписаться на события niri: пропущенное переключение фокуса
# не повторится, и ожидание после него ничего не даёт
for _ in $(seq 1 40); do [ -s "$out" ] && break; sleep 0.25; done
[ -s "$out" ] || fail "полоса не выдала ни строки за 10 с — подписка на события niri не поднялась"

niri msg action focus-window --id "$window" >/dev/null
for _ in $(seq 1 60); do
  grep -q "^STRIP space $reduced .* reduced$" "$out" && break
  sleep 0.5
done
grep -q "^STRIP space $reduced .* reduced$" "$out" \
  || { echo "--- вывод полосы ---"; cat "$out"; fail "полоса не показала уровень reduced у $reduced"; }

echo "PASS: уровень reduced виден в списке, в деталях и в полосе"

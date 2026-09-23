#!/usr/bin/env bash
# VERIFY: буфер обмена хоста недоступен гостю (V-NO-CLIPBOARD)
#
# Тест умеет обе стороны и требует сказать, какую он проверяет:
#   --expect open  — положительный контроль ДО патча waypipe: буфер обязан работать.
#                    Без этого прогона запрет ничего не доказывает.
#   --expect cut   — после патча: буфер обязан быть отрезан в обе стороны.
#
# Проверяются две вещи, и они не заменяют друг друга:
#   1) список глобалов — детерминированный признак наличия протокола;
#   2) настоящая передача текста — единственный признак того, что данные не ходят.
set -euo pipefail

expect=""
console="build/guest/console-3.txt"
secret=""
token=""
while [ $# -gt 0 ]; do
  case "$1" in
    --expect) expect="${2:?--expect ожидает open или cut}"; shift 2 ;;
    --console) console="${2:?--console ожидает путь}"; shift 2 ;;
    --secret) secret="${2:?--secret ожидает строку}"; shift 2 ;;
    --token) token="${2:?--token ожидает строку}"; shift 2 ;;
    *) echo "неизвестный аргумент $1" >&2; exit 2 ;;
  esac
done
case "$expect" in
  open|cut) ;;
  *) echo "нужен --expect open|cut: тест без ожидания ничего не доказывает" >&2; exit 2 ;;
esac
[ -f "$console" ] || { echo "FAIL: нет $console — проба не снималась"; exit 1; }
control="build/clipboard-positive-control.log"

fail() { echo "FAIL: $*"; exit 1; }

globals="$(sed -n '/---MIYORI-GLOBALS-BEGIN---/,/---MIYORI-GLOBALS-END---/p' "$console" \
  | sed '1d;$d' | grep -v '^\[' || true)"
[ -n "$globals" ] || fail "секция GLOBALS пуста — wayland-info не отработал, проверять нечего"

# без базовых глобалов отсутствие клипбордных ничего не значит: гость мог просто не дойти до композитора
for proto in wl_compositor wl_shm xdg_wm_base; do
  grep -q "$proto" <<<"$globals" || fail "нет $proto — гость не дошёл до композитора"
done

clip_globals=0
for proto in wl_data_device_manager zwp_primary_selection_device_manager_v1 \
             gtk_primary_selection_device_manager zwlr_data_control_manager_v1 \
             ext_data_control_manager_v1; do
  # here-string, а не труба: SIGPIPE под pipefail дал бы ложный PASS на запрещённом протоколе
  if grep -q "$proto" <<<"$globals"; then
    echo "  глобал у гостя: $proto"
    clip_globals=$((clip_globals + 1))
  fi
done

block="$(sed -n '/---MIYORI-CLIPBOARD-BEGIN---/,/---MIYORI-CLIPBOARD-END---/p' "$console" || true)"
[ -n "$block" ] || fail "нет блока MIYORI-CLIPBOARD — гость запускался без MIYORI_CLIPBOARD_PROBE=1"

guest_read=""
if [ -n "$secret" ] && grep -q -- "$secret" <<<"$block"; then
  guest_read="да"
fi

host_read=""
if [ -n "$token" ]; then
  if command -v wl-paste >/dev/null 2>&1 && grep -q -- "$token" < <(wl-paste --no-newline 2>/dev/null); then
    host_read="да"
  fi
fi

echo "  глобалов буфера у гостя: $clip_globals"
echo "  гость прочитал секрет хоста: ${guest_read:-нет}"
echo "  хост прочитал строку гостя: ${host_read:-нет}"

if [ "$expect" = "open" ]; then
  [ "$clip_globals" -gt 0 ] || fail "положительный контроль: у гостя нет ни одного глобала буфера — проверять будет нечего"
  [ -n "$guest_read" ] || fail "положительный контроль: гость НЕ прочитал секрет хоста, значит тест не умеет увидеть утечку"
  [ -n "$host_read" ] || fail "положительный контроль: хост НЕ прочитал строку гостя, значит обратное направление тест не видит"
  mkdir -p build
  printf 'PASS %s console=%s\n' "$(date -Is)" "$console" > "$control"
  echo "PASS: буфер РАБОТАЕТ в обе стороны — тест умеет увидеть утечку"
  exit 0
fi

# без записанного положительного контроля зелёный запрет неотличим от теста, который просто ничего не умеет
[ -f "$control" ] || fail "нет $control — положительный контроль не прогонялся, запрет доказывать нечем"

[ "$clip_globals" -eq 0 ] || fail "гостю всё ещё доступны глобалы буфера ($clip_globals)"
[ -z "$guest_read" ] || fail "гость прочитал секрет хоста — буфер не отрезан"
[ -z "$host_read" ] || fail "хост прочитал строку гостя — обратное направление не отрезано"
echo "PASS: глобалов буфера нет, текст не прошёл ни в одну сторону"

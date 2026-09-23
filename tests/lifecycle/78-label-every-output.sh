#!/usr/bin/env bash
# VERIFY: V-LABEL-EVERY-OUTPUT — полоса стоит и говорит правду на КАЖДОМ выходе
#
# Дополнение к плану (плану менеджера
# «Дополнение к плану: полоса на каждом выходе»): `miyori-label` создаёт одну
# layer-shell поверхность и выхода не выбирает — композитор кладёт её куда решит
# сам, и на двух мониторах из трёх индикатор доверия не сообщает вообще ничего.
# `V-LABEL` (тест 75) этого не ловит: он спрашивает, что полоса РЕШИЛА сказать
# (через --dry-run, без единого пикселя), а не где она физически ВИДНА.
#
# Этот тест смотрит на настоящие пиксели: `grim` с каждого выхода niri, и на
# каждом ищет саму полосу — не по фиксированной строке y=0, а по факту: полоса
# ставит exclusive_zone и должна быть где-то у верхнего края, но если там уже
# сидит чужой layer-shell top (в этой сессии — waybar), компоситор кладёт её
# НИЖЕ него, а не поверх. Поэтому "верх выхода" здесь — это полоса сплошного
# цвета, которой не было на снимке ДО запуска miyori-label.
#
# Что нужно до запуска (всё — в сессии niri, от обычного пользователя,
# см. документации проекта, фаза 4):
#   1. поднят демон и в нём есть хотя бы один запущенный спейс с окном
#      (заголовок начинается с "[space:");
#   2. запущен miyori-guid, иначе окна спейса не будет вовсе;
#   3. в niri есть хотя бы одно окно хоста (терминал, из которого это
#      запускается, годится);
#   4. в PATH есть niri, grim и python3.
set -euo pipefail
cd "$(dirname "$0")/../.."

socket="${MIYORI_SOCKET:-/run/miyorios/control.sock}"
label="target/release/miyori-label"
# HOST_BACKGROUND из components/label/src/bar.rs — константа, не догадка
HOST_HEX="202020"
# физических строк от верха, в которых ищем полосу: с запасом на пару чужих top-панелей над ней
SCAN_ROWS=300

fail() { echo "FAIL: $*"; exit 1; }

# дешёвые отказы раньше сборки и раньше запуска чего-либо на экране
[ -n "${NIRI_SOCKET:-}" ] || fail "нет NIRI_SOCKET — тест запускается изнутри сессии niri"
command -v niri >/dev/null || fail "нет niri в PATH"
command -v grim >/dev/null || fail "нет grim в PATH"
command -v python3 >/dev/null || fail "нет python3 в PATH"
[ -S "$socket" ] || fail "нет $socket — демон не запущен, полосе неоткуда взять уровень спейса"
if pgrep -x miyori-label >/dev/null; then
  fail "miyori-label уже запущен — тест не убивает чужой процесс, погаси его руками и повтори"
fi
[ -x "$label" ] || cargo build --release --offline -p miyori-label

windows="$(niri msg --json windows)"

space_id="$(python3 -c '
import json, sys
for w in json.load(sys.stdin):
    if (w.get("title") or "").startswith("[space:"):
        print(w["id"]); break
' <<<"$windows")"
host_id="$(python3 -c '
import json, sys
for w in json.load(sys.stdin):
    if not (w.get("title") or "").startswith("[space:"):
        print(w["id"]); break
' <<<"$windows")"
restore="$(python3 -c '
import json, sys
for w in json.load(sys.stdin):
    if w.get("is_focused"):
        print(w["id"]); break
' <<<"$windows")"

[ -n "$space_id" ] || fail "в niri нет ни одного окна спейса — проверять нечего, подними стенд"
[ -n "$host_id" ] || fail "в niri нет ни одного окна хоста — переключать не на что"

mapfile -t outs < <(niri msg --json outputs | python3 -c '
import json, sys
for name in json.load(sys.stdin):
    print(name)
')
[ "${#outs[@]}" -ge 1 ] || fail "niri не назвал ни одного выхода"

shotdir="$(mktemp -d)"
pid=""
cleanup() {
  set +e
  # kill -- -pid, а не kill pid: полоса сама поднимает "niri msg --json event-stream" как дочерний
  # процесс, и одиночный kill оставил бы его сиротой; setsid делает pid и лидером группы, и лидером сессии
  [ -n "$pid" ] && kill -- -"$pid" 2>/dev/null
  [ -n "$restore" ] && niri msg action focus-window --id "$restore" >/dev/null 2>&1
  rm -rf "$shotdir"
}
trap cleanup EXIT

# разбор PPM обычным python3: P6, заголовок "P6\n<w> <h>\n255\n", дальше RGB-тройки без разделителей.
# ищем не "первую непохожую на фон строку", а ТРИ подряд одинаковых: на границе с чужим
# layer-shell-слоем компоситор при дробном scale (1.5) смешивает крайние пиксели, и одна
# строка на стыке может дать случайный цвет — трём подряд одинаковым так подделаться нечем
cat > "$shotdir/findrow.py" <<'PY'
import sys
from collections import Counter


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:2] != b"P6":
        raise SystemExit(f"{path}: не P6")
    idx = 2
    vals = []
    while len(vals) < 3:
        while data[idx : idx + 1].isspace():
            idx += 1
        if data[idx : idx + 1] == b"#":
            while data[idx : idx + 1] != b"\n":
                idx += 1
            continue
        start = idx
        while not data[idx : idx + 1].isspace():
            idx += 1
        vals.append(int(data[start:idx]))
    idx += 1  # ровно один пробельный байт отделяет maxval от бинарных данных
    width, height, _maxval = vals
    return width, height, data[idx:]


def row_stat(pixels, width, y):
    start = y * width * 3
    counts = Counter(bytes(pixels[start + x * 3 : start + x * 3 + 3]) for x in range(width))
    color, count = counts.most_common(1)[0]
    return color, count / width


THRESH = 0.9
CONSEC = 3

base_path, probe_path, scanmax = sys.argv[1], sys.argv[2], int(sys.argv[3])
bw, bh, bpix = read_ppm(base_path)
pw, ph, ppix = read_ppm(probe_path)
if bw != pw:
    raise SystemExit(f"ширина сменилась между снимками: было {bw}, стало {pw}")

run_color, run_len, run_start = None, 0, None
for y in range(min(scanmax, bh, ph)):
    pcolor, pfrac = row_stat(ppix, pw, y)
    changed = False
    if pfrac >= THRESH:
        bcolor, bfrac = row_stat(bpix, bw, y)
        changed = bfrac < THRESH or bcolor != pcolor
    if changed and pcolor == run_color:
        run_len += 1
    elif changed:
        run_color, run_len, run_start = pcolor, 1, y
    else:
        run_color, run_len = None, 0
    if run_len >= CONSEC:
        print(f"{run_start} {run_color.hex()}")
        sys.exit(0)

print("ABSENT")
PY

# снимок ДО полосы: без него нечем измерить, что вообще на экране поменялось
for out in "${outs[@]}"; do
  grim -o "$out" -t ppm "$shotdir/baseline-$out.ppm" >/dev/null 2>&1 \
    || fail "grim не смог снять исходный вид выхода $out"
done

probe() {
  local out="$1"
  grim -o "$out" -t ppm "$shotdir/probe-$out.ppm" >/dev/null 2>&1 || fail "grim не смог снять выход $out"
  python3 "$shotdir/findrow.py" "$shotdir/baseline-$out.ppm" "$shotdir/probe-$out.ppm" "$SCAN_ROWS"
}

# ждём, пока чтение на одном и том же выходе не повторится дважды подряд без изменений:
# в обычном режиме полоса не печатает в stdout (в отличие от --dry-run), и единственный
# способ узнать "перерисовалась и успокоилась" — сравнить два последовательных снимка
wait_stable() {
  local out="$1" prev="" cur=""
  local tries=0
  while [ "$tries" -lt 12 ]; do
    cur="$(probe "$out")"
    [ "$cur" = "$prev" ] && { echo "$cur"; return 0; }
    prev="$cur"
    tries=$((tries + 1))
    sleep 0.3
  done
  echo "$cur"
}

# обычный режим, не --dry-run: только он реально кладёт поверхность на экран
setsid "$label" --socket "$socket" >"$shotdir/label.log" 2>&1 &
pid=$!

# шаг 1: дождаться, пока полоса ДЕЙСТВИТЕЛЬНО нарисуется — хоть где-то
drawn=""
for _ in $(seq 1 30); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "--- лог полосы ---"; cat "$shotdir/label.log"
    fail "miyori-label умер, не успев ничего нарисовать"
  fi
  for out in "${outs[@]}"; do
    reading="$(probe "$out")"
    if [ "$reading" != "ABSENT" ]; then
      drawn="$out"
      break 2
    fi
  done
  sleep 0.2
done
[ -n "$drawn" ] || fail "полоса не нарисовалась ни на одном из выходов (${outs[*]}) за время ожидания"

# шаг 2: фаза "спейс" — фокус на окне спейса, снимок с КАЖДОГО выхода
niri msg action focus-window --id "$space_id" >/dev/null
declare -A SPACE_STATUS SPACE_HEX
for out in "${outs[@]}"; do
  reading="$(wait_stable "$out")"
  if [ "$reading" = "ABSENT" ]; then
    SPACE_STATUS[$out]="ABSENT"
  else
    read -r _ hex <<<"$reading"
    SPACE_STATUS[$out]="OK"
    SPACE_HEX[$out]="$hex"
  fi
done

# шаг 3: фаза "хост" — фокус на окне НЕ спейса, снимок с КАЖДОГО выхода
niri msg action focus-window --id "$host_id" >/dev/null
declare -A HOST_STATUS HOST_GOT
for out in "${outs[@]}"; do
  reading="$(wait_stable "$out")"
  if [ "$reading" = "ABSENT" ]; then
    HOST_STATUS[$out]="ABSENT"
  else
    read -r _ hex <<<"$reading"
    HOST_STATUS[$out]="OK"
    HOST_GOT[$out]="$hex"
  fi
done

echo "ok: сняты обе фазы на выходах: ${outs[*]}"

# 4. PASS только если:
#    (a) в фазе "хост" на КАЖДОМ выходе стоит именно #202020 — это самый прямой признак
#        того, что полоса там вообще есть, а не что-то постороннее того же оттенка;
present=() missing=()
for out in "${outs[@]}"; do
  if [ "${HOST_STATUS[$out]}" = "OK" ] && [ "${HOST_GOT[$out]}" = "$HOST_HEX" ]; then
    present+=("$out")
  elif [ "${HOST_STATUS[$out]}" = "ABSENT" ]; then
    missing+=("$out(нет полосы)")
  else
    missing+=("$out(#${HOST_GOT[$out]})")
  fi
done
if [ "${#missing[@]}" -gt 0 ]; then
  fail "полоса не на каждом выходе — есть (цвет хоста #$HOST_HEX) на: ${present[*]:-ни на одном}; нет (или другой цвет) на: ${missing[*]}"
fi

#    (b) в фазе "спейс" цвет один и тот же на всех выходах;
ref_out="${outs[0]}"
ref_color="${SPACE_HEX[$ref_out]:-}"
uneven=()
for out in "${outs[@]}"; do
  if [ "${SPACE_STATUS[$out]}" != "OK" ]; then
    uneven+=("$out(нет полосы)")
  elif [ "${SPACE_HEX[$out]}" != "$ref_color" ]; then
    uneven+=("$out=#${SPACE_HEX[$out]}")
  fi
done
if [ "${#uneven[@]}" -gt 0 ]; then
  fail "цвет полосы спейса разный по выходам: эталон $ref_out=#$ref_color; отличаются: ${uneven[*]}"
fi

#    (c) и этот цвет отличается от цвета хоста — иначе полоса ничего не сообщает.
same_as_host=()
for out in "${outs[@]}"; do
  [ "${SPACE_HEX[$out]}" = "${HOST_GOT[$out]}" ] && same_as_host+=("$out")
done
if [ "${#same_as_host[@]}" -gt 0 ]; then
  fail "на выходах ${same_as_host[*]} цвет фазы «спейс» не отличается от цвета фазы «хост»"
fi

echo "PASS: полоса на всех выходах (${outs[*]}) верно показывает состояние — спейс #$ref_color, хост #$HOST_HEX"

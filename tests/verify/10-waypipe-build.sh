#!/usr/bin/env bash
# VERIFY: собранный waypipe новее пакетного и умеет vsock/secctx/title-prefix
set -euo pipefail
WP="${1:-components/waypipe/bin/waypipe}"

[ -x "$WP" ] || { echo "FAIL: $WP не найден или не исполняем"; exit 1; }

ver="$("$WP" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
major="${ver%%.*}"; rest="${ver#*.}"; minor="${rest%%.*}"
if [ "$major" -eq 0 ] && [ "$minor" -lt 11 ]; then
  echo "FAIL: waypipe $ver < 0.11"; exit 1
fi

for flag in --vsock --secctx --title-prefix --no-gpu; do
  grep -q -- "$flag" < <("$WP" --help 2>&1) || { echo "FAIL: нет флага $flag"; exit 1; }
done

# ADR-4: ffmpeg и Vulkan не должны попадать в парсер недоверенного протокола
for feat in dmabuf video lz4 zstd; do
  grep -qE "^ *$feat: false" < <("$WP" --version) \
    || { echo "FAIL: фича $feat включена вопреки решению о минимальном наборе"; exit 1; }
done

# бухгалтерия, а не доказательство: PINNED пишет наш же скрипт. Настоящая проверка отреза — тест 44
pinned="$(dirname "$WP")/../PINNED"
[ -f "$pinned" ] || { echo "FAIL: нет $pinned — непонятно, из чего собран бинарь"; exit 1; }

# до положительного контроля непропатченный waypipe — это НУЖНОЕ состояние: на нём и доказывают,
# что тест умеет увидеть работающий буфер. После контроля тот же бинарь означает устаревшую сборку
if [ -f build/clipboard-positive-control.log ]; then
  grep -q "^patches=.*waypipe-cut-clipboard.patch" "$pinned" \
    || { echo "FAIL: положительный контроль снят, а $pinned не помнит патча буфера — пересобери waypipe"; exit 1; }
  echo "PASS: waypipe $ver со всеми нужными флагами, без лишних фич и с патчем буфера"
else
  echo "PASS: waypipe $ver со всеми нужными флагами и без лишних фич"
  echo "  ЗАМЕТКА: патч буфера ещё не требуется — положительный контроль (тест 44 --expect open) не снимался"
fi

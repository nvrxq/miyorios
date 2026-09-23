#!/usr/bin/env bash
# Удаляет шаблоны образов, за которые никто не держится. Считает заново в момент запуска,
# а не по заранее собранному списку: между сборкой списка и удалением спейс мог переехать.
# Спейс лежит qcow2-оверлеем поверх root.qcow2 шаблона — удалить чужой digest значит
# уничтожить диск спейса, поэтому пропускаем и latest, и всё, на что ссылается config.toml.
set -euo pipefail

state="${MIYORI_STATE_DIR:-/var/lib/miyorios}"
[ -d "$state/templates" ] || { echo "FAIL: нет $state/templates" >&2; exit 1; }
dry=0
[ "${1:-}" = "--dry-run" ] && dry=1

# find, а не глоб: под pipefail пустой spaces/ уронил бы скрипт на коде возврата cat
used="$(find "$state/spaces" -mindepth 2 -maxdepth 2 -name config.toml -exec cat {} + 2>/dev/null \
  | awk -F= '/^digest/ { gsub(/[" ]/, "", $2); print $2 }')"

# без этой проверки нечитаемый store означал бы «ни один digest не занят» — и снос живых дисков
spaces="$(find "$state/spaces" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)"
if [ "$spaces" -gt 0 ] && [ -z "$used" ]; then
  echo "FAIL: спейсов $spaces, а digest'ы не прочитались — под этим пользователем удалять нельзя" >&2
  exit 1
fi

total=0
for profile in "$state"/templates/*/; do
  keep="$(readlink "$profile/latest" 2>/dev/null || true)"
  for dir in "$profile"*/; do
    # профиль без единого образа оставляет глоб нераскрытым, и дальше du убивает скрипт
    # посреди работы: часть уже удалена, отчёта нет
    [ -d "$dir" ] || continue
    d="$(basename "$dir")"
    [ "$d" = "latest" ] && continue
    [ "$d" = "$keep" ] && continue
    printf '%s\n' "$used" | grep -qx "$d" && continue
    mb="$(du -sm "$dir" | cut -f1)"
    total=$((total + mb))
    printf '  %-14s %s  %sM\n' "$(basename "$profile")" "${d:0:12}" "$mb"
    [ "$dry" = "1" ] && continue
    chmod -R u+w "$dir" && rm -rf "$dir"
  done
done

if [ "$dry" = "1" ]; then
  echo "освободит: ${total}M (запусти без --dry-run)"
else
  echo "освобождено: ${total}M"
fi

#!/usr/bin/env bash
# Показывает демону профили репозитория: он читает <state>/profiles, а там симлинки (ручной стенд).
# Новый каталог в profiles/ сам туда не попадает — из-за этого профиль не виден в менеджере.
set -euo pipefail
cd "$(dirname "$0")/.."

state="${MIYORI_STATE_DIR:-/var/lib/miyorios}/profiles"
[ -d "$state" ] || {
  echo "FAIL: нет $state — платформа не установлена (install/install-system.sh)" >&2; exit 1; }

linked=0
for dir in profiles/*/; do
  name="$(basename "$dir")"
  target="$state/$name"
  if [ -L "$target" ]; then continue; fi
  if [ -e "$target" ]; then
    echo "пропускаю $name: $target уже существует и это не симлинк" >&2
    continue
  fi

  ln -s "$(realpath "$dir")" "$target"
  echo "связан $name"
  linked=$((linked + 1))
done

echo "готово: связано $linked, всего у демона $(find "$state" -mindepth 1 -maxdepth 1 | wc -l)"
# менеджер показывает и профиль со сломанным манифестом — с пометкой, а не пропажей
echo "профиль со сломанным манифестом виден в менеджере как сломанный, а не исчезает"

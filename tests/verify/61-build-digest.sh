#!/usr/bin/env bash
# V-BUILD: сборка адресуется по содержимому, и невоспроизводимость зафиксирована, а не спрятана
set -euo pipefail
cd "$(dirname "$0")/../.."

profile="${1:-spike}"
root="build/templates/$profile"

[ -d "$root" ] || { echo "FAIL: нет $root — сначала bash tools/build-profile.sh profiles/$profile"; exit 1; }

digests=$(find "$root" -maxdepth 1 -mindepth 1 -type d ! -name '.stage-*' -printf '%f\n' | sort)
count=$(printf '%s\n' "$digests" | grep -c . || true)
[ "$count" -ge 1 ] || { echo "FAIL: ни одного шаблона"; exit 1; }

# .stage-* — остаток прерванной сборки, а не шаблон: судить о нём V-BUILD не должна, но и молчать о мусоре нельзя
stage_count=$(find "$root" -maxdepth 1 -mindepth 1 -type d -name '.stage-*' | wc -l)
[ "$stage_count" -eq 0 ] || echo "примечание: $stage_count .stage-* в $root — остатки прерванной сборки, не в счёт"

for d in $digests; do
  case "$d" in *[!0-9a-f]*) echo "FAIL: $d содержит не-hex символы"; exit 1 ;; esac
  [ "${#d}" -eq 64 ] || { echo "FAIL: длина digest $d равна ${#d}, а не 64"; exit 1; }
  for f in root.qcow2 vmlinuz initrd.img MANIFEST; do
    [ -f "$root/$d/$f" ] || { echo "FAIL: в $d нет $f"; exit 1; }
  done
  grep -q '^manifest-sha256:' "$root/$d/MANIFEST" || { echo "FAIL: в MANIFEST $d нет хеша манифеста"; exit 1; }
  grep -q '^digest:' "$root/$d/MANIFEST" || { echo "FAIL: в MANIFEST $d нет digest"; exit 1; }
  # демон запускает QEMU под uid спейса, а не под оператором: ядро приезжает из образа режимом 0600
  # и после a-w осталось бы 0400, а run-guest.sh этого не ловит — он идёт от владельца файлов
  for f in root.qcow2 vmlinuz initrd.img; do
    mode="$(stat -c '%a' "$root/$d/$f")"
    [ "$mode" = 444 ] || {
      echo "FAIL: $root/$d/$f имеет режим $mode; шаблон обязан быть читаем uid'ом спейса (444)"; exit 1; }
  done
done

# честность про воспроизводимость: два прогона обязаны дать разные digest (спека §4.2) — но это
# наблюдение, а не структурная проверка; при одном шаблоне вторая половина V-BUILD не пройдена и не провалена
if [ "$count" -ge 2 ]; then
  echo "ok: $count разных digest — невоспроизводимость сборки наблюдается, а не декларируется"
  echo "PASS: V-BUILD ($count шаблон(ов) профиля $profile)"
else
  echo "не проверено: один шаблон — невоспроизводимость не пронаблюдана, соберите профиль второй раз"
  echo "PASS: структура шаблона профиля $profile корректна; половина V-BUILD про невоспроизводимость НЕ проверена"
fi

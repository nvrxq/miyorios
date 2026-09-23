#!/usr/bin/env bash
# Гейт M1: все проверки обязаны пройти
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1 || exit 1
# двухфазные тесты, которым нужен особый стенд: молча пропускать их нельзя, но и запускать
# без их процедуры бессмысленно — печатаем громко, чтобы про них нельзя было забыть
manual="44-clipboard.sh"

fail=0
ran=0
for t in tests/verify/[0-9]*.sh; do
  base="$(basename "$t")"
  case " $manual " in
    *" $base "*)
      echo "--- $t"
      echo "РУЧНОЙ: буфер обмена проверяется в две фазы, см. ручные фазы стендаа и 1б"
      continue
      ;;
  esac
  echo "--- $t"
  ran=$((ran + 1))
  bash "$t" || { echo "^^^ FAILED"; fail=1; }
done
# пустой список тестов не должен выглядеть как пройденный гейт
[ "$ran" -ge 9 ] || { echo "GATE M1: FAIL — найдено тестов: $ran"; exit 1; }
[ "$fail" = "0" ] && echo "GATE M1: PASS" || echo "GATE M1: FAIL"
exit "$fail"

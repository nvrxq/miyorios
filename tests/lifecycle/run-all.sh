#!/usr/bin/env bash
# Третья батарея (план 2, см. ручной стенд, фаза 3): закрывается строкой ГЕЙТ ЖИЗНЕННОГО ЦИКЛА: PASS — либо нет
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash tests/lifecycle/run-all.sh)" >&2; exit 1; }

tests=(
  "71-lifecycle.sh|create/start/stop/destroy; reset-system и reset-all рушат систему и хранят данные"
  "72-graceful.sh|том отмонтирован до убийства QEMU; положительный контроль умеет увидеть грязный том"
  "73-agent-nonce.sh|без верного per-boot nonce гость живым не считается"
  "74-build.sh|build кладёт шаблон под root без права записи; профиля нет -> profile-not-found (нужна сеть)"
  "79-encrypted-disk.sh|V-DISK-ENCRYPTED: засеянная метка не видна в data.qcow2, без пароля спейс не поднять"
)

log="build/lifecycle-gate.log"
mkdir -p build
: > "$log"

# батарея идёт под sudo — иначе оставит в build/ root-овские файлы, как и tests/net/run-all.sh
# shellcheck disable=SC2317 # вызывается только через trap EXIT, shellcheck не видит вызова
restore_build_ownership() {
  [ -n "${SUDO_UID:-}" ] && [ -n "${SUDO_GID:-}" ] || return 0
  chown -R "$SUDO_UID:$SUDO_GID" build 2>/dev/null || true
}
trap restore_build_ownership EXIT

passed=0
failed=0
absent=0

for entry in "${tests[@]}"; do
  file="${entry%%|*}"
  what="${entry#*|}"
  path="tests/lifecycle/$file"

  if [ ! -f "$path" ]; then
    # ненаписанный тест — это не «пропуск», а незакрытый гейт
    printf 'НЕТ ТЕСТА  %-28s %s\n' "$file" "$what"
    absent=$((absent + 1))
    continue
  fi

  # решает код возврата теста, а не grep по общему логу: там нашёлся бы PASS соседа
  out="$(bash "$path" 2>&1)"
  rc=$?
  {
    echo "════════ $file ════════"
    printf '%s\n' "$out"
    echo "── код выхода: $rc"
  } >> "$log"

  if [ "$rc" -eq 0 ] && grep -q '^PASS' <<<"$out"; then
    printf 'PASS       %-28s %s\n' "$file" "$what"
    passed=$((passed + 1))
  else
    printf 'FAIL       %-28s %s\n' "$file" "$what"
    printf '%s\n' "$out" | head -5 | sed 's/^/           /'
    failed=$((failed + 1))
  fi
done

echo
echo "прошло: $passed, упало: $failed, не написано: $absent; полный вывод в $log"
# эти три требуют сессии niri и живых окон, а батарея идёт под sudo — их гоняют руками, фаза 4
echo "не входят сюда и гоняются руками (ручная фаза стенда): 75-label.sh, 76-no-zombie.sh, 77-reduced-visible.sh"

if [ "$failed" -eq 0 ] && [ "$absent" -eq 0 ]; then
  echo
  echo "ГЕЙТ ЖИЗНЕННОГО ЦИКЛА: PASS"
  exit 0
fi
echo
echo "ГЕЙТ ЖИЗНЕННОГО ЦИКЛА: NOT CLOSED"
exit 1

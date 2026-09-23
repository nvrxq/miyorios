#!/usr/bin/env bash
# VERIFY: все библиотеки nekobox и waypipe разрешаются внутри образа miyori-net, а не с хоста
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

image="build/templates/miyori-net/latest/root.qcow2"
[ -f "$image" ] || {
  echo "FAIL: нет $image — сначала bash tools/build-profile.sh components/net/miyori-net"; exit 1; }

# content-addressed шаблон хранит только qcow2, а не распакованное дерево — извлекаем сами
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
qemu-img convert -O raw "$image" "$work/root.raw"
root="$work/rootfs"
mkdir -p "$root"
debugfs -R "rdump / $root" "$work/root.raw" >/dev/null 2>&1

[ -d "$root/opt/nekobox" ] || {
  echo "FAIL: нет $root/opt/nekobox — образ miyori-net не собран"; exit 1; }

# каталоги образа и только они: хостовые пути сюда не попадают, иначе проверка ничего не значит
libdirs=(
  "$root/opt/nekobox/usr/lib"
  "$root/usr/lib/x86_64-linux-gnu"
  "$root/lib/x86_64-linux-gnu"
  "$root/usr/lib"
)

resolve() {
  for d in "${libdirs[@]}"; do
    [ -e "$d/$1" ] && { printf '%s\n' "$d/$1"; return 0; }
  done
  return 1
}

seen=" "
missing=0
checked=0

# NEEDED разрешается транзитивно: одного уровня мало, дырка обычно на втором
walk() {
  local obj="$1" so path
  for so in $(objdump -p "$obj" 2>/dev/null | awk '/NEEDED/{print $2}'); do
    case "$seen" in *" $so "*) continue ;; esac
    seen="$seen$so "
    if path="$(resolve "$so")"; then
      checked=$((checked + 1))
      walk "$path"
    else
      echo "  НЕ РАЗРЕШАЕТСЯ: $so (нужна для $(basename "$obj"))"
      missing=$((missing + 1))
    fi
  done
}

# плагины Qt подгружаются через dlopen, в NEEDED их нет — перечисляем явно
roots=(
  "$root/opt/nekobox/nekobox"
  "$root/opt/nekobox/nekobox_core"
  "$root/usr/local/bin/waypipe"
  "$root/opt/nekobox/usr/plugins/platforms/libqwayland.so"
  "$root/opt/nekobox/usr/plugins/wayland-shell-integration/libxdg-shell.so"
)

for r in "${roots[@]}"; do
  [ -f "$r" ] || { echo "FAIL: нет $r"; exit 1; }
  walk "$r"
done

[ "$checked" -gt 0 ] || {
  echo "FAIL: не разрешено ни одной библиотеки — objdump не отработал, проверка ничего не доказывает"
  exit 1; }

[ "$missing" -eq 0 ] || {
  echo "FAIL: библиотек не хватает: $missing"; exit 1; }

echo "PASS: разрешено $checked библиотек внутри образа, недостающих нет"

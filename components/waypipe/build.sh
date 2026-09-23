#!/usr/bin/env bash
# Собирает waypipe вне дерева репозитория: корневой vendor-override сломал бы чужую сборку
set -euo pipefail
# компонент держит и пин, и патчи, и собранный бинарь — путь до корня репозитория ему не нужен
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=/dev/null
. "$here/waypipe.pin"

src="${XDG_CACHE_HOME:-$HOME/.cache}/miyorios/waypipe-src"
out="$here"

# страховка: ниже по коду есть rm -rf, и он не должен получить неожиданный путь
case "$src" in
  */miyorios/waypipe-src) ;;
  *) echo "FAIL: путь кеша выглядит неверно: $src" >&2; exit 1 ;;
esac

mkdir -p "$(dirname "$src")" "$out/bin"

# пин сдвинули на новый коммит: shallow-клон не дотянет другой тег, переклонируем
if [ -d "$src/.git" ] && [ "$(git -C "$src" rev-parse HEAD)" != "$WAYPIPE_COMMIT" ]; then
  echo "клон устарел, пересоздаю $src"
  rm -rf "$src"
fi

if [ ! -d "$src/.git" ]; then
  git clone --depth 1 --branch "$WAYPIPE_TAG" "$WAYPIPE_REPO" "$src"
fi

commit="$(git -C "$src" rev-parse HEAD)"
if [ "$commit" != "$WAYPIPE_COMMIT" ]; then
  echo "FAIL: HEAD $commit не совпал с пином $WAYPIPE_COMMIT" >&2
  exit 1
fi

lock_sha="$(sha256sum "$src/Cargo.lock" | cut -d' ' -f1)"

# патчи накладываются на чистый пин: иначе повторный запуск наложил бы их дважды или молча не наложил
git -C "$src" checkout -- .
patch_sha=""
for patch in "$here"/patches/waypipe-*.patch; do
  [ -f "$patch" ] || continue
  git -C "$src" apply --check "$patch" || { echo "FAIL: патч $patch не ложится на пин $WAYPIPE_TAG" >&2; exit 1; }
  git -C "$src" apply "$patch"
  patch_sha="$patch_sha $(basename "$patch"):$(sha256sum "$patch" | cut -d' ' -f1)"
  echo "наложен $(basename "$patch")"
done

# запуск из каталога клона, а не из репозитория — иначе наследуется подмена crates-io
# --no-default-features убирает ffmpeg, Vulkan и gbm из TCB, см. ADR-4
( cd "$src" && cargo build --release --locked --no-default-features )

install -m 0755 "$src/target/release/waypipe" "$out/bin/waypipe"
cp "$out/bin/waypipe" "$out/bin/waypipe.guest"
printf 'tag=%s\ncommit=%s\ncargo_lock_sha256=%s\nfeatures=none\npatches=%s\n' \
  "$WAYPIPE_TAG" "$commit" "$lock_sha" "${patch_sha# }" > "$out/PINNED"
echo "waypipe собран: $commit (Cargo.lock sha256 $lock_sha)"

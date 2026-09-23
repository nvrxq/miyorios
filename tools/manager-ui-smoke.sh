#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none G_DEBUG=fatal-criticals
export GIO_USE_VFS=local GSETTINGS_BACKEND=memory LP_NUM_THREADS=2
unset WAYLAND_DISPLAY
test_env=$(mktemp -d /tmp/miyori-ui-smoke.XXXXXX)
trap 'rm -rf -- "$test_env"' EXIT
export XDG_DATA_HOME="$test_env/data" XDG_CONFIG_HOME="$test_env/config" XDG_CACHE_HOME="$test_env/cache"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"
dbus-run-session -- xvfb-run -a -s '-screen 0 1440x1000x24' \
  cargo test -p miyori-manager --locked --offline \
  gtk_refresh_preserves_selection_and_navigation -- --ignored --test-threads=1 --nocapture

#!/usr/bin/env bash
# VERIFY: гость не получает привилегированные Wayland-глобалы
set -euo pipefail
out="${1:-build/guest/console-3.txt}"
[ -f "$out" ] || { echo "FAIL: нет $out — проба не снималась"; exit 1; }

globals="$(sed -n '/---MIYORI-GLOBALS-BEGIN---/,/---MIYORI-GLOBALS-END---/p' "$out" \
  | sed '1d;$d' | grep -v '^\[' || true)"
[ -n "$globals" ] || { echo "FAIL: секция GLOBALS пуста — wayland-info не отработал"; exit 1; }

# положительный контроль: без базовых глобалов список запрещённых ничего не значит
for proto in wl_compositor wl_shm xdg_wm_base; do
  grep -q "$proto" <<<"$globals" \
    || { echo "FAIL: нет $proto — гость не дошёл до композитора"; exit 1; }
done

# ext_data_control и gtk_primary_selection добавлены 2026-08-28: сегодня их не отдаёт niri,
# и до сих пор это никем не проверялось — смена версии композитора прошла бы незамеченной
for proto in zwlr_screencopy_manager_v1 zwlr_data_control_manager_v1 \
             ext_data_control_manager_v1 gtk_primary_selection_device_manager \
             zwp_virtual_keyboard_manager_v1 zwlr_virtual_pointer_manager_v1 \
             zwlr_layer_shell_v1 ext_session_lock_manager_v1 \
             zwlr_foreign_toplevel_manager_v1 zwlr_export_dmabuf_manager_v1; do
  # here-string, а не труба: grep -q выходит на первом совпадении, и SIGPIPE под pipefail
  # превратил бы найденный запрещённый протокол в "не найден" — ложный PASS
  if grep -q "$proto" <<<"$globals"; then
    echo "FAIL: гостю доступен $proto"; printf '%s\n' "$globals"; exit 1
  fi
done
echo "PASS: базовые глобалы есть, привилегированные недоступны"

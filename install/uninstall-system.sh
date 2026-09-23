#!/usr/bin/env bash
# Откатывает ровно то, что поставил install-system.sh
set -euo pipefail
cd "$(dirname "$0")/.."

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash install/uninstall-system.sh)" >&2; exit 1; }

operator="${SUDO_USER:-$(id -un)}"
runtime_dir=""
if [ "$operator" != root ]; then
  runtime_dir="/run/user/$(id -u "$operator")"
fi

systemctl disable --now miyorid.service 2>/dev/null || true

if [ -n "$runtime_dir" ] && [ -S "$runtime_dir/bus" ]; then
  sudo -u "$operator" env XDG_RUNTIME_DIR="$runtime_dir" \
    systemctl --user disable --now miyori-guid.service miyori-label.service 2>/dev/null || true
else
  # без шины сессии юниты просто останутся файлами до удаления ниже — процессы это не остановит
  echo "нет активной сессии оператора $operator — пользовательские юниты не остановлены, только удалены"
fi

# киллсвитч снимаем его же командой: она вернёт accept_ra, DNS и уберёт таблицу вместе
# с drop-in резолвера. Без этого хост после удаления грузился бы без имён вовсе
if [ -x /usr/local/sbin/miyori-host-killswitch ]; then
  /usr/local/sbin/miyori-host-killswitch off 2>/dev/null || true
fi
systemctl disable --now miyori-host-killswitch.service miyori-host-dns.service 2>/dev/null || true

rm -f /etc/systemd/system/miyorid.service
rm -f /etc/systemd/system/miyori-host-killswitch.service /etc/systemd/system/miyori-host-dns.service
rm -f /etc/systemd/system/miyori-net.service /etc/systemd/system/miyori-net-uplink.service \
      /etc/systemd/system/miyori-net-fixture.service
rm -f /usr/local/sbin/miyori-host-killswitch /etc/sysctl.d/99-miyori-no-ipv6.conf
rm -f /etc/systemd/user/miyori-guid.service /etc/systemd/user/miyori-label.service
rm -f /etc/tmpfiles.d/miyorios.conf
rm -f /usr/share/applications/miyori-manager.desktop
rm -f /usr/share/icons/hicolor/512x512/apps/miyori.png
rm -f /usr/local/sbin/miyorid /usr/local/bin/miyori-guid /usr/local/bin/miyori-label /usr/local/bin/miyori-manager
rm -f /usr/local/lib/miyorios/waypipe
rmdir /usr/local/lib/miyorios 2>/dev/null || true

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database /usr/share/applications
fi

systemctl daemon-reload
if [ -n "$runtime_dir" ] && [ -S "$runtime_dir/bus" ]; then
  sudo -u "$operator" env XDG_RUNTIME_DIR="$runtime_dir" systemctl --user daemon-reload
fi

cat <<'EOF'
готово: бинари, юниты, tmpfiles и ярлык менеджера сняты.
НЕ тронуто: /etc/miyorios/spaces.toml и всё в /var/lib/miyorios (образы и тома
спейсов) — их создавал не install-system.sh, и uninstall-system.sh их не удаляет.
EOF

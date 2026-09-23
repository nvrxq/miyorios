#!/usr/bin/env bash
# Переносит демон/брокер/полосу со стенда (ручной запуск из target/release) в автозапуск systemd
set -euo pipefail
cd "$(dirname "$0")/.."

[ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root (sudo bash install/install-system.sh)" >&2; exit 1; }

# SUDO_USER — так же определяет оператора components/net/net-fixture.sh; без него имя пришлось бы вписывать в код
operator="${SUDO_USER:-$(id -un)}"
[ "$operator" != root ] || {
  echo "FAIL: не вижу оператора — запускай через sudo из-под обычного пользователя, не из root-шелла" >&2
  exit 1
}
operator_group="$(id -gn "$operator")"
operator_uid="$(id -u "$operator")"
runtime_dir="/run/user/$operator_uid"

# пользовательские юниты живут в сессии niri оператора — без неё systemctl --user не достучится до шины
[ -S "$runtime_dir/bus" ] || {
  echo "FAIL: нет $runtime_dir/bus — запускай установщик изнутри графической сессии niri оператора $operator" >&2
  exit 1
}

bin_dir="target/release"
waypipe_bin="components/waypipe/bin/waypipe"

missing=()
for bin in miyorid miyori-guid miyori-label miyori-manager; do
  [ -x "$bin_dir/$bin" ] || missing+=("$bin_dir/$bin")
done
if [ "${#missing[@]}" -ne 0 ]; then
  echo "FAIL: не собраны бинари: ${missing[*]}" >&2
  echo "почини: cargo build --release --offline" >&2
  exit 1
fi
[ -x "$waypipe_bin" ] || {
  echo "FAIL: не собран патченый waypipe: $waypipe_bin" >&2
  echo "почини: bash components/waypipe/build.sh" >&2
  exit 1
}

# демон зовёт сборщик образов относительным путём — юниту нужен корень репозитория
repo="$(pwd -P)"
echo "оператор: $operator, группа сокета демона: $operator_group, репозиторий: $repo"

# /etc/miyorios — где демон держит реестр, а брокер его читает; на чистой машине каталога нет
install -d -m 0755 /usr/local/lib/miyorios /etc/miyorios

install -m 0755 "$bin_dir/miyorid" /usr/local/sbin/miyorid
install -m 0755 "$bin_dir/miyori-guid" /usr/local/bin/miyori-guid
install -m 0755 "$bin_dir/miyori-label" /usr/local/bin/miyori-label
install -m 0755 "$bin_dir/miyori-manager" /usr/local/bin/miyori-manager
install -m 0755 "$waypipe_bin" /usr/local/lib/miyorios/waypipe
# киллсвитч ставится копией, а не путём в репозиторий: его юнит стартует до монтирования /home
install -m 0755 components/killswitch/host-killswitch.sh /usr/local/sbin/miyori-host-killswitch

# components/daemon/systemd/miyorid.service и install/tmpfiles/miyorios.conf — шаблоны: имя оператора в git не впишешь
render() {
  sed -e "s/@@MIYORI_USER@@/$operator/g" -e "s/@@MIYORI_GROUP@@/$operator_group/g" \
      -e "s|@@MIYORI_REPO@@|$repo|g" "$1"
}
render components/daemon/systemd/miyorid.service >/etc/systemd/system/miyorid.service.new
install -m 0644 /etc/systemd/system/miyorid.service.new /etc/systemd/system/miyorid.service
rm -f /etc/systemd/system/miyorid.service.new

# сетевая машина демону не принадлежит (решение C) — свои юниты, свой ручной запуск
for src in components/net/systemd/miyori-net-fixture.service \
           components/net/systemd/miyori-net-uplink.service \
           components/net/systemd/miyori-net.service \
           components/killswitch/systemd/miyori-host-killswitch.service \
           components/killswitch/systemd/miyori-host-dns.service; do
  unit="$(basename "$src")"
  render "$src" >"/etc/systemd/system/$unit.new"
  install -m 0644 "/etc/systemd/system/$unit.new" "/etc/systemd/system/$unit"
  rm -f "/etc/systemd/system/$unit.new"
done

# адрес карты и режим аплинка правит оператор — установщик не имеет права затирать его выбор
if [ -f /etc/miyorios/miyori-net.env ]; then
  echo "/etc/miyorios/miyori-net.env уже есть — не трогаю"
else
  install -m 0644 install/defaults/miyori-net.env /etc/miyorios/miyori-net.env
fi

# запрет IPv6 обязан пережить перезагрузку: без файла sysctl роутер вернёт адрес и маршрут
install -m 0644 components/killswitch/sysctl/99-miyori-no-ipv6.conf /etc/sysctl.d/99-miyori-no-ipv6.conf

render install/tmpfiles/miyorios.conf >/etc/tmpfiles.d/miyorios.conf.new
install -m 0644 /etc/tmpfiles.d/miyorios.conf.new /etc/tmpfiles.d/miyorios.conf
rm -f /etc/tmpfiles.d/miyorios.conf.new

install -m 0644 components/guid/systemd/miyori-guid.service /etc/systemd/user/miyori-guid.service
install -m 0644 components/label/systemd/miyori-label.service /etc/systemd/user/miyori-label.service
install -m 0644 components/manager/desktop/miyori-manager.desktop /usr/share/applications/miyori-manager.desktop
# имя файла .desktop обязано совпадать с app_id окна, иначе композитор не свяжет с ним иконку
install -D -m 0644 components/manager/icons/miyori.png /usr/share/icons/hicolor/512x512/apps/miyori.png

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database /usr/share/applications
fi

# /run/miyorios нужен сразу, а не только после следующей перезагрузки
systemd-tmpfiles --create /etc/tmpfiles.d/miyorios.conf

systemctl daemon-reload
systemctl enable miyorid.service
# киллсвитч включаем в автозапуск: слой IPv6 обязан вставать сам, без участия оператора
systemctl enable miyori-host-killswitch.service
# слой DNS цепляется к сетевой машине, а не к загрузке: без неё резолвера нет и включать нечего
systemctl enable miyori-host-dns.service

sudo -u "$operator" env XDG_RUNTIME_DIR="$runtime_dir" systemctl --user daemon-reload
sudo -u "$operator" env XDG_RUNTIME_DIR="$runtime_dir" systemctl --user enable miyori-guid.service miyori-label.service

cat <<EOF
готово: бинари, юниты, tmpfiles и ярлык менеджера поставлены, автозапуск включён.
демон и пользовательские юниты НЕ запущены сейчас — если стенд уже поднят руками,
останавливать его и переключаться на systemd-версию должен оператор осознанно:
  sudo systemctl start miyorid
  systemctl --user start miyori-guid miyori-label
ярлык менеджера — в меню приложений, "MiyoriOS Manager".

юниты сетевой машины поставлены, но НЕ включены и НЕ запущены: отдать карту в VFIO
и остаться без сети хост должен по команде оператора, а не по факту загрузки.
  ${EDITOR:-nano} /etc/miyorios/miyori-net.env          # режим аплинка, адрес карты, имя интерфейса
  sudo systemctl start miyori-net-uplink        # карта уходит хосту в VFIO (только MIYORI_UPLINK=vfio)
  sudo systemctl start miyori-net               # фикстура поднимется сама, консоль VM — в journalctl -fu miyori-net
  sudo systemctl stop miyori-net miyori-net-uplink miyori-net-fixture   # обратный путь
подробности и ловушки — в README
EOF

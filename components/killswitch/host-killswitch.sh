#!/usr/bin/env bash
# Хостовый киллсвитч: гасит IPv6 наружу и не пускает DNS мимо резолвера miyori-net.
# Это не граница доверия к хосту — хост доверенный. Это защита от утечки: сбежавшее
# приложение, вернувшийся по RA маршрут, ожившая локальная резолвилка (документации проекта, инв. 14).
#
# Слой IPv6 включается всегда: он ни от чего не зависит и при загрузке машины обязан
# встать раньше сети. Слой DNS — только если резолвер уже отвечает, иначе правила и
# resolvectl оставили бы хост без имён и без способа это починить. Двигаются они парой.
set -euo pipefail
# без cd в корень репозитория намеренно: юнит зовёт установленную копию до монтирования /home,
# а скрипту из дерева проекта ничего не нужно — только ip, nft, sysctl и resolvectl

link="${MIYORI_HOST_LINK_IFC:-wlp13s0}"
lan="${MIYORI_HOST_LAN:-192.168.1.0/24}"
resolver="${MIYORI_RESOLVER:-10.60.0.1}"
resolved_dropin=/etc/systemd/resolved.conf.d/99-miyori-dns.conf
mode_file=/etc/miyorios/killswitch.mode

usage() {
  cat >&2 <<EOF
использование: host-killswitch.sh on [--strict] | off | status

  on          гасит IPv6 наружу; DNS уводит на $resolver, если тот отвечает
  on --strict дополнительно запрещает любой IPv4 через $link мимо локалки:
              упала miyori-net — хост без интернета. Это и есть смысл киллсвитча
  off         снимает всё, возвращает приём RA и DNS провайдера
  status      что загружено и сколько поймали счётчики

переменные: MIYORI_HOST_LINK_IFC (сейчас $link), MIYORI_HOST_LAN ($lan), MIYORI_RESOLVER ($resolver)
EOF
  exit 1
}

need_root() {
  [ "$(id -u)" -eq 0 ] || { echo "FAIL: нужен root — правим sysctl, nft и resolvectl." >&2; exit 1; }
}

resolver_answers() {
  command -v dig >/dev/null || return 1
  # без маршрута до резолвера ждать нечего: на ранней загрузке br-host ещё нет, и dig
  # сжигал бы пять секунд времени старта сети на каждой загрузке
  ip route get "$resolver" >/dev/null 2>&1 || return 1
  dig +short +time=5 +tries=1 "@$resolver" example.com A 2>/dev/null | grep -qE '^[0-9]+\.'
}

# Таблица собирается целиком под нужный набор слоёв и грузится одним куском: частично
# применённый киллсвитч опаснее отсутствующего — он выглядит включённым
load_nft() {
  local strict="$1" dns="$2" dns_rules="" strict_rules=""
  if [ "$dns" = "1" ]; then
    dns_rules="
		# режем по адресу назначения, а не по интерфейсу: маршрут по умолчанию у хоста идёт
		# через br-host, и правило на \$link запросы мимо резолвера не видело вовсе.
		# Петля разрешена выше — заглушка resolved на 127.0.0.53 работает, как работала
		ip daddr != $resolver udp dport 53 counter drop
		ip daddr != $resolver tcp dport 53 counter drop
		# DoT наружу — та же утечка имён, просто зашифрованная от постороннего, но не от него
		tcp dport 853 counter drop"
  fi
  if [ "$strict" = "1" ]; then
    # DHCP-широковещалка в локалку по маске не попадает и без этой строки умрёт вместе с арендой
    strict_rules="
		oifname \"$link\" ip daddr 255.255.255.255 accept
		oifname \"$link\" ip daddr != $lan counter drop"
  fi
  nft -f - <<EOF
table inet miyori-host {
	chain output {
		type filter hook output priority filter; policy accept;
		oif "lo" accept

		# последний слой против v6: переживает и правку sysctl руками, и очередной RA
		meta nfproto ipv6 counter drop$dns_rules$strict_rules
	}
}
EOF
}

on() {
  need_root
  local strict=0
  [ "${1:-}" = "--strict" ] && strict=1
  # режим переживает перезагрузку: юнит слоя DNS перезагружает таблицу целиком, и без записи
  # запуск miyori-net молча снял бы строгий режим, который включил оператор
  mkdir -p /etc/miyorios
  printf '%s\n' "${1:-}" > "$mode_file"

  sysctl -q -w net.ipv6.conf.all.disable_ipv6=1 net.ipv6.conf.default.disable_ipv6=1 \
               net.ipv6.conf.all.accept_ra=0 net.ipv6.conf.default.accept_ra=0
  sysctl -q -w net.ipv6.conf.lo.disable_ipv6=0
  # адреса гаснут вместе с disable_ipv6, а маршрут по умолчанию остаётся висеть на мёртвом линке.
  # unreachable, а не удаление: connect() падает мгновенно, и браузер уходит на IPv4 сразу,
  # вместо ожидания таймаута на каждой AAAA-записи
  ip -6 route replace unreachable default metric 1 2>/dev/null || true

  # при загрузке резолвер живёт в VM и отвечает позже нас: без ожидания слой DNS молча не встаёт
  local dns=0 waited=0
  while true; do
    if resolver_answers; then dns=1; break; fi
    if [ "$waited" -ge "${MIYORI_RESOLVER_WAIT:-0}" ]; then break; fi
    sleep 3
    waited=$((waited + 3))
  done
  if [ "$dns" = "0" ]; then
    echo "ВНИМАНИЕ: резолвер $resolver молчит — слой DNS не включён, имена уходят мимо него" >&2
    echo "          открытым текстом. Подними miyori-net и повтори эту команду." >&2
  fi

  nft delete table inet miyori-host 2>/dev/null || true
  load_nft "$strict" "$dns" || { echo "FAIL: правила не загрузились, киллсвитча нет." >&2; exit 1; }

  if [ "$dns" = "1" ]; then
    # per-link настройки мало: глобальный DNS из resolved.conf.d перебивает её, а сам $link
    # маршрута по умолчанию не держит. Измерено 2026-09-02: имена уходили на 8.8.8.8 открытым текстом
    mkdir -p /etc/systemd/resolved.conf.d
    cat > "$resolved_dropin" <<EOF
[Resolve]
DNS=$resolver
FallbackDNS=
Domains=~.
EOF
    systemctl restart systemd-resolved 2>/dev/null || true
    if command -v resolvectl >/dev/null; then resolvectl flush-caches 2>/dev/null || true; fi
  elif [ -f "$resolved_dropin" ]; then
    # drop-in от прошлого раза держал бы DNS на молчащем резолвере: это не «утечка открытым
    # текстом», как говорит предупреждение выше, а полное отсутствие имён
    rm -f "$resolved_dropin"
    systemctl try-restart systemd-resolved 2>/dev/null || true
  fi

  echo "IPv6 закрыт"
  [ "$strict" = "1" ] && echo "строгий режим: IPv4 через $link только в $lan"
  [ "$dns" = "1" ] && echo "DNS уведён на $resolver"
  status
}

off() {
  need_root
  nft delete table inet miyori-host 2>/dev/null || true
  rm -f "$resolved_dropin" "$mode_file"
  systemctl restart systemd-resolved 2>/dev/null || true
  sysctl -q -w net.ipv6.conf.all.disable_ipv6=0 net.ipv6.conf.default.disable_ipv6=0 \
               net.ipv6.conf.all.accept_ra=1 net.ipv6.conf.default.accept_ra=1
  ip -6 route del unreachable default metric 1 2>/dev/null || true
  if command -v resolvectl >/dev/null; then
    resolvectl revert "$link" 2>/dev/null || true
    resolvectl flush-caches 2>/dev/null || true
  fi
  echo "киллсвитч снят: IPv6 разрешён, маршрут по умолчанию вернёт роутер очередным RA"
  echo "постоянный /etc/sysctl.d/99-miyori-no-ipv6.conf, если установлен, вернёт запрет после перезагрузки"
}

status() {
  printf '  %-30s %s\n' "IPv6 выключен (all)" "$(sysctl -n net.ipv6.conf.all.disable_ipv6 2>/dev/null)"
  printf '  %-30s %s\n' "глобальные v6-адреса" "$(ip -o -6 addr show scope global 2>/dev/null | wc -l)"
  printf '  %-30s %s\n' "v6-маршрут по умолчанию" "$(ip -6 route show default 2>/dev/null | head -1 || true)"
  if nft list table inet miyori-host &>/dev/null; then
    printf '  %-30s %s\n' "таблица inet miyori-host" "загружена"
    nft list table inet miyori-host 2>/dev/null | grep -E 'counter packets' | sed 's/^/    /'
  else
    printf '  %-30s %s\n' "таблица inet miyori-host" "НЕТ"
  fi
  if command -v resolvectl >/dev/null; then
    printf '  %-30s %s\n' "действующий DNS" \
      "$(resolvectl status 2>/dev/null | awk -F': ' '/Current DNS Server/ { print $2; exit }')"
  fi
}

case "${1:-}" in
  on)     shift; on "$@" ;;
  off)    off ;;
  status) status ;;
  *)      usage ;;
esac

#!/usr/bin/env bash
# VERIFY: V-HOST-NO-IPV6 — с хоста не уходит ни один IPv6-пакет. У br-host адреса v6 нет,
# значит любой исходящий v6 — это обход miyori-net с настоящим адресом оператора.
# Проверяем три слоя порознь: их три именно потому, что каждый в одиночку снимается
# (sysctl — руками, маршрут — очередным RA от роутера), и уцелеть должен хотя бы один.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

[ "$(id -u)" -eq 0 ] || {
  echo "FAIL: нужен root — таблицу nft хоста иначе не прочитать."
  echo "Запусти: sudo bash tests/net/60-host-no-ipv6.sh"
  exit 1; }

fail=0
ok()   { printf '  %-42s %s\n' "$1" "$2"; }
bad()  { printf '  %-42s %s\n' "$1" "$2"; fail=1; }

# 1. глобальные адреса. Link-local (fe80::) оставляем: он не маршрутизируется наружу
globals="$(ip -o -6 addr show scope global 2>/dev/null | awk '{print "      "$2" "$4}')"
if [ -n "$globals" ]; then
  bad "глобальные IPv6-адреса" "ЕСТЬ"
  printf '%s\n' "$globals"
else
  ok "глобальные IPv6-адреса" "нет"
fi

# 2. маршрут по умолчанию. unreachable считается закрытым: connect() падает мгновенно,
# и браузер уходит на IPv4 сразу, а не ждёт таймаута, как было бы при молчаливом drop
v6def="$(ip -6 route show default 2>/dev/null | awk '!/^unreachable/{print "      "$0}')"
if [ -n "$v6def" ]; then
  bad "IPv6-маршрут по умолчанию" "ЕСТЬ"
  printf '%s\n' "$v6def"
else
  ok "IPv6-маршрут по умолчанию" "нет (или unreachable)"
fi

# 3. подстраховка nft — последний слой, переживающий и sysctl, и RA
if nft list table inet miyori-host &>/dev/null; then
  if nft list table inet miyori-host | grep -q 'nfproto ipv6.*drop'; then
    ok "правило drop для IPv6 в inet miyori-host" "загружено"
  else
    bad "правило drop для IPv6 в inet miyori-host" "таблица есть, правила нет"
  fi
else
  bad "таблица inet miyori-host" "не загружена"
fi

# 4. живая проба. Без положительного контроля она ничего не значит: при оборванной
# сети v6 тоже «не уходит», и тест позеленел бы на сломанной машине
probe6="2606:4700:4700::1111"
probe4="1.1.1.1"
if curl -4 -sS -m 15 -o /dev/null "https://$probe4/" 2>/dev/null; then
  ok "контроль: IPv4 до $probe4" "отвечает"
  if curl -6 -sS -m 8 -o /dev/null "https://[$probe6]/" 2>/dev/null; then
    bad "проба: IPv6 до $probe6" "ПРОШЛА — трафик уходит мимо туннеля"
  else
    ok "проба: IPv6 до $probe6" "не прошла"
  fi
else
  bad "контроль: IPv4 до $probe4" "молчит — тест бессмыслен, сеть лежит"
fi

if [ "$fail" -eq 0 ]; then
  echo "VERIFY V-HOST-NO-IPV6: PASS"
else
  echo "VERIFY V-HOST-NO-IPV6: FAIL"
fi
exit "$fail"

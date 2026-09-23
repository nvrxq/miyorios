#!/usr/bin/env bash
# VERIFY: замена 20-guest-no-nic.sh — M1 требовал ноль интерфейсов, M2 даёт спейсу ровно один NIC в miyori-net
set -euo pipefail
out="${1:-build/guest/console-3.txt}"
cid="${2:-3}"

[ -f "$out" ] || { echo "FAIL: нет $out — гость не запускался"; exit 1; }

# консоль гостя приходит с CRLF: без tr любой grep с якорем на $ молча не совпадёт
log="$(tr -d '\r' < "$out")"

section() {
  printf '%s\n' "$log" | sed -n "/---MIYORI-$1-BEGIN---/,/---MIYORI-$1-END---/p" \
    | sed '1d;$d' | grep -v '^\[' || true
}

expected_mac="$(printf '52:54:00:6d:59:%02x' "$cid")"
expected_ip="10.59.0.$cid/24"

nics="$(section NIC)"
[ -n "$nics" ] || { echo "FAIL: секция NIC пуста — проба не снялась"; exit 1; }

count="$(printf '%s\n' "$nics" | grep -c '^[0-9]\+: ' || true)"
[ "$count" = "2" ] || {
  echo "FAIL: интерфейсов $count, ожидались lo и один NIC"; printf '%s\n' "$nics"; exit 1; }

grep -q '^[0-9]\+: lo:' <<<"$nics" || {
  echo "FAIL: среди интерфейсов нет lo"; printf '%s\n' "$nics"; exit 1; }

nic_line="$(printf '%s\n' "$nics" | grep -v '^[0-9]\+: lo:' || true)"
grep -q "link/ether $expected_mac " <<<"$nic_line" || {
  echo "FAIL: MAC интерфейса не совпадает с ожидаемым $expected_mac"; printf '%s\n' "$nic_line"; exit 1; }

addrs="$(section ADDR)"
[ -n "$addrs" ] || { echo "FAIL: секция ADDR пуста — проба не снялась"; exit 1; }

grep -q "inet $expected_ip " < <(grep -v '^[0-9]\+: lo ' <<<"$addrs") || {
  echo "FAIL: у интерфейса нет ожидаемого адреса $expected_ip"; printf '%s\n' "$addrs"; exit 1; }

if grep -q 'inet6' <<<"$addrs"; then
  echo "FAIL: у гостя есть IPv6-адрес"; printf '%s\n' "$addrs"; exit 1
fi

routes="$(section ROUTE)"
[ -n "$routes" ] || { echo "FAIL: секция ROUTE пуста — проба не снялась"; exit 1; }

defaults="$(printf '%s\n' "$routes" | grep -c '^default ' || true)"
[ "$defaults" = "1" ] || {
  echo "FAIL: маршрутов по умолчанию: $defaults, ожидался один"; printf '%s\n' "$routes"; exit 1; }

grep -q '^default via 10\.59\.0\.1 ' <<<"$routes" || {
  echo "FAIL: маршрут по умолчанию не через 10.59.0.1"; printf '%s\n' "$routes"; exit 1; }

echo "PASS: у гостя один NIC ($expected_mac, $expected_ip), маршрут по умолчанию через 10.59.0.1, IPv6 нет"

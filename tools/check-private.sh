#!/bin/sh
# Личное ловится по форме, а не по списку: список личных строк в репозитории сам был бы утечкой.
set -eu
cd "$(git rev-parse --show-toplevel)"

allow=tools/check-private.allow
words=local/private-words
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'rm -rf "$work"; exit 1' INT HUP TERM

# каталоги оператора публикуются только по ошибке (git add -f) — ловим это прямо здесь
leaked=$(git ls-files -- 'local/*' CLAUDE.md CLAUDE.local.md)
if [ -n "$leaked" ]; then
  echo "check-private: локальные файлы попали в индекс:" >&2
  echo "$leaked" >&2
  exit 1
fi

# третья сторона не наша: third_party — чужой опубликованный код
vendor=':!third_party'
: > "$work/hits"

ipv4='\b([0-9]{1,3}\.){3}[0-9]{1,3}\b'
ipv6='\b[23][0-9a-fA-F]{3}:[0-9a-fA-F:]{2,}'
home='/home/[A-Za-z0-9_.-]+'
email='[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'
uuid='[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}'
mac='\b([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}\b'
secret='-----BEGIN [A-Z ]*PRIVATE KEY-----|ssh-(ed25519|rsa|ecdsa)[^ ]* AAAA[0-9A-Za-z+/]+|ghp_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{20,}|xox[abp]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|sk-[A-Za-z0-9]{24,}|(vless|vmess|trojan|ss)://[^ "]+|pbk=[A-Za-z0-9_-]{40,}'

# отказ сканера — это отказ проверки, а не «совпадений не найдено»
check_rc() {
  [ -s "$work/e" ] && cat "$work/e" >&2
  [ "$1" -le 1 ] || { echo "check-private: $2 завершился с кодом $1" >&2; exit 1; }
}

# отбрасывает разрешённые токены и дописывает метку
keep() {
  awk -v tag="$1" -v allow="$allow" '
      BEGIN { n = 0; while ((getline l < allow) > 0) if (l != "" && l !~ /^#/) re[n++] = l }
      { tok = $0; sub(/^[^:]*:[0-9]+:/, "", tok); low = tolower(tok)
        for (i = 0; i < n; i++) if (tok ~ re[i] || low ~ re[i]) next
        print $0 ":" tag }' "$work/g" >> "$work/hits"
}

# --cached — то, что уйдёт в коммит; --untracked — рабочее дерево и ещё не добавленные файлы
scan() { # $1 метка, $2 ERE, $3 --cached|--untracked, $4 -I|-a
  set +e
  git grep "$3" "$4" -n -o -E -e "$2" -- . "$vendor" > "$work/g" 2> "$work/e"
  rc=$?
  set -e
  check_rc "$rc" "git grep ($1)"
  keep "$1"
}

for where in --cached --untracked; do
  for spec in "ipv4 $ipv4" "ipv6 $ipv6" "home $home" "email $email" "uuid $uuid" "mac $mac"; do
    scan "${spec%% *}" "${spec#* }" "$where" -I
  done
  # ключи и токены ищем и в бинарных файлах: NUL перед секретом иначе прятал бы его целиком
  scan secret "$secret" "$where" -a
done

# имена файлов и цели симлинков публикуются вместе с содержимым, а git grep их не читает
git ls-files -z -- . "$vendor" | tr '\0' '\n' > "$work/names"
git ls-files -z --others --exclude-standard -- . "$vendor" | tr '\0' '\n' >> "$work/names"
git ls-files -s -- . "$vendor" | awk '$1 == "120000" { print $2 }' > "$work/shas"
while IFS= read -r sha; do
  [ -n "$sha" ] || continue
  git cat-file blob "$sha"
  printf '\n'
done < "$work/shas" >> "$work/names"

for spec in "ipv4 $ipv4" "ipv6 $ipv6" "home $home" "email $email" "uuid $uuid" "mac $mac" "secret $secret"; do
  set +e
  grep -noE -e "${spec#* }" -- "$work/names" > "$work/g" 2> "$work/e"
  rc=$?
  set -e
  check_rc "$rc" "grep (имена, ${spec%% *})"
  sed -i 's#^#имя-или-симлинк:#' "$work/g"
  keep "${spec%% *}"
done

# слова самого оператора (имена, проекты, хосты) — только у него, файл в .gitignore
if [ -f "$words" ]; then
  for where in --cached --untracked; do
    set +e
    git grep "$where" -a -n -o -w -F -f "$words" -- . "$vendor" > "$work/g" 2> "$work/e"
    rc=$?
    set -e
    check_rc "$rc" "git grep (слова)"
    sed 's/$/:word/' "$work/g" >> "$work/hits"
  done
  set +e
  grep -nowFf "$words" -- "$work/names" > "$work/g" 2> "$work/e"
  rc=$?
  set -e
  check_rc "$rc" "grep (имена, слова)"
  sed 's#^#имя-или-симлинк:#; s/$/:word/' "$work/g" >> "$work/hits"
fi

if [ -s "$work/hits" ]; then
  echo "check-private: личное в дереве (file:line:token:kind):" >&2
  sort -u "$work/hits" >&2
  exit 1
fi
echo "check-private: чисто"

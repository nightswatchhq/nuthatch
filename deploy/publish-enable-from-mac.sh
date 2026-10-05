#!/usr/bin/env bash
# Turns on continuous mirroring (RFC-0052) for Helsinki units, run from a workstation that can
# `ssh root@<host>`. Appends `--publish-target <target>` to each unit's ExecStart and loads the bucket
# credentials from a dedicated drop-in, so they never touch nuthatch.toml or the unit file.
#   deploy/publish-enable-from-mac.sh                       the four default units
#   deploy/publish-enable-from-mac.sh graph-allocations-nest-next
#   deploy/publish-enable-from-mac.sh --check               prechecks only, edits nothing
# Every unit is prechecked before any is edited: its ExecStart file, a binary of at least 4.9.0 (the
# release whose data identity ignores hidden files) that takes --publish-target, /ready on that
# version, the credentials file, and a dry `publish status` against the target with them. Then one
# unit at a time: back up, edit, restart, require /ready on the same version and a publish pass, and
# restore the backup and restart if either fails, stopping there.
# A pass logs nothing on success; it is proved by publish.json at the public address being rewritten
# after the restart. A dead-lettered object, or no fresh publish.json within PUBLISH_WAIT_SECS, fails.
set -euo pipefail
host=${PUBLISH_HOST:-root@89.167.109.4}
target=${PUBLISH_TARGET:-s3://nestsss}
public=${PUBLISH_PUBLIC:-https://pub-bc282d5016f242a783a6c28cbcd4401a.r2.dev}
env_file=${PUBLISH_ENV:-/etc/nuthatch/mirror-r2.env}
wait_secs=${PUBLISH_WAIT_SECS:-1800}
check=0
if [ "${1:-}" = --check ]; then check=1; shift; fi
units=("$@")
if [ ${#units[@]} -eq 0 ]; then
  units=(graph-gns-nest-next nuthatch-dips data-services-nest graph-staking-legacy-readonly)
fi
# Everything below crosses ssh as words of a remote command line, so each is held to a safe alphabet.
for u in "${units[@]}"; do [[ $u =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "not a unit name: $u" >&2; exit 2; }; done
[[ $target =~ ^s3://[a-z0-9][a-z0-9.-]*(/[A-Za-z0-9._-]+)*$ ]] || { echo "not an s3 target: $target" >&2; exit 2; }
[[ $public =~ ^https://[A-Za-z0-9.-]+$ ]] || { echo "not a public base address: $public" >&2; exit 2; }
[[ $env_file =~ ^/[A-Za-z0-9._/-]+$ ]] || { echo "not a path: $env_file" >&2; exit 2; }
[[ $wait_secs =~ ^[0-9]+$ ]] || { echo "PUBLISH_WAIT_SECS is not a number: $wait_secs" >&2; exit 2; }

ssh "$host" bash -s -- "$check" "$target" "$public" "$env_file" "$wait_secs" "${units[@]}" <<'REMOTE'
# Braced and fed /dev/null, so no command in it can read the rest of this script off stdin.
{
set -euo pipefail
check=$1 target=$2 public=$3 env_file=$4 wait_secs=$5; shift 5
min=4.9.0
UNIT_DIR=/etc/systemd/system
dropin_name=50-mirror-publish.conf
stamp=$(date -u +%Y%m%dT%H%M%SZ)

die() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }
ok() { printf 'ok   %s\n' "$*"; }

# The last file setting a non-empty ExecStart wins (#1729), as in scripts/deploy-nest.sh.
execstart_file() {
  local u=$1 last="" f
  for f in "$UNIT_DIR/$u.service" "$UNIT_DIR/$u.service.d"/*.conf; do
    [ -f "$f" ] && grep -qE '^ExecStart=/' "$f" && last=$f
  done
  [ -n "$last" ] && echo "$last"
}
flag_value() {
  local -a w; local i
  read -ra w <<<"$1"
  for i in "${!w[@]}"; do [ "${w[$i]}" = "$2" ] && { echo "${w[$((i + 1))]:-}"; return 0; }; done
  return 0
}
ready_field() {
  echo "$1" | grep -oE "\"$2\"[[:space:]]*:[[:space:]]*(\"[^\"]*\"|[a-z0-9]+)" | head -1 |
    sed -E 's/^[^:]*:[[:space:]]*//; s/"//g' || true
}
await_ready() {
  local port=$1 want=$2 body
  for _ in $(seq 1 150); do
    sleep 2
    body=$(curl -sS -m5 "http://$port/ready" 2>/dev/null || true)
    [ "$(ready_field "$body" version)" = "$want" ] && [ "$(ready_field "$body" ready)" = true ] && return 0
  done
  return 1
}
metric() { curl -sS -m5 "http://$1/metrics" 2>/dev/null | awk -v n="$2" '$1 ~ "^"n"[{ ]" || $1 == n { print $2; exit }'; }
# Runs a publish subcommand with the credentials in a subshell and scrubs their values from its output.
publish_cmd() {
  (
    set -a; . "$env_file"; set +a
    cd /tmp
    out=$(timeout 300 "$@" 2>&1) && rc=0 || rc=$?
    for v in "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"; do [ -z "$v" ] || out=${out//"$v"/<redacted>}; done
    printf '%s\n' "$out"
    exit "$rc"
  )
}
# Epoch seconds of publish.json's Last-Modified at the public address, or nothing when absent.
published_at() {
  local h code lm
  h=$(curl -sSI -m10 "$public/$1/publish.json" 2>/dev/null || true)
  code=$(printf '%s\n' "$h" | awk 'NR == 1 { print $2 }')
  [ "$code" = 200 ] || return 0
  lm=$(printf '%s\n' "$h" | tr -d '\r' | awk -F': ' 'tolower($1) == "last-modified" { print $2; exit }')
  [ -n "$lm" ] || return 0
  date -d "$lm" +%s 2>/dev/null || true
}

declare -A F BIN VER DIR PORT DATASET SKIP
[ -f "$env_file" ] && [ -r "$env_file" ] || die "$env_file is missing or unreadable"
for k in AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_ENDPOINT_URL AWS_REGION; do
  grep -qE "^$k=." "$env_file" || die "$env_file does not set $k"
done
ok "credentials file $env_file sets the four AWS_* variables"

for u in "$@"; do
  systemctl is-active --quiet "$u" || die "$u is not active"
  f=$(execstart_file "$u") || die "$u: no file under $UNIT_DIR sets its ExecStart"
  line=$(grep -E '^ExecStart=/' "$f" | tail -1)
  [[ $line == *\\ ]] && die "$u: the ExecStart in $f continues onto the next line; edit it by hand"
  read -ra w <<<"$line"
  bin=${w[0]#ExecStart=} mode=${w[1]:-}
  [[ $mode == dev || $mode == serve ]] || die "$u: ExecStart runs '$mode', not dev or serve"
  [ -x "$bin" ] || die "$u: $bin is not executable"
  ver=$("$bin" --version 2>/dev/null | awk '{print $2}')
  [ -n "$ver" ] || die "$u: cannot tell what version $bin is"
  [ "$(printf '%s\n%s\n' "$min" "$ver" | sort -V | head -1)" = "$min" ] ||
    die "$u runs $ver; publishing before $min fills the bucket under a data identity the next roll moves"
  "$bin" "$mode" --help 2>/dev/null | grep -q -- '--publish-target' || die "$u: $bin $mode has no --publish-target"
  dir=$(flag_value "$line" --dir) port=$(flag_value "$line" --listen)
  [ -n "$dir" ] && [ -f "$dir/nuthatch.toml" ] || die "$u: no nest at --dir '${dir}'"
  [ ! -e "$dir/mounts.toml" ] || die "$u: $dir is a runtime directory, which publishes from [mounts.publish]"
  [ -n "$port" ] || die "$u: ExecStart has no --listen"
  body=$(curl -sS -m5 "http://$port/ready" 2>/dev/null || true)
  [ "$(ready_field "$body" version)" = "$ver" ] && [ "$(ready_field "$body" ready)" = true ] ||
    die "$u is not ready on $ver before the change: ${body:-no answer}"
  if [[ " $line " == *" --publish-target "* ]]; then
    [ "$(flag_value "$line" --publish-target)" = "$target" ] || die "$u already publishes somewhere else"
    SKIP[$u]=1
  elif [ -e "$UNIT_DIR/$u.service.d/$dropin_name" ]; then
    die "$u has $dropin_name but no --publish-target; inspect it by hand"
  fi
  status=$(publish_cmd "$bin" publish status --dir "$dir" --target "$target") ||
    die "$u: publish status against $target failed: $status"
  dataset=$(printf '%s\n' "$status" | awk '$1 == "dataset" { print $2 }')
  [[ $dataset =~ ^[0-9a-f]{64}$ ]] || die "$u: publish status named no dataset: $status"
  printf '%s\n' "$status" | grep -qE '^last error +none$' || die "$u: publish status reports an error: $status"
  pending=$(printf '%s\n' "$status" | awk '$1 == "pending" { $1 = ""; print }')
  F[$u]=$f BIN[$u]=$bin VER[$u]=$ver DIR[$u]=$dir PORT[$u]=$port DATASET[$u]=$dataset
  ok "$u: $ver, ExecStart in $f, dataset $dataset, pending$pending"
done

[ "$check" = 0 ] || { ok "prechecks passed for every unit; --check edits nothing"; exit 0; }

in_flight="" bak="" dropin=""
revert() {
  local u=$1
  printf '\033[31mrevert\033[0m %s: restoring %s\n' "$u" "${F[$u]}" >&2
  cp -p "$bak" "${F[$u]}"
  rm -f "$dropin"
  systemctl daemon-reload
  systemctl restart "$u"
  if await_ready "${PORT[$u]}" "${VER[$u]}"; then
    echo "revert $u: back on ${VER[$u]} and ready, without publishing" >&2
  else
    echo "revert $u: restored and restarted, but NOT ready on ${VER[$u]}; look at it now" >&2
  fi
}
trap '[ -z "$in_flight" ] || revert "$in_flight"' EXIT
fail() { local u=$in_flight; printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; revert "$u"; in_flight=""; exit 1; }

for u in "$@"; do
  if [ -n "${SKIP[$u]:-}" ]; then ok "$u already publishes to $target; left alone"; continue; fi
  f=${F[$u]} port=${PORT[$u]} ver=${VER[$u]} dataset=${DATASET[$u]}
  bak="$f.bak-publish-$stamp" dropin="$UNIT_DIR/$u.service.d/$dropin_name"
  cp -p "$f" "$bak"
  in_flight=$u

  mapfile -t lines <"$f"
  idx=-1
  for i in "${!lines[@]}"; do [[ ${lines[$i]} == ExecStart=/* ]] && idx=$i; done
  [ "$idx" -ge 0 ] || fail "$u: lost the ExecStart line in $f"
  lines[idx]+=" --publish-target $target"
  tmp=$(mktemp "$f.publish.XXXXXX")
  printf '%s\n' "${lines[@]}" >"$tmp"
  chmod --reference="$bak" "$tmp"
  mv "$tmp" "$f"
  mkdir -p "$UNIT_DIR/$u.service.d"
  printf '[Service]\nEnvironmentFile=%s\n' "$env_file" >"$dropin"
  systemctl daemon-reload
  systemctl show -p ExecStart "$u" | grep -q -- "--publish-target $target" ||
    fail "$u: systemd does not see --publish-target after the edit"

  t0=$(date +%s)
  systemctl restart "$u"
  await_ready "$port" "$ver" || fail "$u is not ready on $ver after restart"
  [ -n "$(metric "$port" nuthatch_publish_errors_total)" ] || fail "$u: /metrics carries no publish series"

  # Last-Modified has one-second resolution, so a pass in the restart's own second still counts.
  passed=""
  while [ $(($(date +%s) - t0)) -lt "$wait_secs" ]; do
    at=$(published_at "$dataset")
    if [ -n "$at" ] && [ "$at" -ge $((t0 - 1)) ]; then passed=1; break; fi
    [ "$(metric "$port" nuthatch_publish_dead_letter)" != 1 ] || fail "$u: the publisher dead-lettered an object"
    [ "$(systemctl is-active "$u")" = active ] || fail "$u stopped while publishing"
    sleep 15
  done
  [ -n "$passed" ] || fail "$u: no publish pass reached $public/$dataset/publish.json within ${wait_secs}s"
  in_flight=""
  ok "$u: published to $target/$dataset in $(($(date +%s) - t0))s; bytes $(metric "$port" nuthatch_publish_bytes_total), sealed_through $(metric "$port" nuthatch_publish_sealed_through), errors $(metric "$port" nuthatch_publish_errors_total); backup $bak"
done
} </dev/null
REMOTE

# shellcheck shell=bash
# Sourced by release-gate.sh: production's budget, how a query set is read, and the canonical
# form answers are compared in.
# The caller defines die.

# Production's budget: the environment the allocations nest runs under on the Lodestar box (unit
# nuthatch-alloc, port 8107), copied from its systemd unit on 2026-10-03. Change it here when the
# unit changes, or the gate tests a budget nobody runs. --env replaces it with another nest's.
# shellcheck disable=SC2034
PROD_ENV=(
  NUTHATCH_SQL_MAX_CONCURRENCY=2
  NUTHATCH_ANALYTICS_MEMORY_LIMIT=256MB
  NUTHATCH_ENGINE=burrmill
  NUTHATCH_BURRMILL_MEMORY_LIMIT=2GB
  NUTHATCH_ANALYTICS_THREADS=8
  NUTHATCH_MAX_RSS=6GB
)

# gate_load_env <file>: PROD_ENV from a nest's NUTHATCH_*=VALUE lines, read from its unit.
gate_load_env() {
  local env_file=$1 line v none=
  [ -f "$env_file" ] || die "no environment file at $env_file"
  PROD_ENV=()
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "# none: the unit sets no NUTHATCH_* settings") none=1 && continue ;;
      '' | '#'*) continue ;;
    esac
    printf '%s\n' "$line" | grep -Eq '^NUTHATCH_[A-Z0-9_]+=[^[:space:]]*$' \
      || die "not a NUTHATCH_*=VALUE line in $env_file: ${line:0:80}"
    PROD_ENV+=("$line")
  done <"$env_file"
  # An empty read is a fault; a unit that sets nothing says so, in the installer's "none" line.
  [ ${#PROD_ENV[@]} -gt 0 ] || [ -n "$none" ] || die "no NUTHATCH_* settings in $env_file"
  # Production's environment is the file's alone: a NUTHATCH_* setting of the caller's own would
  # serve the copy under a budget nobody runs.
  for v in $(compgen -e); do
    case "$v" in NUTHATCH_*) unset "$v" ;; esac
  done
}

if command -v sha256sum >/dev/null; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "neither sha256sum nor shasum is on PATH"
fi

# gate_load_set <file>: the set as parallel arrays indexed by query number (ids, consumers, sqls,
# and modes, how each answer is compared), n their length, volatile the tagged ids.
gate_load_set() {
  local set_file=$1 line v id consumer q seen i
  ids=() consumers=() sqls=() volatile=" "
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      '# volatile: '*)
        v=${line#'# volatile: '}
        v=${v%% *}
        [ -n "$v" ] || die "a volatile tag without an id in $set_file"
        volatile="$volatile$v "
        continue
        ;;
      ''|'#'*) continue ;;
    esac
    id=$(printf '%s' "$line" | cut -f1)
    consumer=$(printf '%s' "$line" | cut -f2)
    q=$(printf '%s' "$line" | cut -f4-)
    [ -n "$id" ] && [ -n "$q" ] || die "malformed line in $set_file (want id<TAB>consumer<TAB>site<TAB>sql): ${line:0:80}"
    for seen in ${ids[@]+"${ids[@]}"}; do
      [ "$seen" != "$id" ] || die "duplicate query id $id in $set_file"
    done
    ids+=("$id"); consumers+=("$consumer"); sqls+=("$q")
  done < "$set_file"
  n=${#ids[@]}
  [ "$n" -gt 0 ] || die "no queries in $set_file"
  # Volatile: held to its row count. Otherwise its order_of.
  modes=()
  i=0
  while [ $i -lt "$n" ]; do
    case "$volatile" in
      *" ${ids[$i]} "*) modes+=(volatile) ;;
      *) modes+=("$(order_of "${sqls[$i]}")") ;;
    esac
    i=$((i + 1))
  done
  for v in $volatile; do
    case " ${ids[*]} " in *" $v "*) ;; *) die "volatile tag for $v, which is not in $set_file" ;; esac
  done
}

# ordered: a top-level ORDER BY, so row order is part of the answer. sorted: none, so rows are
# compared as a set. sorted:unsure: a comment, a stray quote or unbalanced parentheses, so it could
# not tell, and sorts. Parentheses hide a window's or a subquery's ORDER BY; quotes hide literals.
order_of() {
  printf '%s\n' "$1" | awk '
    { s = s $0 " " }
    END {
      n = length(s); depth = 0; top = ""; unsure = 0; i = 1
      while (i <= n) {
        c = substr(s, i, 1)
        if (c == "\047" || c == "\"") {
          j = i + 1; closed = 0
          while (j <= n) {
            if (substr(s, j, 1) == c) {
              if (substr(s, j + 1, 1) == c) { j += 2; continue }
              closed = 1; break
            }
            j++
          }
          if (!closed) unsure = 1
          i = j + 1; top = top " "; continue
        }
        if (c == "-" && substr(s, i + 1, 1) == "-") unsure = 1
        if (c == "/" && substr(s, i + 1, 1) == "*") unsure = 1
        if (c == "(") depth++
        else if (c == ")") { depth--; if (depth < 0) unsure = 1 }
        else if (depth == 0) top = top toupper(c)
        i++
      }
      if (depth != 0) unsure = 1
      gsub(/[ \t\r\n]+/, " ", top)
      if (unsure) print "sorted:unsure"
      else if (top ~ /(^|[^A-Z0-9_])ORDER BY([^A-Z0-9_]|$)/) print "ordered"
      else print "sorted"
    }'
}

# One row per line, keys sorted. A number written as an integer is kept exactly; any other is
# rounded to 12 significant digits, so a float summed in another order still compares equal. So is
# a string in exponent form: a DOUBLE cast to VARCHAR, which no integer or DECIMAL renders as.
CANON_JQ='
def canon_float:
  if . == 0 then 0
  else
    (if . < 0 then -1 else 1 end) as $sign
    | fabs as $a
    | ($a | log10 | floor) as $e
    | (if $e >= 11 then $a / pow(10; $e - 11) else $a * pow(10; 11 - $e) end | round) as $m
    | [$m, $e - 11]
    | until(.[0] % 10 != 0; [.[0] / 10, .[1] + 1])
    | (if .[1] >= 0 then .[0] * pow(10; .[1]) else .[0] / pow(10; -.[1]) end) * $sign
  end;
.rows[] | walk(
  if type == "number" and (tojson | test("^-?[0-9]+$") | not) then canon_float
  elif type == "string" and test("^-?[0-9]+(\\.[0-9]+)?[eE][-+]?[0-9]+$") then tonumber | canon_float | tostring
  else . end)'

# canon_answer <body> <mode> <dest>: writes the canonical rows to dest and prints their sha256.
canon_answer() {
  jq -c -S "$CANON_JQ" "$1" >"$3.tmp" || return 1
  if [ "$2" = ordered ]; then
    mv "$3.tmp" "$3" || return 1
  else
    LC_ALL=C sort "$3.tmp" >"$3" || return 1
    rm -f "$3.tmp"
  fi
  sha256_of "$3"
}

# first_diff <a> <b>: the first row at which two canonical answers differ, as n<TAB>a's<TAB>b's,
# "(no row)" standing in for the shorter side.
first_diff() {
  awk -v A="$1" '
    FILENAME == A { a[FNR] = $0; na = FNR; next }
    { nb = FNR
      if (!done && (FNR > na || a[FNR] != $0)) {
        printf "%d\t%s\t%s\n", FNR, (FNR > na ? "(no row)" : a[FNR]), $0; done = 1
      } }
    END { if (!done) { r = nb + 1; printf "%d\t%s\t%s\n", r, (r > na ? "(no row)" : a[r]), "(no row)" } }' "$1" "$2"
}

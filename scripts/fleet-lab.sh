#!/usr/bin/env bash
# Stand up a throwaway nuthatch verification lab on Hetzner Cloud, run docs/verification.md against it,
# and destroy it. The same path an operator would take: it installs the **published release artifacts**,
# not a local build, so what gets verified is what people actually download.
#
#   export HCLOUD_TOKEN=...            # a Hetzner Cloud project API token (read+write)
#   export HCLOUD_SSH_KEY="my-key"     # the *name* of an SSH key already in that project
#
#   ./scripts/fleet-lab.sh up single    # one box: levels 0-4 + the compose fleet on one host
#   ./scripts/fleet-lab.sh up multi     # three boxes + a private network: adds 5.4/5.5 across machines
#   ./scripts/fleet-lab.sh verify
#   ./scripts/fleet-lab.sh partition    # cut a writer off the control-plane API, assert it keeps indexing
#   ./scripts/fleet-lab.sh skew         # push a writer clock forward, assert the lease does not move
#   ./scripts/fleet-lab.sh down         # destroy everything this script created
#
# ## This spends money
#
# Hetzner bills **hourly**, which is what makes this cheap: a single box is ~€0.01/hr and three are
# ~€0.03/hr, so a day of verification is small change. It is only cheap if you destroy it, so `down`
# is a first-class verb, every resource is tagged, and `up` prints the running cost before creating
# anything.
#
# ## Why cloud-init rather than a pile of ssh commands
#
# The box configures itself from a declarative file you can read before it runs. That is the difference
# between a lab someone else can trust and a sequence of steps they have to take on faith - and this
# script exists partly so GraphOps can read it rather than be told what we did.

set -euo pipefail

API="https://api.hetzner.cloud/v1"
LABEL="nuthatch-lab"                  # every resource carries this; `down` deletes by it
TYPE_SINGLE="${TYPE_SINGLE:-cx33}"    # 4 vCPU / 8 GB - holds Postgres + control + 2 writers + 2 FEs
TYPE_WRITER="${TYPE_WRITER:-cx33}"    # a writer wants headroom above the 2 GB cursor budget
TYPE_SMALL="${TYPE_SMALL:-cx23}"      # 2 vCPU / 4 GB - control plane + Postgres + an FE
# Names checked against the live API: the current shared-vCPU x86 line is cx23/cx33, not cx22/cx32.
# `up` prices from the API rather than from a constant, so a renamed type surfaces as a clear failure
# instead of a wrong number in a comment.
LOCATION="${LOCATION:-hel1}"          # Helsinki, same as the existing production box
IMAGE="${IMAGE:-ubuntu-24.04}"
VERSION="${VERSION:-}"                # release to install; empty = latest

# x86 on purpose. Hetzner's ARM (CAX) boxes are cheaper per GB, and we publish **no ARM Linux binary** -
# a CAX box would mean building from source, where duckdb's unity translation units are 1-2 GB each and
# a 4 GB box OOMs unless you cap parallelism. Not a trap worth walking into for a few euros.

need() { command -v "$1" >/dev/null || { echo "need $1"; exit 1; }; }
need curl; need python3; need ssh

: "${HCLOUD_TOKEN:?set HCLOUD_TOKEN to a Hetzner Cloud API token}"

hc() { # hc METHOD PATH [json]
  local m="$1" p="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -fsS -X "$m" "$API$p" -H "Authorization: Bearer $HCLOUD_TOKEN" \
      -H 'Content-Type: application/json' -d "$body"
  else
    curl -fsS -X "$m" "$API$p" -H "Authorization: Bearer $HCLOUD_TOKEN"
  fi
}
jq_() { python3 -c 'import sys,json;d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d = d[int(k)] if k.isdigit() else d.get(k) if isinstance(d,dict) else d
print(d if d is not None else "")' "$1"; }

price_of() { # rough hourly, from the API so it is never a stale number in a comment
  hc GET "/server_types?name=$1" | python3 -c 'import sys,json
d=json.load(sys.stdin)["server_types"][0]
p=[x for x in d["prices"]][0]
print(f'"'"'{float(p["price_hourly"]["gross"]):.4f}'"'"')'
}

cloud_init() { # cloud_init <role>
  local role="$1"
  cat <<YAML
#cloud-config
package_update: true
packages: [docker.io, docker-compose-v2, git, curl, jq]
runcmd:
  # The published artifact, not a build. If this fails, the release is broken and that is the finding.
  - |
    set -eux
    V="${VERSION}"
    if [ -z "\$V" ]; then
      V=\$(curl -fsSL https://api.github.com/repos/nightswatchhq/nuthatch/releases/latest | jq -r .tag_name)
    fi
    cd /opt
    # Two artifacts per release: the default (embedded) one and a -scaled one carrying worker/control.
    for a in nuthatch nuthatch-scaled; do
      curl -fsSLO "https://github.com/nightswatchhq/nuthatch/releases/download/\$V/\${a}-x86_64-unknown-linux-gnu.tar.gz"
      curl -fsSLO "https://github.com/nightswatchhq/nuthatch/releases/download/\$V/\${a}-x86_64-unknown-linux-gnu.tar.gz.sha256"
      sha256sum -c "\${a}-x86_64-unknown-linux-gnu.tar.gz.sha256"
      tar xzf "\${a}-x86_64-unknown-linux-gnu.tar.gz"
      mv nuthatch "/usr/local/bin/\${a}"
    done
    ln -sf /usr/local/bin/nuthatch /usr/local/bin/nuthatch-embedded
    git clone --depth 1 https://github.com/nightswatchhq/nuthatch /opt/nuthatch-src
    echo "$role" > /opt/nuthatch-role
    touch /opt/lab-ready
YAML
}

create_server() { # create_server <name> <type> <role> [network-id]
  local name="$1" type="$2" role="$3" net="${4:-}"
  local LOCATION="${LOCATION}"
  local ci; ci=$(cloud_init "$role" | python3 -c 'import sys,json;print(json.dumps(sys.stdin.read()))')
  local netpart=""; [ -n "$net" ] && netpart=",\"networks\":[$net]"
  hc POST /servers "{
    \"name\":\"$name\",\"server_type\":\"$type\",\"image\":\"$IMAGE\",\"location\":\"$LOCATION\",
    \"ssh_keys\":[\"${HCLOUD_SSH_KEY:?set HCLOUD_SSH_KEY to an SSH key name in the project}\"],
    \"labels\":{\"$LABEL\":\"true\",\"role\":\"$role\"},
    \"user_data\":$ci$netpart}" | jq_ server.public_net.ipv4.ip
}

ips_by_role() { hc GET "/servers?label_selector=$LABEL" | python3 -c 'import sys,json
for s in json.load(sys.stdin)["servers"]:
    print(s["labels"].get("role","?"), s["public_net"]["ipv4"]["ip"], s["name"])'; }

wait_ready() { # wait_ready <ip>
  echo "  waiting for cloud-init on $1 (installs docker + the release; a few minutes)"
  for _ in $(seq 1 90); do
    if ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 "root@$1" \
         'test -f /opt/lab-ready' 2>/dev/null; then echo "  ready: $1"; return 0; fi
    sleep 10
  done
  echo "  TIMED OUT on $1 - check: ssh root@$1 'cloud-init status; tail -40 /var/log/cloud-init-output.log'"
  return 1
}

cmd_up() {
  local shape="${1:-single}"
  local hourly
  case "$shape" in
    single) hourly=$(price_of "$TYPE_SINGLE") ;;
    multi)  hourly=$(python3 -c "print(f'{2*$(price_of "$TYPE_WRITER")+$(price_of "$TYPE_SMALL"):.4f}')") ;;
    *) echo "shape must be 'single' or 'multi'"; exit 1 ;;
  esac
  cat <<TXT

  shape:    $shape
  location: $LOCATION
  cost:     ~€$hourly/hour  (~€$(python3 -c "print(f'{float($hourly)*24:.2f}')")/day)

  Hetzner bills hourly, so this is only cheap if you destroy it:
      ./scripts/fleet-lab.sh down

TXT
  read -r -p "  create? [y/N] " ok; [ "$ok" = y ] || { echo "  nothing created"; exit 0; }

  if [ "$shape" = single ]; then
    echo "creating one box…"
    # Hetzner returns `resource_unavailable: error during placement` when a region has no capacity for
    # a type - and it can do so *after* accepting the request and handing out an IP, so the server
    # appears to exist and then vanishes. That happened on the first run of this script. Falling back
    # across type and location beats a lab that dies on someone else's capacity planning.
    local ip=""
    for loc in "$LOCATION" fsn1 nbg1 hel1; do
      for t in "$TYPE_SINGLE" "$TYPE_SMALL"; do
        ip=$(LOCATION="$loc" create_server "$LABEL-single" "$t" all 2>/dev/null) || ip=""
        if [ -n "$ip" ]; then echo "  $t in $loc -> $ip"; break 2; fi
        echo "  no capacity: $t in $loc"
      done
    done
    [ -z "$ip" ] && { echo "  no capacity anywhere for these types - try TYPE_SINGLE=cpx31"; exit 1; }
    wait_ready "$ip"
  else
    echo "creating a private network…"
    local net; net=$(hc POST /networks \
      "{\"name\":\"$LABEL-net\",\"ip_range\":\"10.44.0.0/16\",\"subnets\":[{\"type\":\"cloud\",\"network_zone\":\"eu-central\",\"ip_range\":\"10.44.1.0/24\"}],\"labels\":{\"$LABEL\":\"true\"}}" \
      | jq_ network.id)
    echo "  network $net"
    echo "creating three boxes…"
    create_server "$LABEL-cp"      "$TYPE_SMALL"  control "{\"network\":$net}" >/dev/null
    create_server "$LABEL-writer1" "$TYPE_WRITER" writer  "{\"network\":$net}" >/dev/null
    create_server "$LABEL-writer2" "$TYPE_WRITER" writer  "{\"network\":$net}" >/dev/null
    ips_by_role | while read -r _ ip _; do wait_ready "$ip"; done
  fi
  echo; echo "hosts:"; ips_by_role | sed 's/^/  /'
  echo; echo "next: ./scripts/fleet-lab.sh verify"
}

cmd_verify() {
  local target; target=$(ips_by_role | head -1 | awk '{print $2}')
  [ -z "$target" ] && { echo "no lab found - run 'up' first"; exit 1; }
  echo "== levels 0-4 on $target =="
  # `verify.sh` comes from the checkout on the box, so the lab tests the released binary against the
  # runbook at the version that shipped it - not against whatever is on this laptop.
  ssh "root@$target" 'cd /opt/nuthatch-src && NUTHATCH=/usr/local/bin/nuthatch ./scripts/verify.sh 0 2 || true'
  echo
  echo "== the compose fleet, then level 5 =="
  ssh "root@$target" 'set -eux
    cd /opt/nuthatch-src
    mkdir -p nest/abis
    printf "[nest]\nname=\"lab\"\nchain=\"arbitrum-one\"\nchain_id=42161\nrpc_urls=[\"https://arb1.arbitrum.io/rpc\"]\n\n[[contracts]]\nalias=\"usdc\"\naddress=\"0xaf88d065e77c8cC2239327C5EDb3A432268e5831\"\nabi=\"abis/usdc.json\"\n" > nest/nuthatch.toml
    printf "[{\"type\":\"event\",\"name\":\"Transfer\",\"inputs\":[{\"name\":\"from\",\"type\":\"address\",\"indexed\":true},{\"name\":\"to\",\"type\":\"address\",\"indexed\":true},{\"name\":\"value\",\"type\":\"uint256\",\"indexed\":false}],\"anonymous\":false}]" > nest/abis/usdc.json
    chown -R 10001:10001 nest   # the image runs unprivileged; a root-owned mount is unwritable to it
    mkdir -p .img && cp /usr/local/bin/nuthatch-scaled .img/nuthatch && cp Dockerfile .img/
    docker build -q -t nuthatch-scaled:lab .img
    NUTHATCH_IMAGE=nuthatch-scaled:lab docker compose -f docker-compose.scaled.yml --profile fleet up -d --scale writer=2 --scale fe=2
    for i in $(seq 1 30); do curl -fsS -m3 localhost:8290/health >/dev/null 2>&1 && break; sleep 2; done
    docker compose -f docker-compose.scaled.yml --profile fleet ps -a --format "{{.Service}} {{.State}}"
    NUTHATCH=/usr/local/bin/nuthatch-scaled ./scripts/verify.sh 5'
}

# The two things a single host cannot fake. Both free - firewall rules and a clock.
# Both of these used to *print* what to expect and leave the operator to squint at logs. That made
# them unfalsifiable: run it, see no explosion, tick the box - in the one document whose entire job is
# to be falsifiable. They now assert and exit non-zero, so "verified" means a machine checked it.
#
# Writing the assertions surfaced two defects in the tests themselves, which is exactly what an
# unasserted test hides:
#
#   1. `partition` blocked the whole control-plane HOST. In the `multi` shape Postgres runs on that
#      same box, so it cut the writer off from its **hot store** as well - and a writer that cannot
#      reach its store cannot index at all. The stated expectation ("the cursor it holds STILL
#      INDEXING") was therefore impossible to satisfy. It now blocks **only the control-plane API
#      port**, leaving 5432 reachable, which is the invariant we actually care about: the control
#      plane schedules, the store serves, and losing the former must not stop ingestion.
#
#   2. Both drafts polled the writer's `/metrics`. `worker` serves no HTTP at all and publishes no
#      port. Progress is instead read from the **shared Postgres**, which is a better signal anyway:
#      it proves the write landed in the store rather than that a process incremented a counter.
CONTROL_PORT="${CONTROL_PORT:-8290}"

# Run a query on the control box's Postgres. The lab driver reaches it directly, so this keeps working
# while a *writer* is partitioned from it - which is what lets us observe the writer during the cut.
psql_cp() { # psql_cp <control-ip> <sql>
  ssh -o StrictHostKeyChecking=no "root@$1" \
    "docker compose -f docker-compose.scaled.yml exec -T postgres psql -U nuthatch -tAc \"$2\"" \
    2>/dev/null | tr -d '\r'
}

# The nest schema, discovered rather than assumed - `PgStore` creates one schema per nest.
nest_schema() { psql_cp "$1" "select table_schema from information_schema.tables where table_name='meta' limit 1" | head -1; }

meta_value() { # meta_value <control-ip> <schema> <key>
  psql_cp "$1" "select value from \\\"$2\\\".meta where key='$3'" | head -1
}

cmd_partition() {
  local w; w=$(ips_by_role | awk '$1=="writer"{print $2}' | head -1)
  local cp; cp=$(ips_by_role | awk '$1=="control"{print $2}' | head -1)
  [ -z "$w" ] || [ -z "$cp" ] && { echo "needs the 'multi' shape"; exit 1; }
  local sch; sch=$(nest_schema "$cp")
  [ -z "$sch" ] && { echo "FAIL: no nest schema in Postgres - is the fleet indexing? run 'verify' first"; exit 1; }

  local before; before=$(meta_value "$cp" "$sch" last_block)
  [ -z "$before" ] && { echo "FAIL: no last_block in $sch.meta - nothing has indexed yet"; exit 1; }
  echo "last_block=$before; cutting writer $w off the control-plane API ($cp:$CONTROL_PORT) for 90s"
  echo "  (Postgres on :5432 stays reachable - this is a control-plane outage, not a store outage)"

  ssh "root@$w" "iptables -I OUTPUT -d $cp -p tcp --dport $CONTROL_PORT -j DROP"
  # Heal on any exit, including Ctrl-C: a lab box left firewalled is a confusing bill later.
  trap "ssh root@$w 'iptables -D OUTPUT -d $cp -p tcp --dport $CONTROL_PORT -j DROP' 2>/dev/null || true" EXIT
  sleep 90
  local during; during=$(meta_value "$cp" "$sch" last_block)
  ssh "root@$w" "iptables -D OUTPUT -d $cp -p tcp --dport $CONTROL_PORT -j DROP"; trap - EXIT
  echo "healed. last_block during the cut: ${during:-none}"

  if [ -z "$during" ] || [ "$during" -le "$before" ]; then
    echo "FAIL: last_block did not advance during the partition ($before -> ${during:-none})."
    echo "      Losing the control plane must not stop the cursor. That split is the design:"
    echo "      the control plane schedules, the writer indexes."
    exit 1
  fi
  echo "PASS: indexed through the partition ($before -> $during) with the control plane unreachable"

  sleep 30
  local after; after=$(meta_value "$cp" "$sch" last_block)
  if [ -z "$after" ] || [ "$after" -lt "$during" ]; then
    echo "FAIL: did not resume cleanly after healing (during=$during after=${after:-none})"
    exit 1
  fi
  echo "PASS: resumed after healing ($during -> $after)"
}

cmd_skew() {
  local w; w=$(ips_by_role | awk '$1=="writer"{print $2}' | head -1)
  local cp; cp=$(ips_by_role | awk '$1=="control"{print $2}' | head -1)
  [ -z "$w" ] || [ -z "$cp" ] && { echo "needs the 'multi' shape"; exit 1; }
  local sch; sch=$(nest_schema "$cp")
  [ -z "$sch" ] && { echo "FAIL: no nest schema in Postgres - run 'verify' first"; exit 1; }

  # A lease is two rows in the per-nest `meta` table (`lease_owner` / `lease_expires_at`, see
  # `store.rs`); `expires_at` is unix seconds **on the store's clock**, which is the authority this
  # test exists to prove. (An earlier draft queried a `cursor_lease` table that does not exist - it
  # would have returned empty forever and "passed" by never contradicting anything.)
  local owner_before; owner_before=$(meta_value "$cp" "$sch" lease_owner)
  local exp_before;   exp_before=$(meta_value "$cp" "$sch" lease_expires_at)
  [ -z "$owner_before" ] && { echo "FAIL: no lease to observe in $sch.meta"; exit 1; }
  echo "lease before: owner=$owner_before expires_at=$exp_before"

  echo "pushing writer $w's clock 10 minutes forward"
  ssh "root@$w" 'timedatectl set-ntp false && date -s "+10 minutes" && date'
  trap "ssh root@$w 'timedatectl set-ntp true' 2>/dev/null || true" EXIT
  sleep 60
  local owner_after; owner_after=$(meta_value "$cp" "$sch" lease_owner)
  local exp_after;   exp_after=$(meta_value "$cp" "$sch" lease_expires_at)
  ssh "root@$w" 'timedatectl set-ntp true'; trap - EXIT
  echo "lease after:  owner=${owner_after:-none} expires_at=${exp_after:-none}"

  if [ "$owner_before" != "$owner_after" ]; then
    echo "FAIL: the lease changed hands ($owner_before -> ${owner_after:-none}) because a worker's"
    echo "      clock moved. Expiry is evaluated on the database clock; a worker with a wrong clock"
    echo "      must neither gain nor lose a lease it should not have."
    exit 1
  fi
  # A renewing owner pushes expiry forward on the DB clock - by seconds, never by the 600 it skewed.
  local delta=$(( ${exp_after:-0} - ${exp_before:-0} ))
  if [ "$delta" -ge 300 ]; then
    echo "FAIL: expiry jumped ${delta}s - the worker's skewed clock leaked into the lease deadline"
    exit 1
  fi
  echo "PASS: lease held by $owner_after throughout; expiry moved ${delta}s on the DB clock, not 600s"
}

cmd_down() {
  echo "destroying everything labelled $LABEL:"
  ips_by_role | sed 's/^/  /' || true
  read -r -p "  destroy? [y/N] " ok; [ "$ok" = y ] || { echo "  kept"; exit 0; }
  hc GET "/servers?label_selector=$LABEL" | python3 -c 'import sys,json
print("\n".join(str(s["id"]) for s in json.load(sys.stdin)["servers"]))' | while read -r id; do
    [ -n "$id" ] && { hc DELETE "/servers/$id" >/dev/null && echo "  server $id gone"; }
  done
  # Networks refuse deletion while attached, so they go last.
  sleep 5
  hc GET "/networks?label_selector=$LABEL" | python3 -c 'import sys,json
print("\n".join(str(n["id"]) for n in json.load(sys.stdin)["networks"]))' | while read -r id; do
    [ -n "$id" ] && { hc DELETE "/networks/$id" >/dev/null && echo "  network $id gone"; }
  done
  echo "done - billing stops now."
}

case "${1:-}" in
  up)        shift; cmd_up "${1:-single}" ;;
  verify)    cmd_verify ;;
  partition) cmd_partition ;;
  skew)      cmd_skew ;;
  down)      cmd_down ;;
  hosts)     ips_by_role ;;
  *) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//' ;;
esac

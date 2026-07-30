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
#   ./scripts/fleet-lab.sh partition    # cut a writer off the control plane, then heal it
#   ./scripts/fleet-lab.sh skew         # push a writer's clock forward
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
      V=\$(curl -fsSL https://api.github.com/repos/nuthatch-indexer/nuthatch/releases/latest | jq -r .tag_name)
    fi
    cd /opt
    # Two artifacts per release: the default (embedded) one and a -scaled one carrying worker/control.
    for a in nuthatch nuthatch-scaled; do
      curl -fsSLO "https://github.com/nuthatch-indexer/nuthatch/releases/download/\$V/\${a}-x86_64-unknown-linux-gnu.tar.gz"
      curl -fsSLO "https://github.com/nuthatch-indexer/nuthatch/releases/download/\$V/\${a}-x86_64-unknown-linux-gnu.tar.gz.sha256"
      sha256sum -c "\${a}-x86_64-unknown-linux-gnu.tar.gz.sha256"
      tar xzf "\${a}-x86_64-unknown-linux-gnu.tar.gz"
      mv nuthatch "/usr/local/bin/\${a}"
    done
    ln -sf /usr/local/bin/nuthatch /usr/local/bin/nuthatch-embedded
    git clone --depth 1 https://github.com/nuthatch-indexer/nuthatch /opt/nuthatch-src
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
cmd_partition() {
  local w; w=$(ips_by_role | awk '$1=="writer"{print $2}' | head -1)
  local cp; cp=$(ips_by_role | awk '$1=="control"{print $2}' | head -1)
  [ -z "$w" ] || [ -z "$cp" ] && { echo "needs the 'multi' shape"; exit 1; }
  echo "cutting $w off the control plane at $cp for 60s"
  ssh "root@$w" "iptables -I OUTPUT -d $cp -j DROP"
  echo "  expect: reconcile ticks failing, and the cursor it holds STILL INDEXING - a control-plane"
  echo "  outage must stop rescheduling, not ingestion."
  sleep 60
  ssh "root@$w" "iptables -D OUTPUT -d $cp -j DROP"
  echo "healed. expect it to resume reconciling without having lost its lease."
}

cmd_skew() {
  local w; w=$(ips_by_role | awk '$1=="writer"{print $2}' | head -1)
  [ -z "$w" ] && { echo "needs the 'multi' shape"; exit 1; }
  echo "pushing $w's clock 10 minutes forward"
  ssh "root@$w" 'timedatectl set-ntp false && date -s "+10 minutes" && date'
  echo "  expect: no change in lease behaviour. Expiry is measured on the DATABASE's clock, so a"
  echo "  worker with a wrong clock must not gain or lose a lease it should not have."
  echo "  restore with: ssh root@$w 'timedatectl set-ntp true'"
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

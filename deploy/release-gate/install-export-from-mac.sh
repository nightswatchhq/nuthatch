#!/usr/bin/env bash
# Installs the gate-copy export on Helsinki and authorises the ThinkPad's key for it alone (#1774),
# run from a workstation that can `ssh root@<host>` and `ssh thinkpad`. Then checks from the
# ThinkPad that the key is refused anything but the export.
#   deploy/release-gate/install-export-from-mac.sh [root@89.167.109.4] [thinkpad]
set -euo pipefail
host=${1:-root@89.167.109.4}
tp=${2:-thinkpad}
repo=$(cd "$(dirname "$0")/../.." && pwd)
src=$repo/deploy/release-gate/helsinki/gate-export.sh
[ -f "$src" ] || { echo "missing $src; run from an up-to-date checkout" >&2; exit 1; }

ssh "$host" 'cat > /tmp/gate-export.sh' < "$src"
ssh "$host" 'set -e
command -v rsync >/dev/null || { echo "rsync missing on the host" >&2; exit 1; }
install -m 755 /tmp/gate-export.sh /usr/local/bin/nuthatch-gate-export
pid=$(ss -ltnpH "sport = :8107" | grep -o "pid=[0-9]*" | head -n 1 | cut -d= -f2)
[ -n "$pid" ] || { echo "nothing listens on 8107" >&2; exit 1; }
dir=$(tr "\0" "\n" < /proc/$pid/cmdline | sed -n "/^--dir$/{n;p;}")
case "$dir" in /*) ;; "") dir=$(readlink /proc/$pid/cwd) ;; *) dir=$(readlink /proc/$pid/cwd)/$dir ;; esac
test -f "$dir/nuthatch.redb" && test -f "$dir/segments/manifest.json" || { echo "$dir is not the allocations nest" >&2; exit 1; }
mkdir -p /etc/nuthatch
printf "NEST_DIR=%s\nNEST_URL=http://127.0.0.1:8107\n" "$dir" > /etc/nuthatch/gate-export.env
echo "NEST_DIR=$dir"
SSH_ORIGINAL_COMMAND=snapshot nuthatch-gate-export
du -sh /var/lib/nuthatch-gate/stage'

ssh "$tp" 'test -f ~/.ssh/nuthatch-gate || ssh-keygen -q -t ed25519 -N "" -C nuthatch-gate -f ~/.ssh/nuthatch-gate; cat ~/.ssh/nuthatch-gate.pub' \
  | ssh "$host" 'set -e; read -r k
case "$k" in "ssh-ed25519 "*) ;; *) echo "not an ed25519 key" >&2; exit 1 ;; esac
grep -qF "$k" /root/.ssh/authorized_keys || printf "from=\"100.83.44.63\",command=\"/usr/local/bin/nuthatch-gate-export\",restrict %s\n" "$k" >> /root/.ssh/authorized_keys
echo "authorised: $(grep -c nuthatch-gate /root/.ssh/authorized_keys) key line(s)"'

echo "checking the key from the ThinkPad: it must be refused, not given a shell"
out=$(ssh "$tp" 'ssh -i ~/.ssh/nuthatch-gate -o BatchMode=yes -o IdentitiesOnly=yes root@100.82.188.91 hello 2>&1; echo "exit $?"')
echo "$out"
case "$out" in
  *"refused: hello"*"exit 1"*) echo "ok: the key reaches the export command and nothing else" ;;
  *) echo "STOP: expected the export to refuse 'hello'. If Tailscale SSH is on for Helsinki, authorized_keys is bypassed." >&2; exit 1 ;;
esac

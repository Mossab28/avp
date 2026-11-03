#!/bin/bash
# Proofs in a sandbox (AVP_SANDBOX=docker): a worker runs each job in a container of its own. A job
# cannot read another job's folder nor the worker's, cannot reach the cloud's metadata, is not root
# and cannot write outside its folder and /tmp; the next job finds nothing the last one left, file,
# process or cache; its service runs with the job's restrictions; a job past its disk or its time is
# stopped alone; and once the jobs are over no container or network of theirs is left.
# Linux with docker only (a worker), skipped elsewhere; run it as a user of the docker group, not
# root. On an AWS worker, the firewall's rules (worker-up.sh) must be set, or the worker refuses;
# AVP_TEST_FIREWALL=1 says they are, and checks that SMTP is closed too. AVP_SANDBOX_RUNTIME=runsc
# runs the jobs under gVisor and checks that they do.
# Usage: AVP=target/proof/avp bash tests/sandbox.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/sandbox.sh <scratch dir>}/sb.XXXX") && cd "$W" || exit 1
if [ "$(uname)" != Linux ] || ! docker version >/dev/null 2>&1; then
  echo "PASS skipped: the sandbox is tested on Linux with a docker daemon"
  exit 0
fi
if [ "$(id -u)" = 0 ]; then
  echo "FAIL run it as a user of the docker group, not root: steps run as the worker's own user"
  exit 1
fi
IMG=${AVP_SANDBOX_IMAGE:-debian:bookworm-slim}
export AVP_SERVER=http://127.0.0.1:7601 AVP_PERSON=ada
# The server's proofs are strangers' code (AVP_SANDBOX): a sandboxed worker runs them, or none does.
AVP_SANDBOX=docker AVP_WORKERS=1 AVP_WORKER_CLAIM_S=300 AVP_WORKER_RUN_S=90 AVP_SPECULATE=0 $T serve --listen 127.0.0.1:7601 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
WP=
trap 'kill $SP $WP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
owned(){ docker "$@" -q --filter "label=avp.sandbox=$W/worker"; }

# Another organization's job folder on the same worker, what a job must never see.
mkdir -p "$W/worker/work/other-org/slot-0" && echo private > "$W/worker/work/other-org/slot-0/secret.txt"
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
# job.sh is what each task makes of the last step: nothing, or an attack.
cat > avp.toml <<EOF
[check]
image = "$IMG"
[[check.services]]
name = "cache"
image = "redis:7-alpine"
ports = [6379]
[[check.steps]]
name = "alone"
run = "test -f app.txt && test ! -e '$W/worker/work/other-org' && test ! -e '$W/worker/objects' && test ! -e '$W/data'"
[[check.steps]]
name = "no metadata"
run = "! timeout 5 bash -c 'exec 3<>/dev/tcp/169.254.169.254/80'"
[[check.steps]]
name = "fresh"
run = "test ! -e /tmp/left && touch /tmp/left && (sleep 900 >/dev/null 2>&1 &) && test \"\$(id -u)\" != 0 && ! touch /etc/avp 2>/dev/null && test ! -e \"\$HOME/poison\" && touch \"\$HOME/poison\""
[[check.steps]]
name = "service"
run = "bash -c 'exec 3<>/dev/tcp/127.0.0.1/6379 && printf \"PING\\\\r\\\\n\" >&3 && read -t 5 r <&3 && [[ \$r == +PONG* ]]'"
[[check.steps]]
name = "job"
run = "if [ -f job.sh ]; then bash job.sh; fi"
EOF
$T init sb >/dev/null
cd "$W" || exit 1

AVP_SANDBOX=docker AVP_SANDBOX_IMAGE=$IMG AVP_SANDBOX_MEMORY=1g AVP_SANDBOX_PIDS=256 AVP_SANDBOX_DISK=200m $T worker --data "$W/worker" >"$W/worker.log" 2>&1 & WP=$!
for _ in $(seq 240); do grep -q "claiming proofs" worker.log && break; kill -0 $WP 2>/dev/null || break; sleep 0.5; done
ok "the worker starts sandboxed, its network checked" 'grep -q "in a docker sandbox" worker.log'
for i in 1 2; do
  $T open -C v --repo sb --goal "a change proven in a sandbox ($i)" >/dev/null && printf 'v%s\n' "$i" > v/app.txt
  $T -C v --json land >"$W/land$i.json" 2>&1
done
ok "a job sees its own folder, never another job's, the worker's or the server's" 'grep -q "\"position\": 1" land1.json'
ok "the next job finds no file the last one left in /tmp or in its caches, and is not root" 'grep -q "\"position\": 2" land2.json'
ok "the cloud's metadata is out of reach" '! grep -q "no metadata" land1.json land2.json'
ok "a service runs with the job's restrictions and answers in its network" 'grep -q "\"position\": 1" land1.json && ! grep -q "service cache" land1.json'

# Attacks, each its own task, each red alone: the disk, then the time.
attack(){ $T open -C v --repo sb --goal "$1" >/dev/null && printf '%s\n' "$2" > v/job.sh && $T -C v --json land >"$W/$3.json" 2>&1; $T -C v abandon >/dev/null 2>&1; rm -f v/job.sh; }
attack "fills its disk" 'head -c 400M /dev/zero > big; sleep 60' disk
ok "a job past its disk is stopped and says so" 'grep -q "disk limit" disk.json'
ok "a job that filled its folder leaves no byte of it behind" '[ -z "$(find "$W/worker/work" -name big)" ]'
attack "never ends" 'sleep 600' time
ok "a job past its time is killed and says so" 'grep -q "time limit" time.json'
if [ "${AVP_TEST_FIREWALL:-}" = 1 ]; then
  attack "sends mail" '! timeout 5 bash -c "exec 3<>/dev/tcp/smtp.gmail.com/587"' smtp
  ok "SMTP is closed: a worker sends no mail" 'grep -q "\"position\"" smtp.json'
fi
if [ "${AVP_SANDBOX_RUNTIME:-}" = runsc ]; then
  attack "reads its kernel" 'dmesg 2>/dev/null | grep -qi gvisor' kernel
  ok "under gVisor, a job's kernel is its own" 'grep -q "\"position\"" kernel.json'
fi
ok "no container, network or process of the jobs or their services is left" '[ -z "$(owned ps -a)" ] && [ -z "$(owned network ls)" ] && ! pgrep -f "sleep 900" && ! pgrep -f "sleep 600"'
grep -q '"position": 2' land2.json || tail -20 worker.log land1.json land2.json

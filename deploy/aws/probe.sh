#!/usr/bin/env bash
# Phase 0, on AWS: one Graviton instance with Nitro Enclaves, the enclave image on it, the
# egress proxy beside it. Everything the probe creates is tagged tokumai-probe, and `down`
# removes all of it again — an instance left running costs about €4 a day.
#
# The probe writes a line per tunnel (TOKUMAI_EGRESS_LOG=lines in the unit below): it is a
# development machine, and that log is how a fault is found. A production host does NOT set
# it — there the proxy counts by the hour and keeps no per-call timing, because we hold the
# payment records and a log of when each call went out is the other half of a join.
#
# The words are EC2's own, so that what the script does and what the console shows are the
# same thing: launch · start · stop · terminate. A stopped instance keeps its disk (and the
# enclave's book on it) and costs storage only; a terminated one is gone, disk and all.
#
#     deploy/aws/probe.sh launch    # key pair, security group (SSH from this machine's IP only), instance
#     deploy/aws/probe.sh deploy    # copy the EIF + the egress proxy, start both (production mode)
#     deploy/aws/probe.sh debug     # the same, but the enclave in debug mode: console visible,
#                                   # PCRs all zeros — the app refuses it, by design
#     deploy/aws/probe.sh status    # the instance, the enclave, the proxy's recent lines
#     deploy/aws/probe.sh forget    # back to the simulated enclave on this machine
#     deploy/aws/probe.sh ssh
#     deploy/aws/probe.sh stop      # keeps the disk, so the enclave's book is there tomorrow
#     deploy/aws/probe.sh start     # and back again (new address; then `deploy`)
#     deploy/aws/probe.sh terminate # instance, disk, key pair, security group — all gone,
#                                   # the book with them (asks first)
#
# Uses the CLI profile `tokumai` (the IAM user tokumai-probe), region eu-central-1.
set -euo pipefail
cd "$(dirname "$0")/../.."
export AWS_PROFILE=${AWS_PROFILE:-tokumai} AWS_REGION=eu-central-1 AWS_PAGER=""
NAME=tokumai-probe
TYPE=${TYPE:-c7g.xlarge}
TAG=${TAG:-probe-1}
KEY=~/.ssh/$NAME.pem
# The enclave's share of the instance: 2 of 4 vCPUs, 3 GiB of 8.
ENCLAVE_CPUS=2
ENCLAVE_MIB=3072

# running · stopped · pending · stopping, or empty when there is no probe instance. A
# stopped instance has no address, so nothing may try to reach it.
instance_state() {
  aws ec2 describe-instances --filters "Name=tag:Name,Values=$NAME" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].State.Name' --output text
}

instance_id() {
  aws ec2 describe-instances --filters "Name=tag:Name,Values=$NAME" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text
}
public_ip() { aws ec2 describe-instances --instance-ids "$(instance_id)" --query 'Reservations[].Instances[].PublicIpAddress' --output text; }
remote() { ssh -i "$KEY" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 ec2-user@"$(public_ip)" "$@"; }

case "${1:-}" in
  launch)
    [ -n "$(instance_id)" ] && { echo "already up: $(instance_id)"; exit 0; }
    if [ ! -f "$KEY" ]; then
      aws ec2 create-key-pair --key-name $NAME --query KeyMaterial --output text > "$KEY"; chmod 600 "$KEY"
    fi
    MYIP=$(curl -s https://checkip.amazonaws.com)
    SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null)
    if [ "$SG" = "None" ] || [ -z "$SG" ]; then
      SG=$(aws ec2 create-security-group --group-name $NAME --description "tokumai phase-0 probe: SSH from the operator only" --query GroupId --output text)
    fi
    aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MYIP/32" >/dev/null 2>&1 || true
    AMI=$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 --query Parameter.Value --output text)
    USERDATA=$(cat <<UD
#!/bin/bash
set -e
dnf install -y aws-nitro-enclaves-cli
sed -i "s/^memory_mib:.*/memory_mib: $ENCLAVE_MIB/; s/^cpu_count:.*/cpu_count: $ENCLAVE_CPUS/" /etc/nitro_enclaves/allocator.yaml
systemctl enable --now nitro-enclaves-allocator.service
usermod -aG ne ec2-user
touch /var/tmp/tokumai-host-ready
UD
)
    aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name $NAME --security-group-ids "$SG" \
      --iam-instance-profile Name=tokumai-enclave-host --enclave-options Enabled=true \
      --metadata-options HttpTokens=required \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME}]" \
      --user-data "$USERDATA" --query 'Instances[0].InstanceId' --output text
    aws ec2 wait instance-running --instance-ids "$(instance_id)"
    echo "running at $(public_ip); the host prepares itself (nitro-cli, allocator) — 'status' says when"
    ;;
  deploy|debug)
    remote 'test -f /var/tmp/tokumai-host-ready' || { echo "the host is not ready yet (user data still running)"; exit 1; }
    # Stop first: a running proxy binary cannot be overwritten ("text file busy").
    remote "sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true; sudo systemctl stop tokumai-egress 2>/dev/null || true; pkill -f '[t]okumai-egress-host' || true; sleep 1" || true
    # The sealed secrets travel with it: the host cannot read them, and without them the
    # enclave would run on the mock model (deploy/aws/kms.sh secrets writes the file).
    SEALED=dev-data/sealed/sealed.json
    [ -f "$SEALED" ] || { echo "no $SEALED — 'deploy/aws/kms.sh secrets <secrets.json>' first"; exit 1; }
    scp -i "$KEY" -q dev-data/eif/tokumai-$TAG.eif dev-data/linux-release/tokumai-egress-host deploy/egress.allow "$SEALED" ec2-user@"$(public_ip)":/home/ec2-user/
    DEBUG=""; [ "$1" = debug ] && DEBUG="--debug-mode"
    # The proxy runs as a service, not as a background job of an ssh session: started with
    # nohup it died with the session, and an enclave whose host service is gone cannot read
    # its book — so it panicked seconds after starting, which read as "the image is broken".
    remote "set -e
      chmod +x tokumai-egress-host
      sudo tee /etc/systemd/system/tokumai-egress.service >/dev/null <<'UNIT'
[Unit]
Description=tokumai egress proxy and host service
After=network-online.target

[Service]
User=ec2-user
WorkingDirectory=/home/ec2-user
Environment=TOKUMAI_EGRESS_LOG=lines
ExecStart=/home/ec2-user/tokumai-egress-host vsock:4294967295:8080 egress.allow vsock:4294967295:8081 vsock:4294967295:8082 sealed.json book
Restart=always
RestartSec=2
StandardOutput=append:/home/ec2-user/egress.log
StandardError=append:/home/ec2-user/egress.log

[Install]
WantedBy=multi-user.target
UNIT
      : > egress.log
      sudo systemctl daemon-reload
      sudo systemctl enable --now tokumai-egress
      sudo systemctl restart tokumai-egress
      sleep 2
      systemctl is-active tokumai-egress
      nitro-cli run-enclave --eif-path tokumai-$TAG.eif --cpu-count $ENCLAVE_CPUS --memory $ENCLAVE_MIB $DEBUG"
    # Its address, once it is on the mixnet, and the image it runs: dev-data/probe.json is
    # what the app and the dev tools read, so nobody has to remember two environment
    # variables (and talk to the wrong enclave when they forget one).
    echo "waiting for the enclave to come onto the mixnet…"
    # Every door it announces, in the order it announces them (the image's gateway order).
    for _ in $(seq 1 40); do
      ADDRESSES=$(remote "grep -o 'nym-address .*' egress.log | cut -d' ' -f2 | awk '!seen[\$0]++'" 2>/dev/null | tr -d '\r')
      [ -n "$ADDRESSES" ] && sleep 10 && ADDRESSES=$(remote "grep -o 'nym-address .*' egress.log | cut -d' ' -f2 | awk '!seen[\$0]++'" 2>/dev/null | tr -d '\r') && break
      sleep 5
    done
    if [ -z "$ADDRESSES" ]; then echo "it has not announced itself yet — 'status' shows the proxy log"; exit 1; fi
    ADDRESS=$(echo "$ADDRESSES" | head -1)
    PCR0=$(python3 -c "import json,sys; print(json.load(open('dev-data/eif/tokumai-$TAG.pcrs.json'))['Measurements']['PCR0'])")
    python3 - "$ADDRESSES" "$PCR0" <<'PY'
import json, sys, pathlib
doors = [a for a in sys.argv[1].split() if a]
pathlib.Path("dev-data/probe.json").write_text(json.dumps({"addresses": doors, "address": doors[0], "pcr0": sys.argv[2]}, indent=2) + "\n")
PY
    echo "on the mixnet, $(echo "$ADDRESSES" | wc -l | tr -d ' ') door(s):"
    echo "$ADDRESSES" | sed 's/^/  /' 
    remote "grep -E 'unsealed' egress.log | tail -1; ls -l book 2>/dev/null | tail -2" || true
    echo "dev-data/probe.json written — the app and the dev tools now talk to this enclave"
    ;;
  status)
    ID=$(instance_id); [ -z "$ID" ] && { echo "no probe instance — 'launch' makes one"; exit 0; }
    STATE=$(instance_state)
    if [ "$STATE" != "running" ]; then
      echo "instance $ID is $STATE (no address; the disk and the enclave's book are kept)"
      [ "$STATE" = "stopped" ] && echo "'start' brings it back, then 'deploy' puts the enclave on the mixnet"
      exit 0
    fi
    echo "instance $ID at $(public_ip)"
    remote 'test -f /var/tmp/tokumai-host-ready && echo "host ready" || echo "host still preparing"; systemctl is-active tokumai-egress 2>/dev/null | sed "s/^/egress service: /"; nitro-cli describe-enclaves 2>/dev/null | grep -E "EnclaveID|State|Flags" || echo "no enclave running"; tail -n 20 egress.log 2>/dev/null || true'
    ;;
  ssh)
    [ "$(instance_state)" = "running" ] || { echo "the instance is $(instance_state) — 'start' first"; exit 1; }
    exec ssh -i "$KEY" ec2-user@"$(public_ip)"
    ;;
  forget)
    rm -f dev-data/probe.json
    echo "dev-data/probe.json removed — the app talks to the simulated enclave again"
    ;;
  stop)
    ID=$(instance_id); [ -z "$ID" ] && { echo "no probe instance"; exit 0; }
    if [ "$(instance_state)" != "running" ]; then
      echo "instance $ID is already $(instance_state)"
      exit 0
    fi
    # Stopping keeps the root volume, and with it the sealed book the enclave writes to
    # (`down` does not: a fresh instance has a fresh disk and the balances are gone).
    remote "sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true; sudo systemctl stop tokumai-egress 2>/dev/null || true" || true
    aws ec2 stop-instances --instance-ids "$ID" >/dev/null
    aws ec2 wait instance-stopped --instance-ids "$ID"
    echo "stopped — the disk and the enclave's book are kept; 'resume' brings it back"
    ;;
  start)
    ID=$(instance_id); [ -z "$ID" ] && { echo "no probe instance — 'launch' makes one"; exit 1; }
    if [ "$(instance_state)" = "running" ]; then
      echo "instance $ID is already running at $(public_ip)"
      exit 0
    fi
    aws ec2 start-instances --instance-ids "$ID" >/dev/null
    aws ec2 wait instance-running --instance-ids "$ID"
    # A stopped instance comes back with a new address, and SSH is allowed from this
    # machine's address only — both are settled here.
    SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text)
    MY_IP=$(curl -s https://checkip.amazonaws.com | tr -d '[:space:]')
    aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MY_IP/32" >/dev/null 2>&1 || true
    echo "running at $(public_ip) — 'deploy' puts the enclave back on the mixnet (the book is still there)"
    ;;
  terminate)
    # This destroys the root volume, and the enclave's book with it. Say so, and make the
    # person say the word: a probe holding real balances looks exactly like one that does not.
    if [ "${FORCE:-}" != "1" ]; then
      echo "terminate deletes the instance AND its disk — the enclave's book (balances, plans) goes with it."
      echo "to keep it, use 'stop'. type 'terminate' to go ahead:"
      read -r answer
      [ "$answer" = "terminate" ] || { echo "left alone"; exit 1; }
    fi
    ID=$(instance_id)
    if [ -n "$ID" ]; then aws ec2 terminate-instances --instance-ids "$ID" >/dev/null; aws ec2 wait instance-terminated --instance-ids "$ID"; fi
    SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
    [ -n "$SG" ] && [ "$SG" != "None" ] && aws ec2 delete-security-group --group-id "$SG" || true
    aws ec2 delete-key-pair --key-name $NAME >/dev/null 2>&1 || true; rm -f "$KEY" dev-data/probe.json
    echo "everything of the probe is gone"
    ;;
  up | down | pause | resume)
    # The old names, one of which quietly destroyed a disk.
    echo "the words are EC2's own now: launch · start · stop · terminate (see the top of this script)"
    exit 2
    ;;
  *) sed -n 2,20p "$0"; exit 2 ;;
esac

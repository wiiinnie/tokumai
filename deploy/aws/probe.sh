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
#     deploy/aws/probe.sh host      # the host side only: proxy binary, allow list, the two units —
#                                   # the enclave image stays (TAG names the one running)
#     deploy/aws/probe.sh volume    # the book on a disk of its own (survives the instance; see below)
#     deploy/aws/probe.sh backups   # a snapshot of that disk every day, kept 14 days
#     deploy/aws/probe.sh alarm <email>      # mail when the enclave's pulse stops (CloudWatch + SNS)
#     deploy/aws/probe.sh accept-rewind <g> <n>  # acknowledge a restore from backup (see below)
#     deploy/aws/probe.sh terminate # instance, root disk, key pair, security group — gone;
#                                   # the book's own volume is kept (asks first)
#
# The enclave runs under systemd (tokumai-enclave.service, 2026-10-02): when it exits — the
# watchdog's exit 70, a panic, a reboot — the host starts it again by itself. Until then a
# dead enclave stayed dead until a person ran `deploy`.
#
# The book lives on a volume of its own, `tokumai-book`, mounted at /home/ec2-user/book:
# a volume attached after launch is NOT deleted with the instance, so `terminate` and a
# replaced instance keep every balance and plan (before 2026-10-02 it lay on the root
# volume, and a terminated probe took a paid plan with it on 2026-09-23). `launch` looks
# for that volume and starts the new instance beside it.
#
# The host reports the age of the enclave's last pulse line to CloudWatch every minute
# (tokumai-pulse.timer); `alarm` makes the alarm that mails when it passes five minutes or
# the number stops arriving, which is a host that is down.
#
# The book's witness (crates/enclave/src/witness.rs) is the bucket named in the image; the
# enclave refuses a book older than the witness's last mark. A restore from a backup is
# older by definition and must be acknowledged with `accept-rewind <generation> <record>` —
# the numbers are in the enclave's refusal in the host log. Only the operator's identity
# may write that acknowledgement (bucket policy), never the host's role.
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
# The book's own volume: small, encrypted, gp3. Found by its Name tag.
BOOK_VOLUME=tokumai-book
BOOK_GIB=4
BOOK_MOUNT=/home/ec2-user/book
# The witness bucket (also in deploy/enclave/Dockerfile — the image names it, the host
# cannot change it) and the alarm's topic.
WITNESS_BUCKET=tokumai-book-$(aws sts get-caller-identity --query Account --output text 2>/dev/null || echo 946944821363)
ALARM_TOPIC=tokumai-alarms

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
book_volume_id() {
  aws ec2 describe-volumes --filters "Name=tag:Name,Values=$BOOK_VOLUME" "Name=status,Values=available,in-use" \
    --query 'Volumes[0].VolumeId' --output text 2>/dev/null | grep -v None || true
}
book_volume_az() { aws ec2 describe-volumes --volume-ids "$1" --query 'Volumes[0].AvailabilityZone' --output text; }
instance_az() { aws ec2 describe-instances --instance-ids "$(instance_id)" --query 'Reservations[].Instances[].Placement.AvailabilityZone' --output text; }

# The two units on the host, and the script the enclave unit runs. Written at every deploy,
# so a change here reaches the host with the next one.
install_units() {
  remote "set -e
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
    # The enclave: started by this unit, watched by it, started again when it is gone.
    # StartLimitIntervalSec=0: an enclave that dies at once (an image the key policy does
    # not know yet) keeps being tried every RestartSec rather than being given up on.
    # Wants, not Requires: a proxy that crashes and comes back must not take the enclave
    # down with it — the enclave's book writer waits the proxy out (ledger::writer).
    sudo tee /etc/systemd/system/tokumai-enclave.service >/dev/null <<'UNIT'
[Unit]
Description=tokumai enclave (nitro-cli run-enclave, kept running)
After=tokumai-egress.service nitro-enclaves-allocator.service
Wants=tokumai-egress.service nitro-enclaves-allocator.service
StartLimitIntervalSec=0

[Service]
User=ec2-user
WorkingDirectory=/home/ec2-user
ExecStart=/home/ec2-user/tokumai-enclave-run.sh
ExecStop=/bin/sh -c 'nitro-cli terminate-enclave --all >/dev/null 2>&1 || true'
Restart=always
RestartSec=10
StandardOutput=append:/home/ec2-user/egress.log
StandardError=append:/home/ec2-user/egress.log

[Install]
WantedBy=multi-user.target
UNIT
    cat > tokumai-enclave-run.sh <<'RUN'
#!/bin/bash
# Runs the enclave named in enclave.conf and stays alive as long as it does. When the
# enclave is gone — exit 70 from its watchdog, a panic, a host reboot — this ends, and
# systemd (tokumai-enclave.service) runs it again.
set -u
cd /home/ec2-user
. ./enclave.conf
nitro-cli terminate-enclave --all >/dev/null 2>&1 || true
for _ in \$(seq 1 60); do systemctl is-active --quiet tokumai-egress && break; sleep 1; done
echo \"enclave-run: starting \$EIF (\$CPUS vCPU, \$MIB MiB\${FLAGS:+, \$FLAGS})\"
nitro-cli run-enclave --eif-path \"\$EIF\" --cpu-count \"\$CPUS\" --memory \"\$MIB\" \$FLAGS || { echo \"enclave-run: run-enclave failed\"; sleep 5; exit 1; }
while nitro-cli describe-enclaves 2>/dev/null | grep -q '\"State\": \"RUNNING\"'; do sleep 5; done
echo \"enclave-run: the enclave is gone — it will be started again\"
exit 1
RUN
    chmod +x tokumai-enclave-run.sh
    # The pulse: a stamp file touched whenever the enclave's pulse line appears, and once a
    # minute its age sent to CloudWatch as tokumai/PulseAge (the alarm watches that).
    cat > tokumai-pulse-watch.sh <<'WATCH'
#!/bin/bash
cd /home/ec2-user
tail -n0 -F egress.log 2>/dev/null | grep --line-buffered \"pulse runtime\" | while read -r _; do touch pulse.stamp; done
WATCH
    cat > tokumai-pulse-metric.sh <<'METRIC'
#!/bin/bash
cd /home/ec2-user
now=\$(date +%s)
if [ -f pulse.stamp ]; then age=\$(( now - \$(stat -c %Y pulse.stamp) )); else age=9999; fi
aws cloudwatch put-metric-data --region eu-central-1 --namespace tokumai --metric-name PulseAge --unit Seconds --value \"\$age\" 2>>pulse-metric.err || true
METRIC
    chmod +x tokumai-pulse-watch.sh tokumai-pulse-metric.sh
    sudo tee /etc/systemd/system/tokumai-pulse-watch.service >/dev/null <<'UNIT'
[Unit]
Description=tokumai: stamp the enclave's pulse
After=tokumai-egress.service

[Service]
User=ec2-user
WorkingDirectory=/home/ec2-user
ExecStart=/home/ec2-user/tokumai-pulse-watch.sh
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
    sudo tee /etc/systemd/system/tokumai-pulse.service >/dev/null <<'UNIT'
[Unit]
Description=tokumai: report the pulse's age to CloudWatch

[Service]
Type=oneshot
User=ec2-user
WorkingDirectory=/home/ec2-user
ExecStart=/home/ec2-user/tokumai-pulse-metric.sh
UNIT
    sudo tee /etc/systemd/system/tokumai-pulse.timer >/dev/null <<'UNIT'
[Unit]
Description=tokumai: the pulse's age, every minute

[Timer]
OnBootSec=1min
OnUnitActiveSec=1min
AccuracySec=5s

[Install]
WantedBy=timers.target
UNIT
    sudo systemctl daemon-reload
    sudo systemctl enable tokumai-egress tokumai-enclave tokumai-pulse-watch tokumai-pulse.timer >/dev/null 2>&1
    sudo systemctl restart tokumai-pulse-watch tokumai-pulse.timer"
}

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
    # A book volume from an earlier instance: the new one must start in its zone.
    PLACEMENT=()
    VOL=$(book_volume_id)
    if [ -n "$VOL" ]; then PLACEMENT=(--placement "AvailabilityZone=$(book_volume_az "$VOL")"); echo "the book volume $VOL exists — launching beside it"; fi
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
      --metadata-options HttpTokens=required "${PLACEMENT[@]}" \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME}]" \
      --user-data "$USERDATA" --query 'Instances[0].InstanceId' --output text
    aws ec2 wait instance-running --instance-ids "$(instance_id)"
    echo "running at $(public_ip); the host prepares itself (nitro-cli, allocator) — 'status' says when"
    [ -n "$VOL" ] && echo "then 'volume' attaches the book volume, and 'deploy' starts the enclave on it"
    ;;
  deploy|debug)
    remote 'test -f /var/tmp/tokumai-host-ready' || { echo "the host is not ready yet (user data still running)"; exit 1; }
    # Stop first: a running proxy binary cannot be overwritten ("text file busy").
    remote "sudo systemctl stop tokumai-enclave 2>/dev/null || true; sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true; sudo systemctl stop tokumai-egress 2>/dev/null || true; pkill -f '[t]okumai-egress-host' || true; sleep 1" || true
    # The sealed secrets travel with it: the host cannot read them, and without them the
    # enclave would run on the mock model (deploy/aws/kms.sh secrets writes the file).
    SEALED=dev-data/sealed/sealed.json
    [ -f "$SEALED" ] || { echo "no $SEALED — 'deploy/aws/kms.sh secrets <secrets.json>' first"; exit 1; }
    scp -i "$KEY" -q dev-data/eif/tokumai-$TAG.eif dev-data/linux-release/tokumai-egress-host deploy/egress.allow "$SEALED" ec2-user@"$(public_ip)":/home/ec2-user/
    DEBUG=""; [ "$1" = debug ] && DEBUG="--debug-mode"
    # Both run as services, not as background jobs of an ssh session: started with nohup
    # the proxy died with the session, and an enclave whose host service is gone cannot
    # read its book — so it panicked seconds after starting, which read as "the image is
    # broken". The enclave's unit keeps it running (see the top of this script).
    install_units
    if ! remote "findmnt -n $BOOK_MOUNT >/dev/null"; then
      echo "NOTE: the book is on the root volume — 'volume' puts it on a disk of its own"
    fi
    remote "set -e
      chmod +x tokumai-egress-host
      printf 'EIF=/home/ec2-user/tokumai-$TAG.eif\nCPUS=$ENCLAVE_CPUS\nMIB=$ENCLAVE_MIB\nFLAGS=\"$DEBUG\"\n' > enclave.conf
      # The log is turned over, not emptied: what the last enclave said before it was
      # replaced is exactly what a fault report needs. The five most recent are kept.
      [ -s egress.log ] && mv egress.log egress.log.\$(date -u +%Y%m%dT%H%M%SZ) || true
      ls -t egress.log.* 2>/dev/null | tail -n +6 | xargs -r rm -f
      : > egress.log
      sudo systemctl restart tokumai-egress
      sleep 2
      systemctl is-active tokumai-egress
      sudo systemctl restart tokumai-enclave
      sleep 3
      systemctl is-active tokumai-enclave"
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
  host)
    # Everything of `deploy` except the image: for a change on the host side (the proxy,
    # the units, the allow list) while the running image must stay what the apps pin.
    # The enclave is restarted all the same, under its unit from now on.
    [ "$(instance_state)" = "running" ] || { echo "the instance is $(instance_state) — 'start' first"; exit 1; }
    remote "test -f /home/ec2-user/tokumai-$TAG.eif" || { echo "no tokumai-$TAG.eif on the host — TAG must name the image that is deployed"; exit 1; }
    remote "sudo systemctl stop tokumai-enclave 2>/dev/null || true; sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true; sudo systemctl stop tokumai-egress 2>/dev/null || true; pkill -f '[t]okumai-egress-host' || true; sleep 1" || true
    scp -i "$KEY" -q dev-data/linux-release/tokumai-egress-host deploy/egress.allow ec2-user@"$(public_ip)":/home/ec2-user/
    install_units
    remote "set -e
      chmod +x tokumai-egress-host
      printf 'EIF=/home/ec2-user/tokumai-$TAG.eif\nCPUS=$ENCLAVE_CPUS\nMIB=$ENCLAVE_MIB\nFLAGS=\"\"\n' > enclave.conf
      sudo systemctl restart tokumai-egress
      sleep 2
      systemctl is-active tokumai-egress
      sudo systemctl restart tokumai-enclave
      sleep 3
      systemctl is-active tokumai-enclave"
    echo "host updated; the enclave (tokumai-$TAG) is coming back under its unit — 'status' shows its doors"
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
    remote 'test -f /var/tmp/tokumai-host-ready && echo "host ready" || echo "host still preparing"; systemctl is-active tokumai-egress 2>/dev/null | sed "s/^/egress service: /"; systemctl is-active tokumai-enclave 2>/dev/null | sed "s/^/enclave service: /"; findmnt -n -o SOURCE,SIZE,USED /home/ec2-user/book 2>/dev/null | sed "s/^/book volume: /" || echo "book: on the root volume"; nitro-cli describe-enclaves 2>/dev/null | grep -E "EnclaveID|State|Flags" || echo "no enclave running"; tail -n 20 egress.log 2>/dev/null || true'
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
    remote "sudo systemctl stop tokumai-enclave 2>/dev/null || true; sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true; sudo systemctl stop tokumai-egress 2>/dev/null || true" || true
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
  volume)
    # The book on a volume of its own. Idempotent: creates the volume if there is none,
    # attaches it if it is loose, and on the host moves the book onto it once. Both
    # services are stopped for the move — a minute or two with no enclave.
    ID=$(instance_id); [ -z "$ID" ] && { echo "no probe instance — 'launch' first"; exit 1; }
    [ "$(instance_state)" = "running" ] || { echo "the instance is $(instance_state) — 'start' first"; exit 1; }
    VOL=$(book_volume_id)
    if [ -z "$VOL" ]; then
      VOL=$(aws ec2 create-volume --availability-zone "$(instance_az)" --size $BOOK_GIB --volume-type gp3 --encrypted \
        --tag-specifications "ResourceType=volume,Tags=[{Key=Name,Value=$BOOK_VOLUME}]" --query VolumeId --output text)
      aws ec2 wait volume-available --volume-ids "$VOL"
      echo "created $VOL ($BOOK_GIB GiB, encrypted) in $(instance_az)"
    fi
    ATTACHED=$(aws ec2 describe-volumes --volume-ids "$VOL" --query 'Volumes[0].Attachments[0].InstanceId' --output text)
    if [ "$ATTACHED" != "$ID" ]; then
      [ "$ATTACHED" != "None" ] && [ -n "$ATTACHED" ] && { echo "$VOL is attached to $ATTACHED, not this instance — detach it there first"; exit 1; }
      [ "$(book_volume_az "$VOL")" = "$(instance_az)" ] || { echo "$VOL is in $(book_volume_az "$VOL"), the instance in $(instance_az) — 'launch' puts a new instance beside the volume"; exit 1; }
      aws ec2 attach-volume --volume-id "$VOL" --instance-id "$ID" --device /dev/sdf >/dev/null
      aws ec2 wait volume-in-use --volume-ids "$VOL"
      echo "attached $VOL"
    fi
    # On the host: the device by its volume id (Nitro names it nvme*, in no fixed order).
    DEV="/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_$(echo "$VOL" | tr -d '-')"
    remote "set -e
      for _ in \$(seq 1 30); do [ -e $DEV ] && break; sleep 1; done
      [ -e $DEV ] || { echo 'the volume has not appeared on the host'; exit 1; }
      if findmnt -n $BOOK_MOUNT >/dev/null; then echo 'the book is already on its own volume'; exit 0; fi
      if ! sudo blkid $DEV >/dev/null 2>&1; then sudo mkfs.ext4 -q -L tokumai-book $DEV; echo 'made a filesystem on it'; fi
      sudo systemctl stop tokumai-enclave 2>/dev/null || true
      sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true
      sudo systemctl stop tokumai-egress 2>/dev/null || true
      sudo mkdir -p /mnt/tokumai-book && sudo mount $DEV /mnt/tokumai-book
      if [ -d $BOOK_MOUNT ] && [ -n \"\$(ls -A $BOOK_MOUNT 2>/dev/null)\" ]; then
        sudo cp -a $BOOK_MOUNT/. /mnt/tokumai-book/ && echo \"moved the book (\$(ls $BOOK_MOUNT | wc -l) file(s)) onto the volume\"
        sudo mv $BOOK_MOUNT $BOOK_MOUNT.on-root-volume.\$(date +%s)
      fi
      sudo umount /mnt/tokumai-book
      sudo mkdir -p $BOOK_MOUNT
      UUID=\$(sudo blkid -s UUID -o value $DEV)
      grep -q \"\$UUID\" /etc/fstab || echo \"UUID=\$UUID $BOOK_MOUNT ext4 defaults,nofail 0 2\" | sudo tee -a /etc/fstab >/dev/null
      sudo mount $BOOK_MOUNT && sudo chown ec2-user:ec2-user $BOOK_MOUNT
      sudo systemctl start tokumai-egress 2>/dev/null || true
      sudo systemctl start tokumai-enclave 2>/dev/null || true
      findmnt -n -o SOURCE,SIZE,USED $BOOK_MOUNT"
    echo "the book is on $VOL, mounted at $BOOK_MOUNT; it is not deleted with the instance"
    ;;
  backups)
    # A snapshot of the book volume every day at 03:00 UTC, the last 14 kept — by Data
    # Lifecycle Manager, so no credential on the host can touch it. The snapshots are
    # sealed bytes like the volume; they restore a book, they do not open one.
    # The role DLM runs as. The probe's IAM user may neither read nor make roles, so it is
    # made once by an administrator (console: IAM → Roles → Create role → "Data Lifecycle
    # Manager", name AWSDataLifecycleManagerDefaultRole) and named here by its ARN.
    ROLE=AWSDataLifecycleManagerDefaultRole
    ARN="arn:aws:iam::$(aws sts get-caller-identity --query Account --output text):role/$ROLE"
    EXISTING=$(aws dlm get-lifecycle-policies --query "Policies[?Description=='$BOOK_VOLUME daily'].PolicyId" --output text)
    if [ -n "$EXISTING" ] && [ "$EXISTING" != "None" ]; then echo "backups are on: policy $EXISTING"; exit 0; fi
    aws dlm create-lifecycle-policy --description "$BOOK_VOLUME daily" --state ENABLED --execution-role-arn "$ARN" \
      --policy-details "{\"PolicyType\":\"EBS_SNAPSHOT_MANAGEMENT\",\"ResourceTypes\":[\"VOLUME\"],\"TargetTags\":[{\"Key\":\"Name\",\"Value\":\"$BOOK_VOLUME\"}],\"Schedules\":[{\"Name\":\"daily\",\"CreateRule\":{\"Interval\":24,\"IntervalUnit\":\"HOURS\",\"Times\":[\"03:00\"]},\"RetainRule\":{\"Count\":14},\"CopyTags\":true}]}" \
      --query PolicyId --output text | sed 's/^/backups are on: policy /'
    ;;
  alarm)
    # The topic, the address, the alarm. The address gets one mail from AWS asking to
    # confirm the subscription; nothing arrives before that link is clicked.
    EMAIL=${2:?usage: probe.sh alarm <email>}
    TOPIC=$(aws sns create-topic --name $ALARM_TOPIC --query TopicArn --output text)
    if ! aws sns list-subscriptions-by-topic --topic-arn "$TOPIC" --query "Subscriptions[?Endpoint=='$EMAIL'].SubscriptionArn" --output text | grep -q .; then
      aws sns subscribe --topic-arn "$TOPIC" --protocol email --notification-endpoint "$EMAIL" >/dev/null
      echo "a confirmation mail is on its way to $EMAIL — the alarm reaches it once the link in it is clicked"
    fi
    aws cloudwatch put-metric-alarm --alarm-name tokumai-enclave-silent \
      --alarm-description "The tokumai enclave has not written a pulse line for five minutes, or the host is not reporting at all." \
      --namespace tokumai --metric-name PulseAge --statistic Maximum --period 60 --evaluation-periods 5 --datapoints-to-alarm 5 \
      --threshold 300 --comparison-operator GreaterThanThreshold --treat-missing-data breaching \
      --alarm-actions "$TOPIC" --ok-actions "$TOPIC"
    echo "alarm tokumai-enclave-silent is set: PulseAge > 300 s for 5 minutes, or no data, mails $EMAIL (and again when it recovers)"
    ;;
  accept-rewind)
    # The operator says: this older book is the one to run (a restore from backup). The
    # enclave's refusal in the host log names the generation and record to acknowledge.
    G=${2:?usage: probe.sh accept-rewind <generation> <record>}; N=${3:?usage: probe.sh accept-rewind <generation> <record>}
    aws s3api put-object --bucket "$WITNESS_BUCKET" --key "accept/$G-$N" --body /dev/null >/dev/null
    echo "acknowledged generation $G record $N in $WITNESS_BUCKET — restart the enclave (systemctl restart tokumai-enclave on the host, or 'host'); the acknowledgement counts once"
    ;;
  terminate)
    # This destroys the root volume. The book's own volume (if `volume` made one) is kept:
    # it was attached after launch, which EC2 does not delete with the instance. Say so,
    # and make the person say the word.
    if [ "${FORCE:-}" != "1" ]; then
      if [ -n "$(book_volume_id)" ]; then
        echo "terminate deletes the instance and its root disk; the book volume $(book_volume_id) is kept and can be attached to the next instance."
      else
        echo "terminate deletes the instance AND its disk — the enclave's book (balances, plans) goes with it."
      fi
      echo "to keep everything, use 'stop'. type 'terminate' to go ahead:"
      read -r answer
      [ "$answer" = "terminate" ] || { echo "left alone"; exit 1; }
    fi
    ID=$(instance_id)
    if [ -n "$ID" ]; then
      remote "sudo systemctl stop tokumai-enclave tokumai-egress 2>/dev/null; sudo umount $BOOK_MOUNT 2>/dev/null" || true
      aws ec2 terminate-instances --instance-ids "$ID" >/dev/null; aws ec2 wait instance-terminated --instance-ids "$ID"
    fi
    SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
    [ -n "$SG" ] && [ "$SG" != "None" ] && aws ec2 delete-security-group --group-id "$SG" || true
    aws ec2 delete-key-pair --key-name $NAME >/dev/null 2>&1 || true; rm -f "$KEY" dev-data/probe.json
    if [ -n "$(book_volume_id)" ]; then echo "the instance is gone; the book volume $(book_volume_id) is kept"; else echo "everything of the probe is gone"; fi
    ;;
  up | down | pause | resume)
    # The old names, one of which quietly destroyed a disk.
    echo "the words are EC2's own now: launch · start · stop · terminate (see the top of this script)"
    exit 2
    ;;
  *) sed -n 2,20p "$0"; exit 2 ;;
esac

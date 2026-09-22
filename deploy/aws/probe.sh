#!/usr/bin/env bash
# Phase 0, on AWS: one Graviton instance with Nitro Enclaves, the enclave image on it, the
# egress proxy beside it. Everything the probe creates is tagged tokumai-probe, and `down`
# removes all of it again — an instance left running costs about €4 a day.
#
#     deploy/aws/probe.sh up        # key pair, security group (SSH from this machine's IP only), instance
#     deploy/aws/probe.sh deploy    # copy the EIF + the egress proxy, start both (production mode)
#     deploy/aws/probe.sh debug     # the same, but the enclave in debug mode: console visible,
#                                   # PCRs all zeros — the app refuses it, by design
#     deploy/aws/probe.sh status    # the instance, the enclave, the proxy's recent lines
#     deploy/aws/probe.sh ssh
#     deploy/aws/probe.sh down      # terminate and delete everything
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

instance_id() {
  aws ec2 describe-instances --filters "Name=tag:Name,Values=$NAME" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text
}
public_ip() { aws ec2 describe-instances --instance-ids "$(instance_id)" --query 'Reservations[].Instances[].PublicIpAddress' --output text; }
remote() { ssh -i "$KEY" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 ec2-user@"$(public_ip)" "$@"; }

case "${1:-}" in
  up)
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
    scp -i "$KEY" -q dev-data/eif/tokumai-$TAG.eif dev-data/linux-release/tokumai-egress-host deploy/egress.allow ec2-user@"$(public_ip)":/home/ec2-user/
    DEBUG=""; [ "$1" = debug ] && DEBUG="--debug-mode"
    remote "set -e
      sudo nitro-cli terminate-enclave --all >/dev/null 2>&1 || true
      pkill -f tokumai-egress-host || true
      chmod +x tokumai-egress-host
      nohup ./tokumai-egress-host vsock:4294967295:8080 egress.allow vsock:4294967295:8081 > egress.log 2>&1 &
      sleep 1
      nitro-cli run-enclave --eif-path tokumai-$TAG.eif --cpu-count $ENCLAVE_CPUS --memory $ENCLAVE_MIB $DEBUG"
    echo "started. The enclave announces its Nym address in the proxy log: deploy/aws/probe.sh status"
    ;;
  status)
    ID=$(instance_id); [ -z "$ID" ] && { echo "no probe instance"; exit 0; }
    echo "instance $ID at $(public_ip)"
    remote 'test -f /var/tmp/tokumai-host-ready && echo "host ready" || echo "host still preparing"; nitro-cli describe-enclaves 2>/dev/null | grep -E "EnclaveID|State|Flags" || true; tail -n 20 egress.log 2>/dev/null || true'
    ;;
  ssh) exec ssh -i "$KEY" ec2-user@"$(public_ip)" ;;
  down)
    ID=$(instance_id)
    if [ -n "$ID" ]; then aws ec2 terminate-instances --instance-ids "$ID" >/dev/null; aws ec2 wait instance-terminated --instance-ids "$ID"; fi
    SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
    [ -n "$SG" ] && [ "$SG" != "None" ] && aws ec2 delete-security-group --group-id "$SG" || true
    aws ec2 delete-key-pair --key-name $NAME >/dev/null 2>&1 || true; rm -f "$KEY"
    echo "everything of the probe is gone"
    ;;
  *) sed -n 2,16p "$0"; exit 2 ;;
esac

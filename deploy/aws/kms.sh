#!/usr/bin/env bash
# The key that guards the enclave's secrets, and the sealing of those secrets.
#
#   deploy/aws/kms.sh create              # the key (alias tokumai-enclave) with its policy
#   deploy/aws/kms.sh allow <pcr0>        # let an image open the book (a new image = a new PCR0)
#                                         # — run as the KEY ADMIN (see below), not the probe user
#   deploy/aws/kms.sh seal <file>         # seal a file for the enclave → <file>.sealed
#   deploy/aws/kms.sh secrets <secrets.json> [out]   # the whole sealed file the host keeps
#   deploy/aws/kms.sh watch               # mail the alarm topic whenever the policy is changed
#   deploy/aws/kms.sh policy              # what the policy says right now
#
# The policy is the whole point: `kms:Decrypt` and `kms:GenerateDataKey` are allowed ONLY
# for a request that carries a Nitro attestation of an image on the allow list — not for
# us, not for anyone with the host's root. What the operator keeps is `kms:Encrypt`, to
# seal new provider secrets, and the key's housekeeping.
#
# Two kinds of key travel through this one, and the difference matters:
#   - the operator's secrets (provider keys, Stripe): sealed from here, known to us anyway;
#   - the enclave's DATA KEY, which names every account and every payment in the book:
#     never from here. The enclave has KMS make it to its own attestation on its first
#     start (GenerateDataKey with a recipient) and the host keeps the wrapped copy beside
#     the book. `secrets` refuses a dataKey (and the doors' Nym identities, which the
#     enclave makes itself too). Until 2026-10-05 the operator typed the data key into the
#     secrets file, which made the attestation gate a formality against the one party it
#     exists for.
#
# Honest about the limit: whoever can change the policy could allow an image of their own
# and open the book. So that right is not the everyday user's. `kms:PutKeyPolicy` is held
# by one named principal, the KEY ADMIN ($KEY_ADMIN, an IAM user with MFA enforced), only
# with MFA, and every change mails the alarm topic (`watch`). The everyday user
# (tokumai-probe) deploys, seals and reads the policy — and cannot widen it. The way to
# close it further is signed images (PCR8) instead of a list of PCR0s, still on the list.
set -euo pipefail
cd "$(dirname "$0")/../.."
export AWS_PROFILE=${AWS_PROFILE:-tokumai} AWS_REGION=eu-central-1 AWS_PAGER=""
ALIAS=alias/tokumai-enclave
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
ROLE=arn:aws:iam::$ACCOUNT:role/tokumai-enclave-host
# The one principal that may change who opens the book (docs/aws-admin.md, 5). Given as
# `user/…` or `role/…`; the account is filled in here.
KEY_ADMIN=arn:aws:iam::$ACCOUNT:${KEY_ADMIN:-user/tokumai-key-admin}
ALARM_TOPIC=arn:aws:sns:$AWS_REGION:$ACCOUNT:tokumai-alarms

# Some KMS calls take an alias, others insist on the key id — resolve it once.
key_id() { aws kms describe-key --key-id $ALIAS --query KeyMetadata.KeyId --output text 2>/dev/null || true; }

policy_json() {  # $@ = the PCR0s that may open the book
  python3 - "$ACCOUNT" "$ROLE" "$KEY_ADMIN" "$@" <<'PY'
import json, sys
account, role, key_admin, *pcrs = sys.argv[1:]
# The enclave: open what is sealed, and have its data key made — to an attestation of an
# allowed image only. GenerateDataKey is how the data key is born inside it (2026-10-05).
enclave = {
    "Sid": "OnlyAnAttestedTokumaiEnclaveMayDecrypt",
    "Effect": "Allow",
    "Principal": {"AWS": role},
    "Action": ["kms:Decrypt", "kms:GenerateDataKey", "kms:GenerateRandom"],
    "Resource": "*",
    "Condition": {"StringEqualsIgnoreCase": {"kms:RecipientAttestation:PCR0": pcrs}},
} if pcrs else None
statements = [
    # Everybody in the account (through IAM): look, seal, keep the alias — never open, and
    # never change who may. PutKeyPolicy is deliberately NOT here: an IAM policy cannot
    # grant what the key policy does not delegate to the account.
    {
        "Sid": "TheOperatorManagesTheKeyAndMaySealSecretsButNotOpenThem",
        "Effect": "Allow",
        "Principal": {"AWS": f"arn:aws:iam::{account}:root"},
        "Action": ["kms:Describe*", "kms:Get*", "kms:List*", "kms:CancelKeyDeletion",
                   "kms:TagResource", "kms:UntagResource", "kms:Encrypt",
                   "kms:CreateAlias", "kms:UpdateAlias", "kms:DeleteAlias"],
        "Resource": "*",
    },
    # One named principal changes who may open the book, and only with MFA. Named here
    # directly, so no IAM policy is needed — and none can hand the right to anyone else.
    {
        "Sid": "OnlyTheKeyAdminWithMfaChangesWhoMayOpenTheBook",
        "Effect": "Allow",
        "Principal": {"AWS": key_admin},
        "Action": ["kms:PutKeyPolicy", "kms:ScheduleKeyDeletion"],
        "Resource": "*",
        "Condition": {"Bool": {"aws:MultiFactorAuthPresent": "true"}},
    },
]
if enclave:
    statements.append(enclave)
print(json.dumps({"Version": "2012-10-17", "Id": "tokumai-enclave", "Statement": statements}, indent=2))
PY
}

case "${1:-}" in
  create)
    if [ -n "$(key_id)" ]; then echo "the key already exists: $(key_id)"; exit 0; fi
    KEY=$(aws kms create-key --description "tokumai enclave: opened only by an attested enclave" \
      --key-usage ENCRYPT_DECRYPT --key-spec SYMMETRIC_DEFAULT \
      --tags TagKey=Name,TagValue=tokumai-enclave --policy "$(policy_json)" --query KeyMetadata.KeyId --output text)
    aws kms create-alias --alias-name $ALIAS --target-key-id "$KEY"
    echo "$KEY ($ALIAS) — nothing may decrypt yet; 'allow <pcr0>' says which image may"
    ;;
  allow)
    # Several are accepted (policy_json always could) because a sandbox build is a second,
    # differently measured image of the same code. Naming more than one IS a widening,
    # though: every image listed here can open the book. Prefer swapping to one and back.
    #
    # Writes the whole policy, so it is also how the policy's SHAPE changes: the first
    # `allow` after 2026-10-05 (run as whoever still may — the probe user, once) moves
    # PutKeyPolicy to the key admin; every `allow` after that is the key admin's, with
    # MFA: AWS_PROFILE=tokumai-key-admin and an MFA session (docs/aws-admin.md, 5).
    : "${2:?usage: kms.sh allow <pcr0> [<pcr0> …]}"
    shift
    # One image opens the book. More than one is every one of them opening it — a sandbox
    # image beside the production one is a mint for anyone with a free sandbox account
    # (audit C1). An upgrade in flight, where the apps still pin the old image, is the one
    # reason to name two, and it says so: FORCE=1.
    if [ $# -gt 1 ] && [ "${FORCE:-}" != 1 ]; then
      echo "$# images would open the book, and every one of them could. One at a time; FORCE=1 for an upgrade in flight (old and new image, both production), and swap back to one when the apps have moved."
      exit 1
    fi
    WHO=$(aws sts get-caller-identity --query Arn --output text)
    case "$WHO" in *"${KEY_ADMIN#arn:aws:iam::$ACCOUNT:}"*) ;; *) echo "note: running as $WHO, not the key admin ($KEY_ADMIN) — allowed only until the policy is in its new shape" ;; esac
    # KMS refuses a policy that takes PutKeyPolicy away from the caller, unless told that
    # this is meant. It is: the probe user hands the right to the key admin, and the key
    # admin keeps it (named directly, with MFA). The check is for the one case that still
    # locks the key: a key admin that cannot sign in — so that principal and its MFA device
    # must exist before this runs (docs/aws-admin.md, 5).
    aws kms put-key-policy --key-id "$(key_id)" --policy-name default --bypass-policy-lockout-safety-check --policy "$(policy_json "$@")"
    echo "$# image(s) may now open the book — and nothing else:"
    for p in "$@"; do echo "  $p"; done
    echo "who may change this: $KEY_ADMIN, with MFA"
    ;;
  seal)
    FILE=${2:?usage: kms.sh seal <file>}
    aws kms encrypt --key-id $ALIAS --plaintext "fileb://$FILE" --query CiphertextBlob --output text | base64 --decode > "$FILE.sealed"
    echo "$FILE.sealed — open only inside an allowed enclave"
    ;;
  secrets)
    # What the host is given: the operator's secrets under a fresh key of their own, and
    # that key sealed by KMS — which hands it back to an attested enclave and to nobody
    # else. (KMS encrypts at most 4 KiB; the secrets can be larger.)
    #
    # NOT the data key: tokumai-seal refuses a file that carries one. The enclave has it
    # made on its first start and keeps it on the host beside the book (data.key.kms). The
    # doors' identities (nymIdentities) are the enclave's own too (doors.sealed), but an
    # enclave with none on the host reads the operator-sealed set once, so the pinned
    # addresses survive the move — seal again without them after that first start.
    IN=${2:?usage: kms.sh secrets <secrets.json> [out]}
    OUT=${3:-dev-data/sealed/sealed.json}
    mkdir -p "$(dirname "$OUT")"
    ENVELOPE=$(mktemp); KEY=$(mktemp); trap 'rm -f "$ENVELOPE" "$KEY"' EXIT
    # The key is printed once, as hex, and kept by nobody: straight into KMS as raw bytes —
    # sealing the hex TEXT instead would hand the enclave 64 bytes where it wants 32.
    cargo run -q --release -p tokumai-server --bin tokumai-seal -- "$IN" "$ENVELOPE" | tr -d '\n' | xxd -r -p > "$KEY"
    [ "$(wc -c < "$KEY")" -eq 32 ] || { echo "the sealing key is not 32 bytes"; exit 1; }
    WRAPPED=$(aws kms encrypt --key-id $ALIAS --plaintext "fileb://$KEY" --query CiphertextBlob --output text)
    python3 - "$ENVELOPE" "$WRAPPED" "$OUT" <<'PY'
import json, sys
envelope, wrapped, out = sys.argv[1:]
sealed = json.load(open(envelope))
sealed["kmsKey"] = wrapped
json.dump(sealed, open(out, "w"), indent=2)
PY
    chmod 600 "$OUT"
    echo "$OUT — the host keeps this; only an allowed image can open it"
    echo "the input is still in the clear at $IN: keep the provider keys where they live (a password manager), not in the repository tree, and remove the file"
    ;;
  watch)
    # Every change to who may open the book, and every deletion scheduled, mails the alarm
    # topic (probe.sh alarm makes it): an EventBridge rule on CloudTrail's management
    # events for this key. EventBridge sees those events only through a trail that is
    # logging (there is none by default), so one is made here too: multi-region, writing
    # management events only, into a private bucket kept 400 days — the first copy of
    # management events costs nothing, the bucket a few cents. Needs cloudtrail:*, s3 on
    # the new bucket, events:PutRule/PutTargets and sns:SetTopicAttributes — an
    # administrator's, once (docs/aws-admin.md, 5).
    TOPIC=$ALARM_TOPIC
    aws sns get-topic-attributes --topic-arn "$TOPIC" >/dev/null 2>&1 || { echo "no topic $TOPIC — 'deploy/aws/probe.sh alarm <email>' first"; exit 1; }
    TRAIL=tokumai
    TRAIL_BUCKET=tokumai-trail-$ACCOUNT
    if ! aws cloudtrail get-trail --name $TRAIL >/dev/null 2>&1; then
      if ! aws s3api head-bucket --bucket $TRAIL_BUCKET >/dev/null 2>&1; then
        aws s3api create-bucket --bucket $TRAIL_BUCKET --create-bucket-configuration LocationConstraint=$AWS_REGION >/dev/null
        aws s3api put-public-access-block --bucket $TRAIL_BUCKET --public-access-block-configuration BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
        aws s3api put-bucket-lifecycle-configuration --bucket $TRAIL_BUCKET --lifecycle-configuration '{"Rules":[{"ID":"400 days","Status":"Enabled","Filter":{"Prefix":""},"Expiration":{"Days":400}}]}'
      fi
      BUCKET_POLICY=$(python3 - "$TRAIL_BUCKET" "$ACCOUNT" "$AWS_REGION" "$TRAIL" <<'PY'
import json, sys
bucket, account, region, trail = sys.argv[1:]
arn = f"arn:aws:cloudtrail:{region}:{account}:trail/{trail}"
print(json.dumps({"Version": "2012-10-17", "Statement": [
    {"Sid": "CloudTrailMayCheck", "Effect": "Allow", "Principal": {"Service": "cloudtrail.amazonaws.com"},
     "Action": "s3:GetBucketAcl", "Resource": f"arn:aws:s3:::{bucket}", "Condition": {"StringEquals": {"aws:SourceArn": arn}}},
    {"Sid": "CloudTrailMayWrite", "Effect": "Allow", "Principal": {"Service": "cloudtrail.amazonaws.com"},
     "Action": "s3:PutObject", "Resource": f"arn:aws:s3:::{bucket}/AWSLogs/{account}/*",
     "Condition": {"StringEquals": {"s3:x-amz-acl": "bucket-owner-full-control", "aws:SourceArn": arn}}}]}))
PY
)
      aws s3api put-bucket-policy --bucket $TRAIL_BUCKET --policy "$BUCKET_POLICY"
      aws cloudtrail create-trail --name $TRAIL --s3-bucket-name $TRAIL_BUCKET --is-multi-region-trail --enable-log-file-validation >/dev/null
      aws cloudtrail put-event-selectors --trail-name $TRAIL --event-selectors '[{"ReadWriteType":"WriteOnly","IncludeManagementEvents":true}]' >/dev/null
      aws cloudtrail start-logging --name $TRAIL
      echo "trail $TRAIL → s3://$TRAIL_BUCKET: writing management events, all regions, kept 400 days"
    fi
    aws cloudtrail get-trail-status --name $TRAIL --query IsLogging --output text | grep -q True || { echo "the trail $TRAIL is not logging — 'aws cloudtrail start-logging --name $TRAIL'"; exit 1; }
    KEYARN=$(aws kms describe-key --key-id $ALIAS --query KeyMetadata.Arn --output text)
    PATTERN=$(python3 - "$KEYARN" "$(key_id)" "$ALIAS" <<'PY'
import json, sys
print(json.dumps({"source": ["aws.kms"], "detail-type": ["AWS API Call via CloudTrail"],
                  "detail": {"eventSource": ["kms.amazonaws.com"],
                             "eventName": ["PutKeyPolicy", "ScheduleKeyDeletion", "CreateGrant"],
                             "requestParameters": {"keyId": sys.argv[1:]}}}))
PY
)
    aws events put-rule --name tokumai-key-policy-changed --state ENABLED \
      --description "Somebody changed who may open the tokumai book (kms:PutKeyPolicy) or scheduled the key for deletion." \
      --event-pattern "$PATTERN" >/dev/null
    aws events put-targets --rule tokumai-key-policy-changed --targets "Id=alarms,Arn=$TOPIC,InputTransformer={InputPathsMap={who=\$.detail.userIdentity.arn,what=\$.detail.eventName,when=\$.detail.eventTime},InputTemplate='\"tokumai: <what> on the book key by <who> at <when>\"'}" >/dev/null
    # The topic must let EventBridge publish.
    POLICY=$(aws sns get-topic-attributes --topic-arn "$TOPIC" --query Attributes.Policy --output text)
    if ! echo "$POLICY" | grep -q events.amazonaws.com; then
      NEW=$(python3 - "$POLICY" "$TOPIC" <<'PY'
import json, sys
p = json.loads(sys.argv[1])
p.setdefault("Statement", []).append({"Sid": "EventBridgeMayPublish", "Effect": "Allow", "Principal": {"Service": "events.amazonaws.com"}, "Action": "sns:Publish", "Resource": sys.argv[2]})
print(json.dumps(p))
PY
)
      aws sns set-topic-attributes --topic-arn "$TOPIC" --attribute-name Policy --attribute-value "$NEW"
    fi
    echo "rule tokumai-key-policy-changed → $TOPIC: a mail for every PutKeyPolicy, ScheduleKeyDeletion or CreateGrant on $ALIAS, within a few minutes of the call"
    ;;
  policy) aws kms get-key-policy --key-id "$(key_id)" --policy-name default --query Policy --output text | python3 -m json.tool ;;
  *) sed -n 2,11p "$0"; exit 2 ;;
esac

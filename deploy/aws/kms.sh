#!/usr/bin/env bash
# The key that guards the enclave's secrets, and the sealing of those secrets.
#
#   deploy/aws/kms.sh create              # the key (alias tokumai-enclave) with its policy
#   deploy/aws/kms.sh allow <pcr0>        # let an image decrypt (a new image = a new PCR0)
#   deploy/aws/kms.sh seal <file>         # seal a file for the enclave → <file>.sealed
#   deploy/aws/kms.sh secrets <secrets.json> [out]   # the whole sealed file the host keeps
#   deploy/aws/kms.sh policy              # what the policy says right now
#
# The policy is the whole point: `kms:Decrypt` is allowed ONLY for a request that carries
# a Nitro attestation of an image on the allow list — not for us, not for anyone with the
# host's root. What the operator keeps is managing the key (and `kms:Encrypt`, to seal new
# secrets). Honest about the limit: whoever can change the policy could grant themselves
# decryption later. Removing that would make the key unmanageable and the enclave
# un-upgradable; the way out is signed images (PCR8) instead of a list of PCR0s, and it is
# on the list for before launch (docs/enclave-phase0.md).
set -euo pipefail
cd "$(dirname "$0")/../.."
export AWS_PROFILE=${AWS_PROFILE:-tokumai} AWS_REGION=eu-central-1 AWS_PAGER=""
ALIAS=alias/tokumai-enclave
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
ROLE=arn:aws:iam::$ACCOUNT:role/tokumai-enclave-host

# Some KMS calls take an alias, others insist on the key id — resolve it once.
key_id() { aws kms describe-key --key-id $ALIAS --query KeyMetadata.KeyId --output text 2>/dev/null || true; }

policy_json() {  # $@ = the PCR0s that may decrypt
  python3 - "$ACCOUNT" "$ROLE" "$@" <<'PY'
import json, sys
account, role, *pcrs = sys.argv[1:]
enclave = {
    "Sid": "OnlyAnAttestedTokumaiEnclaveMayDecrypt",
    "Effect": "Allow",
    "Principal": {"AWS": role},
    "Action": ["kms:Decrypt", "kms:GenerateRandom"],
    "Resource": "*",
    "Condition": {"StringEqualsIgnoreCase": {"kms:RecipientAttestation:PCR0": pcrs}},
} if pcrs else None
statements = [{
    "Sid": "TheOperatorManagesTheKeyAndMaySealSecretsButNotOpenThem",
    "Effect": "Allow",
    "Principal": {"AWS": f"arn:aws:iam::{account}:root"},
    "Action": ["kms:Describe*", "kms:Get*", "kms:List*", "kms:PutKeyPolicy", "kms:ScheduleKeyDeletion",
               "kms:CancelKeyDeletion", "kms:TagResource", "kms:UntagResource", "kms:Encrypt",
               "kms:CreateAlias", "kms:UpdateAlias", "kms:DeleteAlias"],
    "Resource": "*",
}]
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
    PCR0=${2:?usage: kms.sh allow <pcr0>}
    aws kms put-key-policy --key-id "$(key_id)" --policy-name default --policy "$(policy_json "$PCR0")"
    echo "an enclave running image $PCR0 may now decrypt — and nothing else"
    ;;
  seal)
    FILE=${2:?usage: kms.sh seal <file>}
    aws kms encrypt --key-id $ALIAS --plaintext "fileb://$FILE" --query CiphertextBlob --output text | base64 --decode > "$FILE.sealed"
    echo "$FILE.sealed — open only inside an allowed enclave"
    ;;
  secrets)
    # What the host is given: the secrets under a fresh key of their own, and that key
    # sealed by KMS — which hands it back to an attested enclave and to nobody else.
    # (KMS encrypts at most 4 KiB; the secrets, with the Nym identity, are far larger.)
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
    echo "$OUT — the host keeps this; only an allowed image can open it"
    ;;
  policy) aws kms get-key-policy --key-id "$(key_id)" --policy-name default --query Policy --output text | python3 -m json.tool ;;
  *) sed -n 2,12p "$0"; exit 2 ;;
esac

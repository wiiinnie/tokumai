#!/usr/bin/env bash
# Build the enclave image and print its measurements.
#
#   1. the Docker image (deploy/enclave/Dockerfile, reproducible);
#   2. the Nitro enclave image (EIF) from it, with nitro-cli in a pinned Amazon Linux
#      container — no EC2 host needed;
#   3. PCR0 (the image), PCR1 (kernel), PCR2 (application): PCR0 is what the app pins and
#      what the KMS key policy names.
#
#     deploy/enclave/build.sh            → dev-data/eif/tokumai-<tag>.eif + .pcrs.json
#                                          + deploy/enclave/measurements/<tag>.json (commit this)
#     NO_CACHE=1 TAG=check deploy/enclave/build.sh   (from scratch: must give the same PCR0)
#
# What makes the measurement checkable by somebody else (audit M17, 2026-10-06): the build
# context is `git archive HEAD` — the committed tree, nothing lying around beside it — and
# a tree with uncommitted changes is refused (DIRTY=1 to build one anyway, marked as such);
# the platform is named rather than taken from the machine; and everything that goes into
# PCR0 is written down beside it in deploy/enclave/measurements/<tag>.json: the commit,
# the feature list, strip and debug-info, the two base images' digests, the nitro-cli
# version. A rebuild from that record must give the same PCR0, or the attestation names a
# measurement nobody can account for.
set -euo pipefail
cd "$(dirname "$0")/../.."
TAG=${TAG:-probe-1}
AMAZON_LINUX=amazonlinux@sha256:74c545e3e04db388b00bd31d7cc5640d4e9c12058a6d72af938d113da3c82893
NITRO_CLI=1.5.0
# Nitro Enclaves on Graviton: the image is arm64 whatever machine builds it.
PLATFORM=linux/arm64
mkdir -p dev-data/eif deploy/enclave/measurements
COMMIT=$(git rev-parse HEAD)
DIRTY_TREE=$(git status --porcelain | grep -v '^?? ' || true)
if [ -n "$DIRTY_TREE" ] && [ "${DIRTY:-}" != 1 ]; then
  echo "the tree has uncommitted changes — commit them, or DIRTY=1 to build a measurement nobody can rebuild:"
  echo "$DIRTY_TREE" | head -20
  exit 1
fi
# Every timestamp in the image (layer files, the config's "created") set to the same fixed
# moment: the binary is bit-identical between builds, and the file dates were what made
# PCR2 differ.
EPOCH=$(grep -m1 -o 'SOURCE_DATE_EPOCH=[0-9]*' deploy/enclave/Dockerfile | cut -d= -f2)
# (The docker exporter cannot rewrite timestamps while it unpacks, so: an OCI archive, loaded.)
# FEATURES=nitro,apple-sandbox builds the image that accepts App Store SANDBOX receipts,
# for testing on a real phone. It is a DIFFERENT image with a different PCR0, and that is
# the safeguard: the production image cannot be talked into taking a sandbox purchase, and
# the attestation says which of the two a person is talking to.
FEATURES=${FEATURES:-nitro}
# STRIP=none DEBUGINFO=line-tables-only: symbols kept, so a stack report names functions.
STRIP=${STRIP:-symbols}
DEBUGINFO=${DEBUGINFO:-0}
# The context: the committed tree (plus, with DIRTY=1, what is changed but not committed —
# never what is untracked). .dockerignore applies to it as to a directory.
CONTEXT=$(mktemp -t tokumai-context.XXXXXX)
trap 'rm -f "$CONTEXT"' EXIT
if [ -n "$DIRTY_TREE" ]; then
  # A commit of the changed tree that touches nothing (`git stash create` writes no ref).
  TREE=$(git stash create || true); TREE=${TREE:-HEAD}
  git archive --format=tar "$TREE" > "$CONTEXT"
else
  git archive --format=tar HEAD > "$CONTEXT"
fi
docker buildx build ${NO_CACHE:+--no-cache} --platform $PLATFORM --build-arg SOURCE_DATE_EPOCH=$EPOCH --build-arg FEATURES="$FEATURES" \
  --build-arg STRIP="$STRIP" --build-arg DEBUGINFO="$DEBUGINFO" \
  --output type=oci,name=tokumai-enclave:$TAG,dest=dev-data/eif/tokumai-$TAG.oci.tar,rewrite-timestamp=true \
  -f deploy/enclave/Dockerfile - < "$CONTEXT"
docker load -i dev-data/eif/tokumai-$TAG.oci.tar
docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$PWD/dev-data/eif":/out $AMAZON_LINUX sh -c "
  dnf install -y -q aws-nitro-enclaves-cli-$NITRO_CLI aws-nitro-enclaves-cli-devel-$NITRO_CLI >/dev/null 2>&1 &&
  nitro-cli build-enclave --docker-uri tokumai-enclave:$TAG --output-file /out/tokumai-$TAG.eif" | tee dev-data/eif/tokumai-$TAG.pcrs.json
# The record: everything a rebuild needs, beside what it must come out as.
python3 - "$TAG" "$COMMIT" "${DIRTY_TREE:+dirty}" "$FEATURES" "$STRIP" "$DEBUGINFO" "$PLATFORM" "$NITRO_CLI" "$AMAZON_LINUX" <<'PY'
import json, re, sys, datetime
tag, commit, dirty, features, strip, debuginfo, platform, nitro_cli, amazon_linux = sys.argv[1:]
pcrs = json.load(open(f"dev-data/eif/tokumai-{tag}.pcrs.json"))["Measurements"]
df = open("deploy/enclave/Dockerfile").read()
bases = re.findall(r"^FROM (\S+)", df, re.M)
record = {
    "tag": tag, "commit": commit, "dirty": bool(dirty),
    "built": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "features": features, "strip": strip, "debuginfo": debuginfo, "platform": platform,
    "base_images": bases, "nitro_cli": nitro_cli, "nitro_cli_host": amazon_linux,
    "pcr0": pcrs["PCR0"], "pcr1": pcrs["PCR1"], "pcr2": pcrs["PCR2"],
}
path = f"deploy/enclave/measurements/{tag}.json"
json.dump(record, open(path, "w"), indent=2); open(path, "a").write("\n")
print(f"{path}: PCR0 {pcrs['PCR0'][:16]}… at {commit[:12]}{' (DIRTY)' if dirty else ''} — commit this file")
PY

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
#     NO_CACHE=1 TAG=check deploy/enclave/build.sh   (from scratch: must give the same PCR0)
set -euo pipefail
cd "$(dirname "$0")/../.."
TAG=${TAG:-probe-1}
AMAZON_LINUX=amazonlinux@sha256:74c545e3e04db388b00bd31d7cc5640d4e9c12058a6d72af938d113da3c82893
NITRO_CLI=1.5.0
mkdir -p dev-data/eif
# Every timestamp in the image (layer files, the config's "created") set to the same fixed
# moment: the binary is bit-identical between builds, and the file dates were what made
# PCR2 differ.
EPOCH=$(grep -m1 -o 'SOURCE_DATE_EPOCH=[0-9]*' deploy/enclave/Dockerfile | cut -d= -f2)
# (The docker exporter cannot rewrite timestamps while it unpacks, so: an OCI archive, loaded.)
docker buildx build ${NO_CACHE:+--no-cache} --build-arg SOURCE_DATE_EPOCH=$EPOCH \
  --output type=oci,name=tokumai-enclave:$TAG,dest=dev-data/eif/tokumai-$TAG.oci.tar,rewrite-timestamp=true \
  -f deploy/enclave/Dockerfile .
docker load -i dev-data/eif/tokumai-$TAG.oci.tar
docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$PWD/dev-data/eif":/out $AMAZON_LINUX sh -c "
  dnf install -y -q aws-nitro-enclaves-cli-$NITRO_CLI aws-nitro-enclaves-cli-devel-$NITRO_CLI >/dev/null 2>&1 &&
  nitro-cli build-enclave --docker-uri tokumai-enclave:$TAG --output-file /out/tokumai-$TAG.eif" | tee dev-data/eif/tokumai-$TAG.pcrs.json

#!/usr/bin/env bash
# The enclave's network, simulated with Docker: the simulated enclave runs in a container on
# an internal network with NO internet, and reaches the world only through
# tokumai-egress-host in a second container — as a Nitro enclave only has its vsock tunnel.
# Anything that does not go through the proxy fails here, where on a laptop it would
# silently work.
#
#     deploy/sim-enclave.sh build    # Linux binaries into dev-data/linux (rust container)
#     deploy/sim-enclave.sh up       # both containers; the enclave's Nym address as usual
#     deploy/sim-enclave.sh logs     # what went out, per destination
#     deploy/sim-enclave.sh down
set -euo pipefail
cd "$(dirname "$0")/.."
RUST=rust:1.97.1-slim-bookworm
NET=tokumai-enclave-net
PROXY_IP=172.30.0.2
case "${1:-}" in
  build)
    docker run --rm -v "$PWD":/src -v tokumai-cargo-registry:/usr/local/cargo/registry -v tokumai-target:/src/target -w /src $RUST \
      sh -c 'cargo build -q -p tokumai-server --bin tokumai-enclave-dev && cargo build -q -p tokumai-egress --bin tokumai-egress-host &&
             mkdir -p dev-data/linux && cp target/debug/tokumai-enclave-dev target/debug/tokumai-egress-host dev-data/linux/'
    ;;
  up)
    docker network inspect $NET >/dev/null 2>&1 || docker network create --internal --subnet 172.30.0.0/24 $NET >/dev/null
    docker rm -f tokumai-egress tokumai-enclave >/dev/null 2>&1 || true
    docker run -d --name tokumai-egress -v "$PWD/dev-data/linux":/bin-tk:ro -v "$PWD/deploy":/deploy:ro $RUST \
      /bin-tk/tokumai-egress-host tcp:0.0.0.0:8080 /deploy/egress.allow >/dev/null
    docker network connect --ip $PROXY_IP $NET tokumai-egress
    # The proxy by IP, as in the enclave (127.0.0.1): no name to resolve on the way out.
    docker run -d --name tokumai-enclave --network $NET --env-file .env \
      -e TOKUMAI_EGRESS_PROXY=$PROXY_IP:8080 -e HTTPS_PROXY=http://$PROXY_IP:8080 -e HTTP_PROXY=http://$PROXY_IP:8080 \
      -v "$PWD/dev-data":/work/dev-data -v "$PWD/dev-data/linux":/bin-tk:ro -w /work $RUST /bin-tk/tokumai-enclave-dev --mix >/dev/null
    echo "enclave starting; its address: dev-data/nym-address (docker logs -f tokumai-enclave)"
    ;;
  logs) docker logs tokumai-egress 2>&1 | sed -E 's/up [0-9]+ down [0-9]+/(tunnel)/' | sort | uniq -c ;;
  down) docker rm -f tokumai-egress tokumai-enclave >/dev/null 2>&1 || true ;;
  *) echo "usage: $0 build|up|logs|down"; exit 2 ;;
esac

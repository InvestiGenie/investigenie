#!/usr/bin/env bash
# Removes the containers and network that start.sh created.
set -uo pipefail
docker rm -f ig-te-elasticsearch ig-te-clickhouse ig-te-loki ig-te-mimir ig-te-fixture ig-te-prometheus >/dev/null 2>&1
docker network rm ig-translation-engines >/dev/null 2>&1
exit 0

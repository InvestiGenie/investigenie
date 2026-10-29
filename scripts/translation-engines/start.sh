#!/usr/bin/env bash
# Starts disposable Elasticsearch, ClickHouse, Loki, Prometheus, and Mimir for the live query
# translation matrix (`live_translations_return_the_same_records_in_every_engine`), waits until
# they hold the fixture, and prints the TEST_*_URL variables the test reads, one per line:
#
#   export $(scripts/translation-engines/start.sh)
#
# scripts/translation-engines/stop.sh removes the containers.
set -euo pipefail

here=$(cd "$(dirname "$0")" && (pwd -W 2>/dev/null || pwd))
root=$(cd "$(dirname "$0")/../.." && (pwd -W 2>/dev/null || pwd))
network=ig-translation-engines
# Git Bash would rewrite the containers' absolute paths into Windows paths.
export MSYS_NO_PATHCONV=1

docker network create "$network" >/dev/null 2>&1 || true
start() {
  local name=$1
  shift
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --network "$network" "$@" >/dev/null
}
start ig-te-elasticsearch -p 19200:9200 -e discovery.type=single-node -e xpack.security.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" docker.elastic.co/elasticsearch/elasticsearch:8.17.0
start ig-te-clickhouse -p 18123:8123 -e CLICKHOUSE_SKIP_USER_SETUP=1 clickhouse/clickhouse-server:24.8
start ig-te-loki -p 13100:3100 -v "$root/playground/loki-config.yaml:/etc/loki/config.yaml:ro" \
  grafana/loki:3.4.2 -config.file=/etc/loki/config.yaml
start ig-te-mimir -p 19009:9009 -v "$root/playground/mimir-config.yaml:/etc/mimir/config.yaml:ro" \
  grafana/mimir:2.15.0 -config.file=/etc/mimir/config.yaml -target=all
start ig-te-fixture -v "$here:/srv:ro" -w /srv python:3.12-slim python -I -m http.server 8000
start ig-te-prometheus -p 19090:9090 -v "$here/prometheus.yml:/etc/prometheus/prometheus.yml:ro" \
  prom/prometheus:v3.2.1 --config.file=/etc/prometheus/prometheus.yml

wait_for() {
  local url=$1
  for _ in $(seq 1 150); do
    if curl -fsS "$url" 2>/dev/null | grep -q "${2:-}"; then
      return 0
    fi
    sleep 2
  done
  echo "timed out waiting for $url" >&2
  exit 1
}
wait_for "http://localhost:19200/_cluster/health?wait_for_status=yellow&timeout=5s"
wait_for http://localhost:18123/ping
wait_for http://localhost:13100/ready ready
wait_for http://localhost:19009/ready ready
# The fixture series must reach Prometheus by scraping and Mimir by remote write.
wait_for "http://localhost:19090/api/v1/series?match%5B%5D=ig_matrix" '"rid"'
wait_for "http://localhost:19009/prometheus/api/v1/series?match%5B%5D=ig_matrix" '"rid"'

cat <<EOF
TEST_ELASTICSEARCH_URL=http://localhost:19200
TEST_CLICKHOUSE_URL=http://localhost:18123
TEST_LOKI_URL=http://localhost:13100
TEST_PROMETHEUS_URL=http://localhost:19090
TEST_MIMIR_URL=http://localhost:19009/prometheus
EOF

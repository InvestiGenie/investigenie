#!/usr/bin/env bash
# InvestiGenie playground helper. Run from anywhere: ./playground/playground.sh <command>
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$DIR/.playground-state"

usage() {
  cat <<'EOF'
Usage: playground.sh <command> [options]

  up [--ai docker|host|none] [--gpu] [--observability] [--models LIST]
                         Build and start the playground, seed data and provision connections,
                         data views and AI models. Options are remembered for later commands.
                           --ai docker  run Ollama in Docker (default; CPU unless --gpu)
                           --ai host    use Ollama installed on this machine (fastest on a Mac:
                                        `brew install ollama && ollama serve`)
                           --ai none    do not start local Ollama; configure a remote provider in .env if needed
                           --models     comma list of Ollama models, default qwen2.5:7b
  status                 Containers, seeding state and registered models
  logs [service]         Follow logs (default: seeder, setup, ollama)
  setup                  Re-run provisioning (idempotent)
  model add MODEL...     Pull, tune and register more models, e.g. `model add llama3.2:3b qwen2.5:3b`
  model list             Registered AI models
  model rm NAME          Remove a model profile from InvestiGenie (by name, model or id)
  eval [ai_eval.py args] Score AI investigations on the seeded incidents, e.g.
                           eval                      first registered model, all scenarios
                           eval --model all          compare every registered model
                           eval --model 3b --scenario checkout-bad-deploy -v
  scenarios              Print the incident ground truth
  reseed                 Regenerate the corpus (after changing PLAYGROUND_* seeding settings)
  down                   Stop, keeping data and downloaded models
  reset                  Stop and delete all playground data (models are kept unless --all)
EOF
}

load_state() {
  AI_MODE=docker; GPU=0; OBSERVABILITY=0; MODELS=""
  # shellcheck disable=SC1090
  [[ -f "$STATE" ]] && source "$STATE"
  return 0
}

save_state() {
  printf 'AI_MODE=%q\nGPU=%q\nOBSERVABILITY=%q\nMODELS=%q\n' "$AI_MODE" "$GPU" "$OBSERVABILITY" "$MODELS" > "$STATE"
}

compose() {
  local files=(-f "$DIR/compose.yaml")
  local profiles=()
  read -r -a profiles <<< "${EXTRA_PROFILES:-}"
  [[ "$AI_MODE" == docker ]] && profiles+=(ollama)
  [[ "$AI_MODE" == docker && "$GPU" == 1 ]] && files+=(-f "$DIR/compose.gpu.yaml")
  [[ "$OBSERVABILITY" == 1 ]] && profiles+=(observability)
  # Models chosen with `up --models` stick for later `setup` runs instead of reverting to the default.
  [[ -n "${MODELS:-}" ]] && export PLAYGROUND_AI_MODELS="${PLAYGROUND_AI_MODELS:-$MODELS}"
  case "$AI_MODE" in
    docker) export PLAYGROUND_OLLAMA_DEFAULT_URL="${PLAYGROUND_OLLAMA_DEFAULT_URL:-http://ollama:11434}" ;;
    host)   export PLAYGROUND_OLLAMA_DEFAULT_URL="${PLAYGROUND_OLLAMA_DEFAULT_URL:-http://host.docker.internal:11434}" ;;
    none)   export PLAYGROUND_AI_PROVIDER_DEFAULT="${PLAYGROUND_AI_PROVIDER_DEFAULT:-none}" ;;
  esac
  COMPOSE_PROFILES="$(IFS=,; echo "${profiles[*]-}")" docker compose --project-directory "$DIR" "${files[@]}" "$@"
}

check_host_ollama() {
  if ! curl -fsS -m 3 http://127.0.0.1:11434/api/version >/dev/null 2>&1; then
    echo "Ollama is not answering on http://127.0.0.1:11434." >&2
    echo "Install and start it first: https://ollama.com/download (macOS: brew install ollama && ollama serve)" >&2
    echo "On Linux, start it with OLLAMA_HOST=0.0.0.0 so containers can reach it." >&2
    exit 1
  fi
}

load_state
command="${1:-}"
[[ $# -gt 0 ]] && shift

case "$command" in
  up)
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --ai) AI_MODE="${2:?--ai needs docker, host or none}"; shift 2 ;;
        --gpu) GPU=1; shift ;;
        --no-gpu) GPU=0; shift ;;
        --observability) OBSERVABILITY=1; shift ;;
        --no-observability) OBSERVABILITY=0; shift ;;
        --models) MODELS="${2:?--models needs a list}"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; usage; exit 1 ;;
      esac
    done
    [[ "$AI_MODE" =~ ^(docker|host|none)$ ]] || { echo "--ai must be docker, host or none" >&2; exit 1; }
    [[ "$AI_MODE" == host ]] && check_host_ollama
    save_state
    mkdir -p "$DIR/reports"
    compose up --build -d --remove-orphans
    # Pick up services added after the first seed (e.g. Loki) and backfill them.
    [[ "$OBSERVABILITY" == 1 ]] && compose restart seeder >/dev/null
    echo "Provisioning (the first run downloads the model; this can take several minutes)..."
    compose logs -f --no-log-prefix setup &
    logs_pid=$!
    status=$(compose wait setup 2>/dev/null | tail -n 1 || true)
    kill "$logs_pid" 2>/dev/null || true
    wait "$logs_pid" 2>/dev/null || true
    [[ "$status" == 0 ]] || echo "Provisioning reported a problem; see: ./playground/playground.sh logs setup" >&2
    echo
    echo "InvestiGenie:  http://127.0.0.1:${INVESTIGENIE_PORT:-3000}"
    echo "Ground truth:  http://127.0.0.1:8000/scenarios"
    ;;
  status)
    compose ps
    echo
    curl -fsS -m 5 http://127.0.0.1:8000/status && echo || echo "seeder: not reachable"
    compose run --rm --no-deps setup list 2>/dev/null || true
    ;;
  logs)
    if [[ $# -gt 0 ]]; then
      compose logs -f "$@"
    elif [[ "$AI_MODE" == docker ]]; then
      compose logs -f seeder setup ollama
    else
      compose logs -f seeder setup
    fi
    ;;
  setup)
    compose run --rm setup
    ;;
  model)
    sub="${1:-list}"; [[ $# -gt 0 ]] && shift
    case "$sub" in
      add) [[ $# -gt 0 ]] || { echo "model add needs at least one model name" >&2; exit 1; }
           [[ "$AI_MODE" == host ]] && check_host_ollama
           compose run --rm setup models "$@" ;;
      list|ls) compose run --rm --no-deps setup list ;;
      rm|remove) compose run --rm --no-deps setup remove-model "$@" ;;
      *) echo "Unknown model command: $sub" >&2; usage; exit 1 ;;
    esac
    ;;
  eval)
    mkdir -p "$DIR/reports"
    EXTRA_PROFILES=tools compose run --rm eval "$@"
    ;;
  scenarios)
    curl -fsS http://127.0.0.1:8000/scenarios
    ;;
  reseed)
    PLAYGROUND_RESEED=true compose up -d --force-recreate --no-deps seeder
    echo "Reseeding (follow with: ./playground/playground.sh logs seeder)..."
    deadline=$((SECONDS + 1800))
    until curl -fsS -m 3 http://127.0.0.1:8000/healthz >/dev/null 2>&1; do
      if (( SECONDS > deadline )); then
        echo "Seeding did not finish within 30 minutes; see: ./playground/playground.sh logs seeder" >&2
        exit 1
      fi
      sleep 3
    done
    compose up -d --no-deps seeder   # recreate without PLAYGROUND_RESEED so restarts keep the new corpus
    echo "Reseeded. Ground truth: ./playground/playground.sh scenarios"
    ;;
  down)
    EXTRA_PROFILES="tools ollama observability" compose down
    ;;
  reset)
    if [[ "${1:-}" == "--all" ]]; then
      EXTRA_PROFILES="tools ollama observability" compose down -v
    else
      EXTRA_PROFILES="tools ollama observability" compose down
      project=investigenie-playground
      volumes=$(docker volume ls --format '{{.Name}}')
      for volume in elasticsearch-data investigenie-data loki-data mimir-data prometheus-data seeder-state; do
        volume_name="${project}_${volume}"
        if printf '%s\n' "$volumes" | grep -Fqx -- "$volume_name"; then
          docker volume rm -f "$volume_name" >/dev/null
        fi
      done
      echo "Removed playground data (kept downloaded models; use 'reset --all' to remove them too)."
    fi
    rm -f "$STATE"
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    echo "Unknown command: $command" >&2
    usage
    exit 1
    ;;
esac

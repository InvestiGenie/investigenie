# Local playground

A self-contained environment for trying and testing InvestiGenie end to end, including the AI investigator running on a local open-source model.

One command does the following:

- Builds InvestiGenie from this checkout.
- Starts Elasticsearch and seeds it with a week of realistic application logs that contain **injected incidents with known root causes**.
- Connects InvestiGenie to the seeded data and to the official public [ClickHouse playground](https://play.clickhouse.com).
- Downloads a local LLM (default `qwen2.5:7b` through [Ollama](https://ollama.com)) and registers it as an InvestiGenie AI model.

An evaluation harness then runs AI investigations against the incidents and scores the answers. You can compare models, or check that a change to the AI flow didn't make results worse.

## Quick start

```sh
./playground/playground.sh up                 # Ollama in Docker (CPU; add --gpu for NVIDIA)
./playground/playground.sh up --ai host       # use Ollama installed on this machine
./playground/playground.sh up --ai none       # do not start local Ollama
```

**On macOS, use `--ai host`.** Docker Desktop cannot give containers the Apple GPU, so a 7B model in Docker runs on the CPU and takes minutes per answer. Install Ollama natively (`brew install ollama && ollama serve`, or the app from ollama.com) and the playground will use it through `host.docker.internal`.

The first `up` downloads the model (about 4.7 GB for `qwen2.5:7b`). When provisioning finishes it prints the questions to try:

| Service | URL |
| --- | --- |
| InvestiGenie | <http://127.0.0.1:3000> |
| Seeder status, ground truth, metrics | <http://127.0.0.1:8000/status>, [/scenarios](http://127.0.0.1:8000/scenarios), [/metrics](http://127.0.0.1:8000/metrics) |
| Elasticsearch | <http://127.0.0.1:9200> |
| Ollama (`--ai docker`) | <http://127.0.0.1:11434> |

Requirements: Docker with Compose v2.20 or newer. For a 7B model in Docker, give Docker Desktop at least 10 GB of memory, or pick a smaller model.

## What gets provisioned

| Kind | Name | Details |
| --- | --- | --- |
| Connection | `playground-elasticsearch` | `http://elasticsearch:9200`, no auth |
| Data view | `playground-logs` | `investigenie-logs-*`, time field `@timestamp` |
| Connection | `clickhouse-playground` | `https://play.clickhouse.com:443`, user `explorer`, database `default` (read-only, needs internet access) |
| Data views | `clickhouse-github_events`, `clickhouse-hackernews` | Set with `PLAYGROUND_CLICKHOUSE_DATA_VIEWS=table:time_field,...`. Tables that don't exist are reported along with the tables that do. |
| AI model | `qwen2.5:7b (local Ollama)` | Served as `investigenie/qwen2.5:7b` at `http://ollama:11434/v1` |

Provisioning is idempotent. Re-run it with `./playground/playground.sh setup`.

## The seeded corpus

There are 50,000 baseline events over 7 days, spread across five services (`api-gateway`, `checkout`, `catalog`, `identity`, `payments`) and three regions. Traffic follows a day/night cycle. The events are ECS-shaped (`service.name`, `http.response.status_code`, `error.message`, `error.stack_trace`, `trace.id`, `client.ip`, `user_agent.original`, ...). They include normal 4xx noise and a small amount of unrelated 5xx noise, so the AI has to separate each incident from background errors.

After seeding, the seeder keeps writing live traffic, about 2 events per second, so "last 30 minutes" investigations and histograms stay current. If the machine sleeps, it skips the gap rather than back-filling hours of traffic at once.

### Incident scenarios

| Scenario | When | Difficulty | Question to ask |
| --- | --- | --- | --- |
| `payments-db-pool-exhaustion` | ~51 h ago, 40 min | hard | Customers could not complete payments during this period. What went wrong, and which services were affected? |
| `checkout-bad-deploy` | ~20 h ago, 75 min | medium | Order creation started failing for some customers. What is the root cause? |
| `identity-credential-stuffing` | ~8 h ago, 35 min | easy | Is anything suspicious happening with customer logins? |
| `catalog-eu-slow-queries` | ~3 h ago, 55 min | medium | Users say product pages are very slow. Why? |
| `gateway-search-timeouts` | **ongoing** (live) | easy | Why are search requests failing right now? (default last-30-minutes range) |
| `control-quiet-window` | ~30 h ago, 60 min | control | Were there any significant incidents or outages during this period? (normal traffic only; a good answer reports nothing serious) |

`/scenarios` returns the exact time range for each scenario, the expected root cause and the scoring keywords. The setup log prints the time ranges in UTC. All times are relative to when the corpus was first seeded. Restarts keep the same corpus.

<details><summary>Root causes (spoilers)</summary>

- **payments-db-pool-exhaustion:** a settlement batch job held the payments DB connections. The HikariCP pool was exhausted, so `POST /api/payments` returned 503 after 30 s and checkout returned 502 for orders. The two share `trace.id`s.
- **checkout-bad-deploy:** checkout `1.15.0` was deployed. `POST /api/orders` then threw `NullPointerException` in `OrderService.create` until the rollback to `1.14.2`.
- **identity-credential-stuffing:** `203.0.113.77` (`python-requests`) tried many accounts. Most attempts got 401, then 429 rate limiting, and some accounts were locked.
- **catalog-eu-slow-queries:** only `eu-west-1` was affected. Read replica `catalog-db-eu-2` ran slow queries of 2-6 s, and some requests got 504 timeouts.
- **gateway-search-timeouts:** `api-gateway` returns 504 for `/api/v1/search` because upstream `catalog-search` times out.
- **control-quiet-window:** nothing. Only normal 4xx noise and isolated, unrelated 5xx responses.

</details>

## Local AI models

Every model is pulled into Ollama and copied as `investigenie/<model>` with a 16k context window and a low temperature. Ollama's OpenAI-compatible endpoint can't set the context per request, and its default is smaller than InvestiGenie's prompts, which would silently truncate them. The model is then warmed up, tested with the same JSON probe InvestiGenie uses, and registered.

**To switch models**, choose a different one in the model picker on the AI page. To add more:

```sh
./playground/playground.sh model add qwen2.5:3b llama3.2:3b   # pull, tune, register
./playground/playground.sh model list
./playground/playground.sh model rm llama3.2:3b
./playground/playground.sh up --models qwen2.5:7b,qwen2.5:3b   # choose the set at startup
```

Rough guide (Q4 weights; any [Ollama library](https://ollama.com/library) tag works):

| Model | Download | Notes |
| --- | --- | --- |
| `qwen2.5:7b` (default) | ~4.7 GB | Good JSON discipline for its size. Best default for the AI flow. |
| `qwen2.5:3b` | ~1.9 GB | Faster on CPU. Misses more of the multi-hop scenarios. |
| `llama3.2:3b` | ~2.0 GB | Useful for comparing model families at the same size. |
| `qwen2.5:1.5b`, `qwen2.5:0.5b` | ~1 GB, ~0.4 GB | Smoke tests of the pipeline. Expect malformed or shallow answers. |

**Other servers** such as vLLM, LM Studio, a llama.cpp server or a hosted API work through the OpenAI-compatible provider. Set these in `playground/.env`:

```sh
PLAYGROUND_AI_PROVIDER=openai-compatible
PLAYGROUND_AI_API_URL=http://host.docker.internal:1234/v1   # reachable from the investigenie container
PLAYGROUND_AI_API_KEY=anything
PLAYGROUND_AI_MODELS=qwen2.5-7b-instruct
```

**Timeouts.** On CPU, one investigation step can take minutes. In the AI page, set **Timeout** to 5-10 minutes for local models. InvestiGenie's save-time model probe uses `AI_PROBE_TIMEOUT_SECONDS`, which is 120 s in the playground.

## Evaluating the AI investigation flow

```sh
./playground/playground.sh eval                                   # first registered model, all scenarios
./playground/playground.sh eval --model all                       # compare every registered model
./playground/playground.sh eval --model 3b --scenario checkout-bad-deploy -v
./playground/playground.sh eval --model all --repeat 3                 # local models vary run to run
```

For each model and scenario, the harness does the following:

1. Starts a background investigation through the public API, using the scenario's question and time range.
2. Follows its progress.
3. Answers check-in pauses with "continue", up to `--max-continues` times.
4. Scores the final summary and findings against the scenario's keyword groups. A scenario passes when every group matches. The keywords are evidence only the data contains (the IP, the version, the replica name), never words from the question, so an answer that just restates the question scores zero. The `control-quiet-window` scenario passes only if no finding is rated high or critical, which catches models that invent incidents.
5. Deletes the investigation from the AI history. InvestiGenie keeps the 64 most recently updated conversations, so eval runs would otherwise push your own investigations out. Pass `--keep-history` to inspect runs in the UI.

Each failure is classified as a `wrong-answer` (completed but missed the root cause), `incomplete` (stopped or partial) or `pipeline-error` (timeout, unparseable model output, provider error). This separates "the model reasoned badly" from "the model or the flow broke".

`--fail-on pipeline-error` makes the exit code depend only on the flow breaking, not on wrong answers. The nightly `playground-eval.yml` workflow uses that with `qwen2.5:0.5b`, which is too small to answer well but quickly shows when a change breaks investigations. In GitHub Actions the scorecard is also written to the job summary.

It prints a scorecard (passes, keywords, failure kinds, average time, tokens) and writes the full results, including summaries and findings, to `playground/reports/ai-eval-<timestamp>.json`. Without Docker, run `python3 playground/ai_eval.py --help` directly against the published ports. It uses only the standard library.

## Configuration

Copy `playground/.env.example` to `playground/.env`. The main settings:

| Variable | Default | Purpose |
| --- | --- | --- |
| `PLAYGROUND_EVENTS`, `PLAYGROUND_DAYS`, `PLAYGROUND_SEED` | `50000`, `7`, `731` | Corpus size, history and repeatability (the oldest scenario needs ≥ 3 days) |
| `PLAYGROUND_SCENARIOS` | `all` | `all`, `none`, or a comma list of scenario ids |
| `PLAYGROUND_LIVE_EPS`, `PLAYGROUND_LIVE_INCIDENT` | `2`, `true` | Live traffic rate, and whether the ongoing incident keeps running |
| `PLAYGROUND_LIVE_RETENTION_DAYS` | `3` | Hourly cleanup of daily indices that hold only live traffic. The seeded history is never pruned. `0` disables it. |
| `PLAYGROUND_AI_PROVIDER` | `ollama` | `ollama`, `openai-compatible` or `none` |
| `PLAYGROUND_AI_MODELS` | `qwen2.5:7b` | Models to provision |
| `PLAYGROUND_AI_CONTEXT` | `16384` | `num_ctx` for the tuned Ollama copies |
| `PLAYGROUND_CLICKHOUSE` | `play` | `off` skips the public ClickHouse connection |

After changing corpus settings, run `./playground/playground.sh reseed`. To preview a corpus without Docker, run `python3 playground/generate_data.py --dry-run`.

The seeder's tests check that every scoring keyword actually appears in its scenario's data, that no keyword appears in the question, that the control window is clean, and that the history stream stays time-ordered. Run them after changing scenarios:

```sh
python3 -m unittest discover -s playground/tests
```

## Optional observability stack

```sh
./playground/playground.sh up --observability
```

This adds Loki (with the same logs as JSON lines, labelled by `service_name`, `level` and `environment`), Prometheus (scraping the seeder's `playground_http_requests_total`, `playground_http_request_duration_seconds` and `playground_scenario_active` metrics) and Mimir (Prometheus remote-write target). InvestiGenie has no Loki or Prometheus connector yet. These services are for building and testing one.

```promql
sum by (service, status) (rate(playground_http_requests_total[5m]))
histogram_quantile(0.95, sum by (le, service) (rate(playground_http_request_duration_seconds_bucket[5m])))
```

## Lifecycle

```sh
./playground/playground.sh status     # containers, seeding state, provisioned objects
./playground/playground.sh logs       # seeder, setup and ollama logs
./playground/playground.sh down       # stop; data and models are kept
./playground/playground.sh reset      # delete playground data, keep downloaded models
./playground/playground.sh reset --all
```

`reset` removes only volumes that exist, so it also works when optional observability services were never started. It keeps the Ollama model volume unless `--all` is specified.

The wrapper is a thin layer over `docker compose -f playground/compose.yaml` with the `ollama`, `observability` and `tools` profiles. It remembers the `up` options in `playground/.playground-state`.

## Troubleshooting

- **Elasticsearch exits with a `vm.max_map_count` error.** Raise that setting in Docker's Linux VM to at least `262144`.
- **The model is not registered, and setup logs `ai_model_test_failed`.** The model didn't answer the JSON probe in time. Raise `AI_PROBE_TIMEOUT_SECONDS`, or use a smaller model or `--ai host`, then run `./playground/playground.sh setup`.
- **`--ai host` on Linux.** Start Ollama with `OLLAMA_HOST=0.0.0.0` so containers can reach it.
- **The ClickHouse connection fails.** It needs outbound HTTPS to play.clickhouse.com. Set `PLAYGROUND_CLICKHOUSE=off` to skip it.

All ports are published only on `127.0.0.1`. None of these development services has authentication, so keep the stack local.

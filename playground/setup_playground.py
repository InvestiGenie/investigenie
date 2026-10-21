"""Provisions an InvestiGenie playground through its public API.

Creates (idempotently):

* the "Playground Elasticsearch" connection and the "playground-logs" data view over the seeded corpus;
* a "ClickHouse Playground" connection to the official public ClickHouse playground
  (https://play.clickhouse.com, read-only ``explorer`` user) plus data views over its sample tables;
* one InvestiGenie AI model profile per model in ``PLAYGROUND_AI_MODELS``. With the default Ollama
  provider each model is pulled, given a larger context window, warmed up and then registered, so
  switching models is just a matter of picking a different one in the AI page.

Usage::

    python setup_playground.py                 # everything
    python setup_playground.py models          # only (re)provision AI models
    python setup_playground.py models llama3.2:3b gemma3:4b
    python setup_playground.py list            # show connections, data views and AI models
    python setup_playground.py remove-model "llama3.2:3b (local Ollama)"

Only the Python standard library is used.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

INVESTIGENIE_URL = os.getenv("INVESTIGENIE_URL", "http://investigenie:3000").rstrip("/")
SEEDER_URL = os.getenv("SEEDER_URL", "http://seeder:8000").rstrip("/")
ELASTICSEARCH_URL = os.getenv("ELASTICSEARCH_CONNECTION_URL", "http://elasticsearch:9200").rstrip("/")

CLICKHOUSE_MODE = os.getenv("PLAYGROUND_CLICKHOUSE", "play").strip().lower()
CLICKHOUSE_URL = os.getenv("PLAYGROUND_CLICKHOUSE_URL", "https://play.clickhouse.com:443").strip()
CLICKHOUSE_USER = os.getenv("PLAYGROUND_CLICKHOUSE_USER", "explorer")
CLICKHOUSE_PASSWORD = os.getenv("PLAYGROUND_CLICKHOUSE_PASSWORD", "")
CLICKHOUSE_DATABASE = os.getenv("PLAYGROUND_CLICKHOUSE_DATABASE", "default")
CLICKHOUSE_DATA_VIEWS = os.getenv("PLAYGROUND_CLICKHOUSE_DATA_VIEWS", "github_events:created_at,hackernews:time")

AI_PROVIDER = os.getenv("PLAYGROUND_AI_PROVIDER", "ollama").strip().lower()
AI_MODELS = os.getenv("PLAYGROUND_AI_MODELS", "qwen2.5:7b")
OLLAMA_URL = os.getenv("PLAYGROUND_OLLAMA_URL", "http://ollama:11434").rstrip("/")
AI_CONTEXT = int(os.getenv("PLAYGROUND_AI_CONTEXT", "16384") or 16384)
AI_TEMPERATURE = float(os.getenv("PLAYGROUND_AI_TEMPERATURE", "0.1") or 0.1)
AI_API_URL = os.getenv("PLAYGROUND_AI_API_URL", "").rstrip("/")
AI_API_KEY = os.getenv("PLAYGROUND_AI_API_KEY", "")

ES_CONNECTION = "Playground Elasticsearch"
ES_DATA_VIEW = "playground-logs"
CH_CONNECTION = "ClickHouse Playground"


class ApiFailure(RuntimeError):
    def __init__(self, status, code, message):
        super().__init__(f"{status} {code}: {message}")
        self.status = status
        self.code = code


def log(message=""):
    print(f"[setup] {message}" if message else "", flush=True)


def call(url, method="GET", payload=None, headers=None, timeout=60):
    body = None if payload is None else json.dumps(payload).encode()
    request_headers = {"Accept": "application/json", **(headers or {})}
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, headers=request_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", errors="replace")
        try:
            detail = json.loads(raw).get("error", {})
            if isinstance(detail, str):
                detail = {"code": "error", "message": detail}
        except ValueError:
            detail = {"code": "http_error", "message": raw[:300]}
        raise ApiFailure(error.code, detail.get("code", "error"), detail.get("message", raw[:300])) from error


def api(path, method="GET", payload=None, headers=None, timeout=60):
    return call(f"{INVESTIGENIE_URL}{path}", method, payload, headers, timeout)


def wait_for(name, url, timeout):
    deadline = time.monotonic() + timeout
    last = None
    announced = False
    while time.monotonic() < deadline:
        try:
            call(url, timeout=5)
            return True
        except (OSError, ApiFailure) as error:
            last = error
            if not announced:
                log(f"Waiting for {name}...")
                announced = True
            time.sleep(3)
    log(f"{name} did not become ready: {last}")
    return False


def slug(name):
    return name.strip().lower().replace(" ", "-")


# --------------------------------------------------------------------------------------------
# Connections and data views


def ensure_connection(name, payload):
    connection_id = slug(name)
    existing = {item["id"] for item in api("/api/connections")["connections"]}
    if connection_id in existing:
        log(f"Connection '{name}' already exists")
        return connection_id
    api("/api/connections", "POST", {"name": name, **payload}, timeout=120)
    log(f"Created connection '{name}'")
    return connection_id


def ensure_data_view(name, connection_id, index, time_field, retries=1, delay=10):
    view_id = slug(name)
    if any(view["id"] == view_id for view in api("/api/data-views")["data_views"]):
        log(f"Data view '{name}' already exists")
        return True
    for attempt in range(retries):
        try:
            api(
                "/api/data-views",
                "POST",
                {"name": name, "connection_id": connection_id, "index": index, "time_field": time_field},
                timeout=120,
            )
            log(f"Created data view '{name}' ({index}, time field {time_field})")
            return True
        except ApiFailure as error:
            if attempt + 1 == retries:
                log(f"Could not create data view '{name}' on {index}: {error}")
                return False
            time.sleep(delay)
    return False


def provision_elasticsearch():
    connection_id = ensure_connection(ES_CONNECTION, {"connection_type": "elasticsearch", "url": ELASTICSEARCH_URL})
    if not wait_for("the seeder to finish seeding", f"{SEEDER_URL}/healthz", timeout=1800):
        return False
    return ensure_data_view(ES_DATA_VIEW, connection_id, "investigenie-logs-*", "@timestamp", retries=6)


def provision_clickhouse():
    if CLICKHOUSE_MODE in ("off", "none", "false", "0", ""):
        log("ClickHouse connection disabled (PLAYGROUND_CLICKHOUSE=off)")
        return True
    try:
        connection_id = ensure_connection(
            CH_CONNECTION,
            {
                "connection_type": "clickhouse",
                "url": CLICKHOUSE_URL,
                "database": CLICKHOUSE_DATABASE,
                "username": CLICKHOUSE_USER,
                "password": CLICKHOUSE_PASSWORD,
            },
        )
    except (ApiFailure, OSError) as error:
        log(f"Could not connect to the ClickHouse playground at {CLICKHOUSE_URL}: {error}")
        log("  It needs outbound internet access from Docker. Set PLAYGROUND_CLICKHOUSE=off to skip it.")
        return False
    ok = True
    for spec in [item.strip() for item in CLICKHOUSE_DATA_VIEWS.split(",") if item.strip()]:
        table, _, time_field = spec.partition(":")
        if not time_field:
            log(f"Ignoring ClickHouse data view '{spec}': use table:time_field")
            continue
        if not ensure_data_view(f"clickhouse-{table}", connection_id, table, time_field):
            ok = False
            try:
                tables = [item["name"] for item in api(f"/api/connections/{connection_id}/indices")["indices"]]
                log(f"  Tables visible on the ClickHouse playground: {', '.join(tables[:40])}{' ...' if len(tables) > 40 else ''}")
            except (ApiFailure, OSError, KeyError):
                pass
    return ok


# --------------------------------------------------------------------------------------------
# AI models


def model_list(raw):
    return [item.strip() for item in raw.replace(" ", ",").split(",") if item.strip()]


def profile_name(model, provider):
    suffix = "local Ollama" if provider == "ollama" else "OpenAI-compatible"
    return f"{model} ({suffix})"[:128]


def ollama_tags():
    return {item["name"] for item in call(f"{OLLAMA_URL}/api/tags", timeout=30).get("models", [])}


def ollama_pull(model):
    names = ollama_tags()
    if model in names or f"{model}:latest" in names:
        log(f"Ollama already has {model}")
        return
    log(f"Pulling {model} into Ollama (first time only; 7B models are several GB)...")
    req = urllib.request.Request(
        f"{OLLAMA_URL}/api/pull",
        data=json.dumps({"model": model, "stream": True}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    last_report = 0.0
    with urllib.request.urlopen(req, timeout=7200) as response:
        for line in response:
            if not line.strip():
                continue
            update = json.loads(line)
            if "error" in update:
                raise RuntimeError(f"Ollama could not pull {model}: {update['error']}")
            total, done = update.get("total"), update.get("completed")
            if total and done and time.monotonic() - last_report > 10:
                log(f"  {model}: {update.get('status', '')} {done / total:6.1%} of {total / 1e9:.1f} GB")
                last_report = time.monotonic()
            elif update.get("status") == "success":
                log(f"  {model}: pulled")


def ollama_tuned(model):
    """Creates investigenie/<model> with a context window large enough for investigation prompts.

    Ollama's OpenAI-compatible endpoint cannot set num_ctx per request and its default context is
    smaller than InvestiGenie's prompts (up to 64 KiB), which would silently truncate them.
    """
    base = model.rsplit("/", 1)[-1]
    tuned = f"investigenie/{base}" if ":" in base else f"investigenie/{base}:latest"
    try:
        call(
            f"{OLLAMA_URL}/api/create",
            "POST",
            {
                "model": tuned,
                "from": model,
                "stream": False,
                "parameters": {"num_ctx": AI_CONTEXT, "temperature": AI_TEMPERATURE},
            },
            timeout=600,
        )
        log(f"  Created {tuned} (num_ctx={AI_CONTEXT}, temperature={AI_TEMPERATURE})")
        return tuned
    except (ApiFailure, OSError) as error:
        log(f"  Could not create a tuned copy of {model} ({error}); using it with Ollama's default context")
        return model


def ollama_warm(model):
    started = time.monotonic()
    call(f"{OLLAMA_URL}/api/generate", "POST", {"model": model, "prompt": "", "keep_alive": "60m", "stream": False}, timeout=900)
    probe = {
        "model": model,
        "messages": [
            {"role": "system", "content": "Return exactly a JSON object with the boolean field ok set to true."},
            {"role": "user", "content": '{"probe":true}'},
        ],
        "response_format": {"type": "json_object"},
        "max_tokens": 32,
    }
    loaded = time.monotonic()
    reply = call(f"{OLLAMA_URL}/v1/chat/completions", "POST", probe, headers={"Authorization": "Bearer ollama"}, timeout=900)
    answered = time.monotonic()
    content = reply["choices"][0]["message"].get("content", "")
    log(f"  Loaded in {loaded - started:.1f}s; JSON probe answered in {answered - loaded:.1f}s: {content.strip()[:60]}")
    return answered - loaded


def register_model(name, api_url, model, api_key):
    payload = {"name": name, "api_url": api_url, "model": model, "api_key": api_key}
    existing = next((item for item in api("/api/ai/models")["models"] if item["name"].lower() == name.lower()), None)
    for attempt in range(3):
        try:
            if existing is None:
                api("/api/ai/models", "POST", payload, timeout=900)
                log(f"Registered AI model '{name}' -> {model} at {api_url}")
            else:
                api(f"/api/ai/models/{existing['id']}", "PUT", payload, headers={"If-Match": f'"{existing["revision"]}"'}, timeout=900)
                log(f"Updated AI model '{name}' -> {model} at {api_url}")
            return True
        except ApiFailure as error:
            log(f"  InvestiGenie rejected '{name}' (attempt {attempt + 1}/3): {error}")
            if error.code == "ai_model_test_failed":
                log("  The save-time probe failed. Slow hardware? Raise AI_PROBE_TIMEOUT_SECONDS for the investigenie service.")
            time.sleep(5)
    return False


def provision_models(models):
    if AI_PROVIDER in ("none", "off", ""):
        log("AI provisioning disabled (PLAYGROUND_AI_PROVIDER=none)")
        return True
    if not models:
        log("No models listed in PLAYGROUND_AI_MODELS")
        return True
    if AI_PROVIDER in ("openai", "openai-compatible", "openai_compatible"):
        if not AI_API_URL or not AI_API_KEY:
            log("PLAYGROUND_AI_API_URL and PLAYGROUND_AI_API_KEY are required for the openai-compatible provider")
            return False
        return all([register_model(profile_name(model, "openai"), AI_API_URL, model, AI_API_KEY) for model in models])
    if AI_PROVIDER != "ollama":
        log(f"Unknown PLAYGROUND_AI_PROVIDER '{AI_PROVIDER}' (use ollama, openai-compatible or none)")
        return False
    if not wait_for(f"Ollama at {OLLAMA_URL}", f"{OLLAMA_URL}/api/version", timeout=120):
        log("Ollama is not reachable, so no local model was registered. Start it with:")
        log("  ./playground/playground.sh up --ai docker   (Ollama in Docker, CPU or NVIDIA GPU)")
        log("  ./playground/playground.sh up --ai host     (Ollama installed natively; fastest on Apple silicon)")
        return False
    ok = True
    for model in models:
        try:
            ollama_pull(model)
            served = ollama_tuned(model)
            ollama_warm(served)
            ok = register_model(profile_name(model, "ollama"), f"{OLLAMA_URL}/v1", served, "ollama") and ok
        except (ApiFailure, OSError, RuntimeError, KeyError, ValueError) as error:
            log(f"Could not provision {model}: {error}")
            ok = False
    return ok


# --------------------------------------------------------------------------------------------


def show():
    log("Connections:")
    for item in api("/api/connections")["connections"]:
        log(f"  {item['id']:<28} {item['connection_type']:<14} {item['url']}")
    log("Data views:")
    for item in api("/api/data-views")["data_views"]:
        log(f"  {item['id']:<28} {item['connection_id']:<28} {item['index']} ({item['time_field']})")
    log("AI models:")
    for item in api("/api/ai/models")["models"]:
        log(f"  {item['name']:<40} {item['model']:<32} {item['api_url']}")


def remove_model(name):
    for item in api("/api/ai/models")["models"]:
        wanted = name.lower()
        if (
            wanted in (item["name"].lower(), item["model"].lower(), item["id"])
            or item["model"].lower().endswith("/" + wanted)
            or item["name"].lower().startswith(wanted + " (")
        ):
            api(f"/api/ai/models/{item['id']}", "DELETE", headers={"If-Match": f'"{item["revision"]}"'})
            log(f"Removed AI model '{item['name']}'")
            return True
    log(f"No AI model named '{name}'")
    return False


def summary():
    log()
    log("Playground ready: http://127.0.0.1:3000")
    try:
        truth = call(f"{SEEDER_URL}/scenarios", timeout=10)
        log("Try these in the AI page (data view 'playground-logs'); the time range is printed in UTC:")
        for item in truth["scenarios"]:
            start = time.strftime("%Y-%m-%d %H:%M", time.gmtime(item["time_range"]["start"] / 1000))
            end = time.strftime("%H:%M", time.gmtime(item["time_range"]["end"] / 1000))
            window = "last 30 minutes" if item["live"] and truth.get("live_traffic") else f"{start} -> {end} UTC"
            log(f"  [{item['difficulty']:<6}] {item['question']}  ({window})")
        log("Score a model on all scenarios with: ./playground/playground.sh eval")
    except (ApiFailure, OSError, KeyError):
        pass


def main():
    parser = argparse.ArgumentParser(description="Provision the InvestiGenie playground")
    parser.add_argument("command", nargs="?", default="all", choices=("all", "models", "list", "remove-model"))
    parser.add_argument("names", nargs="*", help="models for 'models', or the model to remove")
    args = parser.parse_args()

    if not wait_for("InvestiGenie", f"{INVESTIGENIE_URL}/api/health/ready", timeout=600):
        return 1
    if args.command == "list":
        show()
        return 0
    if args.command == "remove-model":
        return 0 if args.names and all([remove_model(name) for name in args.names]) else 1
    if args.command == "models":
        return 0 if provision_models(args.names or model_list(AI_MODELS)) else 1

    results = [provision_elasticsearch(), provision_clickhouse(), provision_models(model_list(AI_MODELS))]
    show()
    summary()
    if not all(results):
        log("Finished with warnings (see above). The rest of the playground is usable.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

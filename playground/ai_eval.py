"""Runs InvestiGenie AI investigations against the playground's injected incidents and scores them.

For every selected model and scenario it starts a background investigation through the public API
with the scenario's question and time range, follows its progress, automatically continues when the
job pauses for a decision, and checks the final summary and findings for the root-cause keywords
published by the seeder on ``/scenarios``. Results are printed as a scorecard and written to a JSON
report, so models (and prompt/orchestration changes) can be compared on the same corpus.

Usage::

    python ai_eval.py                                  # first registered model, all scenarios
    python ai_eval.py --model all                      # compare every registered model
    python ai_eval.py --model qwen2.5 --scenario checkout-bad-deploy
    python ai_eval.py --list

Only the Python standard library is used.
"""

import argparse
import datetime as dt
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections import Counter

INVESTIGENIE_URL = os.getenv("INVESTIGENIE_URL", "http://127.0.0.1:3000").rstrip("/")
SEEDER_URL = os.getenv("SEEDER_URL", "http://127.0.0.1:8000").rstrip("/")
TERMINAL = {"completed", "terminated", "failed"}


def call(url, method="GET", payload=None, timeout=60):
    body = None if payload is None else json.dumps(payload).encode()
    headers = {"Accept": "application/json"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return json.loads(response.read() or b"null")
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {url} returned {error.code}: {raw[:300]}") from error


def api(path, method="GET", payload=None):
    return call(f"{INVESTIGENIE_URL}{path}", method, payload)


def pick_models(selectors):
    models = api("/api/ai/models")["models"]
    if not models:
        raise SystemExit("No AI models are registered. Run: ./playground/playground.sh model add qwen2.5:7b")
    if not selectors:
        return models[:1]
    if any(selector.lower() == "all" for selector in selectors):
        return models
    chosen = []
    for selector in selectors:
        matches = [m for m in models if selector.lower() in (m["name"] + " " + m["model"]).lower() or selector == m["id"]]
        if not matches:
            raise SystemExit(f"No registered AI model matches '{selector}'. Registered: {', '.join(m['name'] for m in models)}")
        chosen.extend(m for m in matches if m not in chosen)
    return chosen


def result_text(result):
    parts = [result.get("summary", "")]
    for finding in result.get("findings", []):
        parts += [finding.get("title", ""), finding.get("detail", "")]
    return "\n".join(parts).lower()


QUIET_FAILING_SEVERITIES = {"critical", "high"}
MAX_CONVERSATIONS = 64  # InvestiGenie keeps this many conversations and evicts the least recently updated


def score(result, expected):
    text = result_text(result)
    groups = []
    for group in expected:
        hit = next((term for term in group if term.lower() in text), None)
        groups.append({"any_of": group, "matched": hit})
    matched = sum(1 for group in groups if group["matched"])
    return matched, len(groups), groups


def score_quiet(result):
    """Negative control: the model should not report a high/critical incident in normal traffic."""
    alarms = [f.get("title", "") for f in result.get("findings", []) if str(f.get("severity", "")).lower() in QUIET_FAILING_SEVERITIES]
    return (0 if alarms else 1), 1, [{"no_high_severity_findings": not alarms, "flagged": alarms}]


def outcome_kind(status, result, passed):
    if passed:
        return "pass"
    if status == "failed" or not result:
        return "pipeline-error"  # timeouts, unparseable model output, provider errors
    if status == "terminated" or result.get("complete") is False:
        return "incomplete"
    return "wrong-answer"


def delete_conversation(job_id):
    """Investigations are saved as conversations; remove eval runs so history doesn't hit its cap."""
    try:
        conversation = api(f"/api/ai/conversations/{job_id}")["conversation"]
        req = urllib.request.Request(
            f"{INVESTIGENIE_URL}/api/ai/conversations/{job_id}",
            method="DELETE",
            headers={"If-Match": f'"{conversation["revision"]}"'},
        )
        urllib.request.urlopen(req, timeout=30).read()
    except (RuntimeError, OSError, KeyError):
        pass


def check_history_capacity(planned, keep_history):
    try:
        used = len(api("/api/ai/conversations")["conversations"])
    except (RuntimeError, OSError, KeyError):
        return
    evicted = used + planned - MAX_CONVERSATIONS
    if keep_history and evicted > 0:
        print(
            f"Note: keeping {planned} eval runs in the AI history ({used}/{MAX_CONVERSATIONS} used) will evict "
            f"up to {min(evicted, used)} of the oldest existing conversations.",
            flush=True,
        )


def run_one(model, scenario, data_views, args):
    payload = {
        "question": scenario["question"],
        "data_view_ids": data_views,
        "ai_model_id": model["id"],
        "time_range": scenario["time_range"],
        "ai_timeout_seconds": args.ai_timeout,
        "time_budget_seconds": args.budget,
    }
    started = time.monotonic()
    job = api("/api/ai/investigations", "POST", payload)["investigation"]
    continues = 0
    last_stage = None
    stopped = False
    while job["status"] not in TERMINAL:
        if not stopped and time.monotonic() - started > args.max_minutes * 60:
            stopped = True
            print(f"      giving up after {args.max_minutes:g} minutes", flush=True)
            try:
                api(f"/api/ai/investigations/{job['id']}/decision", "POST", {"action": "terminate"})
            except RuntimeError:
                pass
        elif job["status"] == "awaiting_decision":
            action = "continue" if continues < args.max_continues and not stopped else "terminate"
            continues += action == "continue"
            print(f"      paused ({job.get('pause_reason') or 'time budget'}); {action}", flush=True)
            api(f"/api/ai/investigations/{job['id']}/decision", "POST", {"action": action})
        time.sleep(args.poll)
        job = api(f"/api/ai/investigations/{job['id']}")["investigation"]
        progress = job.get("progress", {})
        stage = progress.get("stage")
        if stage != last_stage and args.verbose:
            print(
                f"      {stage}: {progress.get('records_examined', 0)} records, {progress.get('batches_processed', 0)} batches", flush=True
            )
            last_stage = stage
    elapsed = time.monotonic() - started
    if not args.keep_history:
        delete_conversation(job["id"])
    result = job.get("result") or {}
    if scenario.get("expect_quiet"):
        matched, total, groups = score_quiet(result) if result else (0, 1, [])
    else:
        matched, total, groups = score(result, scenario["expected_keywords"]) if result else (0, len(scenario["expected_keywords"]), [])
    passed = bool(result) and matched == total
    tokens = sum((usage.get("total_tokens") or 0) for usage in result.get("token_usage", []))
    return {
        "model": model["name"],
        "model_id": model["model"],
        "scenario": scenario["id"],
        "difficulty": scenario["difficulty"],
        "status": job["status"],
        "error": job.get("error"),
        "complete": result.get("complete"),
        "score": matched,
        "max_score": total,
        "passed": passed,
        "outcome": outcome_kind(job["status"], result, passed),
        "keyword_groups": groups,
        "seconds": round(elapsed, 1),
        "records_examined": job.get("progress", {}).get("records_examined"),
        "total_tokens": tokens,
        "continues": continues,
        "summary": result.get("summary"),
        "findings": result.get("findings", []),
        "caveats": result.get("caveats", []),
    }


def write_markdown_summary(path, truth, models, results):
    lines = [f"### AI investigation eval (corpus anchored {truth.get('anchor')})", "", "| model | scenario | result | keywords | seconds |"]
    lines.append("| --- | --- | --- | --- | --- |")
    for row in results:
        verdict = "pass" if row["passed"] else row["outcome"]
        lines.append(f"| {row['model']} | {row['scenario']} | {verdict} | {row['score']}/{row['max_score']} | {row['seconds']} |")
    with open(path, "a") as handle:
        handle.write("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description="Score InvestiGenie AI investigations on the playground scenarios")
    parser.add_argument("--model", action="append", default=[], help="model name/id substring; repeat, or 'all'")
    parser.add_argument("--scenario", action="append", default=[], help="scenario id; repeat for several (default: all)")
    parser.add_argument("--data-view", action="append", default=[], help="data view id (default: playground-logs)")
    parser.add_argument(
        "--ai-timeout",
        type=int,
        default=int(os.getenv("EVAL_AI_TIMEOUT", "300")),
        help="seconds to wait for each model reply (5-600; local CPU models need minutes)",
    )
    parser.add_argument(
        "--budget",
        type=int,
        default=int(os.getenv("EVAL_TIME_BUDGET", "900")),
        help="seconds before the investigation pauses for a decision (30-3600)",
    )
    parser.add_argument("--max-continues", type=int, default=3, help="automatic 'continue' decisions per run")
    parser.add_argument("--max-minutes", type=float, default=60, help="hard stop per investigation")
    parser.add_argument("--poll", type=float, default=3.0)
    parser.add_argument("--repeat", type=int, default=1, help="run each scenario N times (local models are not deterministic)")
    parser.add_argument(
        "--keep-history", action="store_true", help="keep eval investigations in the AI history (capped at 64; the oldest are evicted)"
    )
    parser.add_argument(
        "--fail-on",
        choices=("any", "pipeline-error", "never"),
        default="any",
        help="exit non-zero when any run fails (default), only when the flow itself breaks, or never",
    )
    parser.add_argument("--summary-file", default=os.getenv("GITHUB_STEP_SUMMARY"), help="append a Markdown scorecard here")
    parser.add_argument("--report-dir", default=os.getenv("EVAL_REPORT_DIR", "reports"))
    parser.add_argument("--list", action="store_true", help="list scenarios and models, then exit")
    parser.add_argument("-v", "--verbose", action="store_true", help="print investigation stages")
    args = parser.parse_args()

    truth = call(f"{SEEDER_URL}/scenarios")
    scenarios = truth["scenarios"]
    if args.list:
        for item in scenarios:
            print(f"{item['id']:<32} {item['difficulty']:<7} {item['question']}")
        for model in api("/api/ai/models")["models"]:
            print(f"model: {model['name']}  ({model['model']} @ {model['api_url']})")
        return 0
    if args.scenario:
        unknown = set(args.scenario) - {item["id"] for item in scenarios}
        if unknown:
            raise SystemExit(f"Unknown scenario(s): {', '.join(sorted(unknown))}")
        scenarios = [item for item in scenarios if item["id"] in args.scenario]
    models = pick_models(args.model)
    data_views = args.data_view or ["playground-logs"]

    check_history_capacity(len(scenarios) * len(models) * args.repeat, args.keep_history)
    print(
        f"Corpus anchored at {truth['anchor']}; {len(scenarios)} scenario(s) x {len(models)} model(s)"
        f"{f' x {args.repeat} runs' if args.repeat > 1 else ''}",
        flush=True,
    )
    results = []
    for model in models:
        print(f"\n== {model['name']}  ({model['model']})", flush=True)
        for scenario, run in [(s, r) for s in scenarios for r in range(args.repeat)]:
            print(f"   {scenario['id']}{f' (run {run + 1})' if args.repeat > 1 else ''}: {scenario['question']}", flush=True)
            try:
                outcome = run_one(model, scenario, data_views, args)
            except (RuntimeError, OSError) as error:  # keep going and still write the report
                outcome = {
                    "model": model["name"],
                    "model_id": model["model"],
                    "scenario": scenario["id"],
                    "difficulty": scenario["difficulty"],
                    "status": "error",
                    "error": str(error),
                    "outcome": "pipeline-error",
                    "score": 0,
                    "passed": False,
                    "seconds": 0,
                    "max_score": 1 if scenario.get("expect_quiet") else len(scenario["expected_keywords"]),
                }
            outcome["run"] = run + 1
            results.append(outcome)
            verdict = "PASS" if outcome["passed"] else f"FAIL ({outcome['outcome']})"
            detail = f" error={outcome['error']}" if outcome.get("error") else ""
            print(
                f"      {verdict} {outcome['score']}/{outcome['max_score']} keywords, {outcome['status']}, "
                f"{outcome['seconds']}s, {outcome.get('total_tokens') or 0} tokens{detail}",
                flush=True,
            )
            if outcome.get("summary"):
                print(f"      summary: {outcome['summary'][:300]}", flush=True)

    print("\nScorecard")
    print(f"{'model':<36} {'pass':>6} {'keywords':>9} {'wrong':>6} {'incompl':>8} {'errors':>7} {'avg s':>7} {'tokens':>8}")
    for model in models:
        rows = [r for r in results if r["model"] == model["name"]]
        passed = sum(r["passed"] for r in rows)
        keywords = sum(r["score"] for r in rows)
        possible = sum(r["max_score"] for r in rows)
        seconds = sum(r["seconds"] for r in rows) / max(1, len(rows))
        tokens = sum(r.get("total_tokens") or 0 for r in rows)
        kinds = Counter(r["outcome"] for r in rows)
        print(
            f"{model['name'][:36]:<36} {passed:>3}/{len(rows):<2} {keywords:>4}/{possible:<4} {kinds['wrong-answer']:>6} "
            f"{kinds['incomplete']:>8} {kinds['pipeline-error']:>7} {seconds:>7.0f} {tokens:>8}"
        )

    os.makedirs(args.report_dir, exist_ok=True)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = os.path.join(args.report_dir, f"ai-eval-{stamp}.json")
    with open(path, "w") as handle:
        json.dump(
            {
                "generated_at": stamp,
                "corpus": truth.get("corpus"),
                "anchor": truth.get("anchor"),
                "data_views": data_views,
                "results": results,
            },
            handle,
            indent=2,
        )
    print(f"\nReport written to {path}")
    if args.summary_file:
        write_markdown_summary(args.summary_file, truth, models, results)
    if args.fail_on == "never":
        return 0
    if args.fail_on == "pipeline-error":
        return 2 if any(r["outcome"] == "pipeline-error" for r in results) else 0
    return 0 if all(r["passed"] for r in results) else 2


if __name__ == "__main__":
    sys.exit(main())

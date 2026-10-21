"""InvestiGenie playground seeder.

Writes a deterministic, realistic log corpus to Elasticsearch and (optionally) Loki:

* baseline traffic for five services with a day/night cycle, normal 4xx noise and a small
  amount of unrelated 5xx noise;
* a set of injected incidents ("scenarios") with known root causes, published as ground truth
  on ``/scenarios`` so AI investigations can be scored (see ``ai_eval.py``);
* optional live traffic after seeding, including an ongoing incident so investigations over the
  default "last 30 minutes" window have something to find.

Seeding is idempotent: the corpus is anchored to the time it was first generated and recorded in
each sink, so container restarts go straight to live mode instead of writing duplicates.

Only the Python standard library is used. Run ``python generate_data.py --dry-run`` to print the
corpus statistics and scenario samples without contacting any service.
"""

import argparse
import bisect
import datetime as dt
import hashlib
import heapq
import json
import math
import os
import random
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter, defaultdict
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CORPUS_VERSION = 2
BATCH_SIZE = 500
LOG_INDEX_PREFIX = "investigenie-logs-"
META_INDEX = "investigenie-playground-meta"
ENVIRONMENT = "playground"

SERVICES = ("api-gateway", "checkout", "catalog", "identity", "payments")
SERVICE_WEIGHTS = (30, 22, 22, 12, 14)
SERVICE_VERSIONS = {
    "api-gateway": "2.4.1",
    "checkout": "1.14.2",
    "catalog": "3.2.0",
    "identity": "1.9.3",
    "payments": "2.1.7",
}
SERVICE_TEAMS = {
    "api-gateway": "platform",
    "checkout": "commerce",
    "catalog": "discovery",
    "identity": "security",
    "payments": "commerce",
}
SERVICE_LATENCY = {  # median seconds
    "api-gateway": 0.045,
    "checkout": 0.16,
    "catalog": 0.09,
    "identity": 0.07,
    "payments": 0.22,
}
REGIONS = ("us-east-1", "us-west-2", "eu-west-1")
REGION_WEIGHTS = (45, 25, 30)
ROUTES = {
    "api-gateway": (("GET", "/api/v1/search"), ("POST", "/api/v1/investigations"), ("GET", "/api/v1/recommendations")),
    "checkout": (("GET", "/api/orders/{order_id}"), ("POST", "/api/orders"), ("GET", "/api/cart")),
    "catalog": (("GET", "/api/products/{product_id}"), ("GET", "/api/categories"), ("GET", "/api/products")),
    "identity": (("POST", "/api/sessions"), ("DELETE", "/api/sessions/{session_id}"), ("GET", "/api/users/me")),
    "payments": (("POST", "/api/payments"), ("GET", "/api/payments/{payment_id}"), ("POST", "/api/refunds")),
}
BASELINE_STATUS = (
    (200, 86.0),
    (201, 6.0),
    (204, 1.0),
    (400, 2.4),
    (401, 0.9),
    (404, 2.4),
    (409, 0.4),
    (429, 0.3),
    (500, 0.35),
    (502, 0.1),
    (503, 0.15),
)
CLIENT_ERRORS = {
    400: (
        "Request validation failed: field 'quantity' must be a positive integer",
        "Request validation failed: missing required field 'currency'",
        "Malformed JSON body: unexpected end of input",
    ),
    401: ("Missing or expired access token",),
    404: ("Resource not found",),
    409: ("Idempotency key already used with a different request body",),
    429: ("Rate limit exceeded for API key",),
}
SERVER_NOISE = {  # unrelated background failures so incidents have to be told apart from noise
    500: ("Unhandled exception while serialising response", "Timeout reading from cache cluster redis-main after 250ms"),
    502: ("Upstream connection reset by peer",),
    503: ("Service temporarily unavailable during pod shutdown",),
}
APP_LOGS = {
    "INFO": (
        "Cache refresh completed: {n} keys in {ms}ms",
        "Kafka consumer group {service}-events rebalanced: {n} partitions assigned",
        "Scheduled job cleanup-expired-sessions finished in {ms}ms",
        "Feature flag snapshot reloaded ({n} flags)",
        "Health check passed for dependency postgres-primary",
    ),
    "WARN": (
        "GC pause of {ms}ms exceeded target of 200ms",
        "Retrying request to inventory-service (attempt 2/3)",
        "Slow cache lookup for key prefix session:* took {ms}ms",
    ),
}
HISTOGRAM_BOUNDS = (0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, math.inf)

ES_TEMPLATE = {
    "index_patterns": [f"{LOG_INDEX_PREFIX}*"],
    "priority": 500,
    "template": {
        "settings": {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "5s"},
        "mappings": {
            "properties": {
                "@timestamp": {"type": "date"},
                "message": {"type": "text"},
                "log": {"properties": {"level": {"type": "keyword"}, "logger": {"type": "keyword"}}},
                "service": {
                    "properties": {
                        "name": {"type": "keyword"},
                        "version": {"type": "keyword"},
                        "environment": {"type": "keyword"},
                    }
                },
                "event": {
                    "properties": {
                        "dataset": {"type": "keyword"},
                        "duration": {"type": "long"},
                        "outcome": {"type": "keyword"},
                    }
                },
                "http": {
                    "properties": {
                        "request": {"properties": {"method": {"type": "keyword"}}},
                        "response": {"properties": {"status_code": {"type": "integer"}}},
                    }
                },
                "url": {"properties": {"path": {"type": "keyword"}}},
                "host": {"properties": {"name": {"type": "keyword"}}},
                "trace": {"properties": {"id": {"type": "keyword"}}},
                "user": {"properties": {"id": {"type": "keyword"}}},
                "user_agent": {"properties": {"original": {"type": "keyword"}}},
                "client": {"properties": {"ip": {"type": "ip"}}},
                "labels": {
                    "properties": {
                        "region": {"type": "keyword"},
                        "team": {"type": "keyword"},
                    }
                },
                "error": {
                    "properties": {
                        "code": {"type": "keyword"},
                        "type": {"type": "keyword"},
                        "message": {"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 512}}},
                        "stack_trace": {"type": "text"},
                    }
                },
            }
        },
    },
}


# --------------------------------------------------------------------------------------------
# HTTP helpers


class HttpFailure(RuntimeError):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


def request(url, method="GET", body=None, content_type="application/json", timeout=30, headers=None):
    all_headers = {"Accept": "application/json"}
    if body is not None:
        all_headers["Content-Type"] = content_type
    all_headers.update(headers or {})
    req = urllib.request.Request(url, data=body, headers=all_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")[:500]
        raise HttpFailure(error.code, f"{method} {url} returned {error.code}: {detail}") from error


def resolvable(url):
    host = urllib.parse.urlparse(url).hostname
    try:
        socket.getaddrinfo(host, None)
        return True
    except OSError:
        return False


def wait_for_service(name, url, timeout=240, headers=None):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            request(url, timeout=5, headers=headers)
            log(f"{name} is ready")
            return
        except (OSError, HttpFailure) as error:
            last_error = error
            time.sleep(2)
    raise RuntimeError(f"Timed out waiting for {name}: {last_error}")


def log(message):
    print(f"[seeder] {message}", flush=True)


# --------------------------------------------------------------------------------------------
# Event model


def iso(ts):
    return dt.datetime.fromtimestamp(ts, dt.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def trace_id(rng):
    return f"{rng.getrandbits(128):032x}"


def pods_for(service):
    digest = hashlib.sha1(service.encode()).hexdigest()
    return [f"{service}-{digest[:6]}-{digest[6 + i * 5 : 11 + i * 5]}" for i in range(3)]


PODS = {service: pods_for(service) for service in SERVICES}


def weighted(rng, pairs):
    values, weights = zip(*pairs)
    return rng.choices(values, weights=weights, k=1)[0]


def latency_for(rng, service, scale=1.0):
    return min(rng.lognormvariate(math.log(SERVICE_LATENCY[service] * scale), 0.75), 25.0)


def level_for(status):
    return "ERROR" if status >= 500 else "WARN" if status >= 400 and status != 404 else "INFO"


def access_event(rng, ts, service, method, path, status, latency, **extra):
    """Builds an ECS-shaped HTTP access log event."""
    event = {
        "@timestamp": iso(ts),
        "message": f"{method} {path} {status} {latency * 1000:.0f}ms",
        "log": {"level": extra.pop("level", level_for(status)), "logger": extra.pop("logger", f"{service}.http")},
        "service": {
            "name": service,
            "version": extra.pop("version", SERVICE_VERSIONS[service]),
            "environment": ENVIRONMENT,
        },
        "event": {
            "dataset": f"{service}.access",
            "duration": max(1, int(latency * 1_000_000_000)),
            "outcome": "failure" if status >= 400 else "success",
        },
        "http": {"request": {"method": method}, "response": {"status_code": status}},
        "url": {"path": path},
        "host": {"name": extra.pop("host", rng.choice(PODS[service]))},
        "trace": {"id": extra.pop("trace", None) or trace_id(rng)},
        "user": {"id": extra.pop("user", None) or f"user-{rng.randrange(1, 20000):05d}"},
        "user_agent": {"original": extra.pop("user_agent", "shop-web/5.12 (+https://shop.example)")},
        "client": {"ip": extra.pop("client_ip", None) or f"198.51.100.{rng.randrange(1, 255)}"},
        "labels": {"region": extra.pop("region", None) or weighted(rng, zip(REGIONS, REGION_WEIGHTS)), "team": SERVICE_TEAMS[service]},
    }
    error = extra.pop("error", None)
    if error:
        event["error"] = error
    if extra:
        raise ValueError(f"Unknown event attributes: {sorted(extra)}")
    return event


def app_event(rng, ts, service, level, message, logger=None, dataset=None, region=None, host=None, error=None, version=None):
    """Builds an ECS-shaped application (non-HTTP) log event."""
    event = {
        "@timestamp": iso(ts),
        "message": message,
        "log": {"level": level, "logger": logger or f"{service}.app"},
        "service": {"name": service, "version": version or SERVICE_VERSIONS[service], "environment": ENVIRONMENT},
        "event": {"dataset": dataset or f"{service}.app"},
        "host": {"name": host or rng.choice(PODS[service])},
        "labels": {"region": region or weighted(rng, zip(REGIONS, REGION_WEIGHTS)), "team": SERVICE_TEAMS[service]},
    }
    if error:
        event["error"] = error
    return event


def _dig(value, path):
    for key in path:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


# --------------------------------------------------------------------------------------------
# Baseline traffic


class Baseline:
    """Background traffic. ``version_overrides`` lets scenarios model deployments."""

    def __init__(self, version_overrides=None):
        self.version_overrides = version_overrides or []

    def version(self, service, ts):
        for override_service, start, end, version in self.version_overrides:
            if service == override_service and start <= ts < end:
                return version
        return SERVICE_VERSIONS[service]

    def event(self, rng, ts):
        service = rng.choices(SERVICES, weights=SERVICE_WEIGHTS, k=1)[0]
        if rng.random() < 0.12:
            level = "WARN" if rng.random() < 0.2 else "INFO"
            template = rng.choice(APP_LOGS[level])
            message = template.format(n=rng.randrange(8, 5000), ms=rng.randrange(120, 900), service=service)
            return app_event(rng, ts, service, level, message, version=self.version(service, ts))
        method, path = rng.choice(ROUTES[service])
        status = weighted(rng, BASELINE_STATUS)
        if status == 401 and service not in ("identity", "api-gateway"):
            status = 200
        latency = latency_for(rng, service)
        error = None
        if status in CLIENT_ERRORS:
            error = {"code": f"HTTP_{status}", "type": "ClientError", "message": rng.choice(CLIENT_ERRORS[status])}
        elif status in SERVER_NOISE:
            error = {"code": f"HTTP_{status}", "type": "ServerError", "message": rng.choice(SERVER_NOISE[status])}
        return access_event(rng, ts, service, method, path, status, latency, error=error, version=self.version(service, ts))


def diurnal_weight(ts):
    hour = (ts / 3600.0) % 24
    return 0.35 + 0.65 * (0.5 - 0.5 * math.cos(2 * math.pi * (hour - 3) / 24))  # quiet ~03:00 UTC, busy ~15:00 UTC


def baseline_timestamps(rng, start, end, count):
    span = end - start
    stamps = []
    while len(stamps) < count:
        ts = start + rng.random() * span
        if rng.random() <= diurnal_weight(ts):
            stamps.append(ts)
    stamps.sort()
    return stamps


def poisson(rng, mean):
    if mean <= 0:
        return 0
    if mean > 30:
        return max(0, int(round(rng.gauss(mean, math.sqrt(mean)))))
    threshold, k, product = math.exp(-mean), 0, rng.random()
    while product > threshold:
        k += 1
        product *= rng.random()
    return k


# --------------------------------------------------------------------------------------------
# Scenarios (injected incidents with ground truth)


class Scenario:
    id = ""
    title = ""
    difficulty = "medium"
    offset_hours = 0.0  # start of the incident, hours before the anchor
    duration_minutes = 30
    services = ()
    question = ""
    root_cause = ""
    expected = ()  # keyword groups; each group is satisfied by any one of its terms
    live = False
    expect_quiet = False  # negative control: passes only if no high/critical finding is reported
    rate_per_minute = 10.0

    def window(self, anchor):
        start = anchor - self.offset_hours * 3600
        return start, start + self.duration_minutes * 60

    def investigation_range(self, anchor, now=None):
        if self.live:
            end = now or time.time()
            return end - 30 * 60, end
        start, end = self.window(anchor)
        return start - 15 * 60, end + 15 * 60

    def baseline_overrides(self, anchor):
        return []

    def events(self, rng, anchor, until=None):
        """Yields (ts, event) for the incident window, clipped to ``until``."""
        start, end = self.window(anchor)
        if until is not None:
            end = min(end, until)
        ts = start
        while True:
            ts += rng.expovariate(self.rate_per_minute / 60.0)
            if ts >= end:
                break
            yield from self.emit(rng, ts, start)
        yield from self.markers(rng, anchor, until)

    def emit(self, rng, ts, start):
        raise NotImplementedError

    def markers(self, rng, anchor, until):
        return ()

    def ground_truth(self, anchor, now=None):
        start, end = self.investigation_range(anchor, now)
        incident_start, incident_end = self.window(anchor)
        return {
            "id": self.id,
            "title": self.title,
            "difficulty": self.difficulty,
            "live": self.live,
            "services": list(self.services),
            "question": self.question,
            "root_cause": self.root_cause,
            "expected_keywords": [list(group) for group in self.expected],
            "expect_quiet": self.expect_quiet,
            "incident_window": {"start": iso(incident_start), "end": None if self.live else iso(incident_end)},
            "time_range": {"start": int(start * 1000), "end": int(end * 1000)},
        }


class PaymentsPoolExhaustion(Scenario):
    id = "payments-db-pool-exhaustion"
    title = "Payments DB connection pool exhausted by settlement batch job"
    difficulty = "hard"
    offset_hours = 51.0
    duration_minutes = 40
    services = ("payments", "checkout")
    question = "Customers could not complete payments during this period. What went wrong, and which services were affected?"
    root_cause = (
        "A settlement batch job started on payments-db held most connections, exhausting the HikariCP pool "
        "(50/50 active). POST /api/payments returned 503 after 30s and checkout surfaced 502s for orders."
    )
    expected = (
        ("hikari", "connection pool", "pool exhausted", "connection is not available"),
        ("settlement", "batch job"),
        ("checkout", "502"),
    )
    rate_per_minute = 28.0

    def emit(self, rng, ts, start):
        trace = trace_id(rng)
        user = f"user-{rng.randrange(1, 20000):05d}"
        region = weighted(rng, zip(REGIONS, REGION_WEIGHTS))
        waiting = rng.randrange(140, 260)
        latency = rng.uniform(30.0, 30.08)
        yield (
            ts,
            access_event(
                rng,
                ts,
                "payments",
                "POST",
                "/api/payments",
                503,
                latency,
                trace=trace,
                user=user,
                region=region,
                logger="com.zaxxer.hikari.pool.HikariPool",
                error={
                    "code": "DB_POOL_EXHAUSTED",
                    "type": "java.sql.SQLTransientConnectionException",
                    "message": (
                        "HikariPool-1 - Connection is not available, request timed out after 30000ms "
                        f"(total=50, active=50, idle=0, waiting={waiting})"
                    ),
                },
            ),
        )
        if rng.random() < 0.8:
            ts2 = ts + latency + rng.uniform(0.001, 0.05)
            yield (
                ts2,
                access_event(
                    rng,
                    ts2,
                    "checkout",
                    "POST",
                    "/api/orders",
                    502,
                    latency + rng.uniform(0.01, 0.2),
                    trace=trace,
                    user=user,
                    region=region,
                    error={
                        "code": "PAYMENT_UPSTREAM_FAILED",
                        "type": "UpstreamServiceError",
                        "message": "payments-service returned 503 Service Unavailable after 30.0s",
                    },
                ),
            )
        if rng.random() < 0.06:
            yield (
                ts,
                app_event(
                    rng,
                    ts,
                    "payments",
                    "WARN",
                    f"HikariPool-1 - Pool stats (total=50, active=50, idle=0, waiting={waiting})",
                    logger="com.zaxxer.hikari.pool.HikariPool",
                    dataset="payments.db",
                ),
            )

    def markers(self, rng, anchor, until):
        start, end = self.window(anchor)
        job_start = start - 6 * 60
        events = [
            (
                job_start,
                app_event(
                    rng,
                    job_start,
                    "payments",
                    "INFO",
                    "Batch settlement job started: reconciling 48211 transactions (parallelism=48)",
                    logger="com.shop.payments.settlement.SettlementJob",
                    dataset="payments.jobs",
                    host=PODS["payments"][0],
                ),
            ),
            (
                end - 60,
                app_event(
                    rng,
                    end - 60,
                    "payments",
                    "INFO",
                    "Batch settlement job finished: 48211 transactions reconciled in 39m12s",
                    logger="com.shop.payments.settlement.SettlementJob",
                    dataset="payments.jobs",
                    host=PODS["payments"][0],
                ),
            ),
        ]
        return [item for item in events if until is None or item[0] < until]


class CheckoutBadDeploy(Scenario):
    id = "checkout-bad-deploy"
    title = "Checkout 1.15.0 deploy throws NullPointerException on order creation"
    difficulty = "medium"
    offset_hours = 20.0
    duration_minutes = 75
    services = ("checkout",)
    question = "Order creation started failing for some customers. What is the root cause?"
    root_cause = (
        "checkout 1.15.0 was deployed and POST /api/orders began failing with HTTP 500 caused by a "
        "NullPointerException in OrderService.create (cart is null). Rolling back to 1.14.2 resolved it."
    )
    expected = (("1.15.0",), ("nullpointer", "getitems", "cart is null"), ("deploy", "rollback", "release"))
    rate_per_minute = 12.0

    def baseline_overrides(self, anchor):
        start, end = self.window(anchor)
        return [("checkout", start, end, "1.15.0")]

    def emit(self, rng, ts, start):
        yield (
            ts,
            access_event(
                rng,
                ts,
                "checkout",
                "POST",
                "/api/orders",
                500,
                latency_for(rng, "checkout", 0.6),
                version="1.15.0",
                logger="com.shop.checkout.OrderController",
                error={
                    "code": "ORDER_CREATE_FAILED",
                    "type": "java.lang.NullPointerException",
                    "message": 'Cannot invoke "com.shop.checkout.Cart.getItems()" because "cart" is null',
                    "stack_trace": (
                        'java.lang.NullPointerException: Cannot invoke "com.shop.checkout.Cart.getItems()" because "cart" is null\n'
                        "\tat com.shop.checkout.OrderService.create(OrderService.java:142)\n"
                        "\tat com.shop.checkout.OrderController.submit(OrderController.java:58)\n"
                        "\tat org.springframework.web.servlet.FrameworkServlet.service(FrameworkServlet.java:897)"
                    ),
                },
            ),
        )

    def markers(self, rng, anchor, until):
        start, end = self.window(anchor)
        events = [
            (
                start - 30,
                app_event(
                    rng,
                    start - 30,
                    "checkout",
                    "INFO",
                    "Deployment started: checkout version 1.15.0 (previous 1.14.2) by ci-pipeline #4821",
                    logger="deploy-controller",
                    dataset="checkout.deploy",
                    version="1.15.0",
                ),
            ),
            (
                end,
                app_event(
                    rng,
                    end,
                    "checkout",
                    "INFO",
                    "Rollback completed: checkout version 1.14.2 restored (reverted 1.15.0)",
                    logger="deploy-controller",
                    dataset="checkout.deploy",
                ),
            ),
        ]
        return [item for item in events if until is None or item[0] < until]


class CredentialStuffing(Scenario):
    id = "identity-credential-stuffing"
    title = "Credential-stuffing attack against the login endpoint"
    difficulty = "easy"
    offset_hours = 8.0
    duration_minutes = 35
    services = ("identity",)
    question = "Is anything suspicious happening with customer logins?"
    root_cause = (
        "A single client (203.0.113.77, python-requests user agent) sent a burst of POST /api/sessions "
        "attempts against many different accounts: mostly 401 failures, then 429 rate limiting and account lockouts."
    )
    expected = (("203.0.113.77",), ("401", "credential", "brute", "429", "rate limit"))
    rate_per_minute = 70.0

    def emit(self, rng, ts, start):
        attacker = "203.0.113.77" if rng.random() < 0.85 else f"203.0.113.{rng.randrange(60, 90)}"
        user = f"user-{rng.randrange(1, 20000):05d}"
        rate_limited = ts - start > 12 * 60 and rng.random() < 0.45
        status = 429 if rate_limited else 401
        error = (
            {
                "code": "RATE_LIMITED",
                "type": "RateLimitExceeded",
                "message": f"Rate limit exceeded for client {attacker}: {rng.randrange(90, 160)} requests/min (limit 30)",
            }
            if rate_limited
            else {"code": "INVALID_CREDENTIALS", "type": "AuthenticationFailed", "message": f"Invalid credentials for {user}"}
        )
        yield (
            ts,
            access_event(
                rng,
                ts,
                "identity",
                "POST",
                "/api/sessions",
                status,
                latency_for(rng, "identity"),
                user=user,
                client_ip=attacker,
                user_agent="python-requests/2.31.0",
                region="us-east-1",
                logger="identity.auth",
                error=error,
            ),
        )
        if rng.random() < 0.04:
            yield (
                ts,
                app_event(
                    rng,
                    ts,
                    "identity",
                    "WARN",
                    f"Account locked after 5 failed login attempts: {user}",
                    logger="identity.lockout",
                    dataset="identity.security",
                    region="us-east-1",
                ),
            )


class CatalogRegionalLatency(Scenario):
    id = "catalog-eu-slow-queries"
    title = "Catalog latency in eu-west-1 caused by a degraded read replica"
    difficulty = "medium"
    offset_hours = 3.0
    duration_minutes = 55
    services = ("catalog",)
    question = "Users say product pages are very slow. Why?"
    root_cause = (
        "Only eu-west-1 was affected: catalog queries against read replica catalog-db-eu-2 took 2-6s "
        "(threshold 1s), product requests slowed down and some timed out with 504."
    )
    expected = (("eu-west-1",), ("catalog-db-eu-2", "replica", "slow query"))
    rate_per_minute = 16.0

    def emit(self, rng, ts, start):
        query_ms = rng.randrange(2100, 6200)
        path = rng.choice(("/api/products/{product_id}", "/api/categories", "/api/products"))
        if rng.random() < 0.12:
            yield (
                ts,
                access_event(
                    rng,
                    ts,
                    "catalog",
                    "GET",
                    path,
                    504,
                    5.0 + rng.random() * 0.05,
                    region="eu-west-1",
                    error={
                        "code": "QUERY_TIMEOUT",
                        "type": "QueryTimeoutException",
                        "message": "Query timeout after 5000ms on replica catalog-db-eu-2",
                    },
                ),
            )
        else:
            yield (
                ts,
                access_event(
                    rng,
                    ts,
                    "catalog",
                    "GET",
                    path,
                    200,
                    query_ms / 1000 + rng.uniform(0.01, 0.08),
                    region="eu-west-1",
                    level="WARN",
                ),
            )
        if rng.random() < 0.5:
            yield (
                ts,
                app_event(
                    rng,
                    ts,
                    "catalog",
                    "WARN",
                    f"Slow query products_by_category took {query_ms}ms (threshold 1000ms) on replica catalog-db-eu-2",
                    logger="catalog.db.QueryMonitor",
                    dataset="catalog.db",
                    region="eu-west-1",
                ),
            )


class GatewaySearchTimeouts(Scenario):
    id = "gateway-search-timeouts"
    title = "Ongoing: api-gateway search requests time out on the catalog-search upstream"
    difficulty = "easy"
    offset_hours = 20 / 60  # began 20 minutes before seeding and continues while live traffic runs
    duration_minutes = 20
    services = ("api-gateway",)
    question = "Why are search requests failing right now?"
    root_cause = (
        "api-gateway GET /api/v1/search requests return 504 because the catalog-search upstream "
        "(10.0.4.17:8080) does not respond within 10s."
    )
    expected = (("catalog-search",), ("504", "timeout", "timed out"))
    live = True
    rate_per_minute = 9.0

    def emit(self, rng, ts, start):
        yield (
            ts,
            access_event(
                rng,
                ts,
                "api-gateway",
                "GET",
                "/api/v1/search",
                504,
                10.0 + rng.random() * 0.02,
                logger="gateway.proxy",
                error={
                    "code": "UPSTREAM_TIMEOUT",
                    "type": "UpstreamTimeout",
                    "message": "upstream request timeout: catalog-search (10.0.4.17:8080) did not respond within 10000ms",
                },
            ),
        )


class QuietWindowControl(Scenario):
    """Negative control: an hour of normal traffic. A good investigation reports no significant incident."""

    id = "control-quiet-window"
    title = "Control: normal traffic only"
    difficulty = "control"
    offset_hours = 30.0
    duration_minutes = 60
    services = ()
    question = "Were there any significant incidents or outages during this period?"
    root_cause = "None. Only normal background noise: client 4xx errors and isolated, unrelated 5xx responses."
    expected = ()
    expect_quiet = True

    def events(self, rng, anchor, until=None):
        return iter(())

    def investigation_range(self, anchor, now=None):
        return self.window(anchor)


ALL_SCENARIOS = (
    PaymentsPoolExhaustion,
    CheckoutBadDeploy,
    CredentialStuffing,
    CatalogRegionalLatency,
    GatewaySearchTimeouts,
    QuietWindowControl,
)


def select_scenarios(spec, days):
    spec = (spec or "all").strip().lower()
    if spec in ("none", "off", "false", "0"):
        return []
    wanted = None if spec == "all" else {item.strip() for item in spec.split(",") if item.strip()}
    known = {cls.id for cls in ALL_SCENARIOS}
    if wanted and wanted - known:
        raise SystemExit(f"Unknown scenario(s): {', '.join(sorted(wanted - known))}. Known: {', '.join(sorted(known))}")
    selected = []
    for cls in ALL_SCENARIOS:
        if wanted is not None and cls.id not in wanted:
            continue
        if cls.offset_hours > days * 24 - 1:
            log(f"Skipping scenario {cls.id}: it needs PLAYGROUND_DAYS >= {math.ceil((cls.offset_hours + 1) / 24)}")
            continue
        selected.append(cls())
    return selected


def scenario_rng(seed, scenario_id):
    return random.Random(int(hashlib.sha256(f"{seed}:{scenario_id}".encode()).hexdigest()[:16], 16))


# --------------------------------------------------------------------------------------------
# Corpus


class Corpus:
    def __init__(self, seed, events, days, scenarios, anchor):
        self.seed = seed
        self.events = events
        self.days = days
        self.scenarios = scenarios
        self.anchor = anchor
        overrides = [item for scenario in scenarios for item in scenario.baseline_overrides(anchor)]
        self.baseline = Baseline(overrides)

    def stream(self):
        """Yields (ts, event) in timestamp order: baseline merged with every scenario."""
        rng = random.Random(self.seed)
        start = self.anchor - self.days * 86400
        stamps = baseline_timestamps(rng, start, self.anchor, self.events)
        baseline = ((ts, self.baseline.event(rng, ts)) for ts in stamps)
        incident_streams = []
        for scenario in self.scenarios:
            items = [
                item
                for item in scenario.events(scenario_rng(self.seed, scenario.id), self.anchor, until=self.anchor)
                if start <= item[0] < self.anchor
            ]
            items.sort(key=lambda item: item[0])
            incident_streams.append(items)
        return heapq.merge(baseline, *incident_streams, key=lambda item: item[0])

    def ground_truth(self, now=None):
        return {
            "corpus": fingerprint_dict(self.seed, self.events, self.days, self.scenarios),
            "anchor": iso(self.anchor),
            "scenarios": [scenario.ground_truth(self.anchor, now) for scenario in self.scenarios],
        }


def fingerprint_dict(seed, events, days, scenarios):
    return {
        "version": CORPUS_VERSION,
        "seed": seed,
        "events": events,
        "days": days,
        "scenarios": sorted(scenario.id for scenario in scenarios),
    }


# --------------------------------------------------------------------------------------------
# Sinks


class ElasticsearchSink:
    name = "elasticsearch"

    def __init__(self, url):
        self.url = url.rstrip("/")

    def wait(self):
        wait_for_service("Elasticsearch", f"{self.url}/_cluster/health?wait_for_status=yellow&timeout=5s")

    def stored_meta(self):
        try:
            return json.loads(request(f"{self.url}/{META_INDEX}/_doc/corpus"))["_source"]
        except HttpFailure as error:
            if error.status == 404:
                return None
            raise

    def prepare(self, reset):
        request(f"{self.url}/_index_template/investigenie-logs", method="PUT", body=json.dumps(ES_TEMPLATE).encode())
        if reset:
            indices = json.loads(request(f"{self.url}/_cat/indices/{LOG_INDEX_PREFIX}*?format=json&h=index"))
            names = [item["index"] for item in indices]
            for offset in range(0, len(names), 50):
                request(f"{self.url}/{','.join(names[offset : offset + 50])}", method="DELETE")
            if names:
                log(f"Elasticsearch: removed {len(names)} indices from a previous corpus")

    def write(self, rows):
        lines = []
        for doc_id, ts, event in rows:
            action = {"_index": f"{LOG_INDEX_PREFIX}{dt.datetime.fromtimestamp(ts, dt.timezone.utc):%Y.%m.%d}"}
            if doc_id:
                action["_id"] = doc_id
            lines.append(json.dumps({"index": action}))
            lines.append(json.dumps(event, separators=(",", ":")))
        body = ("\n".join(lines) + "\n").encode()
        result = json.loads(request(f"{self.url}/_bulk", method="POST", body=body, content_type="application/x-ndjson", timeout=120))
        if result.get("errors"):
            first = next(item["index"]["error"] for item in result["items"] if "error" in item["index"])
            raise RuntimeError(f"Elasticsearch bulk indexing failed: {first}")

    def prune(self, anchor, retention_days):
        """Deletes daily indices that hold only live traffic and are older than the retention.

        The seeded corpus (anchor day and earlier) is never touched, so scenarios stay intact.
        """
        anchor_day = dt.datetime.fromtimestamp(anchor, dt.timezone.utc).date()
        cutoff = dt.datetime.now(dt.timezone.utc).date() - dt.timedelta(days=retention_days)
        indices = json.loads(request(f"{self.url}/_cat/indices/{LOG_INDEX_PREFIX}*?format=json&h=index"))
        expired = []
        for item in indices:
            try:
                day = dt.datetime.strptime(item["index"][len(LOG_INDEX_PREFIX) :], "%Y.%m.%d").date()
            except ValueError:
                continue
            if anchor_day < day < cutoff:
                expired.append(item["index"])
        if expired:
            request(f"{self.url}/{','.join(expired[:50])}", method="DELETE")
            log(f"Elasticsearch: removed {len(expired[:50])} live-traffic indices older than {retention_days} days")

    def mark(self, meta):
        request(f"{self.url}/{META_INDEX}/_doc/corpus?refresh=true", method="PUT", body=json.dumps(meta).encode())
        request(f"{self.url}/{LOG_INDEX_PREFIX}*/_refresh", method="POST")


class LokiSink:
    name = "loki"

    def __init__(self, url, state_dir):
        self.url = url.rstrip("/")
        self.marker = os.path.join(state_dir, "loki.json")

    def wait(self):
        wait_for_service("Loki", f"{self.url}/ready")

    def stored_meta(self):
        try:
            with open(self.marker) as handle:
                return json.load(handle)
        except (OSError, ValueError):
            return None

    def prepare(self, reset):
        if reset and self.stored_meta() is not None:
            log("Loki: data from a previous corpus cannot be removed in place; run `playground.sh reset` for a clean Loki")

    def write(self, rows):
        streams = {}
        for _, ts, event in rows:
            labels = {
                "service_name": event["service"]["name"],
                "environment": ENVIRONMENT,
                "level": event["log"]["level"],
            }
            key = tuple(labels.items())
            streams.setdefault(key, {"stream": labels, "values": []})["values"].append(
                [str(int(ts * 1_000_000_000)), json.dumps(event, separators=(",", ":"))]
            )
        body = json.dumps({"streams": list(streams.values())}, separators=(",", ":")).encode()
        request(f"{self.url}/loki/api/v1/push", method="POST", body=body, timeout=60)

    def mark(self, meta):
        with open(self.marker, "w") as handle:
            json.dump(meta, handle)


# --------------------------------------------------------------------------------------------
# Metrics


class Metrics:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = Counter()
        self.buckets = defaultdict(lambda: [0] * len(HISTOGRAM_BOUNDS))
        self.sums = Counter()
        self.counts = Counter()
        self.events = Counter()
        self.started = time.monotonic()

    def observe(self, event, source):
        with self.lock:
            self.events[source] += 1
            status = _dig(event, ("http", "response", "status_code"))
            if status is None:
                return
            service = event["service"]["name"]
            method = event["http"]["request"]["method"]
            seconds = event["event"]["duration"] / 1_000_000_000
            self.requests[(service, method, status)] += 1
            self.buckets[service][bisect.bisect_left(HISTOGRAM_BOUNDS, seconds)] += 1
            self.sums[service] += seconds
            self.counts[service] += 1

    def render(self, active):
        with self.lock:
            lines = [
                "# HELP playground_events_generated_total Synthetic log events written, by source.",
                "# TYPE playground_events_generated_total counter",
            ]
            lines += [f'playground_events_generated_total{{source="{source}"}} {count}' for source, count in sorted(self.events.items())]
            lines += [
                "# HELP playground_http_requests_total Synthetic request counts by service, method, and status.",
                "# TYPE playground_http_requests_total counter",
            ]
            lines += [
                f'playground_http_requests_total{{service="{s}",method="{m}",status="{c}"}} {n}'
                for (s, m, c), n in sorted(self.requests.items())
            ]
            lines += [
                "# HELP playground_http_request_duration_seconds Synthetic request duration distribution.",
                "# TYPE playground_http_request_duration_seconds histogram",
            ]
            for service in SERVICES:
                cumulative = 0
                for index, bound in enumerate(HISTOGRAM_BOUNDS):
                    cumulative += self.buckets[service][index]
                    label = "+Inf" if math.isinf(bound) else f"{bound:g}"
                    lines.append(f'playground_http_request_duration_seconds_bucket{{service="{service}",le="{label}"}} {cumulative}')
                lines.append(f'playground_http_request_duration_seconds_sum{{service="{service}"}} {self.sums[service]:.6f}')
                lines.append(f'playground_http_request_duration_seconds_count{{service="{service}"}} {self.counts[service]}')
            lines += [
                "# HELP playground_scenario_active Whether an injected incident is currently being generated.",
                "# TYPE playground_scenario_active gauge",
            ]
            lines += [f'playground_scenario_active{{scenario="{sid}"}} {1 if on else 0}' for sid, on in sorted(active.items())]
            lines += [
                "# HELP playground_exporter_uptime_seconds Time since the seeder started.",
                "# TYPE playground_exporter_uptime_seconds gauge",
                f"playground_exporter_uptime_seconds {time.monotonic() - self.started:.1f}",
                "",
            ]
            return "\n".join(lines).encode()


class Status:
    def __init__(self):
        self.lock = threading.Lock()
        self.state = {"phase": "starting", "sinks": {}, "seeded": False, "live": False, "live_incident": False}
        self.corpus = None

    def update(self, **values):
        with self.lock:
            self.state.update(values)

    def sink(self, name, value):
        with self.lock:
            self.state["sinks"][name] = value

    def snapshot(self):
        with self.lock:
            return json.loads(json.dumps(self.state))


def active_scenarios(corpus, state, now=None):
    now = time.time() if now is None else now
    active = {}
    for scenario in corpus.scenarios:
        start, end = scenario.window(corpus.anchor)
        active[scenario.id] = bool(state.get("live_incident", False)) if scenario.live else start <= now < end
    return active


def scenario_response(corpus, state, now=None):
    truth = corpus.ground_truth(now)
    live_incident = bool(state.get("live_incident", False))
    truth["live_traffic"] = bool(state.get("live", False))
    truth["live_incident"] = live_incident
    if not live_incident:
        for item in truth["scenarios"]:
            if item["live"]:
                scenario = next(scenario for scenario in corpus.scenarios if scenario.id == item["id"])
                _, end = scenario.window(corpus.anchor)
                item["live"] = False
                item["title"] = item["title"].replace("Ongoing:", "Historical:", 1)
                item["incident_window"]["end"] = iso(end)
                item["time_range"] = {"start": int((end - 25 * 60) * 1000), "end": int((end + 60) * 1000)}
    return truth


def start_http_server(port, metrics, status):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            path = urllib.parse.urlparse(self.path).path
            if path == "/metrics":
                corpus = status.corpus
                active = active_scenarios(corpus, status.snapshot()) if corpus else {}
                self.reply(200, metrics.render(active), "text/plain; version=0.0.4; charset=utf-8")
            elif path == "/scenarios":
                if status.corpus is None:
                    self.reply(503, b'{"error":"corpus not generated yet"}')
                else:
                    truth = scenario_response(status.corpus, status.snapshot())
                    self.reply(200, json.dumps(truth, indent=2).encode())
            elif path in ("/healthz", "/status"):
                snapshot = self.status_snapshot()
                self.reply(200 if snapshot["seeded"] or path == "/status" else 503, json.dumps(snapshot).encode())
            else:
                self.send_error(404)

        def status_snapshot(self):
            return status.snapshot()

        def reply(self, code, body, content_type="application/json"):
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format_string, *args):
            return

    server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    log(f"Status, ground truth and metrics on :{port} (/healthz, /scenarios, /metrics)")


# --------------------------------------------------------------------------------------------
# Orchestration


def env_int(name, default, minimum=None):
    raw = os.getenv(name, "").strip()
    try:
        value = int(raw) if raw else default
    except ValueError:
        raise SystemExit(f"{name} must be an integer") from None
    if minimum is not None and value < minimum:
        raise SystemExit(f"{name} must be >= {minimum}")
    return value


def env_float(name, default):
    raw = os.getenv(name, "").strip()
    try:
        return float(raw) if raw else default
    except ValueError:
        raise SystemExit(f"{name} must be a number") from None


def env_flag(name, default):
    raw = os.getenv(name, "").strip().lower()
    return default if not raw else raw in ("1", "true", "yes", "on")


def build_sinks(state_dir):
    sinks = []
    es_url = os.getenv("ELASTICSEARCH_URL", "http://elasticsearch:9200").strip()
    if es_url:
        sinks.append(ElasticsearchSink(es_url))
    loki_url = os.getenv("LOKI_URL", "http://loki:3100").strip()
    if loki_url:
        sinks.append(LokiSink(loki_url, state_dir))
    return sinks


def connect_sinks(sinks, status):
    """Waits for each sink. Sinks whose host does not resolve (profile not started) are skipped."""
    ready = []
    for sink in sinks:
        url = sink.url
        if not resolvable(url):
            optional = isinstance(sink, LokiSink)
            log(
                f"{sink.name}: {urllib.parse.urlparse(url).hostname} is not running; skipping"
                + ("" if optional else " (is the service up?)")
            )
            status.sink(sink.name, "not running")
            continue
        sink.wait()
        ready.append(sink)
    return ready


def load_state(state_dir):
    try:
        with open(os.path.join(state_dir, "corpus.json")) as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def save_state(state_dir, meta):
    os.makedirs(state_dir, exist_ok=True)
    path = os.path.join(state_dir, "corpus.json")
    with open(path + ".tmp", "w") as handle:
        json.dump(meta, handle)
    os.replace(path + ".tmp", path)


def resolve_anchor(fingerprint, state_dir, stored, force):
    """Reuses the anchor of an identical existing corpus so restarts are idempotent."""
    if not force:
        candidates = [load_state(state_dir)] + list(stored.values())
        for meta in candidates:
            if meta and meta.get("fingerprint") == fingerprint and meta.get("anchor"):
                return float(meta["anchor"])
    return float(int(time.time()))


def seed(corpus, sinks, metrics, status, meta):
    pending = [sink for sink in sinks if status.snapshot()["sinks"].get(sink.name) != "seeded"]
    if not pending:
        for _, event in corpus.stream():
            metrics.observe(event, "seed")
        log("All sinks already hold this corpus; skipping seeding")
        return
    names = ", ".join(sink.name for sink in pending)
    log(f"Seeding {corpus.events:,} baseline events over {corpus.days} days plus {len(corpus.scenarios)} scenario(s) into {names}")
    batch = []
    written = 0
    started = time.monotonic()
    for sequence, (ts, event) in enumerate(corpus.stream()):
        metrics.observe(event, "seed")
        batch.append((f"{corpus.seed}-{int(corpus.anchor)}-{sequence}", ts, event))
        if len(batch) >= BATCH_SIZE:
            for sink in pending:
                sink.write(batch)
            written += len(batch)
            batch.clear()
            if written % 10_000 == 0:
                log(f"  {written:,} events written ({written / max(0.001, time.monotonic() - started):,.0f}/s)")
    if batch:
        for sink in pending:
            sink.write(batch)
        written += len(batch)
    for sink in pending:
        sink.mark(meta)
        status.sink(sink.name, "seeded")
    log(f"Seeded {written:,} events in {time.monotonic() - started:,.1f}s")


MAX_LIVE_GAP_SECONDS = 10.0  # after sleep/suspend, skip the gap instead of back-filling hours at once


def live_loop(corpus, sinks, metrics, eps, incident, retention_days):
    rng = random.Random()
    next_prune = 0.0
    live_scenarios = [s for s in corpus.scenarios if s.live] if incident else []
    last = time.time()
    buffer = []
    last_flush = time.monotonic()
    failures = {}
    log(
        f"Live traffic: ~{eps:g} events/s"
        + (f" with ongoing incident(s): {', '.join(s.id for s in live_scenarios)}" if live_scenarios else "")
    )
    while True:
        time.sleep(1.0)
        now = time.time()
        if now - last > MAX_LIVE_GAP_SECONDS:
            log(f"Live traffic resumed after a {now - last:,.0f}s pause; not back-filling the gap")
            last = now - 1.0
        span = now - last
        items = [(ts, corpus.baseline.event(rng, ts)) for ts in sorted(last + rng.random() * span for _ in range(poisson(rng, eps * span)))]
        for scenario in live_scenarios:
            for _ in range(poisson(rng, scenario.rate_per_minute / 60.0 * span)):
                ts = last + rng.random() * span
                items.extend(scenario.emit(rng, ts, corpus.anchor))
        items.sort(key=lambda item: item[0])
        last = now
        for ts, event in items:
            metrics.observe(event, "live")
            buffer.append((None, ts, event))
        if buffer and (time.monotonic() - last_flush >= 5 or len(buffer) >= BATCH_SIZE):
            for sink in sinks:  # one failing sink must not starve the others
                try:
                    sink.write(buffer)
                    failures[sink.name] = 0
                except Exception as error:  # keep the playground alive through transient sink outages
                    failures[sink.name] = count = failures.get(sink.name, 0) + 1
                    if count in (1, 10) or count % 100 == 0:
                        log(f"Live write to {sink.name} failed ({count}x): {error}")
            buffer.clear()
            last_flush = time.monotonic()
        if retention_days > 0 and time.monotonic() >= next_prune:
            next_prune = time.monotonic() + 3600
            for sink in sinks:
                prune = getattr(sink, "prune", None)
                if prune:
                    try:
                        prune(corpus.anchor, retention_days)
                    except Exception as error:
                        log(f"Pruning {sink.name} failed: {error}")


def dry_run(corpus):
    services, statuses, levels, per_scenario = Counter(), Counter(), Counter(), Counter()
    scenario_samples = {}
    total = 0
    previous = -math.inf
    markers = {}
    for scenario in corpus.scenarios:
        for ts, event in scenario.events(scenario_rng(corpus.seed, scenario.id), corpus.anchor, until=corpus.anchor):
            markers[(ts, event["message"])] = scenario.id
    for ts, event in corpus.stream():
        assert ts >= previous, "stream must be time ordered"
        previous = ts
        total += 1
        services[event["service"]["name"]] += 1
        levels[event["log"]["level"]] += 1
        statuses[_dig(event, ("http", "response", "status_code")) or "app"] += 1
        owner = markers.get((ts, event["message"]))
        if owner:
            per_scenario[owner] += 1
            scenario_samples.setdefault(owner, event)
    print(f"events: {total:,}  anchor: {iso(corpus.anchor)}")
    print("by service:", dict(services.most_common()))
    print("by level:", dict(levels.most_common()))
    print("by status:", dict(statuses.most_common()))
    print("scenario events:", dict(per_scenario))
    for scenario_id, event in scenario_samples.items():
        print(f"\n--- sample from {scenario_id}\n{json.dumps(event, indent=2)}")
    print("\n--- ground truth")
    print(json.dumps(corpus.ground_truth(), indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dry-run", action="store_true", help="generate the corpus in memory and print statistics")
    parser.add_argument("--reseed", action="store_true", help="discard any existing corpus and seed again")
    args = parser.parse_args()

    events = env_int("PLAYGROUND_EVENTS", 50_000, minimum=1)
    days = env_int("PLAYGROUND_DAYS", 7, minimum=1)
    seed_value = env_int("PLAYGROUND_SEED", 731)
    scenarios = select_scenarios(os.getenv("PLAYGROUND_SCENARIOS", "all"), days)
    eps = env_float("PLAYGROUND_LIVE_EPS", 2.0)
    incident = env_flag("PLAYGROUND_LIVE_INCIDENT", True)
    force = args.reseed or env_flag("PLAYGROUND_RESEED", False)
    state_dir = os.getenv("STATE_DIR", "/state")
    fingerprint = fingerprint_dict(seed_value, events, days, scenarios)

    if args.dry_run:
        dry_run(Corpus(seed_value, events, days, scenarios, float(int(time.time()))))
        return

    metrics, status = Metrics(), Status()
    start_http_server(env_int("METRICS_PORT", 8000), metrics, status)
    sinks = connect_sinks(build_sinks(state_dir), status)
    if not sinks:
        raise SystemExit("No data stores are reachable; nothing to seed")

    stored = {sink.name: sink.stored_meta() for sink in sinks}
    anchor = resolve_anchor(fingerprint, state_dir, stored, force)
    meta = {"fingerprint": fingerprint, "anchor": anchor, "anchor_iso": iso(anchor)}
    for sink in sinks:
        current = stored[sink.name]
        up_to_date = bool(current) and current.get("fingerprint") == fingerprint and current.get("anchor") == anchor
        sink.prepare(reset=force or not up_to_date)
        status.sink(sink.name, "seeded" if up_to_date and not force else "pending")
    save_state(state_dir, meta)

    corpus = Corpus(seed_value, events, days, scenarios, anchor)
    status.corpus = corpus
    status.update(phase="seeding", anchor=iso(anchor), fingerprint=fingerprint)
    seed(corpus, sinks, metrics, status, meta)
    status.update(phase="ready", seeded=True)

    live_incident = incident and any(s.live for s in scenarios)
    if eps > 0 or live_incident:
        status.update(live=True, live_incident=live_incident, phase="live")
        live_loop(corpus, sinks, metrics, max(eps, 0.0), incident, env_int("PLAYGROUND_LIVE_RETENTION_DAYS", 3, minimum=0))
    else:
        log("Live traffic disabled; serving status and metrics only")
        while True:
            time.sleep(3600)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)

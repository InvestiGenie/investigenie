"""Tests for the playground seeder. Run: python3 -m unittest discover -s playground/tests"""

import datetime as dt
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import generate_data as gd  # noqa: E402

ANCHOR = float(int(dt.datetime(2026, 10, 19, 12, 0, tzinfo=dt.timezone.utc).timestamp()))


def corpus(events=2000, days=7, spec="all"):
    return gd.Corpus(731, events, days, gd.select_scenarios(spec, days), ANCHOR)


def scenario_events(scenario):
    return list(scenario.events(gd.scenario_rng(731, scenario.id), ANCHOR, until=ANCHOR))


class CorpusTests(unittest.TestCase):
    def test_stream_is_time_ordered_bounded_and_deterministic(self):
        first = list(corpus().stream())
        second = list(corpus().stream())
        stamps = [ts for ts, _ in first]
        self.assertEqual(stamps, sorted(stamps), "Loki rejects out-of-order history, so the stream must be sorted")
        self.assertGreaterEqual(min(stamps), ANCHOR - 7 * 86400)
        self.assertLess(max(stamps), ANCHOR)
        self.assertEqual(json.dumps(first[:200]), json.dumps(second[:200]))

    def test_scenarios_requiring_more_history_are_skipped(self):
        ids = {s.id for s in gd.select_scenarios("all", 1)}
        self.assertNotIn("payments-db-pool-exhaustion", ids)
        self.assertNotIn("control-quiet-window", ids)
        self.assertIn("checkout-bad-deploy", ids)
        with self.assertRaises(SystemExit):
            gd.select_scenarios("no-such-scenario", 7)

    def test_scenario_windows_do_not_overlap(self):
        windows = sorted(s.window(ANCHOR) for s in gd.select_scenarios("all", 7))
        for (_, end), (start, _) in zip(windows, windows[1:]):
            self.assertLessEqual(end, start)


class GroundTruthTests(unittest.TestCase):
    def test_every_keyword_group_is_findable_in_the_scenario_data(self):
        for scenario in gd.select_scenarios("all", 7):
            if scenario.expect_quiet:
                continue
            text = "\n".join(json.dumps(event).lower() for _, event in scenario_events(scenario))
            for group in scenario.expected:
                self.assertTrue(any(term.lower() in text for term in group), f"{scenario.id}: no event mentions {group}")

    def test_keywords_cannot_be_satisfied_by_echoing_the_question(self):
        for scenario in gd.select_scenarios("all", 7):
            question = scenario.question.lower()
            for group in scenario.expected:
                self.assertFalse(any(term.lower() in question for term in group), f"{scenario.id}: {group} is in the question")

    def test_control_window_contains_only_baseline_traffic(self):
        control = next(s for s in gd.select_scenarios("all", 7) if s.expect_quiet)
        self.assertEqual(scenario_events(control), [])
        start, end = control.investigation_range(ANCHOR)
        incident_ids = {"DB_POOL_EXHAUSTED", "ORDER_CREATE_FAILED", "INVALID_CREDENTIALS", "QUERY_TIMEOUT", "UPSTREAM_TIMEOUT"}
        for ts, event in corpus(events=20000).stream():
            if start <= ts < end:
                self.assertNotIn(gd._dig(event, ("error", "code")), incident_ids)

    def test_ground_truth_time_ranges_cover_the_incident(self):
        for scenario in gd.select_scenarios("all", 7):
            truth = scenario.ground_truth(ANCHOR, now=ANCHOR)
            start, end = truth["time_range"]["start"] / 1000, truth["time_range"]["end"] / 1000
            stamps = [ts for ts, _ in scenario_events(scenario)]
            if stamps:
                self.assertGreaterEqual(min(stamps), start, scenario.id)
                self.assertLessEqual(min(max(stamps), ANCHOR), end, scenario.id)


class LiveIncidentTests(unittest.TestCase):
    def test_baseline_traffic_does_not_extend_or_activate_a_disabled_incident(self):
        data = corpus()
        state = {"live": True, "live_incident": False}
        now = ANCHOR + 3600

        truth = gd.scenario_response(data, state, now)
        gateway = next(item for item in truth["scenarios"] if item["id"] == "gateway-search-timeouts")
        _, incident_end = next(s for s in data.scenarios if s.id == gateway["id"]).window(data.anchor)

        self.assertTrue(truth["live_traffic"])
        self.assertFalse(truth["live_incident"])
        self.assertFalse(gateway["live"])
        self.assertTrue(gateway["title"].startswith("Historical:"))
        self.assertIsNotNone(gateway["incident_window"]["end"])
        self.assertEqual(
            gateway["time_range"],
            {"start": int((incident_end - 25 * 60) * 1000), "end": int((incident_end + 60) * 1000)},
        )
        self.assertFalse(gd.active_scenarios(data, state, now)["gateway-search-timeouts"])

    def test_enabled_live_incident_is_reported_active(self):
        data = corpus()
        state = {"live": True, "live_incident": True}
        now = ANCHOR + 3600

        truth = gd.scenario_response(data, state, now)
        gateway = next(item for item in truth["scenarios"] if item["id"] == "gateway-search-timeouts")

        self.assertTrue(truth["live_incident"])
        self.assertTrue(gateway["live"])
        self.assertEqual(gateway["time_range"], {"start": int((now - 30 * 60) * 1000), "end": int(now * 1000)})
        self.assertTrue(gd.active_scenarios(data, state, now)["gateway-search-timeouts"])


class PruneTests(unittest.TestCase):
    def test_prune_removes_only_old_live_only_indices(self):
        anchor = dt.datetime.now(dt.timezone.utc).timestamp() - 8 * 86400  # seeded 8 days ago, live since
        anchor_day = dt.datetime.fromtimestamp(anchor, dt.timezone.utc).date()
        today = dt.datetime.now(dt.timezone.utc).date()
        days = [anchor_day - dt.timedelta(days=2), anchor_day] + [today - dt.timedelta(days=n) for n in range(0, 10)]
        names = sorted({f"{gd.LOG_INDEX_PREFIX}{day:%Y.%m.%d}" for day in days})
        deleted = []

        def fake_request(url, method="GET", **_):
            if method == "DELETE":
                deleted.extend(url.rsplit("/", 1)[1].split(","))
                return b"{}"
            return json.dumps([{"index": name} for name in names]).encode()

        original, gd.request = gd.request, fake_request
        try:
            gd.ElasticsearchSink("http://es").prune(anchor, retention_days=3)
        finally:
            gd.request = original
        cutoff = today - dt.timedelta(days=3)
        for name in deleted:
            day = dt.datetime.strptime(name[len(gd.LOG_INDEX_PREFIX) :], "%Y.%m.%d").date()
            self.assertTrue(anchor_day < day < cutoff, name)
        self.assertEqual(len(deleted), 4, deleted)  # anchor+1 .. today-4
        self.assertNotIn(f"{gd.LOG_INDEX_PREFIX}{anchor_day:%Y.%m.%d}", deleted)


if __name__ == "__main__":
    unittest.main()

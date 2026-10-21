"""Tests for AI evaluation outcome classification."""

import os
import sys
import unittest
from contextlib import redirect_stderr
from io import StringIO
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import ai_eval  # noqa: E402


class OutcomeTests(unittest.TestCase):
    def test_only_complete_successful_investigations_can_pass(self):
        complete = {"complete": True}
        partial = {"complete": False}

        self.assertTrue(ai_eval.investigation_passed("completed", complete, 2, 2))
        self.assertFalse(ai_eval.investigation_passed("failed", complete, 2, 2))
        self.assertFalse(ai_eval.investigation_passed("terminated", partial, 2, 2))
        self.assertFalse(ai_eval.investigation_passed("completed", partial, 2, 2))
        self.assertFalse(ai_eval.investigation_passed("completed", complete, 1, 2))

    def test_failed_and_timeout_terminated_runs_are_pipeline_errors(self):
        partial = {"complete": False, "summary": "partial findings"}

        self.assertEqual(ai_eval.outcome_kind("failed", partial, True), "pipeline-error")
        self.assertEqual(
            ai_eval.outcome_kind("terminated", partial, False, provider_timeout_exhausted=True),
            "pipeline-error",
        )

    def test_ordinary_budget_termination_remains_incomplete(self):
        partial = {"complete": False, "summary": "partial findings"}

        self.assertEqual(ai_eval.outcome_kind("terminated", partial, False), "incomplete")
        self.assertEqual(ai_eval.outcome_kind("completed", partial, False), "incomplete")

    def test_timeout_pause_is_distinguished_from_time_budget_pause(self):
        self.assertTrue(ai_eval.is_provider_timeout_pause("The AI provider did not respond within the configured timeout. Retry."))
        self.assertFalse(ai_eval.is_provider_timeout_pause("The investigation has run for 2 minutes without finishing."))

    def test_run_one_reports_exhausted_provider_timeouts_as_pipeline_errors(self):
        timeout_reason = "The AI provider did not respond within the configured timeout. Retry the request."
        budget_reason = "The investigation has run for 2 minutes without finishing."
        result = {
            "complete": False,
            "summary": "catalog-search returned 504 after a timeout",
            "findings": [],
            "caveats": [],
            "token_usage": [],
        }
        args = SimpleNamespace(
            ai_timeout=300,
            budget=900,
            max_minutes=60,
            max_continues=1,
            poll=0,
            verbose=False,
            keep_history=True,
        )
        scenario = {
            "id": "gateway-search-timeouts",
            "difficulty": "easy",
            "question": "Why did search fail?",
            "time_range": {"start": 1, "end": 2},
            "expected_keywords": [["catalog-search"], ["504"]],
        }
        model = {"id": "model-1", "name": "local", "model": "qwen-local"}

        for second_pause, expected_outcome, timeout_exhausted in (
            (timeout_reason, "pipeline-error", True),
            (budget_reason, "incomplete", False),
        ):
            with self.subTest(second_pause=second_pause):
                polls = iter(
                    [
                        {"id": "job-1", "status": "awaiting_decision", "pause_reason": second_pause, "result": result, "progress": {}},
                        {"id": "job-1", "status": "terminated", "result": result, "progress": {}},
                    ]
                )
                decisions = []

                def fake_api(path, method="GET", payload=None, polls=polls, decisions=decisions):
                    if path == "/api/ai/investigations" and method == "POST":
                        return {
                            "investigation": {
                                "id": "job-1",
                                "status": "awaiting_decision",
                                "pause_reason": timeout_reason,
                                "result": result,
                                "progress": {},
                            }
                        }
                    if path.endswith("/decision"):
                        decisions.append(payload["action"])
                        return {}
                    return {"investigation": next(polls)}

                with patch.object(ai_eval, "api", side_effect=fake_api), patch.object(ai_eval.time, "sleep"):
                    row = ai_eval.run_one(model, scenario, ["view-1"], args)

                self.assertEqual(decisions, ["continue", "terminate"])
                self.assertEqual(row["outcome"], expected_outcome)
                self.assertFalse(row["passed"])
                self.assertEqual(row["provider_timeout_exhausted"], timeout_exhausted)


class EvaluationInputTests(unittest.TestCase):
    def test_repeat_must_be_positive_before_any_api_request(self):
        for repeat in ("0", "-1", "not-a-number"):
            with self.subTest(repeat=repeat):
                with patch.object(sys, "argv", ["ai_eval.py", "--repeat", repeat]):
                    with patch.object(ai_eval, "call") as api_call:
                        with redirect_stderr(StringIO()):
                            with self.assertRaises(SystemExit) as error:
                                ai_eval.main()

                self.assertEqual(error.exception.code, 2)
                api_call.assert_not_called()

    def test_empty_scenario_selection_fails_even_when_fail_on_is_never(self):
        with patch.object(sys, "argv", ["ai_eval.py", "--fail-on", "never"]):
            with patch.object(ai_eval, "call", return_value={"scenarios": []}):
                with patch.object(ai_eval, "pick_models") as pick_models:
                    with self.assertRaisesRegex(SystemExit, "No scenarios selected"):
                        ai_eval.main()

        pick_models.assert_not_called()

    def test_empty_model_selection_fails_before_writing_a_report(self):
        scenario = {
            "id": "example",
            "question": "What happened?",
            "difficulty": "easy",
            "time_range": {"start": 1, "end": 2},
            "expected_keywords": [["database"]],
        }
        with patch.object(sys, "argv", ["ai_eval.py", "--fail-on", "never"]):
            with patch.object(ai_eval, "call", return_value={"scenarios": [scenario]}):
                with patch.object(ai_eval, "pick_models", return_value=[]):
                    with patch.object(ai_eval.os, "makedirs") as make_directories:
                        with self.assertRaisesRegex(SystemExit, "No AI models selected"):
                            ai_eval.main()

        make_directories.assert_not_called()


if __name__ == "__main__":
    unittest.main()

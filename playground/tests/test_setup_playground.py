"""Tests for playground API provisioning."""

import os
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import setup_playground as setup  # noqa: E402


class RegisterModelTests(unittest.TestCase):
    def test_existing_profile_is_updated_with_supplied_credentials(self):
        existing = {
            "id": "model-1",
            "name": "Local model",
            "api_url": "https://models.example/v1",
            "model": "qwen-local",
            "revision": "revision-1",
            "has_api_key": True,
        }
        calls = []

        def fake_api(path, method="GET", payload=None, headers=None, timeout=60):
            if path == "/api/ai/models" and method == "GET":
                return {"models": [existing]}
            calls.append({"path": path, "method": method, "payload": payload, "headers": headers, "timeout": timeout})
            return {}

        with patch.object(setup, "api", side_effect=fake_api), patch.object(setup, "log"):
            self.assertTrue(
                setup.register_model(
                    "Local model",
                    "https://models.example/v1/",
                    "qwen-local",
                    "replacement-api-key",
                )
            )

        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["path"], "/api/ai/models/model-1")
        self.assertEqual(calls[0]["method"], "PUT")
        self.assertEqual(calls[0]["headers"], {"If-Match": '"revision-1"'})
        self.assertEqual(calls[0]["payload"]["api_key"], "replacement-api-key")


class DisabledProviderTests(unittest.TestCase):
    def test_models_command_rejects_disabled_effective_provider(self):
        for provider in ("none", "off", ""):
            with self.subTest(provider=provider):
                with patch.object(setup, "AI_PROVIDER", provider):
                    with patch.object(setup, "wait_for", return_value=True):
                        with patch.object(setup, "provision_models") as provision_models:
                            with patch.object(setup, "log"):
                                with patch.object(sys, "argv", ["setup_playground.py", "models", "qwen2.5:3b"]):
                                    self.assertEqual(setup.main(), 1)

                provision_models.assert_not_called()

    def test_models_command_allows_an_enabled_remote_provider(self):
        with patch.object(setup, "AI_PROVIDER", "openai-compatible"):
            with patch.object(setup, "wait_for", return_value=True):
                with patch.object(setup, "provision_models", return_value=True) as provision_models:
                    with patch.object(setup, "log"):
                        with patch.object(sys, "argv", ["setup_playground.py", "models", "model-name"]):
                            self.assertEqual(setup.main(), 0)

        provision_models.assert_called_once_with(["model-name"])

    def test_full_setup_may_still_skip_optional_ai_provisioning(self):
        with patch.object(setup, "AI_PROVIDER", "none"):
            with patch.object(setup, "wait_for", return_value=True):
                with patch.object(setup, "provision_elasticsearch", return_value=True):
                    with patch.object(setup, "provision_clickhouse", return_value=True):
                        with patch.object(setup, "show"):
                            with patch.object(setup, "summary"):
                                with patch.object(setup, "log"):
                                    with patch.object(sys, "argv", ["setup_playground.py"]):
                                        self.assertEqual(setup.main(), 0)


if __name__ == "__main__":
    unittest.main()

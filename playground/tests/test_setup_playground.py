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


if __name__ == "__main__":
    unittest.main()

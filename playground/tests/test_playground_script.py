"""Tests for the playground wrapper's mode-specific configuration."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

PLAYGROUND_DIR = Path(__file__).resolve().parents[1]


class PlaygroundScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.playground = self.root / "playground"
        self.playground.mkdir()
        self.script = self.playground / "playground.sh"
        self.script.write_bytes((PLAYGROUND_DIR / "playground.sh").read_bytes().replace(b"\r\n", b"\n"))
        self.removed = self.root / "removed-volumes.txt"

        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.capture = self.root / "docker-env.txt"
        docker = self.bin / "docker"
        docker.write_text(
            "#!/usr/bin/env bash\n"
            "if [[ \"$1\" == volume && \"$2\" == ls ]]; then\n"
            "  printf '%s\\n' \"${DOCKER_VOLUMES-}\"\n"
            "  exit 0\n"
            "fi\n"
            "if [[ \"$1\" == volume && \"$2\" == rm ]]; then\n"
            "  if ! printf '%s\\n' \"${DOCKER_VOLUMES-}\" | grep -Fxq -- \"$4\"; then\n"
            "    echo 'no such volume' >&2\n"
            "    exit 1\n"
            "  fi\n"
            "  printf '%s\\n' \"$4\" >> \"$DOCKER_REMOVED\"\n"
            "  exit 0\n"
            "fi\n"
            "{\n"
            "  printf 'PLAYGROUND_OLLAMA_URL=%s\\n' \"${PLAYGROUND_OLLAMA_URL-<unset>}\"\n"
            "  printf 'PLAYGROUND_OLLAMA_DEFAULT_URL=%s\\n' \"${PLAYGROUND_OLLAMA_DEFAULT_URL-<unset>}\"\n"
            "  printf 'PLAYGROUND_AI_PROVIDER=%s\\n' \"${PLAYGROUND_AI_PROVIDER-<unset>}\"\n"
            "  printf 'PLAYGROUND_AI_PROVIDER_DEFAULT=%s\\n' \"${PLAYGROUND_AI_PROVIDER_DEFAULT-<unset>}\"\n"
            "  printf 'ARGS='\n"
            "  printf '%q ' \"$@\"\n"
            "  printf '\\n'\n"
            f"}} > {self.capture}\n"
        )
        docker.chmod(0o755)

    def tearDown(self):
        self.temp.cleanup()

    def write_state(self, mode):
        (self.playground / ".playground-state").write_text(f"AI_MODE={mode}\nGPU=0\nOBSERVABILITY=0\nMODELS=\n")

    def run_script(self, *args, extra_env=None):
        env = os.environ.copy()
        env.pop("PLAYGROUND_OLLAMA_URL", None)
        env["PATH"] = f"{self.bin}{os.pathsep}{env['PATH']}"
        env["DOCKER_CAPTURE"] = str(self.capture)
        env["DOCKER_REMOVED"] = str(self.removed)
        env.update(extra_env or {})
        return subprocess.run(["bash", str(self.script), *args], capture_output=True, text=True, env=env)

    def test_mode_default_does_not_override_dotenv_ollama_url(self):
        self.write_state("host")
        (self.playground / ".env").write_text("PLAYGROUND_OLLAMA_URL=http://remote-ollama:11434\n")

        result = self.run_script("setup")

        self.assertEqual(result.returncode, 0, result.stderr)
        captured = self.capture.read_text()
        self.assertIn("PLAYGROUND_OLLAMA_URL=<unset>", captured)
        self.assertIn("PLAYGROUND_OLLAMA_DEFAULT_URL=http://host.docker.internal:11434", captured)
        compose = (PLAYGROUND_DIR / "compose.yaml").read_text()
        self.assertIn(
            "${PLAYGROUND_OLLAMA_URL:-${PLAYGROUND_OLLAMA_DEFAULT_URL:-http://ollama:11434}}",
            compose,
        )

    def test_explicit_shell_ollama_url_is_preserved(self):
        self.write_state("docker")

        result = self.run_script("setup", extra_env={"PLAYGROUND_OLLAMA_URL": "http://custom-ollama:11434"})

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PLAYGROUND_OLLAMA_URL=http://custom-ollama:11434", self.capture.read_text())

    def test_no_ollama_mode_preserves_dotenv_remote_provider(self):
        self.write_state("none")
        (self.playground / ".env").write_text("PLAYGROUND_AI_PROVIDER=openai-compatible\n")

        result = self.run_script("setup")

        self.assertEqual(result.returncode, 0, result.stderr)
        captured = self.capture.read_text()
        self.assertIn("PLAYGROUND_AI_PROVIDER=<unset>", captured)
        self.assertIn("PLAYGROUND_AI_PROVIDER_DEFAULT=none", captured)
        compose = (PLAYGROUND_DIR / "compose.yaml").read_text()
        self.assertIn("${PLAYGROUND_AI_PROVIDER:-${PLAYGROUND_AI_PROVIDER_DEFAULT:-ollama}}", compose)

    def test_model_add_is_delegated_to_effective_provider_validation(self):
        self.write_state("none")
        (self.playground / ".env").write_text("PLAYGROUND_AI_PROVIDER=openai-compatible\n")

        result = self.run_script("model", "add", "qwen2.5:3b")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("setup models qwen2.5:3b", self.capture.read_text())

    def test_reset_skips_missing_optional_volumes(self):
        self.write_state("none")
        existing = [
            "investigenie-playground_elasticsearch-data",
            "investigenie-playground_investigenie-data",
            "investigenie-playground_seeder-state",
        ]

        result = self.run_script("reset", extra_env={"DOCKER_VOLUMES": "\n".join(existing)})

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.removed.read_text().splitlines(), existing)
        self.assertFalse((self.playground / ".playground-state").exists())


if __name__ == "__main__":
    unittest.main()

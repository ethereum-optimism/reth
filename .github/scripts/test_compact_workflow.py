import os
from pathlib import Path
import subprocess
import shutil
import textwrap
import tempfile
import unittest

SCRIPT_DIRECTORY = Path(__file__).resolve().parent


class CompactWorkflowTests(unittest.TestCase):
    def test_comparison_base(self):
        cases = [
            ("op-v2.6.0", "refs/heads/topic", "ethereum-optimism/reth", "2.6.0", "op-v2.6.0"),
            ("", "refs/heads/op-v2.6.0", "ethereum-optimism/reth", "2.6.0", "v2.6.0"),
            ("", "refs/heads/topic", "ethereum-optimism/reth", "2.6.0", "v2.6.0"),
            ("", "refs/heads/topic", "paradigmxyz/reth", "2.8.0", "main"),
        ]
        for base_ref, workflow_ref, repository, version, expected in cases:
            with self.subTest(base_ref=base_ref, repository=repository, workflow_ref=workflow_ref):
                with tempfile.TemporaryDirectory() as workspace:
                    Path(workspace, "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
                    environment = dict(os.environ, BASE_REF=base_ref, REF=workflow_ref, REPOSITORY=repository)
                    result = subprocess.run(
                        ["bash", str(SCRIPT_DIRECTORY / "compact-base.sh")],
                        capture_output=True,
                        cwd=workspace,
                        env=environment,
                        text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), expected)

    def test_unmapped_release_fails(self):
        cases = [("refs/heads/op-topic", "2.6.0"), ("refs/heads/topic", "2.6.0-dev")]
        for workflow_ref, version in cases:
            with self.subTest(version=version, workflow_ref=workflow_ref):
                with tempfile.TemporaryDirectory() as workspace:
                    Path(workspace, "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
                    environment = dict(os.environ, BASE_REF="", REF=workflow_ref, REPOSITORY="ethereum-optimism/reth")
                    result = subprocess.run(
                        ["bash", str(SCRIPT_DIRECTORY / "compact-base.sh")],
                        capture_output=True,
                        cwd=workspace,
                        env=environment,
                        text=True,
                    )
                    self.assertNotEqual(result.returncode, 0)

    def test_invalid_release_fails_workflow_step(self):
        workflow = (SCRIPT_DIRECTORY.parent / "workflows" / "compact.yml").read_text()
        select_step = workflow.split("      - name: Select base\n", 1)[1].split("      - name: Checkout base", 1)[0]
        command = textwrap.dedent(select_step.split("        run: |\n", 1)[1])
        with tempfile.TemporaryDirectory() as workspace:
            root = Path(workspace)
            scripts = root / ".github" / "scripts"
            scripts.mkdir(parents=True)
            for name in ["compact-base.sh", "compact-clean-source.sh"]:
                shutil.copyfile(SCRIPT_DIRECTORY / name, scripts / name)
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "2.6.0-dev"\n')
            output_path = root / "github-output"
            environment = dict(os.environ, BASE_REF="", REF="refs/heads/topic", REPOSITORY="ethereum-optimism/reth", GITHUB_OUTPUT=str(output_path), RUNNER_TEMP=str(root))
            result = subprocess.run(["bash", "-e", "-c", command], capture_output=True, cwd=root, env=environment, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(output_path.exists())
            self.assertFalse((root / "compact-clean-source.sh").exists())

    def test_cleanup_requires_a_checkout(self):
        with tempfile.TemporaryDirectory() as workspace:
            result = subprocess.run(
                ["bash", str(SCRIPT_DIRECTORY / "compact-clean-source.sh")],
                capture_output=True,
                cwd=workspace,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)

    def test_source_cleanup_preserves_vectors_and_build_cache(self):
        with tempfile.TemporaryDirectory() as workspace:
            root = Path(workspace)
            subprocess.run(["git", "init", "--quiet"], check=True, cwd=root)
            stale_source = root / "proof_task" / "mod.rs"
            stale_source.parent.mkdir()
            stale_source.write_text("stale module")
            vectors = root / "testdata" / "micro" / "compact" / "Header.json"
            vectors.parent.mkdir(parents=True)
            vectors.write_text("generated vectors")
            subprocess.run(["git", "add", "."], check=True, cwd=root)
            build_cache = root / "target" / "cached-artifact"
            build_cache.parent.mkdir()
            build_cache.write_text("cached build")
            subprocess.run(["bash", str(SCRIPT_DIRECTORY / "compact-clean-source.sh")], check=True, cwd=root)
            self.assertFalse(stale_source.exists())
            self.assertEqual(vectors.read_text(), "generated vectors")
            self.assertEqual(build_cache.read_text(), "cached build")
            self.assertTrue((root / ".git").exists())


if __name__ == "__main__":
    unittest.main()

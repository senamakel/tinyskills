"""Exercise release packaging decisions without Cargo, GitHub, or network I/O."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / "workflows/release.yml"
KNOWN_FAILURE = '''error: failed to prepare local package for uploading

Caused by:
  failed to select a version for the requirement `tinytools = "^0.6"`
  candidate versions found which didn't match: 1.1.2, 1.1.1, 1.1.0, ...
  location searched: crates.io index
  required by package `tinyskills v0.5.0 (/checkout/crates/tinyskills)`
'''


class ReleasePackagingTests(unittest.TestCase):
    def run_packaging(self, output, status):
        workflow = WORKFLOW.read_text()
        # Run the workflow's actual packaging/release block with isolated tools.
        block = workflow[workflow.index("          # tinytools") :]
        script = "set -euo pipefail\nnext=0.5.0\n" + "\n".join(
            line[10:] for line in block.splitlines()
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cargo = root / "cargo"
            cargo.write_text(
                '#!/bin/bash\nprintf "%s\\n" "$PACKAGE_OUTPUT" >&2\n'
                'exit "$PACKAGE_STATUS"\n'
            )
            gh = root / "gh"
            gh.write_text('#!/bin/bash\nprintf "%s\\n" "$*" >> "$RELEASE_CALLS"\n')
            cargo.chmod(0o755)
            gh.chmod(0o755)
            calls = root / "calls"
            result = subprocess.run(
                ["bash", "-c", script],
                cwd=root,
                env={
                    **os.environ,
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "PACKAGE_OUTPUT": output,
                    "PACKAGE_STATUS": str(status),
                    "RELEASE_CALLS": str(calls),
                },
                capture_output=True,
                text=True,
                check=False,
            )
            return result, calls.read_text() if calls.exists() else ""

    def test_success_attaches_crate(self):
        result, calls = self.run_packaging("Packaged tinyskills", 0)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("target/package/tinyskills-0.5.0.crate", calls)

    def test_known_tinytools_resolution_failure_releases_without_crate(self):
        result, calls = self.run_packaging(KNOWN_FAILURE, 101)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, "release create v0.5.0 --generate-notes --title v0.5.0\n")

    def test_unrelated_failures_stop_before_release(self):
        for output in (
            "error: failed to download from crates.io: connection timed out",
            "error: invalid package metadata",
            "error: missing packaged file",
            "error: failed to verify package: tinytools failed to compile",
            KNOWN_FAILURE.replace('tinytools = "^0.6"', 'serde = "^99"'),
            KNOWN_FAILURE.replace("location searched: crates.io index", "location searched: other registry"),
        ):
            with self.subTest(output=output):
                result, calls = self.run_packaging(output, 101)
                self.assertEqual(result.returncode, 101, result.stdout + result.stderr)
                self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()

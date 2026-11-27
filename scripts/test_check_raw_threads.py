#!/usr/bin/env python3
"""Tests for `scripts/check_raw_threads.py`.

Every test builds a throw-away `fixtures/plugins`-shaped tree, runs the real
CLI against it with `--plugins-root`, and asserts the exit code and the printed
diagnostics. The synthetic samples are the four cases the rule distinguishes:
a violation, a justified exemption, an empty justification, and test-only
code; plus the "this is not code" cases (comments, strings, `tests/`).
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "check_raw_threads.py"


def make_tree(root: Path, files: dict[str, str]) -> Path:
    """Write `{relative path: source}` under `root/plugins` and return that dir."""
    plugins_root = root / "plugins"
    for relative, source in files.items():
        path = plugins_root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")
    return plugins_root


def run_check(plugins_root: Path) -> tuple[int, str, str]:
    process = subprocess.run(
        [sys.executable, str(SCRIPT), "--plugins-root", str(plugins_root)],
        capture_output=True,
        text=True,
        check=False,
    )
    return process.returncode, process.stdout, process.stderr


class RawThreadCheckTests(unittest.TestCase):
    def test_clean_production_code_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    let ticket = ServiceThreads::spawn(\"demo\", || {});\n"
                        "    drop(ticket);\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("0 raw spawn(s) outside #[cfg(test)]", out)
        self.assertIn("0 violation(s)", out)

    def test_bare_thread_spawn_is_a_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:2: 违规:", out)
        self.assertIn("1 violation(s)", out)

    def test_std_thread_spawn_is_a_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    std::thread::spawn(move || {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:2: 违规:", out)

    def test_reason_marker_exempts_the_next_line(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // cordis:allow-raw-thread: 一次性任务，join 在函数内完成\n"
                        "    let handle = thread::spawn(|| {});\n"
                        "    handle.join().expect(\"join\");\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("0 violation(s), 1 exemption(s)", out)

    def test_marker_skips_blank_lines_to_the_previous_non_empty_line(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // cordis:allow-raw-thread: 一次性任务\n"
                        "\n"
                        "\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("1 exemption(s)", out)

    def test_empty_reason_is_a_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // cordis:allow-raw-thread:\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:3: 违规:", out)
        self.assertIn("缺少非空理由", out)
        self.assertIn("0 exemption(s)", out)

    def test_whitespace_only_reason_is_a_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // cordis:allow-raw-thread:    \n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("缺少非空理由", out)

    def test_marker_only_covers_the_immediately_following_statement(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // cordis:allow-raw-thread: 一次性任务\n"
                        "    let _ = 1;\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:4: 违规:", out)

    def test_cfg_test_module_is_excluded(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {}\n"
                        "\n"
                        "#[cfg(test)]\n"
                        "mod tests {\n"
                        "    use super::*;\n"
                        "    fn helper() {\n"
                        "        thread::spawn(|| {});\n"
                        "    }\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("0 raw spawn(s) outside #[cfg(test)]", out)

    def test_cfg_test_item_is_excluded(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "#[cfg(test)]\n"
                        "fn only_in_tests() {\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                        "fn boot() {}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)

    def test_cfg_test_mod_declaration_does_not_hide_later_code(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "#[cfg(test)]\n"
                        "mod tests;\n"
                        "fn boot() {\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:4: 违规:", out)

    def test_commented_out_code_is_not_a_violation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    // thread::spawn(|| {});\n"
                        "    /*\n"
                        "    std::thread::spawn(|| {});\n"
                        "    */\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("0 raw spawn(s) outside #[cfg(test)]", out)

    def test_string_literals_are_not_code(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    let a = \"thread::spawn\";\n"
                        "    let b = r#\"std::thread::spawn\"#;\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)

    def test_marker_inside_a_block_comment_does_not_exempt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": (
                        "fn boot() {\n"
                        "    /*\n"
                        "    // cordis:allow-raw-thread: 假标记\n"
                        "    */\n"
                        "    thread::spawn(|| {});\n"
                        "}\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 1)
        self.assertIn("plugins/demo/src/lib.rs:5: 违规:", out)

    def test_tests_directory_is_not_scanned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/lib.rs": "fn boot() {}\n",
                    "demo/tests/integration.rs": "thread::spawn(|| {});\n",
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("1 file(s)", out)

    def test_nested_src_files_are_scanned_and_both_exemptions_counted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = make_tree(
                Path(tmp),
                {
                    "demo/src/net/mod.rs": (
                        "// cordis:allow-raw-thread: 一次性任务\n"
                        "thread::spawn(|| {});\n"
                        "\n"
                        "// cordis:allow-raw-thread: 一次性任务\n"
                        "std::thread::spawn(|| {});\n"
                    )
                },
            )
            code, out, _ = run_check(root)
        self.assertEqual(code, 0)
        self.assertIn("0 violation(s), 2 exemption(s)", out)

    def test_missing_plugins_root_is_a_usage_error(self) -> None:
        code, _, err = run_check(Path("/no/such/plugins-root"))
        self.assertEqual(code, 2)
        self.assertIn("plugins root not found", err)


if __name__ == "__main__":
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Rework regressions for the backlog consistency checker (findings F1-F6)."""
# The corrective rework added enough regression coverage that a single test
# module would exceed the 500-line limit enforced by static analysis, so the
# findings F1-F6 cases live here: record shapes (F1), link destinations (F2),
# fence tracking (F3), root containment (F4), tracked enumeration (F5), and
# the CLI exit-status contract (F6).  Shared fixture builders are imported
# from the main suite module.

import os
import subprocess  # nosec B404 - index-authoritative git fixtures, fixed argv
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_backlog_consistency as checker  # noqa: E402  (path set above)
from test_check_backlog_consistency import (  # noqa: E402  (path set above)
    archived_remediation_doc,
    counter_paragraph,
    sample_records,
)


class BacklogConsistencyReworkTests(unittest.TestCase):
    """One regression per corrective-rework finding plus its failure paths."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def write_archived_fixture(self, root, **kwargs):
        return self.write(root, checker.ARCHIVED_INDEX_NAME, archived_remediation_doc(**kwargs))

    def _git(self, *argv):
        """Run git with a fixed argument vector inside the test tree."""
        return subprocess.run(  # nosec B603, B607 - fixed argv, no shell
            ["git", *argv], capture_output=True, text=True, check=True, timeout=60
        )

    # ── record shapes (rework finding F1) ───────────────────────────────────
    def test_malformed_top_level_checkbox_is_reported(self):
        text = counter_paragraph() + sample_records() + "- [ ] Investigate regression\n"
        # parse_records silently omits the box: it is not a countable record.
        self.assertEqual(len(checker.parse_records(text)), 4)
        # The shape check must surface the omission with its source line.
        problems = checker.check_record_shapes(text, Path("docs/task.md"))
        self.assertEqual(len(problems), 1)
        self.assertIn(f"task.md:{text.count(chr(10))}", problems[0])
        self.assertIn("Investigate regression", problems[0])

    def test_run_checks_reports_missing_stable_id_before_counters(self):
        root = self.make_root()
        text = counter_paragraph() + sample_records() + "- [ ] Investigate regression\n"
        self.write(root, "docs/task.md", text)
        self.write_archived_fixture(root)
        records, _, problems = checker.run_checks(root, root / checker.TASK_INDEX_NAME)
        self.assertEqual(len(records), 4)
        self.assertEqual(len(problems), 1, problems)
        self.assertTrue(problems[0].startswith("record: task.md:7"), problems)

    def test_indented_subcheckbox_is_not_a_record_finding(self):
        text = (
            counter_paragraph()
            + sample_records()
            + "  - [ ] nested detail under the last record\n"
        )
        self.assertEqual(checker.check_record_shapes(text, Path("docs/task.md")), [])

    # ── link destinations (rework finding F2) ────────────────────────────────
    def test_angle_destination_with_spaces_is_checked(self):
        root = self.make_root()
        guide = self.write(root, "docs/guide.md", "[missing](<missing file.md>)\n")
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing file.md'", problems[0])

    def test_valid_angle_destination_passes_with_and_without_title(self):
        root = self.make_root()
        self.write(root, "docs/valid file.md", "# Valid\n")
        guide = self.write(
            root,
            "docs/guide.md",
            "[ok](<valid file.md>) and [titled](<valid file.md> \"the title\")\n",
        )
        self.assertEqual(checker.check_links(root, [guide]), [])

    def test_plain_destination_with_title_is_checked(self):
        root = self.make_root()
        guide = self.write(root, "docs/guide.md", "[bad](missing.md \"doc\")\n")
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing.md'", problems[0])

    def test_definition_with_angle_destination_resolves(self):
        root = self.make_root()
        self.write(root, "docs/user guide.md", "# User guide\n")
        guide = self.write(
            root, "docs/guide.md", "[guide]: <user guide.md>\nSee [the guide][guide].\n"
        )
        self.assertEqual(checker.check_links(root, [guide]), [])

    def test_definition_with_broken_angle_destination_is_reported(self):
        root = self.make_root()
        guide = self.write(
            root, "docs/guide.md", "[guide]: <missing file.md>\nSee [the guide][guide].\n"
        )
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing file.md'", problems[0])

    def test_close_fence_allows_trailing_tab(self):
        # CommonMark permits trailing whitespace including tabs on the closing
        # fence; only spaces used to be accepted, which left the fence open
        # and swallowed every following record, heading and link.
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "```markdown\nfenced [x](missing.md)\n```\t\n[after](missing.md)\n",
        )
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing.md'", problems[0])

    # ── fence tracking (rework finding F3) ───────────────────────────────────
    def test_longer_backtick_fence_keeps_inner_content_literal(self):
        # A four-backtick fence is not closed by the triple-backtick pair
        # inside it: the sample link between them stays literal content.
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "````markdown\n```shell\necho hi\n```\n[example](missing.md)\n````\n",
        )
        self.assertEqual(checker.check_links(root, [page]), [])
        self.assertEqual(checker.document_anchors(page), set())

    def test_longer_tilde_fence_keeps_inner_content_literal(self):
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "~~~~markdown\n~~~\n[example](missing.md)\n~~~\n~~~~\n",
        )
        self.assertEqual(checker.check_links(root, [page]), [])
        self.assertEqual(checker.document_anchors(page), set())

    def test_fence_of_different_character_does_not_close(self):
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "~~~markdown\n```\n[example](missing.md)\n```\n~~~\n",
        )
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_links_outside_fences_are_still_validated(self):
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "````markdown\n```\nfenced [x](missing.md)\n```\n````\n[outside](missing.md)\n",
        )
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing.md'", problems[0])

    # ── root containment (rework finding F4) ─────────────────────────────────
    def test_link_escaping_root_is_reported(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-escape-")
        self.addCleanup(temporary.cleanup)
        parent = Path(temporary.name).resolve()
        root = parent / "repo"
        root.mkdir()
        (parent / "outside.md").write_text("# Outside\n", encoding="utf-8")
        guide = self.write(root, "docs/guide.md", "[out](../../outside.md)\n")
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("resolves outside the repository root", problems[0])
        self.assertIn("'../../outside.md'", problems[0])

    def test_symlink_escape_is_reported(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-outside-")
        self.addCleanup(temporary.cleanup)
        outside_dir = Path(temporary.name).resolve()
        outside = outside_dir / "target.md"
        outside.write_text("# Outside\n", encoding="utf-8")
        root = self.make_root()
        guide = self.write(root, "docs/guide.md", "[escape](escape.md)\n")
        os.symlink(outside, root / "docs" / "escape.md")
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("resolves outside the repository root", problems[0])

    def test_in_tree_parent_link_still_passes(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "# Target\n")
        page = self.write(root, "docs/sub/page.md", "[up](../target.md)\n")
        self.assertEqual(checker.check_links(root, [target, page]), [])

    # ── tracked enumeration (rework finding F5) ──────────────────────────────
    def _staged_git_root(self):
        """Return a Git root where scratch.md and ignored.md stay untracked."""
        root = self.make_root()
        self._git("-C", str(root), "init", "-q")
        self.write(root, "docs/task.md", counter_paragraph() + sample_records())
        self.write_archived_fixture(root)
        self.write(root, "docs/guide.md", "See [index](task.md).\n")
        self.write(root, "scratch.md", "[broken](missing.md)\n")
        self.write(root, "ignored.md", "[broken](missing.md)\n")
        self.write(root, ".gitignore", "ignored.md\n")
        self._git(
            "-C", str(root),
            "add", "docs/task.md", checker.ARCHIVED_INDEX_NAME, "docs/guide.md", ".gitignore",
        )
        return root

    def test_git_root_enumerates_only_tracked_markdown(self):
        root = self._staged_git_root()
        files = checker.collect_markdown_files(root)
        self.assertEqual(
            files,
            [
                root / "docs" / "guide.md",
                root / "docs" / "task-remediation-2026-07.md",
                root / "docs" / "task.md",
            ],
        )

    def test_git_root_validation_ignores_untracked_and_ignored_links(self):
        root = self._staged_git_root()
        self.assertEqual(checker.main(["--root", str(root), "--quiet"]), 0)

    def test_non_git_root_falls_back_to_walking(self):
        root = self.make_root()
        self.write(root, "docs/task.md", counter_paragraph() + sample_records())
        self.write_archived_fixture(root)
        self.write(root, "scratch.md", "[broken](missing.md)\n")
        self.assertEqual(checker.main(["--root", str(root), "--quiet"]), 1)

    def test_tracked_enumeration_requires_git_checkout(self):
        self.assertIsNone(checker._tracked_markdown_files(self.make_root()))

    # ── CLI error paths (rework finding F6) ──────────────────────────────────
    def test_main_returns_2_for_missing_task_index(self):
        root = self.make_root()
        self.assertEqual(checker.main(["--root", str(root), "--quiet"]), 2)

    def test_main_returns_2_for_missing_ledger_snapshot(self):
        root = self.make_root()
        self.write(root, "docs/task.md", counter_paragraph() + sample_records())
        self.write_archived_fixture(root)
        self.assertEqual(
            checker.main(
                ["--root", str(root), "--ledger", str(root / "nope.json"), "--quiet"]
            ),
            2,
        )


class InlineSyntaxTests(unittest.TestCase):
    """Regressions for the 2026-09-18 refinery rework (threads jroR8/jroR-/jroSB)."""

    # Three current-head defects made the every-tracked-file audit report
    # false results: inline-code and escaped-bracket link examples were
    # validated as real links (jroR8), plain destinations were truncated at
    # the first ")" so balanced-paren targets were falsely broken (jroR-),
    # and heading anchors were slugged from raw markup instead of rendered
    # text (jroSB).

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    # ── code spans and escaped brackets (jroR8) ──────────────────────────────
    def test_code_span_link_example_is_not_validated(self):
        # A link shown as inline code is documentation, not a reference:
        # validating it failed CI on ordinary examples.
        root = self.make_root()
        page = self.write(
            root, "docs/guide.md", "Write `` `[example](missing.md)` `` inline.\n"
        )
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_escaped_bracket_example_is_not_validated(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "Literal \\[example\\](missing.md).\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_real_link_on_code_span_line_is_still_validated(self):
        # Masking must not over-match: only the code-span example is
        # exempted, the genuine broken link beside it is still reported.
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "`[example](absent-one.md)` but [real](absent-two.md)\n",
        )
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'absent-two.md'", problems[0])
        self.assertNotIn("absent-one.md", problems[0])

    def test_double_backslash_bracket_is_a_real_link(self):
        # ``\\[label](target.md)`` escapes the backslash, so the bracket is
        # active and the link must be checked.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "\\\\[label](missing.md)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing.md'", problems[0])

    # ── balanced plain destinations (jroR-) ──────────────────────────────────
    def test_balanced_paren_destination_that_exists_passes(self):
        # ``[^)\s]+`` used to truncate the destination at the first ``)``,
        # checking ``ADR_(draft`` and falsely reporting the file broken.
        root = self.make_root()
        self.write(root, "docs/ADR_(draft).md", "# Draft\n")
        page = self.write(root, "docs/guide.md", "[design](ADR_(draft).md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_broken_balanced_paren_destination_reports_full_target(self):
        # No lost detections: a genuinely missing target is still reported,
        # now under its full balanced name rather than the truncated one.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[broken](missing_(part).md)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing_(part).md'", problems[0])

    def test_escaped_paren_destination_resolves(self):
        root = self.make_root()
        self.write(root, "docs/file(1).md", "# One\n")
        page = self.write(root, "docs/guide.md", "[one](file\\(1\\).md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_escaped_paren_broken_target_is_reported(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[nope](no\\(pe\\).md)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'no(pe).md'", problems[0])

    def test_unclosed_destination_stays_literal(self):
        # A destination without a closing parenthesis is not linkified by
        # GitHub, so it must neither pass nor be reported as a broken link.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "text [x](no-close\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    # ── rendered-text heading anchors (jroSB) ────────────────────────────────
    def test_link_heading_anchor_uses_rendered_text(self):
        # GitHub exposes ``#api`` for ``## [API](guide.md)``; the anchor from
        # raw markup (``apiguidemd``) is no longer accepted.
        root = self.make_root()
        target = self.write(root, "docs/target.md", "## [API](guide.md)\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#api)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_raw_markup_heading_anchor_is_rejected(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "## [API](guide.md)\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#apiguidemd)\n")
        problems = checker.check_links(root, [guide, target])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("missing anchor '#apiguidemd'", problems[0])

    def test_code_span_heading_keeps_content_anchor(self):
        # ``## `API` usage`` renders as ``API usage``: the backticks go, the
        # words stay, and the anchor is ``api-usage``.
        root = self.make_root()
        target = self.write(root, "docs/target.md", "## `API` usage\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#api-usage)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_emphasis_heading_anchor_ignores_markers(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "## **Bold** move\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#bold-move)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])


class CounterSearchTextTests(unittest.TestCase):
    """Regressions for the filtered-counter-search review finding."""

    # check_counters and check_archived_counter used to search raw text, so a
    # fenced documentation example could be selected as the prose counter or
    # reported as an arithmetic inconsistency.  Every counter search now runs
    # on the same fence-filtered view the record parser uses.

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def write_archived_fixture(self, root, **kwargs):
        return self.write(root, checker.ARCHIVED_INDEX_NAME, archived_remediation_doc(**kwargs))

    def test_fenced_prose_counter_example_is_not_selected(self):
        # A fenced example of the counter wording must neither be selected
        # over the real prose counter nor contradict it.
        text = (
            "```markdown\n"
            "Current status: **9/9 (100.0%)** records complete. The retained\n"
            "baseline is **9/9**, with **9/9** new corrective and **9/9**\n"
            "engineering records complete.\n"
            "```\n"
            + counter_paragraph()
            + sample_records()
        )
        records = checker.parse_records(text)
        self.assertEqual(checker.check_counters(text, records), [])

    def test_fenced_arithmetic_example_does_not_fail(self):
        # ``**1/3 (50.0%)**`` is arithmetically inconsistent; inside a fence
        # it is documentation and must not be reported as a bad percentage.
        text = (
            "```markdown\n"
            "Progress example: **1/3 (50.0%)**\n"
            "```\n"
            + counter_paragraph()
            + sample_records()
        )
        records = checker.parse_records(text)
        self.assertEqual(checker.check_counters(text, records), [])

    def test_fenced_archived_counter_example_is_not_selected(self):
        # A fenced archived-counter example with wrong numbers must not be
        # taken as the counter: the real prose counter is validated instead.
        root = self.make_root()
        text = (
            "```markdown\n"
            "The archived remediation remains **1/3 (50.0%)**.\n"
            "```\n"
            + counter_paragraph()
        )
        self.write(root, "docs/task.md", text)
        self.write_archived_fixture(root)
        self.assertEqual(checker.check_archived_counter(text, root), [])

    def test_real_counter_drift_is_still_reported(self):
        # No lost detections: with the fenced example present, a genuinely
        # wrong prose counter still fails.
        text = (
            "```markdown\n"
            "Progress example: **1/3 (50.0%)**\n"
            "```\n"
            + counter_paragraph(overall=(4, 4, 100.0))
            + sample_records()
        )
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("overall says 4/4" in problem for problem in problems), problems)


if __name__ == "__main__":
    unittest.main()

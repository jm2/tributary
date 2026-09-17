#!/usr/bin/env python3
"""Exercise the read-only backlog consistency checker against synthetic trees."""

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_backlog_consistency as checker  # noqa: E402  (path set above)


REPOSITORY = Path(__file__).resolve().parent.parent


def counter_paragraph(
    *,
    overall=(1, 4, 25.0),
    baseline=(1, 2),
    corrective=(0, 1),
    engineering=(0, 1),
    archived=(223, 226, 98.7),
    include_labels=True,
):
    """Build a counter paragraph matching the production wording."""
    overall_text = f"{overall[0]}/{overall[1]} ({overall[2]}%)"
    baseline_text = f"{baseline[0]}/{baseline[1]}"
    corrective_text = f"{corrective[0]}/{corrective[1]}"
    engineering_text = f"{engineering[0]}/{engineering[1]}"
    archived_text = f"{archived[0]}/{archived[1]} ({archived[2]}%)"
    if include_labels:
        return (
            f"Current status: **{overall_text}** implementation records complete: the "
            f"retained baseline is\n**{baseline_text}**, with **{corrective_text}** new "
            f"corrective and **{engineering_text}** engineering records complete. The "
            f"archived remediation remains **{archived_text}**.\n"
        )
    return (
        f"Current status: **{overall_text}** records complete. Some other **{baseline_text}** "
        f"note and an unrelated **{archived_text}** sentence.\n"
    )


def sample_records():
    """Return a four-record index body matching the default counter paragraph."""
    return (
        "- [x] **P1.1-A** — done\n"
        "- [ ] **P1.1-B** — pending\n"
        "- [ ] **R1** — corrective\n"
        "- [ ] **Q1** — engineering\n"
    )


class BacklogConsistencyTests(unittest.TestCase):
    """One test per invariant plus the pass paths and the real repository."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    # ── records ──────────────────────────────────────────────────────────────
    def test_parse_records_reads_checkbox_state(self):
        records = checker.parse_records(
            "# t\n- [x] **P1.1-A** — a\n- [ ] **R1 — title**\n- [ ] **Q1 — t** (P2; ...)\n"
        )
        self.assertEqual([r.identifier for r in records], ["P1.1-A", "R1", "Q1"])
        self.assertEqual([r.complete for r in records], [True, False, False])
        self.assertEqual([r.line for r in records], [2, 3, 4])

    def test_duplicate_ids_fail(self):
        root = self.make_root()
        index = self.write(root, "docs/task.md", "- [ ] **R1** — a\n- [ ] **R1** — b\n")
        records = checker.parse_records(index.read_text(encoding="utf-8"))
        problems = checker.check_unique_ids(records, root, index)
        self.assertEqual(len(problems), 1)
        self.assertIn("R1", problems[0])
        self.assertIn("docs/task.md:1", problems[0])
        self.assertIn("docs/task.md:2", problems[0])

    def test_unique_ids_pass(self):
        root = self.make_root()
        index = self.write(root, "docs/task.md", "- [ ] **R1** — a\n- [ ] **R2** — b\n")
        records = checker.parse_records(index.read_text(encoding="utf-8"))
        self.assertEqual(checker.check_unique_ids(records, root, index), [])

    # ── counters ─────────────────────────────────────────────────────────────
    def test_correct_counters_pass(self):
        text = counter_paragraph() + sample_records()
        records = checker.parse_records(text)
        self.assertEqual(checker.check_counters(text, records), [])

    def test_counter_mismatch_fails(self):
        text = counter_paragraph(overall=(4, 4, 100.0)) + sample_records()
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("overall says 4/4" in problem for problem in problems), problems)

    def test_category_counter_mismatch_fails(self):
        text = counter_paragraph(corrective=(1, 1)) + sample_records()
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("corrective says 1/1" in problem for problem in problems), problems)

    def test_percentage_mismatch_fails(self):
        text = counter_paragraph(overall=(1, 4, 50.0)) + sample_records()
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("states 50.0%" in problem for problem in problems), problems)

    def test_archived_counter_arithmetic_is_checked(self):
        text = counter_paragraph(archived=(223, 226, 99.9)) + sample_records()
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("223/226" in problem for problem in problems), problems)

    def test_missing_counter_wording_fails(self):
        text = counter_paragraph(include_labels=False) + sample_records()
        records = checker.parse_records(text)
        problems = checker.check_counters(text, records)
        self.assertTrue(any("baseline" in problem for problem in problems), problems)
        self.assertTrue(any("corrective" in problem for problem in problems), problems)

    # ── links ────────────────────────────────────────────────────────────────
    def test_valid_link_and_anchor_pass(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "# Target\n\n## Topic one\n")
        guide = self.write(
            root,
            "docs/guide.md",
            "See [topic one](target.md#topic-one) and [target](target.md).\n",
        )
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_broken_file_link_fails(self):
        root = self.make_root()
        guide = self.write(root, "docs/guide.md", "See [missing](missing.md).\n")
        problems = checker.check_links(root, [guide])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing.md'", problems[0])

    def test_broken_anchor_fails(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "## Present\n")
        guide = self.write(root, "docs/guide.md", "See [gone](target.md#gone).\n")
        problems = checker.check_links(root, [guide, target])
        self.assertEqual(len(problems), 1)
        self.assertIn("missing anchor '#gone'", problems[0])

    def test_same_file_anchor_is_checked(self):
        root = self.make_root()
        good = self.write(root, "docs/guide.md", "## Topic\n\n[jump](#topic)\n")
        self.assertEqual(checker.check_links(root, [good]), [])
        bad = self.write(root, "docs/other.md", "[jump](#topic)\n")
        problems = checker.check_links(root, [bad])
        self.assertEqual(len(problems), 1)
        self.assertIn("missing anchor '#topic'", problems[0])

    def test_external_image_and_definition_targets(self):
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "[site](https://example.com/x)\n"
            "![image](missing.png)\n"
            "[label]: https://example.com/y\n",
        )
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_relative_definition_target_is_checked(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[ref]: missing.md\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1)
        self.assertIn("broken link target 'missing.md'", problems[0])

    def test_fenced_code_is_ignored(self):
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "```markdown\n# Not a heading\n[broken](missing.md)\n```\n",
        )
        self.assertEqual(checker.check_links(root, [page]), [])
        self.assertEqual(checker.document_anchors(page), set())

    def test_duplicate_headings_get_suffixes(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "## Same\n\n## Same\n\n[x](guide.md#same-1)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    # ── ledger snapshot ──────────────────────────────────────────────────────
    def records_fixture(self):
        text = counter_paragraph() + sample_records()
        return checker.parse_records(text)

    def test_ledger_missing_active_mapping_fails(self):
        records = self.records_fixture()
        snapshot = {"records": {"P1.1-B": {"bead": "tr-b", "issue": 1}}}
        problems = checker.check_ledger(
            snapshot, records, Path("docs/task.md")
        )
        self.assertTrue(any("'R1' has no mapping entry" in p for p in problems), problems)
        self.assertTrue(any("'Q1' has no mapping entry" in p for p in problems), problems)

    def test_ledger_complete_record_may_be_omitted(self):
        records = self.records_fixture()
        snapshot = {
            "records": {
                "P1.1-B": {"bead": "tr-b", "issue": 1},
                "R1": {"bead": "tr-r", "issue": 2},
                "Q1": {"bead": "tr-q", "issue": 3},
            }
        }
        self.assertEqual(checker.check_ledger(snapshot, records, Path("docs/task.md")), [])

    def test_ledger_active_record_needs_bead_and_issue(self):
        records = self.records_fixture()
        snapshot = {
            "records": {
                "P1.1-B": {"bead": "tr-b", "issue": 1},
                "R1": {"issue": 2},
                "Q1": {"bead": "tr-q"},
            }
        }
        problems = checker.check_ledger(snapshot, records, Path("docs/task.md"))
        self.assertTrue(any("'R1' has no bead mapping" in p for p in problems), problems)
        self.assertTrue(any("'Q1' has no issue mapping" in p for p in problems), problems)

    def test_ledger_unknown_id_fails(self):
        records = self.records_fixture()
        snapshot = {"records": {"Z9": {"bead": "tr-z", "issue": 9}}}
        problems = checker.check_ledger(snapshot, records, Path("docs/task.md"))
        self.assertTrue(any("unknown record ID 'Z9'" in p for p in problems), problems)

    def test_ledger_merged_but_unchecked_fails(self):
        records = self.records_fixture()
        snapshot = {
            "records": {
                "P1.1-B": {"bead": "tr-b", "issue": 1},
                "R1": {"bead": "tr-r", "issue": 2},
                "Q1": {"bead": "tr-q", "issue": 3, "merged": True},
            }
        }
        problems = checker.check_ledger(snapshot, records, Path("docs/task.md"))
        self.assertTrue(any("merged-but-unreconciled" in p for p in problems), problems)

    def test_ledger_stale_review_head_fails(self):
        records = self.records_fixture()
        snapshot = {
            "records": {
                "P1.1-B": {"bead": "tr-b", "issue": 1},
                "R1": {
                    "bead": "tr-r",
                    "issue": 2,
                    "head_sha": "aaaaaaaaaaaa",
                    "reviewed_sha": "bbbbbbbbbbbb",
                },
                "Q1": {"bead": "tr-q", "issue": 3},
            }
        }
        problems = checker.check_ledger(snapshot, records, Path("docs/task.md"))
        self.assertTrue(any("stale review head" in p for p in problems), problems)

    def test_ledger_requires_records_object(self):
        problems = checker.check_ledger([], [], Path("docs/task.md"))
        self.assertEqual(len(problems), 1)
        self.assertIn("'records'", problems[0])

    # ── integration ──────────────────────────────────────────────────────────
    def test_repository_index_is_consistent(self):
        records, markdown_files, problems = checker.run_checks(
            REPOSITORY, REPOSITORY / checker.TASK_INDEX_NAME
        )
        self.assertEqual(problems, [])
        self.assertEqual(len(records), 57)
        self.assertEqual(sum(1 for record in records if record.complete), 17)
        self.assertGreater(len(markdown_files), 0)

    def test_main_is_read_only(self):
        root = self.make_root()
        index = self.write(
            root, "docs/task.md", counter_paragraph() + sample_records()
        )
        target = self.write(root, "docs/target.md", "## Topic\n")
        before = {
            path: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (index, target)
        }
        self.assertEqual(checker.main(["--root", str(root)]), 0)
        after = {
            path: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (index, target)
        }
        self.assertEqual(before, after)

    def test_main_reports_failures(self):
        root = self.make_root()
        self.write(root, "docs/task.md", counter_paragraph() + sample_records())
        self.write(root, "docs/broken.md", "[x](missing.md)\n")
        self.assertEqual(checker.main(["--root", str(root), "--quiet"]), 1)

    def test_main_accepts_ledger_snapshot(self):
        root = self.make_root()
        self.write(root, "docs/task.md", counter_paragraph() + sample_records())
        snapshot = self.write(
            root,
            "docs/ledger.json",
            json.dumps(
                {
                    "records": {
                        "P1.1-B": {"bead": "tr-b", "issue": 1},
                        "R1": {"bead": "tr-r", "issue": 2},
                        "Q1": {"bead": "tr-q", "issue": 3},
                    }
                }
            ),
        )
        self.assertEqual(
            checker.main(["--root", str(root), "--ledger", str(snapshot), "--quiet"]), 0
        )


if __name__ == "__main__":
    unittest.main()

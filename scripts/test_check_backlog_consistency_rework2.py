#!/usr/bin/env python3
"""Rework round 2 regressions for the backlog consistency checker (J1-J3)."""
# The corrective rounds added enough regression coverage that no single test
# module can hold it under the 500-line limit enforced by static analysis, so
# each refinery round gets its own module.  This one covers the 2026-09-19
# parser-gap round: nested-bracket link labels (J1), setext heading anchors
# (J2), and query components in relative targets (J3).

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_backlog_consistency as checker  # noqa: E402  (path set above)


class ParserGapTests(unittest.TestCase):
    """One regression per parser-gap finding plus its failure path."""

    # Three defects at the re-reviewed head made the every-tracked-file
    # audit report false results in both directions: nested-bracket link
    # labels never matched INLINE_LINK_OPEN so broken destinations bypassed
    # the audit (j5b2Z), setext headings contributed no anchors so valid
    # links were reported missing (j5b2e), and query components were statted
    # as literal filename text so ``guide.md?plain=1`` was reported broken
    # (j5b2m).

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    # ── nested-bracket link labels (j5b2Z) ───────────────────────────────────
    def test_nested_bracket_label_broken_destination_is_reported(self):
        # ``[^]]*`` stopped at the first ``]``, so a label with one level of
        # balanced brackets could never close and the link vanished from
        # the audit; the broken destination must be reported.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[outer [inner]](missing.md)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing.md'", problems[0])

    def test_nested_bracket_label_valid_destination_passes(self):
        # A nested-bracket label pointing at an existing file is a real
        # GitHub link and must pass.
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "[outer [inner]](real.md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    # ── setext heading anchors (j5b2e) ───────────────────────────────────────
    def test_setext_equals_anchor_resolves(self):
        # GitHub exposes ``#title`` for ``Title`` under ``====`` exactly as
        # for ``# Title``; the link must not be reported missing.
        root = self.make_root()
        target = self.write(root, "docs/target.md", "Title\n=====\n\nbody\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#title)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_setext_dash_anchor_resolves(self):
        root = self.make_root()
        target = self.write(root, "docs/target.md", "Subtitle\n---\n\nbody\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#subtitle)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_absent_anchor_after_setext_heading_still_fails(self):
        # Recognising setext headings must not loosen the audit: a link to
        # an anchor no heading exposes is still reported.
        root = self.make_root()
        target = self.write(root, "docs/target.md", "Title\n=====\n\nbody\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#absent)\n")
        problems = checker.check_links(root, [guide, target])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("missing anchor '#absent'", problems[0])

    def test_duplicate_setext_headings_get_count_suffixes(self):
        # Repeated heading text exposes ``#slug`` then ``#slug-1``, the
        # same duplicate-count suffixing ATX headings already get.
        root = self.make_root()
        target = self.write(
            root, "docs/target.md", "Title\n=====\n\nbody\n\nTitle\n=====\n\nmore\n"
        )
        guide = self.write(root, "docs/guide.md", "[a](target.md#title) [b](target.md#title-1)\n")
        self.assertEqual(checker.check_links(root, [guide, target]), [])

    def test_thematic_break_after_blank_line_makes_no_anchor(self):
        # An all-dash line under a blank line is a thematic break, not a
        # setext heading; it must expose no anchor for a link to hit.
        root = self.make_root()
        target = self.write(root, "docs/target.md", "Intro\n\n---\n\nbody\n")
        guide = self.write(root, "docs/guide.md", "[jump](target.md#intro)\n")
        problems = checker.check_links(root, [guide, target])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("missing anchor '#intro'", problems[0])

    def test_setext_heading_inside_code_fence_makes_no_anchor(self):
        # Fence-filtered content is literal text: an underlined phrase
        # inside a code block exposes no anchor.
        root = self.make_root()
        target = self.write(
            root, "docs/target.md", "```\nTitle\n=====\n```\n\nbody\n"
        )
        guide = self.write(root, "docs/guide.md", "[jump](target.md#title)\n")
        problems = checker.check_links(root, [guide, target])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("missing anchor '#title'", problems[0])

    # ── query components in relative targets (j5b2m) ─────────────────────────
    def test_query_component_resolves_to_file(self):
        # GitHub resolves ``guide.md?plain=1`` to ``guide.md``; the query
        # must be split off before the filesystem check.
        root = self.make_root()
        self.write(root, "docs/guide.md", "# Guide\n")
        page = self.write(root, "docs/other.md", "[view](guide.md?plain=1)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_query_with_fragment_keeps_both_components(self):
        # The path ends at ``?``, the fragment still starts at ``#``.
        root = self.make_root()
        self.write(root, "docs/guide.md", "# Guide\n\n## Setup\n")
        page = self.write(root, "docs/other.md", "[view](guide.md?plain=1#setup)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_query_on_missing_file_still_fails(self):
        # Splitting the query must not hide a genuinely missing file.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[view](absent.md?plain=1)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'absent.md?plain=1'", problems[0])


if __name__ == "__main__":
    unittest.main()

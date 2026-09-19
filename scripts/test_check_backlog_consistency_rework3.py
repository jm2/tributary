#!/usr/bin/env python3
"""Rework round 3 regressions for the backlog consistency checker (pbF-pbL)."""
# Each refinery round gets its own test module to stay under the 500-line
# static-analysis ceiling.  This one covers the 2026-09-19 round-3 findings
# at head f7dfd176: indented code blocks scanned as links (pbF), link titles
# in only one delimiter style (pbH), protocol-relative targets validated as
# filesystem paths (pbJ), and raw fragments compared against decoded anchors
# (pbL).

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import backlog_markdown  # noqa: E402  (path set above)
import check_backlog_consistency as checker  # noqa: E402  (path set above)


class IndentedCodeTests(unittest.TestCase):
    """Indented code blocks are literal text; lazy continuations are not."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_blank_line_preceded_indented_example_is_not_audited(self):
        # A documentation example indented into an indented code block
        # renders verbatim: its broken destination must not fail the audit.
        root = self.make_root()
        page = self.write(
            root, "docs/guide.md", "Intro\n\n    [example](missing.md)\n\nOutro\n"
        )
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_tab_indented_example_is_not_audited(self):
        # A leading tab is four columns of indentation: code, not prose.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "Intro\n\n\t[example](missing.md)\n\nOutro\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_indented_code_block_ends_at_lesser_indent(self):
        # Prose after the indented block is audited again.
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(
            root,
            "docs/guide.md",
            "Intro\n\n    [example](missing.md)\n\nSee [this](real.md).\n",
        )
        # The example is code, the trailing link is real and valid: silence.
        self.assertEqual(checker.check_links(root, [page]), [])
        page = self.write(root, "docs/guide.md", "Intro\n\n    example\n\nSee [this](absent.md).\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'absent.md'", problems[0])

    def test_lazy_continuation_broken_link_is_still_audited(self):
        # The chosen CommonMark boundary: 4-space indentation opens code
        # only when no paragraph is open.  Directly after paragraph text
        # the indented line lazily continues that paragraph, its links
        # render, and a broken destination must still be reported.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "Intro\n    [example](missing.md)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing.md'", problems[0])

    def test_lazy_continuation_valid_link_passes(self):
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "Intro\n    [example](real.md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_blank_line_between_paragraph_and_indented_line_opens_code(self):
        # One blank line is enough to close the paragraph: the indented
        # line is code even though prose appeared shortly before.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "Intro\n\n    [example](missing.md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_indented_example_after_heading_is_code(self):
        # A heading interrupts the paragraph: the next indented line is
        # code, not a lazy continuation.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "# Title\n    [example](missing.md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_indented_example_after_list_item_is_code(self):
        # A list item is a block start, not paragraph text.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "- item\n    [example](missing.md)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_indented_example_inside_fenced_block_is_not_audited(self):
        # Fence content was never audited; the indented-code filter must
        # not change that.
        root = self.make_root()
        page = self.write(
            root, "docs/guide.md", "```\n    [example](missing.md)\n```\n\nbody\n"
        )
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_indented_code_state_survives_blank_lines_and_stops_at_fence(self):
        # A fence line at column 0 closes the indented block: the code
        # after the fence must not swallow later prose.
        root = self.make_root()
        page = self.write(
            root,
            "docs/guide.md",
            "    [a](missing.md)\n\n```\nfence\n```\n\nSee [this](absent.md).\n",
        )
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'absent.md'", problems[0])

    def test_prose_lines_yields_no_indented_code_lines(self):
        # Unit view of the filter: paragraph and blank lines pass, the
        # space-indented and tab-indented code lines do not.  The blank
        # line between the two code lines stays inside the code block and
        # is not yielded either.
        lines = [
            line
            for _, line in backlog_markdown.iter_prose_lines(
                "para\n\n    code\n\ttabbed\n\nafter\n"
            )
        ]
        self.assertEqual(lines, ["para", "", "after"])


class LinkTitleTests(unittest.TestCase):
    """All three CommonMark title delimiters close an inline link."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_single_quoted_title_broken_destination_is_audited(self):
        # LINK_TAIL knew only double quotes: ``[x](missing.md 't')`` failed
        # the tail match and the broken destination escaped the audit.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[x](missing.md 'title')\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing.md'", problems[0])

    def test_parenthesized_title_broken_destination_is_audited(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[x](missing.md (title))\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing.md'", problems[0])

    def test_single_quoted_title_valid_destination_passes(self):
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "[x](real.md 'title')\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_parenthesized_title_valid_destination_passes(self):
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "[x](real.md (title))\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_parenthesized_title_with_inner_parens_passes(self):
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "[x](real.md (a(b)c))\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_angle_destination_with_single_quoted_title_is_audited(self):
        # The tail applies to angle-bracket destinations too.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[x](<missing guide.md> 't')\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("'missing guide.md'", problems[0])

    def test_double_quoted_title_still_works(self):
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(root, "docs/guide.md", "[x](real.md \"title\")\n")
        self.assertEqual(checker.check_links(root, [page]), [])


class ProtocolRelativeTests(unittest.TestCase):
    """``//host/path`` targets are external, never filesystem paths."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_protocol_relative_target_is_skipped(self):
        # Browsers resolve ``//example.com/docs`` against the page scheme;
        # validating it as a repo-relative path fails CI spuriously.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[site](//example.com/docs)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_protocol_relative_with_fragment_is_skipped(self):
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "[site](//example.com/docs#intro)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_repo_relative_control_still_validates(self):
        # Skipping network paths must not weaken the relative audit.
        root = self.make_root()
        self.write(root, "docs/real.md", "# Real\n")
        page = self.write(
            root, "docs/guide.md", "[site](//example.com/docs) [docs](./real.md)\n"
        )
        self.assertEqual(checker.check_links(root, [page]), [])


class FragmentDecodeTests(unittest.TestCase):
    """Fragments are percent-decoded before the anchor comparison."""

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_encoded_fragment_resolves_to_decoded_anchor(self):
        # ``#caf%C3%A9`` decodes to ``café``, the anchor the ``Café``
        # heading exposes; comparing raw made a valid link fail.
        root = self.make_root()
        self.write(root, "docs/guide.md", "# Guide\n\n## Café\n")
        page = self.write(root, "docs/other.md", "[jump](guide.md#caf%C3%A9)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_missing_encoded_fragment_still_fails(self):
        # Decoding must not loosen the audit: a fragment that decodes to an
        # anchor no heading exposes is still reported (with the decoded
        # name the comparison actually searched for).
        root = self.make_root()
        self.write(root, "docs/guide.md", "# Guide\n")
        page = self.write(root, "docs/other.md", "[jump](guide.md#absent%C3%A9)\n")
        problems = checker.check_links(root, [page])
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("missing anchor '#absenté'", problems[0])

    def test_same_file_encoded_fragment_resolves(self):
        # The empty-path branch decodes the fragment too.
        root = self.make_root()
        page = self.write(root, "docs/guide.md", "# Guide\n\n## Café\n\n[jump](#caf%C3%A9)\n")
        self.assertEqual(checker.check_links(root, [page]), [])

    def test_split_target_decodes_fragment_keeps_path_rules(self):
        path_part, fragment = backlog_markdown.split_target("guide.md?plain=1#caf%C3%A9")
        self.assertEqual((path_part, fragment), ("guide.md", "café"))


class ThematicBreakTests(unittest.TestCase):
    """The thematic-break match keeps the old ``_NON_PARAGRAPH`` behavior."""

    # The regex alternative ``([-_*][ \t]*){3,}$`` was replaced by the flat
    # ``_is_thematic_break`` helper to clear a static-analysis finding about
    # an inefficient regular expression.  These tests pin the exact matching
    # semantics of the removed alternative so the refactor cannot silently
    # reclassify block starts as paragraph text (which would add anchors)
    # or vice versa (which would drop them).

    def make_root(self):
        temporary = tempfile.TemporaryDirectory(prefix="tributary-backlog-")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name).resolve()

    def write(self, root, name, text):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_helper_accepts_three_or_more_delimiters(self):
        for line in ("---", "- - -", "***", "_ _ _", "-\t-\t-", "*  *  *",
                     "-_-*", "---   ", "   ---", "  ---", "- - - -", " -  -  -  "):
            self.assertTrue(backlog_markdown._is_thematic_break(line), line)

    def test_helper_rejects_fewer_than_three_delimiters(self):
        for line in ("--", "- -", "* *", "", "   ", "-"):
            self.assertFalse(backlog_markdown._is_thematic_break(line), line)

    def test_helper_rejects_non_delimiter_content(self):
        for line in ("- a - -", "---a", "- - - x", "-, -, -"):
            self.assertFalse(backlog_markdown._is_thematic_break(line), line)

    def test_helper_leaves_tab_and_deep_indent_to_other_alternatives(self):
        # At most three leading spaces open a thematic break; a tab or a
        # deeper indent belongs to the indented-code alternatives of
        # ``_NON_PARAGRAPH``, exactly as in the removed regex branch.
        self.assertFalse(backlog_markdown._is_thematic_break("\t---"))
        self.assertFalse(backlog_markdown._is_thematic_break("  \t -"))
        self.assertFalse(backlog_markdown._is_thematic_break("    ---"))

    def test_blocks_paragraph_still_classifies_thematic_breaks(self):
        for line in ("---", "- - -", "   ***", "    ---", "\t---", "***   "):
            self.assertTrue(backlog_markdown._blocks_paragraph(line), line)

    def test_thematic_break_contributes_no_anchor(self):
        # A thematic break line must never be accumulated as paragraph text
        # (with no pending paragraph its underline is not a setext heading).
        root = self.make_root()
        page = self.write(
            root, "docs/guide.md", "***\n\n# Guide\n\n[jump](#guide)\n"
        )
        self.assertEqual(checker.check_links(root, [page]), [])


if __name__ == "__main__":
    unittest.main()

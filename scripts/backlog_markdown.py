"""Shared Markdown syntax parsing for the backlog consistency checker."""
# This module holds the Markdown-scanning half of check_backlog_consistency:
# fenced-code-block tracking, heading anchor slugs, and inline/definition link
# destination parsing.  Every function here is pure text handling or read-only
# file access; the checker itself owns the repository policy checks that are
# built on top of them.  Parsing follows GitHub's rendering rules so that the
# audit validates exactly the references a reader can click.

from __future__ import annotations

import re
from pathlib import Path
from typing import Iterable
from urllib.parse import unquote

# Inline Markdown links, images and link reference definitions.  Reference-style
# *inline* links (`[text][label]`) are not used in this repository; definitions
# are validated so a future relative definition cannot rot silently.
#
# Inline links are parsed in two stages: INLINE_LINK_OPEN matches the
# ``[text](`` head, then _parse_inline_destination scans the destination with
# GitHub's destination rules.  A link label may hold one level of balanced
# brackets, per CommonMark bracket matching, so ``[outer [inner]](x.md)``
# is a real link whose label renders as ``outer inner``; a flat ``[^]]*``
# can never close such a label, and the destination would silently escape
# the audit.  A plain destination may hold balanced parentheses or
# backslash-escaped ones, so ``[design](ADR_(draft).md)`` resolves to the
# file ``ADR_(draft).md`` rather than the first-``)`` truncation
# ``ADR_(draft``; a destination must still be followed by an optional
# quoted title and the closing parenthesis, so ``[x](file.md`` stays
# literal text exactly as GitHub renders it.
INLINE_LINK_OPEN = re.compile(
    r"(?P<bang>!?)\[(?P<text>(?:[^\[\]]|\[[^\[\]]*\])*)\]\(\s*"
)
# A link tail is the optional title plus the closing parenthesis.  CommonMark
# accepts three title delimiters — double-quoted, single-quoted, and
# parenthesized (with balanced inner parentheses) — so a tail that knows only
# one of them turns ``[x](missing.md 't')`` into non-link text and its broken
# destination silently escapes the audit.
LINK_TAIL = re.compile(
    r"\s*(?:\"[^\"]*\"|'[^']*'|\((?:[^()]|\([^()]*\))*\))?\s*\)"
)
DEFINITION_LINK = re.compile(r"^\[(?P<label>[^\]]+)\]:\s*(?P<target><[^<>]*>|\S+)")
HEADING = re.compile(r"^(?P<hashes>#{1,6})\s+(?P<title>.*?)\s*#*\s*$")
# A setext heading underline: one or more ``=`` or ``-`` characters under a
# paragraph line.  GitHub exposes the same anchor for ``Title`` under ``====``
# as for ``# Title``, so anchors collected from a document must recognise
# both forms or valid ``[x](#title)`` links are reported missing.
SETEXT_UNDERLINE = re.compile(r"^ {0,3}(?P<underline>=+|-+)[ \t]*$")
# A line that opens a block instead of extending a paragraph: blockquote,
# bullet or ordered list item, thematic break, or 4-space-indented code.
# GitHub never renders these as setext heading text, so a following
# underline cannot turn them into an anchor.
_NON_PARAGRAPH = re.compile(
    r"^(?: {4}|\t| {0,3}(?:>|[-+*]\s|\d{1,9}[.)]\s|([-_*][ \t]*){3,}$))"
)
# A link inside a heading title.  GitHub generates heading anchors from the
# RENDERED heading text, so ``## [API](guide.md)`` exposes ``#api``; the link
# is replaced by its text before the slug rules apply.  The destination may
# hold one level of balanced parentheses; deeper nesting stays raw markup.
HEADING_LINK = re.compile(
    r"!?\[(?P<text>[^\]]*)\]\(\s*(?P<dest>(?:[^()\s]|\([^()\s]*\))+)"
    r"(?:\s+\"[^\"]*\")?\s*\)"
)
ABSOLUTE_TARGET = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")
_CODE_RUN = re.compile(r"`+")

# One fenced code block opening line.  CommonMark fences are three or more
# backticks or tildes; the closing fence must use the same character and be
# at least as long as the opening run, so a fence must be tracked by its
# full delimiter run, not merely its first three characters.
FENCE_PATTERN = re.compile(
    r"^(?P<indent> {0,3})(?P<fence>(?P<char>[`~])(?P=char){2,})(?P<info>.*)$"
)


def iter_content_lines(text: str) -> Iterable[tuple[int, str]]:
    """Yield ``(line_number, line)`` pairs outside fenced code blocks."""
    # Fenced code blocks frequently contain shell snippets whose ``#`` comment
    # lines would otherwise be mistaken for headings and whose bracketed text
    # would be mistaken for links.  Fences are tracked by their delimiter
    # character and full opening run: a four-backtick fence is closed only by
    # a run of four or more backticks, so a triple-backtick line inside it
    # stays literal content, and a tilde fence is never closed by backticks.
    fence_char: str | None = None
    fence_len = 0
    for number, line in enumerate(text.splitlines(), start=1):
        if fence_char is None:
            opening = FENCE_PATTERN.match(line)
            if opening is None:
                yield number, line
                continue
            char = opening.group("char")
            info = opening.group("info")
            if char == "`" and "`" in info:
                # CommonMark: a backtick run whose info string contains a
                # backtick is literal text, not a fence opener.
                yield number, line
                continue
            fence_char = char
            fence_len = len(opening.group("fence"))
            continue
        closing = re.fullmatch(
            r" {0,3}" + re.escape(fence_char) + "{%d,}[ \t]*" % fence_len, line
        )
        if closing is not None:
            fence_char = None


def _indent_width(line: str) -> int:
    """Return a line's leading indentation width in columns."""
    # CommonMark expands a tab to the next multiple of four columns, so a
    # leading tab always reaches at least column four.
    width = 0
    for char in line:
        if char == " ":
            width += 1
        elif char == "\t":
            width += 4 - (width % 4)
        else:
            break
    return width


def iter_prose_lines(text: str) -> Iterable[tuple[int, str]]:
    """Yield ``(line_number, line)`` pairs outside fenced and indented code."""
    # Link targets exist only in rendered prose.  Fenced code is literal
    # text (the same fence rules ``iter_content_lines`` applies), and so is
    # an *indented code block*: four-space indentation renders verbatim, so
    # a documentation example indented into a code block must not be
    # audited as a real link.
    #
    # Chosen CommonMark boundary: 4-space indentation opens a code block
    # only when the line does not *lazily continue* an open paragraph.
    # A paragraph line keeps the paragraph open; a blank line, a fence
    # opener or closer, a heading, a setext underline, or any other block
    # start (quote, list item, thematic break) closes it.  An indented
    # line directly following paragraph text is therefore part of that
    # paragraph — its links render and stay audited — while the first
    # indented line after any such break starts code.  Inside the block,
    # blank lines and continued indentation keep it open; any non-blank
    # line with less than four indentation columns closes it.  List-item
    # continuation lines whose content column exceeds four are an accepted
    # approximation: they are classified as code when no paragraph is
    # open, which errs toward skipping literal-looking content rather
    # than auditing it.
    fence_char: str | None = None
    fence_len = 0
    in_code = False
    in_paragraph = False
    for number, line in enumerate(text.splitlines(), start=1):
        if fence_char is not None:
            closing = re.fullmatch(
                r" {0,3}" + re.escape(fence_char) + "{%d,}[ \t]*" % fence_len, line
            )
            if closing is not None:
                fence_char = None
                in_paragraph = False
            continue
        opening = FENCE_PATTERN.match(line)
        if opening is not None:
            char = opening.group("char")
            info = opening.group("info")
            if not (char == "`" and "`" in info):
                # A real fence interrupts any paragraph and any indented
                # code block started before it.
                fence_char = char
                fence_len = len(opening.group("fence"))
                in_paragraph = False
                in_code = False
                continue
            # A backtick run whose info string holds a backtick is literal
            # text, not a fence opener; treat the line as ordinary prose.
        if not line.strip():
            # A blank line ends a paragraph but never closes an indented
            # code block (blank lines are part of one).
            if not in_code:
                yield number, line
            in_paragraph = False
            continue
        if in_code:
            if _indent_width(line) >= 4:
                continue
            in_code = False
        elif _indent_width(line) >= 4:
            if in_paragraph:
                # Lazy continuation: paragraph text renders, links are real.
                yield number, line
                continue
            in_code = True
            continue
        if (
            _NON_PARAGRAPH.match(line) is not None
            or SETEXT_UNDERLINE.match(line) is not None
            or HEADING.match(line) is not None
        ):
            in_paragraph = False
        else:
            in_paragraph = True
        yield number, line


def _code_span_bounds(line: str, index: int) -> tuple[int, int, int] | None:
    """Return the code span bounds for the backtick run opening at *index*."""
    # A code span opens at a backtick run and closes at the next run of
    # exactly the same length; an opening run with no equal-length closing
    # run is literal text (CommonMark), so ``None`` is returned for it.
    # The result is ``(content_start, content_end, end)``: the content is
    # what renders, *end* is the index just past the whole span.
    opening = _CODE_RUN.match(line, index)
    if opening is None:
        return None
    run = opening.end() - opening.start()
    cursor = opening.end()
    while True:
        closing = _CODE_RUN.search(line, cursor)
        if closing is None:
            return None
        if closing.end() - closing.start() == run:
            return opening.end(), closing.start(), closing.end()
        cursor = closing.end()


def _rendered_heading_text(heading: str) -> str:
    """Return a heading title's rendered text with inline links removed."""
    # GitHub generates heading anchors from the rendered text: the title
    # ``[API](guide.md)`` renders as ``API`` and exposes the anchor ``api``,
    # not the raw-markup slug ``apiguidemd``.  Code-span delimiters are
    # dropped but their content is kept (`` `API` `` renders as ``API``),
    # and a backslash-escaped character is literal text, never markup.
    # Emphasis markers survive here; the slug's punctuation strip removes
    # them exactly as it did before.
    pieces: list[str] = []
    index = 0
    total = len(heading)
    while index < total:
        char = heading[index]
        if char == "\\":
            pieces.append(heading[index : index + 2])
            index += 2
            continue
        if char == "`":
            span = _code_span_bounds(heading, index)
            if span is None:
                pieces.append(char)
                index += 1
                continue
            content_start, content_end, after = span
            pieces.append(heading[content_start:content_end])
            index = after
            continue
        opening = HEADING_LINK.match(heading, index)
        if opening is None:
            pieces.append(char)
            index += 1
            continue
        pieces.append(opening.group("text"))
        index = opening.end()
    return "".join(pieces)


def slugify(heading: str) -> str:
    """Approximate GitHub's heading anchor slug for a Markdown heading."""
    text = _rendered_heading_text(heading).strip().lower()
    text = re.sub(r"[`*_]", "", text)
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    return text.replace(" ", "-")


def _record_slug(counts: dict[str, int], title: str) -> None:
    """Add one heading slug to *counts*, tracking duplicates for suffixing."""
    slug = slugify(title)
    counts[slug] = counts.get(slug, 0) + 1


def document_anchors(path: Path) -> set[str]:
    """Return the set of anchors GitHub exposes for *path*."""
    counts: dict[str, int] = {}
    text = path.read_text(encoding="utf-8", errors="replace")
    # Consecutive paragraph lines accumulate until an underline turns them
    # into a setext heading or a non-paragraph line ends them; GitHub slugs
    # a multi-line setext heading from its joined text.
    paragraph: list[str] = []
    for _, line in iter_content_lines(text):
        heading = HEADING.match(line)
        if heading is not None:
            _record_slug(counts, heading.group("title"))
            paragraph = []
            continue
        if SETEXT_UNDERLINE.match(line) is not None:
            # An underline under a pending paragraph is a setext heading;
            # with no pending paragraph it is a thematic break and
            # contributes no anchor.  Either way the paragraph is consumed.
            if paragraph:
                _record_slug(counts, " ".join(paragraph))
            paragraph = []
            continue
        if line.strip() and _NON_PARAGRAPH.match(line) is None:
            paragraph.append(line)
        else:
            paragraph = []
    anchors: set[str] = set()
    for slug, count in counts.items():
        anchors.update(slug if index == 0 else f"{slug}-{index}" for index in range(count))
    return anchors


def _parse_angle_destination(line: str, cursor: int) -> tuple[str, int] | None:
    """Return ``(target, end)`` for an angle-bracket destination, else ``None``."""
    # An angle-bracket destination may contain spaces and parentheses; the
    # brackets are Markdown syntax, not part of the path.
    if line[cursor : cursor + 1] != "<":
        return None
    close = line.find(">", cursor + 1)
    if close == -1:
        return None
    tail = LINK_TAIL.match(line, close + 1)
    if tail is None:
        return None
    return line[cursor + 1 : close], tail.end()


def _plain_scan_step(
    line: str, index: int, depth: int, characters: list[str]
) -> tuple[int, int] | None:
    """Consume one plain-destination character; return ``(next_index, depth)``."""
    # A backslash-escaped parenthesis belongs to the destination.  A plain
    # destination keeps balanced parentheses intact, so ``ADR_(draft).md``
    # arrives whole instead of being truncated at the first ``)``.  The
    # ``None`` return reports a scan stop: whitespace, or an unbalanced
    # closing parenthesis, which GitHub renders as literal text.
    char = line[index]
    if char == "\\" and line[index + 1 : index + 2] in ("(", ")"):
        characters.append(line[index + 1])
        return index + 2, depth
    if char.isspace():
        return None
    if char == "(":
        characters.append(char)
        return index + 1, depth + 1
    if char == ")":
        if depth == 0:
            return None
        characters.append(char)
        return index + 1, depth - 1
    characters.append(char)
    return index + 1, depth


def _scan_plain_destination(line: str, cursor: int) -> tuple[str, int] | None:
    """Scan a plain destination; return ``(target, end)`` or ``None``."""
    # An empty scan or an unbalanced one means GitHub renders this as
    # literal text, which the ``None`` return reports.
    characters: list[str] = []
    depth = 0
    index = cursor
    total = len(line)
    while index < total:
        step = _plain_scan_step(line, index, depth, characters)
        if step is None:
            break
        index, depth = step
    if depth != 0 or not characters:
        return None
    return "".join(characters), index


def _parse_inline_destination(line: str, cursor: int) -> tuple[str | None, int]:
    """Parse one inline link destination starting just past its opening parenthesis."""
    # Returns ``(target, end)`` where *end* is the index just past the link's
    # closing parenthesis, or ``(None, cursor)`` when GitHub would not linkify
    # here.  A destination must still be followed by an optional quoted title
    # and the closing parenthesis, so ``[x](file.md`` stays literal text
    # exactly as GitHub renders it.
    if line[cursor : cursor + 1] == "<":
        # An angle-bracket destination is the only valid use of a leading
        # ``<``: without its closing bracket the link never linkifies.
        angled = _parse_angle_destination(line, cursor)
        return (None, cursor) if angled is None else angled
    scanned = _scan_plain_destination(line, cursor)
    if scanned is None:
        return None, cursor
    target, index = scanned
    tail = LINK_TAIL.match(line, index)
    if tail is None:
        return None, cursor
    return target, tail.end()


def _iter_inline_link_targets(line: str) -> Iterable[str]:
    """Yield every inline ``(...)`` link target written on one content line."""
    # Inline code spans and backslash-escaped characters are never link
    # syntax: a documentation example like ``[example](missing.md)`` inside
    # backticks, or an escaped ``\\[label\\](target.md)``, must not be
    # validated as a real link, while a genuine link on the same line still
    # is.  Escaped parentheses *inside* a destination are the exception —
    # they belong to the target itself, so the destination scanner honours
    # them instead of this walk masking them.
    index = 0
    total = len(line)
    while index < total:
        char = line[index]
        if char == "\\":
            # Escaped character pair: neither character is link syntax.
            index += 2
            continue
        if char == "`":
            span = _code_span_bounds(line, index)
            if span is None:
                index += 1  # unmatched backtick run: literal text
            else:
                index = span[2]  # skip the whole code span
            continue
        opening = INLINE_LINK_OPEN.match(line, index)
        if opening is None:
            index += 1
            continue
        target, end = _parse_inline_destination(line, opening.end())
        if target is None:
            index = opening.end()
            continue
        if not opening.group("bang"):
            # Images are skipped: a missing picture must not fail the audit.
            yield target
        index = end


def iter_link_targets(line: str) -> Iterable[str]:
    """Yield every link target written on one content line."""
    yield from _iter_inline_link_targets(line)
    definition = DEFINITION_LINK.match(line)
    if definition:
        target = definition.group("target")
        if target.startswith("<") and target.endswith(">"):
            # An angle-bracketed definition destination may contain spaces;
            # the brackets are Markdown syntax, not part of the path.
            target = target[1:-1]
        yield target


def split_target(target: str) -> tuple[str | None, str]:
    """Split a link target into ``(path, fragment)`` for one link target."""
    # ``None`` means the target is external and must be skipped.  An empty
    # path means the fragment points inside the containing file.
    target = target.strip()
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    if target.startswith("#"):
        return "", unquote(target[1:])
    if target.startswith("//"):
        # A protocol-relative network-path reference: browsers resolve it
        # against the current page's scheme, so it is external even though
        # it carries no ``scheme:`` prefix and must never reach the
        # filesystem check as a repo-relative path.
        return None, ""
    if not target or ABSOLUTE_TARGET.match(target):
        return None, ""
    # Query and fragment components are URL parts, not filename text: GitHub
    # resolves ``guide.md?plain=1`` to the file ``guide.md``, so the path
    # ends at the first ``?`` or ``#``.  The fragment still starts at the
    # first ``#`` even when a query precedes it, and percent-decoding runs
    # after the split so an encoded ``%3F`` is never mistaken for a
    # component separator.  The fragment decodes too: browsers match a
    # percent-decoded fragment against the rendered element IDs, so
    # ``#caf%C3%A9`` resolves to the anchor a ``Café`` heading exposes.
    path_part = re.split(r"[?#]", target, maxsplit=1)[0]
    _, _, fragment = target.partition("#")
    return unquote(path_part), unquote(fragment)

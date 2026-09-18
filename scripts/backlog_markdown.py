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
# GitHub's destination rules.  A plain destination may hold balanced
# parentheses or backslash-escaped ones, so ``[design](ADR_(draft).md)``
# resolves to the file ``ADR_(draft).md`` rather than the first-``)``
# truncation ``ADR_(draft``; a destination must still be followed by an
# optional quoted title and the closing parenthesis, so ``[x](file.md``
# stays literal text exactly as GitHub renders it.
INLINE_LINK_OPEN = re.compile(r"(?P<bang>!?)\[(?P<text>[^\]]*)\]\(\s*")
LINK_TAIL = re.compile(r"\s*(?:\"[^\"]*\"\s*)?\)")
DEFINITION_LINK = re.compile(r"^\[(?P<label>[^\]]+)\]:\s*(?P<target><[^<>]*>|\S+)")
HEADING = re.compile(r"^(?P<hashes>#{1,6})\s+(?P<title>.*?)\s*#*\s*$")
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


def document_anchors(path: Path) -> set[str]:
    """Return the set of anchors GitHub exposes for *path*."""
    counts: dict[str, int] = {}
    text = path.read_text(encoding="utf-8", errors="replace")
    for _, line in iter_content_lines(text):
        match = HEADING.match(line)
        if match:
            slug = slugify(match.group("title"))
            counts[slug] = counts.get(slug, 0) + 1
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


def _scan_plain_destination(line: str, cursor: int) -> tuple[str, int] | None:
    """Scan a plain destination; return ``(target, end)`` or ``None``."""
    # A plain destination keeps balanced parentheses intact and honours
    # backslash-escaped ones, so ``ADR_(draft).md`` and ``file\\(1\\).md``
    # both arrive whole instead of being truncated at the first ``)``.
    # Unbalanced parentheses or an empty scan mean GitHub renders this as
    # literal text, which the ``None`` return reports.
    characters: list[str] = []
    depth = 0
    index = cursor
    total = len(line)
    while index < total:
        char = line[index]
        if char == "\\" and index + 1 < total and line[index + 1] in "()":
            # A backslash-escaped parenthesis belongs to the destination.
            characters.append(line[index + 1])
            index += 2
            continue
        if char.isspace():
            break
        if char == "(":
            depth += 1
        elif char == ")":
            if depth == 0:
                break
            depth -= 1
        characters.append(char)
        index += 1
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
        return "", target[1:]
    if not target or ABSOLUTE_TARGET.match(target):
        return None, ""
    path_part, _, fragment = target.partition("#")
    return unquote(path_part), fragment

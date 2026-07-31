#!/usr/bin/env python3
"""Build one EPUB 3 book for every NIKL dictionary XML chunk.

Every XML element, attribute, and non-whitespace text/tail value is rendered
as a visible, machine-auditable XHTML record. Dictionary-specific knowledge is
used only for book titles and visual emphasis; it never filters source data.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime as dt
import hashlib
import html
import json
import os
import re
import sys
import tempfile
import traceback
import uuid
import zipfile
from pathlib import Path
from typing import Iterable, Iterator, Sequence
from xml.etree import ElementTree as ET


EPUB_NS = "http://www.idpf.org/2007/ops"
OPF_NS = "http://www.idpf.org/2007/opf"
DC_NS = "http://purl.org/dc/elements/1.1/"
CONTAINER_NS = "urn:oasis:names:tc:opendocument:xmlns:container"
XHTML_NS = "http://www.w3.org/1999/xhtml"
BOOK_UUID_NS = uuid.UUID("34a87ea1-f0e4-4f4d-8894-1de178aa7a3e")

DEFAULT_ENTRIES_PER_CHAPTER = 300
DEFAULT_CHAPTER_BYTES = 1_048_576

# XML 1.0 only permits TAB, LF, and CR below U+0020. The source contains seven
# raw U+0008 bytes. The streaming reader maps forbidden bytes to Plane 15 PUA
# code points for parsing, then SourceRecord restores the original characters.
CONTROL_SENTINEL_BASE = 0xF0000
FORBIDDEN_XML_BYTES = tuple(
    value for value in range(0x20) if value not in (0x09, 0x0A, 0x0D)
)
CONTROL_REPLACEMENTS = {
    bytes([value]): chr(CONTROL_SENTINEL_BASE + value).encode("utf-8")
    for value in FORBIDDEN_XML_BYTES
}


@dataclasses.dataclass(frozen=True)
class DictionaryConfig:
    key: str
    directory: str
    series: str
    filename_prefix: str
    entry_tag: str
    container_tags: frozenset[str]


CONFIGS = (
    DictionaryConfig(
        key="krdict",
        directory="krdict",
        series="한국어기초사전",
        filename_prefix="01-한국어기초사전",
        entry_tag="LexicalEntry",
        container_tags=frozenset({"LexicalResource", "Lexicon"}),
    ),
    DictionaryConfig(
        key="stdict",
        directory="stdict",
        series="표준국어대사전",
        filename_prefix="02-표준국어대사전",
        entry_tag="item",
        container_tags=frozenset({"channel"}),
    ),
    DictionaryConfig(
        key="opendict",
        directory="opendict",
        series="우리말샘",
        filename_prefix="03-우리말샘",
        entry_tag="item",
        container_tags=frozenset({"channel"}),
    ),
)
CONFIG_BY_KEY = {config.key: config for config in CONFIGS}


FRIENDLY_LABELS = {
    "LexicalResource": "어휘 자료",
    "GlobalInformation": "문서 정보",
    "Lexicon": "어휘 목록",
    "LexicalEntry": "표제어 항목",
    "Lemma": "기본형",
    "WordForm": "어형",
    "Sense": "의미",
    "SenseExample": "용례",
    "Equivalent": "다국어 번역",
    "feat": "필드",
    "channel": "문서",
    "item": "표제어 항목",
    "title": "제목",
    "link": "링크",
    "description": "설명",
    "lastBuildDate": "자료 생성일",
    "total": "항목 수",
    "target_code": "대상 코드",
    "group_code": "그룹 코드",
    "group_order": "그룹 순서",
    "wordInfo": "표제어 정보",
    "word_info": "표제어 정보",
    "word": "표제어",
    "word_unit": "어휘 단위",
    "word_type": "어종",
    "original_language_info": "원어 정보",
    "original_language": "원어",
    "language_type": "언어 유형",
    "pronunciation_info": "발음 정보",
    "pronunciation": "발음",
    "conju_info": "활용 정보",
    "conjugation_info": "활용형 정보",
    "conjugation": "활용형",
    "abbreviation_info": "준말 정보",
    "abbreviation": "준말",
    "senseInfo": "의미 정보",
    "sense_info": "의미 정보",
    "sense_no": "의미 번호",
    "sense_code": "의미 코드",
    "pos": "품사",
    "pos_info": "품사 정보",
    "pos_code": "품사 코드",
    "type": "유형",
    "definition": "뜻풀이",
    "definition_original": "원뜻풀이",
    "grammar_info": "문법 정보",
    "grammar": "문법",
    "cat_info": "분류 정보",
    "cat": "분류",
    "example_info": "용례 정보",
    "example": "용례",
    "source": "출전",
    "translation": "번역",
    "origin": "원문",
    "relation_info": "관련어 정보",
    "link_target_code": "연결 대상 코드",
    "comm_pattern_info": "공통 문형 정보",
    "comm_pattern_code": "공통 문형 코드",
    "pattern_info": "문형 정보",
    "pattern": "문형",
    "multimedia_info": "멀티미디어 정보",
    "proverb_info": "관용구·속담 정보",
    "norm_info": "규범 정보",
    "history_info": "역사 정보",
}

FEAT_LABELS = {
    "label": "자료명",
    "creationDate": "자료 생성일",
    "languageCoding": "언어 코드 체계",
    "language": "언어",
    "id": "식별자",
    "homonym_number": "동형어 번호",
    "lexicalUnit": "어휘 단위",
    "partOfSpeech": "품사",
    "writtenForm": "표제어",
    "type": "유형",
    "pronunciation": "발음",
    "sound": "음성 URL",
    "vocabularyLevel": "어휘 등급",
    "semanticCategory": "의미 범주",
    "definition": "뜻풀이",
    "example": "용례",
    "lemma": "번역 표제어",
    "sensePattern": "문형",
    "senseGrammar": "문법",
    "source": "출전",
}


CSS = r"""@charset "UTF-8";

html,
body,
body * {
  word-break: keep-all;
  overflow-wrap: anywhere;
  hyphens: none;
}

body {
  margin: 5%;
  font-family: serif;
  font-size: 1em;
  line-height: 1.75;
  text-align: start;
  color: #1c1c1c;
  background: #fff;
}

h1, h2, h3 {
  line-height: 1.35;
  text-align: start;
}

h1 {
  margin-block: 0 0.75em;
  font-size: 1.8em;
}

h2 {
  margin-block: 1.5em 0.75em;
  font-size: 1.25em;
}

.book-summary {
  margin-block: 1em 2em;
  padding: 0.9em 1em;
  border: 1px solid #d8d8d8;
  border-radius: 0.35em;
  background: #f8f8f8;
}

.book-summary dt {
  margin-block-start: 0.45em;
  color: #666;
  font-size: 0.82em;
}

.book-summary dd {
  margin-inline-start: 0;
}

.source-prolog {
  margin-block: 1em;
  padding: 0.75em;
  border-inline-start: 0.25em solid #aaa;
  background: #f6f6f6;
  font-family: monospace;
  font-size: 0.78em;
  white-space: pre-wrap;
}

.entry {
  margin-block: 0 2.2em;
  padding-block: 0 1.5em;
  border-block-end: 1px solid #ddd;
}

.entry-heading {
  margin-block: 1.4em 0.4em;
  line-height: 1.35;
  font-size: 1.4em;
  font-weight: 700;
  break-after: avoid;
  page-break-after: avoid;
}

.xml-record,
.xml-tail-record {
  margin-block: 0.16em;
  padding-inline-start: calc(var(--xml-indent) * 0.7em);
}

.depth-0 { --xml-indent: 0; }
.depth-1 { --xml-indent: 1; }
.depth-2 { --xml-indent: 2; }
.depth-3 { --xml-indent: 3; }
.depth-4 { --xml-indent: 4; }
.depth-5 { --xml-indent: 5; }
.depth-6 { --xml-indent: 6; }
.depth-7 { --xml-indent: 7; }
.depth-8 { --xml-indent: 8; }

.xml-tag-name,
.xml-attr-name,
.xml-depth-marker {
  color: #777;
  font-family: monospace;
  font-size: 0.72em;
}

.xml-tag-name::before { content: "<"; }
.xml-tag-name::after { content: ">"; }

.xml-friendly-label {
  margin-inline: 0.35em 0.5em;
  color: #4f5c68;
  font-family: sans-serif;
  font-size: 0.78em;
  font-weight: 600;
}

.xml-attribute {
  display: block;
  margin-inline-start: 0.65em;
}

.xml-attr-name::before { content: "@"; }
.xml-attr-name::after { content: " = "; }

.xml-attr-value,
.xml-text,
.xml-tail-value {
  white-space: pre-wrap;
}

.control-character {
  display: inline-block;
  padding-inline: 0.18em;
  border: 1px solid #b45;
  border-radius: 0.2em;
  color: #922;
  font-family: monospace;
  font-size: 0.85em;
}

.semantic-headword {
  margin-block: 1.2em 0.3em;
  font-size: 1.35em;
  font-weight: 700;
  break-after: avoid;
  page-break-after: avoid;
}

.semantic-headword .xml-tag-name,
.semantic-headword .xml-attr-name,
.semantic-headword .xml-friendly-label {
  font-size: 0.58em;
  font-weight: 400;
}

.semantic-definition,
.semantic-example,
.semantic-translation {
  margin-block: 0.45em;
}

.semantic-definition { font-weight: 500; }

.semantic-example {
  padding-inline-start: 1em;
  color: #3e4650;
}

.semantic-translation {
  padding-inline-start: 1em;
  color: #334f72;
}

.semantic-url .xml-text,
.semantic-url .xml-attr-value {
  color: #315f91;
  font-size: 0.88em;
}

.xml-tail-record {
  color: #555;
  font-style: italic;
}

nav ol { padding-inline-start: 1.5em; }
nav li { margin-block: 0.35em; }
"""


@dataclasses.dataclass(frozen=True)
class SourceRecord:
    kind: str
    depth: int
    tag: str = ""
    attributes: tuple[tuple[str, str], ...] = ()
    text: str | None = None
    path: tuple[str, ...] = ()


@dataclasses.dataclass(frozen=True)
class ChapterInfo:
    filename: str
    first_headword: str
    last_headword: str
    entries: int


@dataclasses.dataclass(frozen=True)
class BuildTask:
    dictionary_key: str
    source_file: str
    output_file: str
    volume: int
    volumes: int
    entries_per_chapter: int
    chapter_bytes: int
    overwrite: bool


class CanonicalTracker:
    def __init__(self) -> None:
        self._hash = hashlib.sha256()
        self.elements = 0
        self.attributes = 0
        self.text_values = 0
        self.tail_values = 0
        self.control_characters = 0

    @staticmethod
    def payload(record: SourceRecord) -> list[object]:
        if record.kind == "element":
            return [
                "element",
                record.depth,
                record.tag,
                [list(pair) for pair in record.attributes],
                record.text,
            ]
        if record.kind == "tail":
            return ["tail", record.depth, record.text]
        raise ValueError(f"Unknown record kind: {record.kind}")

    def add(self, record: SourceRecord) -> None:
        encoded = json.dumps(
            self.payload(record), ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        self._hash.update(encoded)
        self._hash.update(b"\n")
        if record.kind == "element":
            self.elements += 1
            self.attributes += len(record.attributes)
            if record.text is not None:
                self.text_values += 1
            values = [record.text or ""] + [value for _, value in record.attributes]
        else:
            self.tail_values += 1
            values = [record.text or ""]
        self.control_characters += sum(
            1
            for value in values
            for character in value
            if ord(character) in FORBIDDEN_XML_BYTES
        )

    def add_all(self, records: Iterable[SourceRecord]) -> None:
        for record in records:
            self.add(record)

    @property
    def digest(self) -> str:
        return self._hash.hexdigest()


class SanitizingHashingReader:
    """Hash original bytes while replacing XML 1.0-forbidden bytes for parsing."""

    def __init__(self, path: Path) -> None:
        self._file = path.open("rb")
        self._hash = hashlib.sha256()

    def read(self, size: int = -1) -> bytes:
        data = self._file.read(size)
        self._hash.update(data)
        for original, replacement in CONTROL_REPLACEMENTS.items():
            if original in data:
                data = data.replace(original, replacement)
        return data

    def close(self) -> None:
        self._file.close()

    @property
    def hexdigest(self) -> str:
        return self._hash.hexdigest()


def local_name(tag: object) -> str:
    if not isinstance(tag, str):
        return str(tag)
    if tag.startswith("{") and "}" in tag:
        return tag.split("}", 1)[1]
    return tag


def restore_control_characters(value: str) -> str:
    result = []
    for character in value:
        codepoint = ord(character)
        if CONTROL_SENTINEL_BASE <= codepoint < CONTROL_SENTINEL_BASE + 0x20:
            result.append(chr(codepoint - CONTROL_SENTINEL_BASE))
        else:
            result.append(character)
    return "".join(result)


def significant(value: str | None) -> str | None:
    if value is None or value == "" or value.isspace():
        return None
    return restore_control_characters(value)


def flatten_element(
    element: ET.Element,
    depth: int,
    path: tuple[str, ...] = (),
    *,
    include_root_tail: bool = True,
) -> list[SourceRecord]:
    tag = str(element.tag)
    local = local_name(tag)
    current_path = path + (local,)
    records = [
        SourceRecord(
            kind="element",
            depth=depth,
            tag=tag,
            attributes=tuple(
                sorted(
                    (str(name), restore_control_characters(str(value)))
                    for name, value in element.attrib.items()
                )
            ),
            text=significant(element.text),
            path=current_path,
        )
    ]
    for child in list(element):
        records.extend(
            flatten_element(
                child,
                depth + 1,
                current_path,
                include_root_tail=True,
            )
        )
    if include_root_tail:
        tail = significant(element.tail)
        if tail is not None:
            records.append(
                SourceRecord(
                    kind="tail",
                    depth=depth,
                    text=tail,
                    path=current_path,
                )
            )
    return records


def shallow_element_record(
    element: ET.Element,
    depth: int,
    path: tuple[str, ...],
) -> SourceRecord:
    local = local_name(element.tag)
    return SourceRecord(
        kind="element",
        depth=depth,
        tag=str(element.tag),
        attributes=tuple(
            sorted(
                (str(name), restore_control_characters(str(value)))
                for name, value in element.attrib.items()
            )
        ),
        text=significant(element.text),
        path=path + (local,),
    )


def semantic_class(record: SourceRecord) -> str:
    if record.kind != "element":
        return ""
    tag = local_name(record.tag)
    attrs = dict(record.attributes)
    feat_name = attrs.get("att", "") if tag == "feat" else ""
    if feat_name == "writtenForm":
        return "semantic-headword"
    if tag == "word" and any(
        part in {"wordInfo", "word_info"} for part in record.path
    ) and not any(
        part in {"relation_info", "proverb_info"} for part in record.path
    ):
        return "semantic-headword"
    if tag in {"definition", "definition_original"} or feat_name == "definition":
        return "semantic-definition"
    if tag == "example" or feat_name == "example":
        return "semantic-example"
    if tag in {"translation", "origin"} or feat_name in {"lemma", "language"}:
        return "semantic-translation"
    values = [record.text or ""] + [value for _, value in record.attributes]
    if any(value.startswith(("http://", "https://")) for value in values):
        return "semantic-url"
    return ""


def friendly_label(record: SourceRecord) -> str:
    tag = local_name(record.tag)
    if tag == "feat":
        feat_name = dict(record.attributes).get("att")
        if feat_name:
            return FEAT_LABELS.get(feat_name, feat_name)
    return FRIENDLY_LABELS.get(tag, tag)


def escape_attr(value: str) -> str:
    if any(ord(character) in FORBIDDEN_XML_BYTES for character in value):
        raise ValueError("Forbidden control character cannot be placed in an attribute")
    return html.escape(value, quote=True)


def value_to_xhtml(value: str) -> str:
    fragments: list[str] = []
    plain: list[str] = []

    def flush_plain() -> None:
        if plain:
            fragments.append(html.escape("".join(plain), quote=False))
            plain.clear()

    for character in value:
        codepoint = ord(character)
        if codepoint in FORBIDDEN_XML_BYTES:
            flush_plain()
            visible = chr(0x2400 + codepoint) if codepoint <= 0x1F else "�"
            fragments.append(
                '<span class="control-character" '
                f'data-codepoint="U+{codepoint:04X}">{visible}</span>'
            )
        else:
            plain.append(character)
    flush_plain()
    return "".join(fragments)


def record_to_xhtml(record: SourceRecord, visual_base_depth: int) -> str:
    visual_depth = min(max(record.depth - visual_base_depth, 0), 8)
    depth_class = f"depth-{visual_depth}"
    if record.kind == "tail":
        return (
            f'<div class="xml-tail-record {depth_class}" '
            f'data-xml-depth="{record.depth}">'
            '<span class="xml-depth-marker">tail</span> '
            f'<span class="xml-tail-value">{value_to_xhtml(record.text or "")}</span>'
            "</div>"
        )

    semantic = semantic_class(record)
    classes = " ".join(
        part for part in ("xml-record", depth_class, semantic) if part
    )
    role = (
        ' role="heading" aria-level="2"'
        if semantic == "semantic-headword"
        else ""
    )
    attributes = []
    for name, value in record.attributes:
        attributes.append(
            '<span class="xml-attribute" '
            f'data-xml-name="{escape_attr(name)}">'
            f'<span class="xml-attr-name">{html.escape(local_name(name))}</span>'
            f'<span class="xml-attr-value">{value_to_xhtml(value)}</span>'
            "</span>"
        )
    text_part = (
        f'<span class="xml-text">{value_to_xhtml(record.text)}</span>'
        if record.text is not None
        else ""
    )
    return (
        f'<div class="{classes}" data-xml-depth="{record.depth}" '
        f'data-xml-tag="{escape_attr(record.tag)}"{role}>'
        f'<code class="xml-tag-name">{html.escape(local_name(record.tag))}</code>'
        f'<span class="xml-friendly-label">{html.escape(friendly_label(record))}</span>'
        f'<span class="xml-attributes">{"".join(attributes)}</span>'
        f"{text_part}"
        "</div>"
    )


def records_to_xhtml(
    records: Sequence[SourceRecord], *, visual_base_depth: int
) -> str:
    return "\n".join(
        record_to_xhtml(record, visual_base_depth) for record in records
    )


def extract_headword(config: DictionaryConfig, entry: ET.Element) -> str:
    if config.key == "krdict":
        for element in entry.iter():
            if (
                local_name(element.tag) == "feat"
                and element.attrib.get("att") == "writtenForm"
            ):
                return restore_control_characters(
                    element.attrib.get("val", "")
                ).strip()
    else:
        container_name = "wordInfo" if config.key == "opendict" else "word_info"
        for container in entry.iter():
            if local_name(container.tag) != container_name:
                continue
            for child in list(container):
                if local_name(child.tag) == "word":
                    return restore_control_characters(child.text or "").strip()
    return ""


def read_prolog(path: Path) -> tuple[str | None, str | None]:
    with path.open("rb") as file:
        prefix = file.read(131_072)
    text = prefix.decode("utf-8-sig", errors="replace")
    declaration_match = re.search(r"<\?xml\b.*?\?>", text, flags=re.DOTALL)
    doctype_match = re.search(r"<!DOCTYPE\b.*?>", text, flags=re.DOTALL)
    return (
        declaration_match.group(0) if declaration_match else None,
        doctype_match.group(0) if doctype_match else None,
    )


def xhtml_document(title: str, body: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        "<!DOCTYPE html>\n"
        f'<html xmlns="{XHTML_NS}" xmlns:epub="{EPUB_NS}" '
        'lang="ko" xml:lang="ko">\n'
        "<head>\n"
        '<meta charset="UTF-8" />\n'
        f"<title>{html.escape(title)}</title>\n"
        '<link rel="stylesheet" type="text/css" href="../styles/book.css" />\n'
        "</head>\n"
        f"<body>\n{body}\n</body>\n"
        "</html>\n"
    )


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8", newline="\n")


def create_title_xhtml(
    *,
    title: str,
    series: str,
    volume: int,
    volumes: int,
    source_name: str,
    entry_count: int,
    first_headword: str,
    last_headword: str,
    declaration: str | None,
    doctype: str | None,
    header_records: Sequence[SourceRecord],
) -> str:
    prolog_lines = [line for line in (declaration, doctype) if line]
    prolog = ""
    if prolog_lines:
        prolog = (
            '<div class="source-prolog">'
            + "<br />".join(html.escape(line) for line in prolog_lines)
            + "</div>"
        )
    summary = (
        '<dl class="book-summary">'
        f"<dt>사전</dt><dd>{html.escape(series)}</dd>"
        f"<dt>권</dt><dd>{volume}/{volumes}</dd>"
        f"<dt>원본 XML</dt><dd>{html.escape(source_name)}</dd>"
        f"<dt>표제어 수</dt><dd>{entry_count}</dd>"
        f"<dt>표제어 범위</dt><dd>{value_to_xhtml(first_headword)}"
        f" — {value_to_xhtml(last_headword)}</dd>"
        "</dl>"
    )
    body = (
        '<section epub:type="titlepage">'
        f"<h1>{html.escape(title)}</h1>{summary}{prolog}"
        "</section>"
        '<section aria-labelledby="source-header-title">'
        '<h2 id="source-header-title">원본 XML 문서 정보</h2>'
        f"{records_to_xhtml(header_records, visual_base_depth=0)}"
        "</section>"
    )
    return xhtml_document(title, body)


def create_chapter_xhtml(
    *,
    title: str,
    chapter_number: int,
    first_headword: str,
    last_headword: str,
    entry_fragments: Sequence[str],
) -> str:
    heading = (
        f"{title} · {chapter_number}장 · "
        f"{first_headword} — {last_headword}"
    )
    body = (
        f'<h1 class="chapter-title">{html.escape(heading)}</h1>\n'
        + "\n".join(entry_fragments)
    )
    return xhtml_document(heading, body)


def create_footer_xhtml(
    *, title: str, footer_records: Sequence[SourceRecord]
) -> str:
    body = (
        '<section aria-labelledby="source-footer-title">'
        '<h1 id="source-footer-title">원본 XML 후행 정보</h1>'
        f"{records_to_xhtml(footer_records, visual_base_depth=0)}"
        "</section>"
    )
    return xhtml_document(f"{title} · 후행 정보", body)


def create_nav_xhtml(
    *, title: str, chapters: Sequence[ChapterInfo], has_footer: bool
) -> str:
    items = ['<li><a href="text/title.xhtml">문서 정보</a></li>']
    for index, chapter in enumerate(chapters, start=1):
        label = (
            f"{index}장 · {chapter.first_headword} — "
            f"{chapter.last_headword} ({chapter.entries}항목)"
        )
        items.append(
            f'<li><a href="text/{html.escape(chapter.filename, quote=True)}">'
            f"{html.escape(label)}</a></li>"
        )
    if has_footer:
        items.append('<li><a href="text/footer.xhtml">후행 정보</a></li>')
    first_chapter = html.escape(chapters[0].filename, quote=True)
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        "<!DOCTYPE html>\n"
        f'<html xmlns="{XHTML_NS}" xmlns:epub="{EPUB_NS}" '
        'lang="ko" xml:lang="ko">\n'
        "<head>\n"
        '<meta charset="UTF-8" />\n'
        f"<title>{html.escape(title)} 목차</title>\n"
        '<link rel="stylesheet" type="text/css" href="styles/book.css" />\n'
        "</head>\n"
        "<body>\n"
        '<nav epub:type="toc" id="toc">\n'
        f"<h1>{html.escape(title)}</h1><ol>{''.join(items)}</ol>\n"
        "</nav>\n"
        '<nav epub:type="landmarks" hidden="hidden"><h2>안내</h2><ol>'
        '<li><a epub:type="titlepage" href="text/title.xhtml">표제</a></li>'
        f'<li><a epub:type="bodymatter" href="text/{first_chapter}">본문</a></li>'
        "</ol></nav>\n"
        "</body>\n"
        "</html>\n"
    )


def create_package_opf(
    *,
    title: str,
    series: str,
    volume: int,
    source_name: str,
    identifier: str,
    modified: str,
    chapters: Sequence[ChapterInfo],
    has_footer: bool,
) -> str:
    manifest_items = [
        '<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" '
        'properties="nav" />',
        '<item id="css" href="styles/book.css" media-type="text/css" />',
        '<item id="title" href="text/title.xhtml" '
        'media-type="application/xhtml+xml" />',
    ]
    spine_items = ['<itemref idref="title" />']
    for index, chapter in enumerate(chapters, start=1):
        item_id = f"chapter-{index:04d}"
        manifest_items.append(
            f'<item id="{item_id}" '
            f'href="text/{html.escape(chapter.filename, quote=True)}" '
            'media-type="application/xhtml+xml" />'
        )
        spine_items.append(f'<itemref idref="{item_id}" />')
    if has_footer:
        manifest_items.append(
            '<item id="footer" href="text/footer.xhtml" '
            'media-type="application/xhtml+xml" />'
        )
        spine_items.append('<itemref idref="footer" />')
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<package xmlns="{OPF_NS}" xmlns:dc="{DC_NS}" version="3.0" '
        'unique-identifier="book-id" xml:lang="ko">\n'
        "<metadata>\n"
        f'<dc:identifier id="book-id">{html.escape(identifier)}</dc:identifier>\n'
        f"<dc:title>{html.escape(title)}</dc:title>\n"
        "<dc:language>ko</dc:language>\n"
        "<dc:creator>국립국어원</dc:creator>\n"
        f"<dc:source>{html.escape(source_name)}</dc:source>\n"
        f'<meta property="dcterms:modified">{html.escape(modified)}</meta>\n'
        f'<meta property="belongs-to-collection" id="collection">'
        f"{html.escape(series)}</meta>\n"
        '<meta refines="#collection" property="collection-type">series</meta>\n'
        f'<meta refines="#collection" property="group-position">{volume}</meta>\n'
        f'<meta name="calibre:series" content="{html.escape(series, quote=True)}" />\n'
        f'<meta name="calibre:series_index" content="{volume}" />\n'
        "</metadata>\n"
        f"<manifest>{''.join(manifest_items)}</manifest>\n"
        f'<spine page-progression-direction="ltr">{"".join(spine_items)}</spine>\n'
        "</package>\n"
    )


def create_container_xml() -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<container version="1.0" xmlns="{CONTAINER_NS}">\n'
        "<rootfiles>"
        '<rootfile full-path="EPUB/package.opf" '
        'media-type="application/oebps-package+xml" />'
        "</rootfiles>\n"
        "</container>\n"
    )


def package_epub(stage: Path, output_file: Path) -> None:
    output_file.parent.mkdir(parents=True, exist_ok=True)
    temp_output = output_file.with_suffix(output_file.suffix + ".tmp")
    if temp_output.exists():
        temp_output.unlink()
    with zipfile.ZipFile(
        temp_output,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=6,
        allowZip64=True,
    ) as archive:
        mimetype_info = zipfile.ZipInfo("mimetype")
        mimetype_info.compress_type = zipfile.ZIP_STORED
        mimetype_info.date_time = (1980, 1, 1, 0, 0, 0)
        mimetype_info.external_attr = 0o644 << 16
        archive.writestr(mimetype_info, b"application/epub+zip")
        for path in sorted(stage.rglob("*")):
            if path.is_file() and path.name != "mimetype":
                archive.write(path, path.relative_to(stage).as_posix())
    os.replace(temp_output, output_file)


def add_record_group(
    records: Sequence[SourceRecord],
    *,
    entry_seen: bool,
    header_records: list[SourceRecord],
    footer_records: list[SourceRecord],
    tracker: CanonicalTracker,
) -> None:
    tracker.add_all(records)
    (footer_records if entry_seen else header_records).extend(records)


def build_one(task: BuildTask) -> dict[str, object]:
    config = CONFIG_BY_KEY[task.dictionary_key]
    source_path = Path(task.source_file)
    output_path = Path(task.output_file)
    if output_path.exists() and not task.overwrite:
        return {
            "status": "skipped",
            "dictionary": config.key,
            "source": str(source_path),
            "output": str(output_path),
        }

    declaration, doctype = read_prolog(source_path)
    tracker = CanonicalTracker()
    header_records: list[SourceRecord] = []
    footer_records: list[SourceRecord] = []
    chapters: list[ChapterInfo] = []
    first_headword = ""
    last_headword = ""
    entry_count = 0
    entry_seen = False
    container_recorded: set[int] = set()
    stack: list[ET.Element] = []

    with tempfile.TemporaryDirectory(prefix=f"nikl-{config.key}-") as temp_name:
        stage = Path(temp_name)
        (stage / "META-INF").mkdir(parents=True)
        (stage / "EPUB" / "text").mkdir(parents=True)
        (stage / "EPUB" / "styles").mkdir(parents=True)
        write_text(stage / "META-INF" / "container.xml", create_container_xml())
        write_text(stage / "EPUB" / "styles" / "book.css", CSS)

        chapter_fragments: list[str] = []
        chapter_size = 0
        chapter_first = ""
        chapter_last = ""
        chapter_entry_count = 0

        def flush_chapter() -> None:
            nonlocal chapter_fragments, chapter_size
            nonlocal chapter_first, chapter_last, chapter_entry_count
            if not chapter_fragments:
                return
            number = len(chapters) + 1
            filename = f"chapter-{number:04d}.xhtml"
            placeholder = f"{config.series} {task.volume}/{task.volumes}"
            write_text(
                stage / "EPUB" / "text" / filename,
                create_chapter_xhtml(
                    title=placeholder,
                    chapter_number=number,
                    first_headword=chapter_first,
                    last_headword=chapter_last,
                    entry_fragments=chapter_fragments,
                ),
            )
            chapters.append(
                ChapterInfo(
                    filename=filename,
                    first_headword=chapter_first,
                    last_headword=chapter_last,
                    entries=chapter_entry_count,
                )
            )
            chapter_fragments = []
            chapter_size = 0
            chapter_first = ""
            chapter_last = ""
            chapter_entry_count = 0

        reader = SanitizingHashingReader(source_path)
        try:
            for event, element in ET.iterparse(reader, events=("start", "end")):
                tag = local_name(element.tag)
                if event == "start":
                    if stack:
                        parent = stack[-1]
                        if (
                            local_name(parent.tag) in config.container_tags
                            and id(parent) not in container_recorded
                        ):
                            parent_depth = len(stack) - 1
                            parent_path = tuple(
                                local_name(node.tag) for node in stack[:-1]
                            )
                            add_record_group(
                                [
                                    shallow_element_record(
                                        parent, parent_depth, parent_path
                                    )
                                ],
                                entry_seen=entry_seen,
                                header_records=header_records,
                                footer_records=footer_records,
                                tracker=tracker,
                            )
                            container_recorded.add(id(parent))
                    stack.append(element)
                    continue

                depth = len(stack) - 1
                parent = stack[-2] if len(stack) >= 2 else None
                parent_tag = local_name(parent.tag) if parent is not None else ""
                path_prefix = tuple(local_name(node.tag) for node in stack[:-1])

                if tag == config.entry_tag:
                    records = flatten_element(element, depth, path_prefix)
                    tracker.add_all(records)
                    headword = extract_headword(config, element) or f"항목 {entry_count + 1}"
                    entry_count += 1
                    entry_seen = True
                    if not first_headword:
                        first_headword = headword
                    last_headword = headword
                    fragment = (
                        f'<article class="entry" id="entry-{entry_count:07d}">'
                        f'<div class="entry-heading">{value_to_xhtml(headword)}</div>'
                        f"{records_to_xhtml(records, visual_base_depth=depth)}"
                        "</article>"
                    )
                    encoded_size = len(fragment.encode("utf-8"))
                    if chapter_fragments and (
                        chapter_entry_count >= task.entries_per_chapter
                        or chapter_size + encoded_size > task.chapter_bytes
                    ):
                        flush_chapter()
                    if not chapter_first:
                        chapter_first = headword
                    chapter_last = headword
                    chapter_fragments.append(fragment)
                    chapter_entry_count += 1
                    chapter_size += encoded_size
                    element.clear()
                    if parent is not None:
                        try:
                            parent.remove(element)
                        except ValueError:
                            pass
                elif (
                    parent is not None
                    and parent_tag in config.container_tags
                    and tag not in config.container_tags
                ):
                    records = flatten_element(element, depth, path_prefix)
                    add_record_group(
                        records,
                        entry_seen=entry_seen,
                        header_records=header_records,
                        footer_records=footer_records,
                        tracker=tracker,
                    )
                    element.clear()
                    try:
                        parent.remove(element)
                    except ValueError:
                        pass
                elif tag in config.container_tags:
                    if id(element) not in container_recorded:
                        add_record_group(
                            [shallow_element_record(element, depth, path_prefix)],
                            entry_seen=entry_seen,
                            header_records=header_records,
                            footer_records=footer_records,
                            tracker=tracker,
                        )
                        container_recorded.add(id(element))
                    tail = significant(element.tail)
                    if tail is not None:
                        add_record_group(
                            [
                                SourceRecord(
                                    kind="tail",
                                    depth=depth,
                                    text=tail,
                                    path=path_prefix + (tag,),
                                )
                            ],
                            entry_seen=entry_seen,
                            header_records=header_records,
                            footer_records=footer_records,
                            tracker=tracker,
                        )
                stack.pop()
        finally:
            reader.close()

        flush_chapter()
        if not chapters:
            raise ValueError(f"No {config.entry_tag!r} entries in {source_path}")
        title = (
            f"{config.series} {task.volume:03d}/{task.volumes:03d} "
            f"— {first_headword} ~ {last_headword}"
        )
        write_text(
            stage / "EPUB" / "text" / "title.xhtml",
            create_title_xhtml(
                title=title,
                series=config.series,
                volume=task.volume,
                volumes=task.volumes,
                source_name=source_path.name,
                entry_count=entry_count,
                first_headword=first_headword,
                last_headword=last_headword,
                declaration=declaration,
                doctype=doctype,
                header_records=header_records,
            ),
        )
        if footer_records:
            write_text(
                stage / "EPUB" / "text" / "footer.xhtml",
                create_footer_xhtml(title=title, footer_records=footer_records),
            )
        write_text(
            stage / "EPUB" / "nav.xhtml",
            create_nav_xhtml(
                title=title,
                chapters=chapters,
                has_footer=bool(footer_records),
            ),
        )
        identifier = "urn:uuid:" + str(
            uuid.uuid5(BOOK_UUID_NS, f"{config.key}/{source_path.name}")
        )
        modified = (
            dt.datetime.now(dt.timezone.utc)
            .replace(microsecond=0)
            .isoformat()
            .replace("+00:00", "Z")
        )
        write_text(
            stage / "EPUB" / "package.opf",
            create_package_opf(
                title=title,
                series=config.series,
                volume=task.volume,
                source_name=source_path.name,
                identifier=identifier,
                modified=modified,
                chapters=chapters,
                has_footer=bool(footer_records),
            ),
        )
        package_epub(stage, output_path)

    return {
        "status": "built",
        "dictionary": config.key,
        "series": config.series,
        "volume": task.volume,
        "volumes": task.volumes,
        "source": str(source_path),
        "source_file": source_path.name,
        "source_bytes": source_path.stat().st_size,
        "source_sha256": reader.hexdigest,
        "output": str(output_path),
        "output_file": output_path.name,
        "output_bytes": output_path.stat().st_size,
        "entries": entry_count,
        "chapters": len(chapters),
        "first_headword": first_headword,
        "last_headword": last_headword,
        "xml_elements": tracker.elements,
        "xml_attributes": tracker.attributes,
        "xml_text_values": tracker.text_values,
        "xml_tail_values": tracker.tail_values,
        "control_characters": tracker.control_characters,
        "field_sha256": tracker.digest,
        "built_at": modified,
    }


def class_tokens(element: ET.Element) -> set[str]:
    return set(element.attrib.get("class", "").split())


def reversible_text(element: ET.Element | None) -> str:
    if element is None:
        return ""
    output: list[str] = []

    def visit(node: ET.Element) -> None:
        if "control-character" in class_tokens(node):
            code = node.attrib.get("data-codepoint", "")
            if not re.fullmatch(r"U\+[0-9A-Fa-f]{4,6}", code):
                raise ValueError(f"Invalid control character marker: {code}")
            output.append(chr(int(code[2:], 16)))
        elif node.text:
            output.append(node.text)
        for child in list(node):
            visit(child)
            if child.tail:
                output.append(child.tail)

    visit(element)
    return "".join(output)


def child_with_class(element: ET.Element, class_name: str) -> ET.Element | None:
    for child in element.iter():
        if class_name in class_tokens(child):
            return child
    return None


def output_records_from_epub(epub_path: Path) -> Iterator[SourceRecord]:
    with zipfile.ZipFile(epub_path) as archive:
        package_root = ET.fromstring(archive.read("EPUB/package.opf"))
        ns = {"opf": OPF_NS}
        manifest = {
            item.attrib["id"]: item.attrib["href"]
            for item in package_root.findall(".//opf:manifest/opf:item", ns)
        }
        spine_hrefs = []
        for itemref in package_root.findall(".//opf:spine/opf:itemref", ns):
            idref = itemref.attrib.get("idref", "")
            if idref not in manifest:
                raise ValueError(f"Spine idref not in manifest: {idref}")
            spine_hrefs.append("EPUB/" + manifest[idref])

        for href in spine_hrefs:
            root = ET.fromstring(archive.read(href))
            for element in root.iter():
                classes = class_tokens(element)
                if "xml-record" in classes:
                    attributes = []
                    for candidate in element.iter():
                        if "xml-attribute" not in class_tokens(candidate):
                            continue
                        name = candidate.attrib.get("data-xml-name")
                        value_element = child_with_class(
                            candidate, "xml-attr-value"
                        )
                        if name is None or value_element is None:
                            raise ValueError(f"Malformed attribute record in {href}")
                        attributes.append((name, reversible_text(value_element)))
                    text_element = child_with_class(element, "xml-text")
                    yield SourceRecord(
                        kind="element",
                        depth=int(element.attrib["data-xml-depth"]),
                        tag=element.attrib["data-xml-tag"],
                        attributes=tuple(sorted(attributes)),
                        text=(
                            reversible_text(text_element)
                            if text_element is not None
                            else None
                        ),
                    )
                elif "xml-tail-record" in classes:
                    value_element = child_with_class(element, "xml-tail-value")
                    if value_element is None:
                        raise ValueError(f"Malformed tail record in {href}")
                    yield SourceRecord(
                        kind="tail",
                        depth=int(element.attrib["data-xml-depth"]),
                        text=reversible_text(value_element),
                    )


def validate_epub(
    epub_path: Path,
    *,
    expected_field_digest: str | None = None,
    expected_entries: int | None = None,
) -> dict[str, object]:
    errors: list[str] = []
    entry_count = 0
    with zipfile.ZipFile(epub_path) as archive:
        names = archive.namelist()
        if not names or names[0] != "mimetype":
            errors.append("mimetype is not the first ZIP member")
        else:
            info = archive.getinfo("mimetype")
            if info.compress_type != zipfile.ZIP_STORED:
                errors.append("mimetype is compressed")
            if archive.read("mimetype") != b"application/epub+zip":
                errors.append("mimetype has an invalid value")
        for required in ("META-INF/container.xml", "EPUB/package.opf"):
            if required not in names:
                errors.append(f"{required} is missing")
        bad_member = archive.testzip()
        if bad_member:
            errors.append(f"CRC failure: {bad_member}")
        for name in names:
            if name.endswith((".xhtml", ".xml", ".opf")):
                try:
                    ET.fromstring(archive.read(name))
                except ET.ParseError as exc:
                    errors.append(f"Invalid XML in {name}: {exc}")
            elif name.endswith(".css"):
                css = archive.read(name).decode("utf-8")
                if "word-break: keep-all" not in css:
                    errors.append(f"word-break: keep-all missing from {name}")
            if name.endswith(".xhtml") and "/text/chapter-" in name:
                root = ET.fromstring(archive.read(name))
                entry_count += sum(
                    1 for element in root.iter() if "entry" in class_tokens(element)
                )

    tracker = CanonicalTracker()
    tracker.add_all(output_records_from_epub(epub_path))
    if expected_field_digest and tracker.digest != expected_field_digest:
        errors.append(
            f"field digest mismatch: {tracker.digest} != {expected_field_digest}"
        )
    if expected_entries is not None and entry_count != expected_entries:
        errors.append(
            f"entry count mismatch: {entry_count} != {expected_entries}"
        )
    if errors:
        raise ValueError("; ".join(errors))
    return {
        "output": str(epub_path),
        "entries": entry_count,
        "xml_elements": tracker.elements,
        "xml_attributes": tracker.attributes,
        "xml_text_values": tracker.text_values,
        "xml_tail_values": tracker.tail_values,
        "control_characters": tracker.control_characters,
        "field_sha256": tracker.digest,
        "valid": True,
    }


def validate_manifest_book(book: dict[str, object]) -> dict[str, object]:
    return validate_epub(
        Path(str(book["output"])),
        expected_field_digest=(
            str(book["field_sha256"]) if book.get("field_sha256") else None
        ),
        expected_entries=(
            int(book["entries"]) if book.get("entries") is not None else None
        ),
    )


def discover_tasks(args: argparse.Namespace) -> list[BuildTask]:
    source_root = Path(args.source).resolve()
    output_root = Path(args.output).resolve()
    only = set(args.only or [])
    tasks: list[BuildTask] = []
    for config in CONFIGS:
        files = sorted((source_root / config.directory).glob("*.xml"))
        if not files:
            raise FileNotFoundError(source_root / config.directory)
        for volume, path in enumerate(files, start=1):
            relative = path.relative_to(source_root).as_posix()
            if only and not any(
                pattern in {config.key, path.name, relative} for pattern in only
            ):
                continue
            output_name = (
                f"{config.filename_prefix}-{volume:03d}-of-{len(files):03d}.epub"
            )
            tasks.append(
                BuildTask(
                    dictionary_key=config.key,
                    source_file=str(path),
                    output_file=str(output_root / output_name),
                    volume=volume,
                    volumes=len(files),
                    entries_per_chapter=args.entries_per_chapter,
                    chapter_bytes=args.chapter_bytes,
                    overwrite=args.overwrite,
                )
            )
    if not tasks:
        raise ValueError("No input files matched --only")
    return tasks


def write_manifest(
    output_root: Path,
    results: Sequence[dict[str, object]],
    filename: str = "manifest.json",
) -> Path:
    payload = {
        "format": "nikl-dictionary-epub-manifest-v1",
        "generated_at": (
            dt.datetime.now(dt.timezone.utc)
            .replace(microsecond=0)
            .isoformat()
            .replace("+00:00", "Z")
        ),
        "books": list(results),
    }
    path = output_root / filename
    temp = path.with_suffix(path.suffix + ".tmp")
    output_root.mkdir(parents=True, exist_ok=True)
    temp.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    os.replace(temp, path)
    return path


def build_command(args: argparse.Namespace) -> int:
    tasks = discover_tasks(args)
    output_root = Path(args.output).resolve()
    output_root.mkdir(parents=True, exist_ok=True)
    results: list[dict[str, object]] = []
    failures: list[dict[str, str]] = []
    print(f"Building {len(tasks)} EPUB file(s) into {output_root}", flush=True)

    def record_result(result: dict[str, object], completed: int) -> None:
        results.append(result)
        print(
            f"[{completed}/{len(tasks)}] {result['status']}: "
            f"{Path(str(result['output'])).name}",
            flush=True,
        )

    if args.jobs == 1:
        for completed, task in enumerate(tasks, start=1):
            try:
                record_result(build_one(task), completed)
            except Exception as exc:
                failures.append(
                    {
                        "source": task.source_file,
                        "output": task.output_file,
                        "error": str(exc),
                        "traceback": traceback.format_exc(),
                    }
                )
                print(f"FAILED: {task.source_file}: {exc}", file=sys.stderr)
                if not args.keep_going:
                    break
    else:
        with concurrent.futures.ProcessPoolExecutor(
            max_workers=args.jobs
        ) as executor:
            future_to_task = {
                executor.submit(build_one, task): task for task in tasks
            }
            for completed, future in enumerate(
                concurrent.futures.as_completed(future_to_task), start=1
            ):
                task = future_to_task[future]
                try:
                    record_result(future.result(), completed)
                except Exception as exc:
                    failures.append(
                        {
                            "source": task.source_file,
                            "output": task.output_file,
                            "error": str(exc),
                            "traceback": traceback.format_exc(),
                        }
                    )
                    print(f"FAILED: {task.source_file}: {exc}", file=sys.stderr)
                    if not args.keep_going:
                        for pending in future_to_task:
                            pending.cancel()
                        break

    results.sort(
        key=lambda item: str(item.get("output_file", item.get("output", "")))
    )
    manifest = write_manifest(output_root, results)
    print(f"Manifest: {manifest}", flush=True)
    if failures:
        failure_path = output_root / "failures.json"
        failure_path.write_text(
            json.dumps(failures, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"Failures: {failure_path}", file=sys.stderr)
        return 1
    return 0


def validate_command(args: argparse.Namespace) -> int:
    output_root = Path(args.output).resolve()
    manifest = json.loads(
        (output_root / "manifest.json").read_text(encoding="utf-8")
    )
    books = manifest.get("books", [])
    failures = []
    validations = []
    with concurrent.futures.ProcessPoolExecutor(
        max_workers=args.jobs
    ) as executor:
        future_to_book = {
            executor.submit(validate_manifest_book, book): book for book in books
        }
        for index, future in enumerate(
            concurrent.futures.as_completed(future_to_book), start=1
        ):
            book = future_to_book[future]
            epub_path = Path(str(book["output"]))
            try:
                validations.append(future.result())
                print(
                    f"[{index}/{len(books)}] valid: {epub_path.name}",
                    flush=True,
                )
            except Exception as exc:
                failures.append({"output": str(epub_path), "error": str(exc)})
                print(
                    f"INVALID: {epub_path}: {exc}",
                    file=sys.stderr,
                    flush=True,
                )
    validations.sort(key=lambda item: str(item["output"]))
    (output_root / "validation.json").write_text(
        json.dumps(
            {
                "validated_at": (
                    dt.datetime.now(dt.timezone.utc)
                    .replace(microsecond=0)
                    .isoformat()
                    .replace("+00:00", "Z")
                ),
                "validations": validations,
                "failures": failures,
            },
            ensure_ascii=False,
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return 1 if failures else 0


def create_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build")
    build.add_argument("--source", required=True)
    build.add_argument("--output", required=True)
    build.add_argument("--only", action="append")
    build.add_argument(
        "--entries-per-chapter", type=int, default=DEFAULT_ENTRIES_PER_CHAPTER
    )
    build.add_argument(
        "--chapter-bytes", type=int, default=DEFAULT_CHAPTER_BYTES
    )
    build.add_argument("--jobs", type=int, default=1)
    build.add_argument("--overwrite", action="store_true")
    build.add_argument("--keep-going", action="store_true")
    build.set_defaults(func=build_command)
    validate = commands.add_parser("validate")
    validate.add_argument("--output", required=True)
    validate.add_argument("--jobs", type=int, default=1)
    validate.set_defaults(func=validate_command)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = create_parser()
    args = parser.parse_args(argv)
    if getattr(args, "jobs", 1) < 1:
        parser.error("--jobs must be at least 1")
    if getattr(args, "entries_per_chapter", 1) < 1:
        parser.error("--entries-per-chapter must be at least 1")
    if getattr(args, "chapter_bytes", 1024) < 1024:
        parser.error("--chapter-bytes must be at least 1024")
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())

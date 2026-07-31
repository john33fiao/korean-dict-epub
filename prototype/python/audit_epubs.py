#!/usr/bin/env python3
"""Independently audit NIKL source XML against rendered EPUB field records."""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime as dt
import hashlib
import json
import os
import re
import sys
import zipfile
from pathlib import Path
from typing import Iterator, Sequence
from xml.etree import ElementTree as ET
from xml.sax import InputSource, handler, make_parser
from xml.sax.handler import feature_external_ges, feature_external_pes


OPF_NS = "http://www.idpf.org/2007/opf"
CONTROL_SENTINEL_BASE = 0xF0000
FORBIDDEN_XML_BYTES = tuple(
    value for value in range(0x20) if value not in (0x09, 0x0A, 0x0D)
)
CONTROL_REPLACEMENTS = {
    bytes([value]): chr(CONTROL_SENTINEL_BASE + value).encode("utf-8")
    for value in FORBIDDEN_XML_BYTES
}


def restore_controls(value: str) -> str:
    output = []
    for character in value:
        codepoint = ord(character)
        if CONTROL_SENTINEL_BASE <= codepoint < CONTROL_SENTINEL_BASE + 0x20:
            output.append(chr(codepoint - CONTROL_SENTINEL_BASE))
        else:
            output.append(character)
    return "".join(output)


def significant(value: str | None) -> str | None:
    if value is None or value == "" or value.isspace():
        return None
    return restore_controls(value)


class SanitizingHashingReader:
    def __init__(self, path: Path) -> None:
        self.file = path.open("rb")
        self.hash = hashlib.sha256()

    def read(self, size: int = -1) -> bytes:
        data = self.file.read(size)
        self.hash.update(data)
        for original, replacement in CONTROL_REPLACEMENTS.items():
            if original in data:
                data = data.replace(original, replacement)
        return data

    def close(self) -> None:
        self.file.close()


class EventDigest:
    def __init__(self) -> None:
        self.hash = hashlib.sha256()
        self.starts = 0
        self.ends = 0
        self.attributes = 0
        self.texts = 0
        self.tails = 0
        self.control_characters = 0

    def add(self, kind: str, *values: object) -> None:
        encoded = json.dumps(
            [kind, *values], ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        self.hash.update(encoded)
        self.hash.update(b"\n")
        if kind == "S":
            self.starts += 1
        elif kind == "E":
            self.ends += 1
        elif kind == "A":
            self.attributes += 1
        elif kind == "T":
            self.texts += 1
        elif kind == "X":
            self.tails += 1
        for value in values:
            if isinstance(value, str):
                self.control_characters += sum(
                    1
                    for character in value
                    if ord(character) in FORBIDDEN_XML_BYTES
                )

    @property
    def hexdigest(self) -> str:
        return self.hash.hexdigest()

    def counts(self) -> dict[str, int]:
        # A single control character appears in exactly one token. Other token
        # strings (element and attribute names) contain no controls.
        return {
            "elements": self.starts,
            "end_elements": self.ends,
            "attributes": self.attributes,
            "text_values": self.texts,
            "tail_values": self.tails,
            "control_characters": self.control_characters,
        }


class HeadwordDigest:
    def __init__(self) -> None:
        self.hash = hashlib.sha256()
        self.count = 0
        self.first = ""
        self.last = ""

    def add(self, value: str) -> None:
        value = value.strip()
        if not self.first:
            self.first = value
        self.last = value
        self.count += 1
        self.hash.update(value.encode("utf-8"))
        self.hash.update(b"\n")

    @property
    def hexdigest(self) -> str:
        return self.hash.hexdigest()


@dataclasses.dataclass
class Frame:
    name: str
    depth: int
    attributes: dict[str, str]
    text_parts: list[str] = dataclasses.field(default_factory=list)
    child_count: int = 0


class SourceHandler(handler.ContentHandler):
    def __init__(self, dictionary: str) -> None:
        super().__init__()
        self.dictionary = dictionary
        self.digest = EventDigest()
        self.headwords = HeadwordDigest()
        self.frames: list[Frame] = []
        self.entry_count = 0
        self._current_krdict_has_headword = False

    def _flush_frame_text(self, frame: Frame) -> str | None:
        raw = "".join(frame.text_parts)
        frame.text_parts.clear()
        value = significant(raw)
        if value is None:
            return None
        if frame.child_count:
            self.digest.add("X", frame.depth + 1, value)
        else:
            self.digest.add("T", frame.depth, value)
        return value

    def startElement(self, name, attrs) -> None:  # noqa: N802
        if self.frames:
            parent = self.frames[-1]
            self._flush_frame_text(parent)
            parent.child_count += 1
        depth = len(self.frames)
        restored_attrs = {
            str(key): restore_controls(str(attrs.getValue(key)))
            for key in attrs.getNames()
        }
        self.digest.add("S", depth, str(name))
        for attr_name, attr_value in sorted(restored_attrs.items()):
            self.digest.add("A", depth, attr_name, attr_value)
        self.frames.append(
            Frame(
                name=str(name),
                depth=depth,
                attributes=restored_attrs,
            )
        )

        entry_tag = "LexicalEntry" if self.dictionary == "krdict" else "item"
        if name == entry_tag:
            self.entry_count += 1
            if self.dictionary == "krdict":
                self._current_krdict_has_headword = False
        if (
            self.dictionary == "krdict"
            and name == "feat"
            and not self._current_krdict_has_headword
            and restored_attrs.get("att") == "writtenForm"
            and "LexicalEntry" in [frame.name for frame in self.frames[:-1]]
            and "Lemma" in [frame.name for frame in self.frames[:-1]]
        ):
            self.headwords.add(restored_attrs.get("val", ""))
            self._current_krdict_has_headword = True

    def characters(self, content: str) -> None:
        if self.frames:
            self.frames[-1].text_parts.append(restore_controls(content))

    def endElement(self, name) -> None:  # noqa: N802
        frame = self.frames[-1]
        raw_value = "".join(frame.text_parts)
        value = self._flush_frame_text(frame)
        if self.dictionary in {"stdict", "opendict"} and name == "word":
            parent_name = self.frames[-2].name if len(self.frames) >= 2 else ""
            expected_parent = (
                "word_info" if self.dictionary == "stdict" else "wordInfo"
            )
            if parent_name == expected_parent:
                headword = significant(raw_value)
                self.headwords.add(headword or "")
        self.digest.add("E", frame.depth, str(name))
        self.frames.pop()


def audit_source(path: Path, dictionary: str) -> dict[str, object]:
    reader = SanitizingHashingReader(path)
    source = InputSource()
    source.setByteStream(reader)
    parser = make_parser()
    for feature in (feature_external_ges, feature_external_pes):
        try:
            parser.setFeature(feature, False)
        except Exception:
            pass
    content_handler = SourceHandler(dictionary)
    parser.setContentHandler(content_handler)
    try:
        parser.parse(source)
    finally:
        reader.close()
    return {
        "sha256": reader.hash.hexdigest(),
        "event_sha256": content_handler.digest.hexdigest,
        "event_counts": content_handler.digest.counts(),
        "entries": content_handler.entry_count,
        "headword_count": content_handler.headwords.count,
        "headword_sha256": content_handler.headwords.hexdigest,
        "first_headword": content_handler.headwords.first,
        "last_headword": content_handler.headwords.last,
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
                raise ValueError(f"Invalid control marker: {code}")
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


@dataclasses.dataclass(frozen=True)
class OutputRecord:
    kind: str
    depth: int
    tag: str = ""
    attributes: tuple[tuple[str, str], ...] = ()
    text: str | None = None


def spine_documents(archive: zipfile.ZipFile) -> list[str]:
    package = ET.fromstring(archive.read("EPUB/package.opf"))
    ns = {"opf": OPF_NS}
    manifest = {
        item.attrib["id"]: item.attrib["href"]
        for item in package.findall(".//opf:manifest/opf:item", ns)
    }
    documents = []
    for itemref in package.findall(".//opf:spine/opf:itemref", ns):
        idref = itemref.attrib.get("idref", "")
        if idref not in manifest:
            raise ValueError(f"Unknown spine idref: {idref}")
        documents.append("EPUB/" + manifest[idref])
    return documents


def output_records(
    archive: zipfile.ZipFile, documents: Sequence[str]
) -> Iterator[OutputRecord]:
    for document in documents:
        root = ET.fromstring(archive.read(document))
        for element in root.iter():
            classes = class_tokens(element)
            if "xml-record" in classes:
                attributes = []
                for candidate in element.iter():
                    if "xml-attribute" not in class_tokens(candidate):
                        continue
                    name = candidate.attrib.get("data-xml-name")
                    value_node = child_with_class(candidate, "xml-attr-value")
                    if name is None or value_node is None:
                        raise ValueError(f"Malformed attribute in {document}")
                    attributes.append((name, reversible_text(value_node)))
                text_node = child_with_class(element, "xml-text")
                yield OutputRecord(
                    kind="element",
                    depth=int(element.attrib["data-xml-depth"]),
                    tag=element.attrib["data-xml-tag"],
                    attributes=tuple(sorted(attributes)),
                    text=(
                        reversible_text(text_node)
                        if text_node is not None
                        else None
                    ),
                )
            elif "xml-tail-record" in classes:
                value_node = child_with_class(element, "xml-tail-value")
                if value_node is None:
                    raise ValueError(f"Malformed tail in {document}")
                yield OutputRecord(
                    kind="tail",
                    depth=int(element.attrib["data-xml-depth"]),
                    text=reversible_text(value_node),
                )


def output_headwords(
    archive: zipfile.ZipFile, documents: Sequence[str]
) -> HeadwordDigest:
    digest = HeadwordDigest()
    for document in documents:
        if "/text/chapter-" not in document:
            continue
        root = ET.fromstring(archive.read(document))
        for element in root.iter():
            if "entry-heading" in class_tokens(element):
                digest.add(reversible_text(element))
    return digest


def audit_output(path: Path) -> dict[str, object]:
    digest = EventDigest()
    open_elements: list[tuple[int, str]] = []
    with zipfile.ZipFile(path) as archive:
        documents = spine_documents(archive)
        headwords = output_headwords(archive, documents)
        for record in output_records(archive, documents):
            if record.kind == "element":
                while open_elements and open_elements[-1][0] >= record.depth:
                    depth, name = open_elements.pop()
                    digest.add("E", depth, name)
                digest.add("S", record.depth, record.tag)
                for name, value in record.attributes:
                    digest.add("A", record.depth, name, value)
                if record.text is not None:
                    digest.add("T", record.depth, record.text)
                open_elements.append((record.depth, record.tag))
            else:
                while open_elements and open_elements[-1][0] >= record.depth:
                    depth, name = open_elements.pop()
                    digest.add("E", depth, name)
                digest.add("X", record.depth, record.text or "")
        while open_elements:
            depth, name = open_elements.pop()
            digest.add("E", depth, name)
    return {
        "event_sha256": digest.hexdigest,
        "event_counts": digest.counts(),
        "entries": headwords.count,
        "headword_count": headwords.count,
        "headword_sha256": headwords.hexdigest,
        "first_headword": headwords.first,
        "last_headword": headwords.last,
    }


def audit_book(book: dict[str, object]) -> dict[str, object]:
    source_path = Path(str(book["source"]))
    output_path = Path(str(book["output"]))
    dictionary = str(book["dictionary"])
    source = audit_source(source_path, dictionary)
    output = audit_output(output_path)
    checks = {
        "source_sha256": source["sha256"] == book.get("source_sha256"),
        "event_sha256": source["event_sha256"] == output["event_sha256"],
        "event_counts": source["event_counts"] == output["event_counts"],
        "entries": source["entries"] == output["entries"] == book.get("entries"),
        "headword_count": source["headword_count"] == output["headword_count"],
        "headword_sha256": source["headword_sha256"] == output["headword_sha256"],
        "first_headword": (
            source["first_headword"]
            == output["first_headword"]
            == book.get("first_headword")
        ),
        "last_headword": (
            source["last_headword"]
            == output["last_headword"]
            == book.get("last_headword")
        ),
    }
    if not all(checks.values()):
        raise ValueError(
            json.dumps(
                {"checks": checks, "source": source, "output": output},
                ensure_ascii=False,
            )
        )
    return {
        "source": str(source_path),
        "output": str(output_path),
        "checks": checks,
        "source": source,
        "rendered": output,
        "valid": True,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--jobs", type=int, default=1)
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")

    output_root = Path(args.output).resolve()
    manifest = json.loads(
        (output_root / "manifest.json").read_text(encoding="utf-8")
    )
    books = manifest.get("books", [])
    results: list[dict[str, object]] = []
    failures: list[dict[str, str]] = []

    with concurrent.futures.ProcessPoolExecutor(
        max_workers=args.jobs
    ) as executor:
        future_to_book = {
            executor.submit(audit_book, book): book for book in books
        }
        for completed, future in enumerate(
            concurrent.futures.as_completed(future_to_book), start=1
        ):
            book = future_to_book[future]
            try:
                result = future.result()
                results.append(result)
                print(
                    f"[{completed}/{len(books)}] audited: "
                    f"{Path(str(book['output'])).name}",
                    flush=True,
                )
            except Exception as exc:
                failures.append(
                    {"output": str(book.get("output")), "error": str(exc)}
                )
                print(
                    f"FAILED: {book.get('output')}: {exc}",
                    file=sys.stderr,
                    flush=True,
                )

    results.sort(key=lambda item: str(item["output"]))
    report = {
        "audited_at": (
            dt.datetime.now(dt.timezone.utc)
            .replace(microsecond=0)
            .isoformat()
            .replace("+00:00", "Z")
        ),
        "results": results,
        "failures": failures,
    }
    report_path = output_root / "content-audit.json"
    temp = report_path.with_suffix(report_path.suffix + ".tmp")
    temp.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    os.replace(temp, report_path)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())

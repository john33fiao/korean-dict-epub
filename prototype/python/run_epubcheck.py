#!/usr/bin/env python3
"""Run EPUBCheck over every book in a generated manifest."""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import json
import os
import subprocess
import time
from pathlib import Path
from typing import Sequence


def check_one(java: str, jar: str, output: str) -> dict[str, object]:
    started = time.monotonic()
    command = [
        java,
        "-jar",
        jar,
        output,
        "--failonwarnings",
        "--quiet",
    ]
    completed = subprocess.run(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    return {
        "output": output,
        "returncode": completed.returncode,
        "seconds": round(time.monotonic() - started, 3),
        "log": completed.stdout,
        "valid": completed.returncode == 0,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--jar", required=True)
    parser.add_argument("--java", default="java")
    parser.add_argument("--jobs", type=int, default=3)
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")

    output_root = Path(args.output).resolve()
    manifest = json.loads(
        (output_root / "manifest.json").read_text(encoding="utf-8")
    )
    books = manifest.get("books", [])
    results: list[dict[str, object]] = []
    failures: list[dict[str, object]] = []

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
        future_to_book = {
            executor.submit(
                check_one,
                args.java,
                str(Path(args.jar).resolve()),
                str(Path(str(book["output"])).resolve()),
            ): book
            for book in books
        }
        for completed_count, future in enumerate(
            concurrent.futures.as_completed(future_to_book), start=1
        ):
            result = future.result()
            results.append(result)
            if not result["valid"]:
                failures.append(result)
            status = "valid" if result["valid"] else "INVALID"
            print(
                f"[{completed_count}/{len(books)}] {status}: "
                f"{Path(str(result['output'])).name}",
                flush=True,
            )

    results.sort(key=lambda item: str(item["output"]))
    report = {
        "checked_at": (
            dt.datetime.now(dt.timezone.utc)
            .replace(microsecond=0)
            .isoformat()
            .replace("+00:00", "Z")
        ),
        "epubcheck_jar": str(Path(args.jar).resolve()),
        "results": results,
        "failures": failures,
    }
    report_path = output_root / "epubcheck.json"
    temp = report_path.with_suffix(report_path.suffix + ".tmp")
    temp.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    os.replace(temp, report_path)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())

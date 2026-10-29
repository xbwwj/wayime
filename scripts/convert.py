#!/usr/bin/env python3
"""Convert Rime wanxiang dict to wayime compatible dict.

Usage:
    python convert.py *.dict.yaml -o dict.tsv
"""

import argparse
import csv
import math
import unicodedata
from collections.abc import Iterator
from pathlib import Path
from typing import TypedDict


class Item(TypedDict):
    text: str
    code: str
    weight: str
    weight_value: float | None
    meta: str


def normalize_syllable(syllable: str) -> str:
    """Normalize one pinyin token to lowercase ASCII letters.

    Important distinction:
    - nǚ / lǜ / nü / lü -> nv / lv / nv / lv
    - jǔ / qù / xuě / yuè -> ju / qu / xue / yue

    In standard Hanyu Pinyin, ü is written as u after j, q, x and y, so those
    ordinary u spellings are not converted to v. An explicit ü is identified by
    the diaeresis combining mark before all tone/diacritic marks are discarded.
    """
    # Support the alternate ASCII spelling u: for ü as well as real Unicode ü.
    source = syllable.strip().lower().replace("u:", "v")
    decomposed = unicodedata.normalize("NFKD", source)

    result: list[str] = []
    i = 0
    while i < len(decomposed):
        ch = decomposed[i]

        # NFKD expands toned vowels (e.g. ǚ) into an ASCII base letter plus
        # combining marks. Inspect the *whole* mark cluster before stripping it:
        # u + diaeresis means ü -> v; u + tone mark alone means ordinary u.
        if ch.isascii() and ch.isalpha():
            j = i + 1
            has_diaeresis = False
            while j < len(decomposed) and unicodedata.combining(decomposed[j]):
                if decomposed[j] == "\u0308":  # COMBINING DIAERESIS
                    has_diaeresis = True
                j += 1

            if ch == "u" and has_diaeresis:
                result.append("v")
            else:
                result.append(ch)
            i = j
            continue

        # Drop combining marks and any punctuation/non-ASCII characters.
        i += 1

    return "".join(result)


def normalize_code(code: str) -> str:
    """Strip each space-delimited token's ';' suffix and normalize its pinyin."""
    result = []
    for token in code.split():
        # The semicolon suffix is supplementary coding that this engine ignores.
        pinyin = token.split(";", 1)[0]
        normalized = normalize_syllable(pinyin)
        if normalized:
            result.append(normalized)
    return " ".join(result)


def parse_weight(raw_weight: str) -> tuple[str, float | None]:
    raw_weight = raw_weight.strip()
    if not raw_weight:
        return "", None

    try:
        value = float(raw_weight)
    except ValueError:
        return "", None

    if not math.isfinite(value):
        return "", None
    return raw_weight, value


def read_dictionary(path: Path) -> Iterator[Item]:
    """Skip the Rime YAML header and stream the TSV entries after '...' ."""
    with path.open(encoding="utf-8-sig", newline="") as f:
        in_data = False
        for line_no, line in enumerate(f, 1):
            if not in_data:
                if line.strip() == "...":
                    in_data = True
                continue

            if not line.strip() or line.lstrip().startswith("#"):
                continue

            row = next(csv.reader([line], delimiter="\t"))
            if len(row) < 2:
                continue

            text = row[0].strip()
            code = normalize_code(row[1])
            weight, weight_value = parse_weight(row[2]) if len(row) > 2 else ("", None)

            if not text or not code:
                continue

            yield {
                "text": text,
                "code": code,
                "weight": weight,
                "weight_value": weight_value,
                "meta": "",
            }

        if not in_data:
            raise ValueError(f"{path}: 未找到 Rime 词库的 YAML 结束标记 '...'")


def main() -> None:
    parser = argparse.ArgumentParser(description="合并 Rime 万象词库并转换为 TSV")
    parser.add_argument("inputs", nargs="+", type=Path, help="输入的 dict.yaml 文件")
    parser.add_argument(
        "-o", "--output", required=True, type=Path, help="输出 TSV 文件"
    )
    args = parser.parse_args()

    # Deduplicate by normalized (text, code), keeping the highest finite weight.
    candidates: dict[tuple[str, str], Item] = {}

    for path in args.inputs:
        count = 0
        for item in read_dictionary(path):
            count += 1
            key = (item["text"], item["code"])
            old = candidates.get(key)

            def score(candidate: Item) -> float:
                value = candidate["weight_value"]
                return value if value is not None else float("-inf")

            if old is None or score(item) > score(old):
                candidates[key] = item

        print(f"{path}: processed {count} entries")

    with args.output.open("w", encoding="utf-8", newline="") as f:
        writer = csv.writer(f, delimiter="\t", lineterminator="\n")
        writer.writerow(["text", "code", "weight", "meta"])
        for item in candidates.values():
            writer.writerow([item["text"], item["code"], item["weight"], item["meta"]])

    print(f"Output: {args.output}")
    print(f"Unique entries: {len(candidates)}")


if __name__ == "__main__":
    main()

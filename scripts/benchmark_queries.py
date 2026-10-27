import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import sqlite3
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    fixture = args.fixture_dir.resolve()
    env = dict(os.environ)
    env.pop("M2K_PASSWORD", None)
    count = 4000
    raw_bytes = 65536

    def synthetic_rows():
        for index in range(count):
            folder = "INBOX" if index < 100 else "Archive"
            title = "合成效能資料 " + str(index)
            yield (
                f"benchmark-{index:05}", "mail", folder, str(index + 1), None, 1,
                title, "benchmark@example.test", "2026-10-06T00:00:00Z", None,
                "這是純合成資料，沒有任何真實郵件。" * 12,
                "base64:" + "A" * raw_bytes, "[]", '["benchmark@example.test"]',
                "[]", "[]", 1791244800,
            )

    if not fixture.exists():
        subprocess.run(
            [binary, "--data-dir", str(fixture), "init", "--username", "benchmark",
             "--server", "mail.example.test"],
            env=env, stdout=subprocess.DEVNULL, check=True,
        )
        connection = sqlite3.connect(fixture / "database.sqlite3")
        connection.executemany(
            "INSERT INTO collections(kind,id,name) VALUES ('mail',?,?)",
            [("INBOX", "INBOX"), ("Archive", "Archive")],
        )
        connection.executemany(
            "INSERT INTO items VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", synthetic_rows(),
        )
        connection.commit()
        connection.close()
    config = json.loads((fixture / "config.json").read_text())
    if config["username"] != "benchmark":
        raise RuntimeError("拒絕對非合成 benchmark 帳號執行量測")
    with sqlite3.connect(fixture / "database.sqlite3") as connection:
        actual = connection.execute(
            "SELECT COUNT(*), SUM(LENGTH(raw)) FROM items"
        ).fetchone()
        if actual != (count, count * (raw_bytes + len("base64:"))):
            raise RuntimeError("合成 fixture 筆數或正文大小不符")
        actual_rows = connection.execute("SELECT * FROM items ORDER BY id")
        if any(actual_row != expected for actual_row, expected in zip(actual_rows, synthetic_rows())):
            raise RuntimeError("拒絕量測與固定合成內容不符的 fixture")
        collections = connection.execute("SELECT kind,id,name FROM collections ORDER BY id").fetchall()
        if collections != [("mail", "Archive", "Archive"), ("mail", "INBOX", "INBOX")]:
            raise RuntimeError("合成 fixture 集合不符")
    cases = {
        "list_limit_10": ["mail", "list", "--limit", "10"],
        "collection_limit_10": ["mail", "list", "--collection", "INBOX", "--limit", "10"],
        "search_no_match": ["mail", "search", "沒有符合的合成關鍵字"],
        "search_cjk_hit": ["mail", "search", "合成效能資料 3999"],
        "search_ascii_hit": ["mail", "search", "benchmark-03999"],
        "search_two_chars_no_match": ["mail", "search", "無關"],
        "tui_snapshot": ["tui", "--snapshot"],
    }
    results = {}
    for name, command in cases.items():
        samples = []
        for iteration in range(6):
            start = time.perf_counter()
            completed = subprocess.run(
                ["/usr/bin/time", "-l", binary, "--json", "--data-dir", str(fixture), *command],
                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                text=True, check=True,
            )
            elapsed = (time.perf_counter() - start) * 1000
            memory = re.search(r"(\d+)\s+maximum resident set size", completed.stderr)
            if memory is None:
                raise RuntimeError("此量測需要 macOS /usr/bin/time -l")
            if iteration:
                samples.append({"wall_ms": elapsed, "max_rss_bytes": int(memory.group(1))})
        results[name] = {
            "median_wall_ms": statistics.median(row["wall_ms"] for row in samples),
            "median_max_rss_bytes": statistics.median(row["max_rss_bytes"] for row in samples),
            "samples": samples,
        }
    output = {
        "binary": binary,
        "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
        "platform": platform.platform(),
        "fixture": str(fixture),
        "mail_count": count,
        "mail_base64_chars_each": raw_bytes,
        "inbox_count": 100,
        "warmup_runs": 1,
        "sample_runs": 5,
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({name: {key: value for key, value in result.items() if key != "samples"} for name, result in results.items()}, ensure_ascii=False))


if __name__ == "__main__":
    main()

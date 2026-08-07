#!/usr/bin/env python3
"""Extract conversation text from opencode SQLite DB into JSONL corpus.

Usage: python3 tools/extract_corpus.py <db> <out.jsonl> [limit] [--parts | --messages]
"""
import sqlite3
import json
import hashlib
import sys

def extract(db_path: str, out_path: str, limit: int, source: str) -> None:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    seen: set[str] = set()
    count = 0
    with open(out_path, "w", encoding="utf-8") as f:
        if source == "parts":
            rows = conn.execute(
                "SELECT data FROM part WHERE json_extract(data, '$.type') = 'text'"
                " AND json_extract(data, '$.text') IS NOT NULL LIMIT ?",
                (limit,),
            )
            for (data,) in rows:
                try:
                    text = json.loads(data).get("text")
                except (json.JSONDecodeError, AttributeError):
                    continue
                if not text or not str(text).strip():
                    continue
                text = str(text)
                h = hashlib.md5(text.encode()).hexdigest()
                if h in seen:
                    continue
                seen.add(h)
                f.write(json.dumps({"text": text}, ensure_ascii=False) + "\n")
                count += 1
        else:
            rows = conn.execute(
                "SELECT data FROM message WHERE json_extract(data, '$.content') IS NOT NULL LIMIT ?",
                (limit,),
            )
            for (data,) in rows:
                try:
                    text = json.loads(data).get("content")
                except (json.JSONDecodeError, AttributeError):
                    continue
                if not text or not str(text).strip():
                    continue
                text = str(text)
                h = hashlib.md5(text.encode()).hexdigest()
                if h in seen:
                    continue
                seen.add(h)
                f.write(json.dumps({"text": text}, ensure_ascii=False) + "\n")
                count += 1
    print(f"wrote {count} records -> {out_path}")

if __name__ == "__main__":
    db = sys.argv[1]
    out = sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 500
    source = sys.argv[4] if len(sys.argv) > 4 else "parts"
    extract(db, out, limit, source)

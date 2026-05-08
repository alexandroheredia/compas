#!/usr/bin/env python3
"""
Compas Evaluation Script

Measures search, outline, and graph quality against the bookswipe repo.
All expected files are verified to exist in the actual repo.

Usage:
    python3 evaluate_compas.py

Requires:
    - compas server running on localhost:3001
    - bookswipe repo indexed
"""

import json
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import List, Optional

REPO = "bookswipe"  # repo name to evaluate (must match registry entry)


@dataclass
class SearchTest:
    query: str
    expected_files: List[str]
    difficulty: str = "easy"  # easy (direct name match) | medium (semantic gap) | hard (vocabulary mismatch)


# All expected files verified against bookswipe/lib/ contents.
SEARCH_TESTS = [
    # --- Easy: direct name or obvious concept ---
    SearchTest(
        query="What does hardcover_service.dart do?",
        expected_files=["hardcover_service.dart"],
        difficulty="easy",
    ),
    SearchTest(
        query="Fetch all books from the database",
        expected_files=["book_service.dart", "book_query_operations.dart"],
        difficulty="easy",
    ),
    SearchTest(
        query="Where are book status enums defined?",
        expected_files=["book_status.dart"],
        difficulty="easy",
    ),
    SearchTest(
        query="Where is the main app entry point?",
        expected_files=["main.dart"],
        difficulty="easy",
    ),

    # --- Medium: requires semantic understanding ---
    SearchTest(
        query="How does the app cache book metadata from Hardcover?",
        expected_files=["hardcover_cache_service.dart", "hardcover_service.dart"],
        difficulty="medium",
    ),
    SearchTest(
        query="Book data model with scores and popularity",
        expected_files=["book_item.dart"],
        difficulty="medium",
    ),
    SearchTest(
        query="Hardcover metadata model with ratings and ISBN",
        expected_files=["hardcover_metadata.dart"],
        difficulty="medium",
    ),
    SearchTest(
        query="Clean up book descriptions using AI",
        expected_files=["claude_service.dart"],
        difficulty="medium",
    ),
    SearchTest(
        query="Category colors for book genres",
        expected_files=["category_colors.dart"],
        difficulty="medium",
    ),
    SearchTest(
        query="Offline book storage and caching",
        expected_files=["book_cache_service.dart"],
        difficulty="medium",
    ),

    # --- Hard: vocabulary mismatch or abstract concepts ---
    SearchTest(
        query="Where is the app initialization and provider setup?",
        expected_files=["main.dart"],
        difficulty="hard",
    ),
    SearchTest(
        query="How are books displayed in the card UI?",
        expected_files=["book_card.dart"],
        difficulty="hard",
    ),
    SearchTest(
        query="Find similar books grid widget",
        expected_files=["similar_books_grid.dart"],
        difficulty="hard",
    ),
]

GRAPH_TESTS = [
    # (symbol, file_hint, expect_calls, expect_callers)
    ("HardcoverCacheService.cacheMetadata", "lib/services/hardcover_cache_service.dart", True, False),
    ("HardcoverService.getBookInfo", "lib/services/hardcover_service.dart", True, False),
    ("BookCacheService.cacheAllBooks", "lib/services/book_cache_service.dart", True, False),
]


def api_request(path: str) -> dict:
    url = f"http://localhost:3001{path}"
    req = urllib.request.Request(url)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())
    except Exception as e:
        return {"error": str(e)}


def test_search(test: SearchTest) -> dict:
    start = time.time()
    result = api_request(f"/search?repo={REPO}&q={urllib.parse.quote(test.query)}&limit=10")
    elapsed = time.time() - start

    if "error" in result:
        return {"success": False, "error": result["error"], "latency_ms": elapsed * 1000}

    results = result.get("results", [])
    found_files = []
    for r in results:
        fp = r.get("chunk", {}).get("file_path", "")
        fname = fp.split("/")[-1] if "/" in fp else fp
        found_files.append(fname)

    hits = []
    for expected in test.expected_files:
        matched = any(expected in f for f in found_files)
        rank = None
        if matched:
            for i, f in enumerate(found_files):
                if expected in f:
                    rank = i + 1
                    break
        hits.append({"expected": expected, "found": matched, "rank": rank})

    precision_at_5 = sum(1 for h in hits if h["found"]) / max(len(hits), 1)
    found_in_top5 = [f for f in found_files[:5] if any(e in f for e in test.expected_files)]

    return {
        "success": True,
        "latency_ms": round(elapsed * 1000, 1),
        "results_count": len(results),
        "found_files": found_files[:5],
        "expected_hits": hits,
        "precision_at_5": round(precision_at_5, 2),
        "found_in_top5": len(found_in_top5),
    }


def test_graph(symbol: str, file_hint: str, expect_calls: bool, expect_callers: bool) -> dict:
    start = time.time()
    path = f"/graph?repo={REPO}&symbol={urllib.parse.quote(symbol)}"
    if file_hint:
        path += f"&file={urllib.parse.quote(file_hint)}"
    result = api_request(path)
    elapsed = time.time() - start

    if "error" in result:
        return {"success": False, "error": result["error"], "latency_ms": elapsed * 1000}

    if isinstance(result, list):
        has_calls = any(len(n.get("calls", [])) > 0 for n in result)
        has_callers = any(len(n.get("called_by", [])) > 0 for n in result)
        calls_ok = has_calls if expect_calls else True
        callers_ok = has_callers if expect_callers else True
        return {
            "success": True,
            "latency_ms": round(elapsed * 1000, 1),
            "match_count": len(result),
            "has_calls": has_calls,
            "has_callers": has_callers,
            "calls_ok": calls_ok,
            "callers_ok": callers_ok,
            "sample": result[0] if result else None,
        }

    return {"success": False, "error": "unexpected response format", "latency_ms": elapsed * 1000}


def run_evaluation():
    print("=" * 72)
    print("  COMPAS EVALUATION  ")
    print("=" * 72)
    print()

    health = api_request("/health")
    if health.get("status") != "ok":
        print(f"ERROR: compas server not healthy: {health}")
        print("Make sure 'compas serve' is running on localhost:3001")
        return
    print(f"Server: {health.get('status', 'unknown')}")
    print()

    # ── 1. Search ────────────────────────────────────────────────────────
    print("─" * 72)
    print("  1. SEMANTIC SEARCH")
    print("─" * 72)
    print()

    search_results = []
    by_difficulty = {"easy": [], "medium": [], "hard": []}

    for test in SEARCH_TESTS:
        result = test_search(test)
        search_results.append({"test": test, "result": result})
        by_difficulty[test.difficulty].append({"test": test, "result": result})

        p5 = result.get("precision_at_5", 0)
        status = "✅" if p5 > 0 else "❌"
        diff_tag = f"[{test.difficulty.upper():4s}]"
        print(f"  {status} {diff_tag} {test.query}")
        print(f"       P@5: {p5}  |  Latency: {result.get('latency_ms', 0):.1f}ms")
        for hit in result.get("expected_hits", []):
            rank = hit.get("rank")
            mark = "✓" if hit["found"] else "✗"
            rank_str = f"#{rank}" if rank else "NOT FOUND"
            print(f"       {mark} {hit['expected']}: {rank_str}")
        print(f"       Top 5: {result.get('found_files', [])}")
        print()

    overall_p5 = sum(r["result"].get("precision_at_5", 0) for r in search_results) / len(search_results)
    easy_p5 = sum(r["result"].get("precision_at_5", 0) for r in by_difficulty["easy"]) / max(len(by_difficulty["easy"]), 1)
    medium_p5 = sum(r["result"].get("precision_at_5", 0) for r in by_difficulty["medium"]) / max(len(by_difficulty["medium"]), 1)
    hard_p5 = sum(r["result"].get("precision_at_5", 0) for r in by_difficulty["hard"]) / max(len(by_difficulty["hard"]), 1)

    print(f"  Overall P@5:   {overall_p5:.2f}")
    print(f"  Easy P@5:      {easy_p5:.2f}  ({len(by_difficulty['easy'])} queries)")
    print(f"  Medium P@5:    {medium_p5:.2f}  ({len(by_difficulty['medium'])} queries)")
    print(f"  Hard P@5:      {hard_p5:.2f}  ({len(by_difficulty['hard'])} queries)")
    print()

    # ── 2. Graph ──────────────────────────────────────────────────────────
    print("─" * 72)
    print("  3. SYMBOL GRAPH")
    print("─" * 72)
    print()

    graph_pass = 0
    for symbol, file_hint, expect_calls, expect_callers in GRAPH_TESTS:
        result = test_graph(symbol, file_hint, expect_calls, expect_callers)
        calls_ok = result.get("calls_ok", False)
        callers_ok = result.get("callers_ok", False)
        all_ok = calls_ok and callers_ok
        status = "✅" if all_ok else "⚠️"
        graph_pass += 1 if all_ok else 0
        print(f"  {status} {symbol}")
        print(f"     Calls: {result.get('has_calls', False)} (expected: {expect_calls}) | Callers: {result.get('has_callers', False)} (expected: {expect_callers})")
        print()

    print(f"  Graph: {graph_pass}/{len(GRAPH_TESTS)} fully correct")
    print()

    # ── Summary ───────────────────────────────────────────────────────────
    print("=" * 72)
    print("  SUMMARY")
    print("=" * 72)
    avg_latency = sum(r["result"].get("latency_ms", 0) for r in search_results) / len(search_results)
    print(f"  Search P@5:    {overall_p5:.2f}  ({len(search_results)} queries, avg latency {avg_latency:.1f}ms)")
    print(f"  Graph:         {graph_pass}/{len(GRAPH_TESTS)} symbols with correct edges")
    print()
    print("  Targets:")
    print("    P@5 ≥ 0.70:  agent reliably finds relevant code")
    print("    P@5 ≥ 0.85:  production quality")
    print()


if __name__ == "__main__":
    run_evaluation()
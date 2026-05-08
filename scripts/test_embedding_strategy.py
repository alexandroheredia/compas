#!/usr/bin/env python3
"""
Comprehensive embedding strategy comparison for Compas search quality.

Tests multiple enrichment strategies against diverse code chunks and queries
from the bookswipe repo. Measures:
  - Raw cosine similarity between queries and their target chunks
  - Discrimination: gap between correct target and best wrong chunk
  - Aggregate scores per strategy

Requires Ollama running with nomic-embed-text pulled.
"""

import urllib.request
import json
import math
import time
import sys

url = "http://localhost:11434/api/embed"
model = "nomic-embed-text"


def embed(text):
    req = urllib.request.Request(
        url,
        data=json.dumps({"model": model, "input": text}).encode(),
        headers={"Content-Type": "application/json"},
    )
    resp = urllib.request.urlopen(req)
    data = json.loads(resp.read().decode())
    return data["embeddings"][0]


def cos_sim(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    norm_a = math.sqrt(sum(x * x for x in a))
    norm_b = math.sqrt(sum(x * x for x in b))
    return dot / (norm_a * norm_b)


# ── Code chunks from bookswipe ──────────────────────────────────────────

chunks = {
    "HardcoverService.getBookInfo": {
        "filepath": "lib/services/hardcover_service.dart",
        "filename": "hardcover_service.dart",
        "kind": "method",
        "qualified_name": "HardcoverService.getBookInfo",
        "doc_comment": "Fetches book information from the Hardcover API by searching for a title and author.",
        "imports": "import 'package:flutter/foundation.dart';\nimport 'package:flutter_dotenv/flutter_dotenv.dart';\nimport 'package:http/http.dart' as http;\nimport 'dart:convert';",
        "summary": "Fetches detailed book information from the Hardcover API by searching for a book title and author, then validating the best match.",
        "raw": """Future<Map<String, dynamic>?> getBookInfo(String title, String author) async {
  try {
    debugPrint('Getting book info for: $title by $author');
    final searchQuery = '''
      query SearchBook(\$query: String!) {
        search(query: \$query, query_type: "Book", per_page: 10) { ids results }
      }
    ''';
    final searchResponse = await _graphqlRequest(searchQuery, variables: {'query': title});
    if (searchResponse['data'] == null) return null;
    final bookIds = searchResponse['data']['search']['ids'] as List?;
    if (bookIds == null || bookIds.isEmpty) return null;
    return await _findBestMatch(bookIds, title, author);
  } catch (e) {
    debugPrint('Error getting book info: $e');
    return null;
  }
}""",
    },
    "BookItem": {
        "filepath": "lib/book_service.dart",
        "filename": "book_service.dart",
        "kind": "class",
        "qualified_name": "BookItem",
        "doc_comment": "Data model representing a book with metadata, scoring, and status tracking.",
        "imports": "import 'package:supabase_flutter/supabase_flutter.dart';\nimport 'package:flutter/foundation.dart';\nimport 'services/book_cache_service.dart';\nimport 'services/logging_service.dart';\nimport 'models/hardcover_metadata.dart';",
        "summary": "Data model class for a book item containing title, author, description, scores, status, metadata, and display information.",
        "raw": """class BookItem {
  final int id;
  final String title;
  final String categoryName;
  final String description;
  final bool aiCleaned;
  final String? isbn;
  final String? tags;
  final String? seriesInfo;
  final int? seriesNumber;
  final String? filetype;
  final String sizeDisplay;
  final int? numfiles;
  final int seeders;
  final int leechers;
  final int timesCompleted;
  final DateTime? addedDate;
  final BookStatus status;
  final DateTime? statusUpdatedAt;
  final String? notes;
  final bool vip;
  final bool free;
  final String? ownerName;
  final String languageCode;
  final String authors;
  final String narrators;
  final DateTime importedAt;
  final DateTime updatedAt;
  final double popularityScore;
  final double personalScore;
  final String? subtitle;
  final double scoreAuthor;
  final double scoreNarrator;
  final double scorePenalties;
  final double? scoreEmbedding;
  final double scoreSeries;
}""",
    },
    "SwipeHandler.handlePanEnd": {
        "filepath": "lib/utils/swipe_handler.dart",
        "filename": "swipe_handler.dart",
        "kind": "method",
        "qualified_name": "SwipeHandler.handlePanEnd",
        "doc_comment": "Handle pan end for completing or canceling swipe based on velocity and position threshold.",
        "imports": "import 'package:flutter/material.dart';\nimport '../book_service.dart';\nimport 'animation_utils.dart';",
        "summary": "Determines whether a card swipe should complete or cancel based on swipe velocity and position thresholds, then triggers the appropriate animation.",
        "raw": """Future<void> handlePanEnd(
    DragEndDetails details,
    AnimationController controller,
    BookItem currentBook,
  ) async {
    final position = controller.value;
    bool shouldComplete = false;
    SwipeDirection? direction;

    if (SwipeUtils.isValidSwipeVelocity(details.velocity)) {
      if (details.velocity.pixelsPerSecond.dx > 0) {
        direction = SwipeDirection.right;
        shouldComplete = true;
      } else if (details.velocity.pixelsPerSecond.dx < 0) {
        direction = SwipeDirection.left;
        shouldComplete = true;
      }
    } else {
      if (position > 0.6) {
        direction = SwipeDirection.right;
        shouldComplete = true;
      } else if (position < -0.6) {
        direction = SwipeDirection.left;
        shouldComplete = true;
      }
    }

    if (shouldComplete && direction != null) {
      await _completeSwipe(direction, controller, currentBook);
    } else {
      await _cancelSwipe(controller);
    }
  }""",
    },
    "BookCacheService.cacheAllBooks": {
        "filepath": "lib/services/book_cache_service.dart",
        "filename": "book_cache_service.dart",
        "kind": "method",
        "qualified_name": "BookCacheService.cacheAllBooks",
        "doc_comment": "Save all books to local cache file with timestamp.",
        "imports": "import 'dart:convert';\nimport 'dart:io';\nimport 'package:flutter/foundation.dart';\nimport 'package:path_provider/path_provider.dart';\nimport 'package:shared_preferences/shared_preferences.dart';\nimport '../book_service.dart';",
        "summary": "Serializes a list of books to JSON and writes them to a local cache file on disk, along with a timestamp for cache validity.",
        "raw": """static Future<void> cacheAllBooks(List<BookItem> books) async {
    if (!enabled) {
      debugPrint('Caching is disabled (skipping cacheAllBooks)');
      return;
    }
    try {
      final cacheFile = await _getCacheFile();
      final timestampFile = await _getTimestampFile();
      final booksJson = books.map((book) => book.toJson()).toList();
      final jsonString = jsonEncode(booksJson);
      await cacheFile.writeAsString(jsonString);
      await timestampFile.writeAsString(
        DateTime.now().millisecondsSinceEpoch.toString(),
      );
      debugPrint(
        'Cached ${books.length} books to file (${(jsonString.length / 1024 / 1024).toStringAsFixed(2)} MB)',
      );
    } catch (e) {
      debugPrint('Error caching books: $e');
    }
  }""",
    },
    "ClaudeService.cleanDescription": {
        "filepath": "lib/services/claude_service.dart",
        "filename": "claude_service.dart",
        "kind": "method",
        "qualified_name": "ClaudeService.cleanDescription",
        "doc_comment": "Clean a book description by removing marketing fluff, quotes, uploader notes, and technical details.",
        "imports": "import 'dart:convert';\nimport 'package:http/http.dart' as http;\nimport 'package:flutter/foundation.dart';\nimport 'package:flutter_dotenv/flutter_dotenv.dart';",
        "summary": "Sends a book description to Claude AI to extract only the actual plot summary, removing marketing phrases, reviewer quotes, and uploader notes, with exponential backoff retry logic.",
        "raw": """static Future<String> cleanDescription(String originalDescription) async {
    if (originalDescription.isEmpty) {
      return originalDescription;
    }
    const maxRetries = 3;
    const initialDelay = Duration(seconds: 2);
    for (int attempt = 0; attempt < maxRetries; attempt++) {
      try {
        if (attempt > 0) {
          final delay = initialDelay * (attempt * 2);
          debugPrint('Retry attempt $attempt after ${delay.inSeconds}s...');
          await Future.delayed(delay);
        }
        debugPrint(
          'Cleaning description with Claude Haiku (attempt ${attempt + 1}/$maxRetries)...',
        );
        final response = await http.post(
          Uri.parse(_apiUrl),
          headers: {
            'Content-Type': 'application/json',
            'x-api-key': _apiKey,
            'anthropic-version': '2023-06-01',
          },
          body: jsonEncode({
            'model': _model,
            'max_tokens': 1024,
          }),
        );
      } catch (e) {
        debugPrint('Error cleaning description: $e');
      }
    }
    return originalDescription;
  }""",
    },
}

# ── Queries mapped to their expected target chunk ────────────────────────

queries = [
    # hardcover_service — direct vocabulary
    ("fetch book metadata from API", "HardcoverService.getBookInfo"),
    ("hardcover book lookup", "HardcoverService.getBookInfo"),
    ("GraphQL book query", "HardcoverService.getBookInfo"),
    # book_service — semantic gap (model/database != class name)
    ("database model for books", "BookItem"),
    ("book data structure with scores", "BookItem"),
    ("book status enum values", "BookItem"),
    # swipe_handler — interaction/UX
    ("swipe gesture handling", "SwipeHandler.handlePanEnd"),
    ("card drag animation", "SwipeHandler.handlePanEnd"),
    ("how does swiping work", "SwipeHandler.handlePanEnd"),
    # book_cache — persistence
    ("caching book data locally", "BookCacheService.cacheAllBooks"),
    ("offline book storage", "BookCacheService.cacheAllBooks"),
    # claude_service — AI/NLP
    ("AI text cleaning", "ClaudeService.cleanDescription"),
    ("clean up book descriptions", "ClaudeService.cleanDescription"),
    # adversarial — vocabulary mismatch
    ("login authentication logic", "HardcoverService.getBookInfo"),
    ("list methods in BookService", "BookItem"),
]


# ── Enrichment strategies ────────────────────────────────────────────────

def build_variants(chunk_id, chunk_data):
    """Return dict of strategy_name -> enriched text for one chunk."""
    raw = chunk_data["raw"]
    filepath = chunk_data["filepath"]
    filename = chunk_data["filename"]
    kind = chunk_data["kind"]
    qualified_name = chunk_data["qualified_name"]
    doc_comment = chunk_data["doc_comment"]
    imports = chunk_data["imports"]
    summary = chunk_data["summary"]

    variants = {
        "raw": raw,
        "+filename": f"{filename}\n{raw}",
        "+filepath": f"{filepath}\n{raw}",
        "+doc_comment": f"{doc_comment}\n{raw}",
        "+summary": f"{summary}\n{raw}",
        "+doc_comment+filename": f"{doc_comment}\n{filename}\n{raw}",
        "+summary+filename": f"{summary}\n{filename}\n{raw}",
    }
    return variants


STRATEGY_NAMES = [
    "raw",
    "+filename",
    "+filepath",
    "+doc_comment",
    "+summary",
    "+doc_comment+filename",
    "+summary+filename",
]

# ── Embedding phase ──────────────────────────────────────────────────────

print("=" * 70)
print("COMPAS EMBEDDING STRATEGY BENCHMARK")
print("=" * 70)
print(f"Chunks:   {len(chunks)}")
print(f"Queries:  {len(queries)}")
print(f"Strategies: {len(STRATEGY_NAMES)}")
print()

all_variants = {}  # (chunk_id, strategy_name) -> text
for cid, cdata in chunks.items():
    variants = build_variants(cid, cdata)
    for sname, text in variants.items():
        all_variants[(cid, sname)] = text

total_embeddings = len(all_variants) + len(queries)
print(f"Total embedding calls: {total_embeddings}")
print()

embeddings = {}  # key -> embedding vector
t0 = time.time()

print("Embedding chunk variants...")
done = 0
for key, text in all_variants.items():
    embeddings[key] = embed(text)
    done += 1
    if done % 10 == 0:
        print(f"  {done}/{len(all_variants)} variants...")

print("Embedding queries...")
for i, (query, _) in enumerate(queries):
    embeddings[("query", i)] = embed(query)

elapsed = time.time() - t0
print(f"\nDone embedding in {elapsed:.1f}s ({total_embeddings} calls)\n")

# ── Results: Per-query table ─────────────────────────────────────────────

print("=" * 70)
print("PER-QUERY RESULTS")
print("=" * 70)
print()

chunk_ids = list(chunks.keys())

for qi, (query, target) in enumerate(queries):
    q_emb = embeddings[("query", qi)]
    target_scores = {}
    wrong_best = {}

    for sname in STRATEGY_NAMES:
        score = cos_sim(q_emb, embeddings[(target, sname)])
        target_scores[sname] = score

        best_wrong = max(
            cos_sim(q_emb, embeddings[(cid, sname)])
            for cid in chunk_ids
            if cid != target
        )
        wrong_best[sname] = best_wrong

    col_w = 11
    query_w = 38

    header = f"  {query[:query_w]:<{query_w}}"
    header += "".join(f"{sname:>{col_w}}" for sname in STRATEGY_NAMES)
    print(header)

    row = f"  {'score':<{query_w}}"
    row += "".join(f"{target_scores[s]:>{col_w}.4f}" for s in STRATEGY_NAMES)
    print(row)

    row = f"  {'gap (score - wrong)':<{query_w}}"
    row += "".join(
        f"{(target_scores[s] - wrong_best[s]):>{col_w}.4f}" for s in STRATEGY_NAMES
    )
    print(row)
    print()

# ── Aggregate: mean target similarity per strategy ──────────────────────

print("=" * 70)
print("AGGREGATE RESULTS")
print("=" * 70)
print()

mean_target = {s: 0.0 for s in STRATEGY_NAMES}
mean_gap = {s: 0.0 for s in STRATEGY_NAMES}
mean_target_no_adversarial = {s: 0.0 for s in STRATEGY_NAMES}
mean_gap_no_adversarial = {s: 0.0 for s in STRATEGY_NAMES}
n_all = len(queries)
n_no_adv = len([q for q in queries if q[1] is not None and "login" not in q[0] and "list methods" not in q[0]])

for qi, (query, target) in enumerate(queries):
    q_emb = embeddings[("query", qi)]
    is_adversarial = "login" in query or "list methods" in query

    for sname in STRATEGY_NAMES:
        target_score = cos_sim(q_emb, embeddings[(target, sname)])
        best_wrong = max(
            cos_sim(q_emb, embeddings[(cid, sname)])
            for cid in chunk_ids
            if cid != target
        )
        gap = target_score - best_wrong

        mean_target[sname] += target_score
        mean_gap[sname] += gap
        if not is_adversarial:
            mean_target_no_adversarial[sname] += target_score
            mean_gap_no_adversarial[sname] += gap

for s in STRATEGY_NAMES:
    mean_target[s] /= n_all
    mean_gap[s] /= n_all
    mean_target_no_adversarial[s] /= n_no_adv
    mean_gap_no_adversarial[s] /= n_no_adv

col_w = 11
print(f"{'Strategy':<22} {'Mean Score':>{col_w}} {'Mean Gap':>{col_w}} {'Score*':>{col_w}} {'Gap*':>{col_w}}")
print(f"{'':.<22} {'(all)':>{col_w}} {'(all)':>{col_w}} {'(no adv)':>{col_w}} {'(no adv)':>{col_w}}")
print("-" * 70)
for s in STRATEGY_NAMES:
    print(
        f"{s:<22} {mean_target[s]:>{col_w}.4f} {mean_gap[s]:>{col_w}.4f} "
        f"{mean_target_no_adversarial[s]:>{col_w}.4f} {mean_gap_no_adversarial[s]:>{col_w}.4f}"
    )

print()
print("* = excluding adversarial queries (login auth, list methods)")
print()

# ── Best strategy per query type ────────────────────────────────────────

print("=" * 70)
print("BEST STRATEGY PER QUERY")
print("=" * 70)
print()

for qi, (query, target) in enumerate(queries):
    q_emb = embeddings[("query", qi)]
    best_sname = max(STRATEGY_NAMES, key=lambda s: cos_sim(q_emb, embeddings[(target, s)]))
    best_score = cos_sim(q_emb, embeddings[(target, best_sname)])
    raw_score = cos_sim(q_emb, embeddings[(target, "raw")])
    delta = best_score - raw_score
    print(f"  {query:<42} -> {best_sname:<22} (score={best_score:.4f}, delta={delta:+.4f})")
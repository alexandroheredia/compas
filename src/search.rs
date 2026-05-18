use crate::graph::Graph;
use crate::models::SearchResult;
use std::collections::{HashMap, HashSet};

const PRIVATE_QUERY_TERMS: &[&str] = &["private", "helper", "internal", "implementation"];
const SYMBOL_BOOST: f32 = 0.12;
const FILE_BOOST: f32 = 0.10;
const CLASS_BOOST: f32 = 0.05;
const METHOD_BOOST: f32 = 0.02;
const GRAPH_BOOST: f32 = 0.10;
const PRIVATE_PENALTY: f32 = 0.15;
// The chunk content already includes doc comments plus filename/symbol enrichment.
// Rewarding query-token hits there helps abstract entry-point queries across repos.
const CONTENT_TOKEN_BOOST: f32 = 0.04;

pub fn rerank_results(
    graph: &Graph,
    raw_results: Vec<SearchResult>,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let query_terms = collect_query_terms(query);
    let query_mentions_private = query_terms
        .iter()
        .any(|term| PRIVATE_QUERY_TERMS.contains(&term.as_str()));

    let boosted: Vec<SearchResult> = raw_results
        .into_iter()
        .map(|mut result| {
            let symbol_lower = result.chunk.symbol.to_lowercase();
            let file_lower = result.chunk.file_path.to_lowercase();
            let content_lower = result.chunk.content.to_lowercase();
            let mut boost = 0.0f32;

            for term in &query_terms {
                if symbol_lower.contains(term) {
                    boost += SYMBOL_BOOST;
                }
                if file_lower.contains(term) {
                    boost += FILE_BOOST;
                }
                if content_lower.contains(term) {
                    boost += CONTENT_TOKEN_BOOST;
                }
            }

            match result.chunk.kind.as_str() {
                "class" => boost += CLASS_BOOST,
                "method" => boost += METHOD_BOOST,
                _ => {}
            }

            if let Some(node) = graph.get(&result.chunk.symbol, &result.chunk.file_path) {
                let related: Vec<String> = node
                    .calls
                    .iter()
                    .chain(node.called_by.iter())
                    .map(|symbol| symbol.to_lowercase())
                    .collect();
                for term in &query_terms {
                    if related.iter().any(|symbol| symbol.contains(term)) {
                        boost += GRAPH_BOOST;
                        break;
                    }
                }
            }

            if result.chunk.symbol.starts_with('_') && !query_mentions_private {
                boost -= PRIVATE_PENALTY;
            }

            result.score += boost;
            result
        })
        .collect();

    dedupe_results(boosted, limit)
}

fn collect_query_terms(query: &str) -> Vec<String> {
    dedupe_terms(normalized_query_terms(query))
}

fn normalized_query_terms(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.to_string())
        .collect()
}

fn dedupe_terms(terms: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped = vec![];
    for term in terms {
        if seen.insert(term.clone()) {
            deduped.push(term);
        }
    }
    deduped
}

fn dedupe_results(results: Vec<SearchResult>, limit: usize) -> Vec<SearchResult> {
    fn strip_part_suffix(name: &str) -> &str {
        name.rfind("_p")
            .and_then(|index| {
                name[index + 2..]
                    .parse::<u32>()
                    .ok()
                    .map(|_| &name[..index])
            })
            .unwrap_or(name)
    }

    let mut best_by_symbol: HashMap<(String, String), SearchResult> = HashMap::new();
    for result in results {
        let stripped_symbol = strip_part_suffix(&result.chunk.symbol).to_string();
        let key = (result.chunk.file_path.clone(), stripped_symbol);
        let should_insert = match best_by_symbol.get(&key) {
            Some(existing) => result.score > existing.score,
            None => true,
        };
        if should_insert {
            best_by_symbol.insert(key, result);
        }
    }

    let mut file_counts: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<SearchResult> = best_by_symbol.into_values().collect();
    deduped.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    deduped.retain(|result| {
        let count = file_counts
            .entry(result.chunk.file_path.clone())
            .or_insert(0);
        if *count < 3 {
            *count += 1;
            true
        } else {
            false
        }
    });
    deduped.truncate(limit);
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_content_boost_matches_experiment() {
        assert_eq!(CONTENT_TOKEN_BOOST, 0.04);
    }

    #[test]
    fn normalized_query_terms_strip_punctuation() {
        let terms = normalized_query_terms("Where is the app initialization and provider setup?");
        assert_eq!(
            terms,
            vec![
                "where",
                "is",
                "the",
                "app",
                "initialization",
                "and",
                "provider",
                "setup"
            ]
        );
    }
}

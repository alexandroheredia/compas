#[cfg(test)]
mod tests {
    use crate::chunker::dart::{
        extract_calls, extract_doc_comments, extract_semantic_references, DartChunker,
    };
    use crate::chunker::Chunker;
    use tree_sitter::{Node, Parser};

    fn init_parser() -> Parser {
        let mut parser = Parser::new();
        let language = unsafe {
            tree_sitter::Language::from_raw(std::mem::transmute::<
                _,
                unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage,
            >(tree_sitter_dart::LANGUAGE.into_raw())())
        };
        parser.set_language(&language).unwrap();
        parser
    }

    fn print_tree(node: Node, code: &str, depth: usize) {
        let indent = "  ".repeat(depth);
        let text = &code[node.start_byte()..node.end_byte().min(code.len())];
        let preview = if text.len() > 30 { &text[..30] } else { text };
        println!(
            "{}{}: '{}'",
            indent,
            node.kind(),
            preview.replace('\n', " ")
        );
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            print_tree(child, code, depth + 1);
        }
    }

    #[test]
    fn debug_dart_ast() {
        let code = r#"class Foo {
  void bar() {
    print("hello");
    baz();
    this.doThing();
  }
}

void topLevel() {
  helper();
}
"#;
        let mut parser = init_parser();
        let tree = parser.parse(code, None).unwrap();
        print_tree(tree.root_node(), code, 0);
    }

    #[test]
    fn test_truncate_long_strings() {
        use crate::chunker::dart::truncate_long_strings;
        let text = r#"debugPrint('short'); debugPrint('''long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long long''');"#;
        let result = truncate_long_strings(text);
        println!("{}", result);
        assert!(result.contains("'...'"));
        assert!(result.contains("'short'"));
    }

    #[test]
    fn test_extract_doc_comments_basic() {
        let code = r#"/// Fetches book info from the API.
/// Includes retry logic and caching.
Future<Map<String, dynamic>?> getBookInfo() async {
  return null;
}
"#;
        let start = code.find("Future").unwrap();
        let comments = extract_doc_comments(code, start);
        assert_eq!(
            comments,
            "/// Fetches book info from the API.\n/// Includes retry logic and caching."
        );
    }

    #[test]
    fn test_extract_doc_comments_with_blank_lines() {
        let code = r#"/// First line of docs.

/// Second line after blank.
void foo() {}
"#;
        let start = code.find("void foo").unwrap();
        let comments = extract_doc_comments(code, start);
        assert_eq!(
            comments,
            "/// First line of docs.\n/// Second line after blank."
        );
    }

    #[test]
    fn test_extract_doc_comments_none() {
        let code = r#"void bar() {}
"#;
        let start = code.find("void bar").unwrap();
        let comments = extract_doc_comments(code, start);
        assert!(comments.is_empty());
    }

    #[test]
    fn test_chunk_factory_constructor() {
        let code = r#"class Product {
  final int id;
  factory Product.fromMap(Map<String, dynamic> json) {
    return Product(id: json['id'] as int);
  }
  factory Product.fromJson(Map<String, dynamic> json) {
    return Product(id: json['id'] as int);
  }
}"#;
        let chunker = DartChunker;
        let chunks = chunker.chunk("lib/product_service.dart", code).unwrap();
        let from_supabase = chunks.iter().find(|c| c.symbol == "Product.fromMap");
        assert!(
            from_supabase.is_some(),
            "Expected Product.fromMap chunk, got symbols: {:?}",
            chunks.iter().map(|c| &c.symbol).collect::<Vec<_>>()
        );
        let from_json = chunks.iter().find(|c| c.symbol == "Product.fromJson");
        assert!(
            from_json.is_some(),
            "Expected Product.fromJson chunk, got symbols: {:?}",
            chunks.iter().map(|c| &c.symbol).collect::<Vec<_>>()
        );
        assert_eq!(from_supabase.unwrap().kind, "constructor");
        assert_eq!(from_json.unwrap().kind, "constructor");
    }

    #[test]
    fn test_chunk_getter_setter() {
        let code = r#"class Product {
  final String categoryName;
  String get name => categoryName;
  String get code => languageCode;
}"#;
        let chunker = DartChunker;
        let chunks = chunker.chunk("lib/product_service.dart", code).unwrap();
        let name = chunks.iter().find(|c| c.symbol == "Product.name");
        assert!(
            name.is_some(),
            "Expected Product.name chunk, got symbols: {:?}",
            chunks.iter().map(|c| &c.symbol).collect::<Vec<_>>()
        );
        assert_eq!(name.unwrap().kind, "getter_setter");
        let lang_code = chunks.iter().find(|c| c.symbol == "Product.code");
        assert!(
            lang_code.is_some(),
            "Expected Product.code chunk, got symbols: {:?}",
            chunks.iter().map(|c| &c.symbol).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_unknown_method() {
        let code = r#"class Product {
  final int id;
  factory Product.fromMap(Map<String, dynamic> json) {
    return Product(id: 1);
  }
  Map<String, dynamic> toJson() {
    return {'id': id};
  }
  String get name => categoryName;
}"#;
        let chunker = DartChunker;
        let chunks = chunker.chunk("lib/product_service.dart", code).unwrap();
        for chunk in &chunks {
            assert!(
                !chunk.symbol.contains("unknown"),
                "Found unknown_method in symbol: {}",
                chunk.symbol
            );
        }
    }

    #[test]
    fn test_extract_calls_static_method() {
        let code = r#"class Foo {
  void bar() {
    StatusUtils.getStatusIcon(status);
    book.fromJson(json);
  }
}

void topLevel() {
  StatusUtils.getStatusColor(status);
}"#;
        let calls = extract_calls(code).unwrap();
        println!("All calls: {:?}", calls);

        // Should find qualified names like StatusUtils.getStatusIcon
        let bar_calls: Vec<_> = calls
            .iter()
            .filter(|(caller, _)| caller == "Foo.bar")
            .map(|(_, callee)| callee.as_str())
            .collect();
        println!("Foo.bar calls: {:?}", bar_calls);

        assert!(
            bar_calls.contains(&"StatusUtils.getStatusIcon"),
            "Foo.bar should call StatusUtils.getStatusIcon, got: {:?}",
            bar_calls
        );
        // Instance call book.fromJson(json) records just "fromJson" (class unknown at parse time)
        assert!(
            bar_calls.contains(&"fromJson"),
            "Foo.bar should call fromJson, got: {:?}",
            bar_calls
        );

        let top_level_calls: Vec<_> = calls
            .iter()
            .filter(|(caller, _)| caller == "topLevel")
            .map(|(_, callee)| callee.as_str())
            .collect();
        println!("topLevel calls: {:?}", top_level_calls);

        assert!(
            top_level_calls.contains(&"StatusUtils.getStatusColor"),
            "topLevel should call StatusUtils.getStatusColor, got: {:?}",
            top_level_calls
        );
    }

    #[test]
    fn test_extract_calls_from_book_details() {
        let code = r#"class _MyScreenState {
  void _buildContent() {
    StatusUtils.getStatusIcon(status);
  }
}"#;
        let calls = extract_calls(code).unwrap();
        println!("Calls from book_details snippet: {:?}", calls);

        let build_body_calls: Vec<_> = calls
            .iter()
            .filter(|(caller, _)| caller == "_MyScreenState._buildContent")
            .map(|(_, callee)| callee.as_str())
            .collect();

        println!("_buildContent calls: {:?}", build_body_calls);
        assert!(!calls.is_empty(), "Expected some calls, got none");
        assert!(
            build_body_calls.contains(&"StatusUtils.getStatusIcon"),
            "Should call StatusUtils.getStatusIcon, got: {:?}",
            build_body_calls
        );
    }

    #[test]
    fn test_semantic_references_capture_getter_reads() {
        let code = r#"class Product {
  String get displayName => name;

  String render() {
    return displayName;
  }
}
"#;

        let analysis = extract_semantic_references(code).unwrap();
        assert!(
            analysis
                .references
                .contains(&("Product.render".to_string(), "displayName".to_string(),)),
            "Expected getter read to be recorded, got: {:?}",
            analysis.references
        );
    }

    #[test]
    fn test_operator_name_preserved_in_chunks() {
        let code = r#"class Product {
  @override
  bool operator ==(Object other) => identical(this, other);
}
"#;

        let chunker = DartChunker;
        let chunks = chunker.chunk("lib/product.dart", code).unwrap();
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.symbol == "Product.operator =="),
            "Expected operator symbol, got: {:?}",
            chunks.iter().map(|c| c.symbol.clone()).collect::<Vec<_>>()
        );
        assert!(
            chunks.iter().all(|chunk| !chunk.symbol.contains("unknown")),
            "Unexpected unknown symbol in {:?}",
            chunks.iter().map(|c| c.symbol.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_semantic_references_traverse_nested_closures() {
        let code = r#"class Product {
  void helper() {}

  void render() {
    items.map((item) {
      helper();
      return item;
    }).toList();
  }
}
"#;

        let analysis = extract_semantic_references(code).unwrap();
        assert!(
            analysis
                .references
                .contains(&("Product.render".to_string(), "helper".to_string())),
            "Expected nested closure reference, got: {:?}",
            analysis.references
        );
    }

    #[test]
    fn test_semantic_references_detect_key_types() {
        let code = r#"class ProductKey {}

Map<ProductKey, String> names = {};
Set<ProductKey> seen = {};
"#;

        let analysis = extract_semantic_references(code).unwrap();
        assert_eq!(analysis.key_types, vec!["ProductKey".to_string()]);
    }

    #[test]
    fn test_semantic_references_detect_family_key_types() {
        let code = r#"
class TravelApprovalsInvoicesRequest {}

final provider = StreamProvider.autoDispose.family<List<String>, TravelApprovalsInvoicesRequest>(
  (ref, request) => const Stream.empty(),
);
"#;

        let analysis = extract_semantic_references(code).unwrap();
        assert_eq!(
            analysis.key_types,
            vec!["TravelApprovalsInvoicesRequest".to_string()]
        );
    }

    #[test]
    fn test_file_fallback_chunk_is_file_kind() {
        let code = r#"part of widgets;"#;
        let chunker = DartChunker;
        let chunks = chunker.chunk("lib/part_only.dart", code).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "file");
        assert_eq!(chunks[0].symbol, "part_only.dart");
    }

    #[test]
    fn test_semantic_references_capture_relative_imports() {
        let code = r#"
import 'src/helpers.dart';
import 'package:tyoajanseuranta/core/providers/provider_auth.dart';
export '../shared/models.dart';
part 'generated.g.dart';
"#;

        let analysis = extract_semantic_references(code).unwrap();
        assert_eq!(
            analysis.import_uris,
            vec![
                "../shared/models.dart".to_string(),
                "generated.g.dart".to_string(),
                "package:tyoajanseuranta/core/providers/provider_auth.dart".to_string(),
                "src/helpers.dart".to_string(),
            ]
        );
    }

    #[test]
    fn test_chunk_includes_doc_comment() {
        let code = r#"import 'dart:io';

class CacheService {
  /// Save all books to cache file.
  static Future<void> saveAll() async {}
}
"#;
        let chunker = DartChunker;
        let chunks = chunker
            .chunk("lib/services/cache_service.dart", code)
            .unwrap();

        let method_chunk = chunks
            .iter()
            .find(|c| c.symbol == "CacheService.saveAll")
            .unwrap();
        assert!(method_chunk
            .content
            .contains("/// Save all books to cache file."));
        assert!(method_chunk
            .content
            .contains("cache_service.dart CacheService.saveAll"));
    }

    #[test]
    fn test_semantic_references_capture_calls_in_provider_arrow_body() {
        // Regression: top-level `final fooProvider = Provider((ref) { ... })`
        // is parsed by tree-sitter-dart as `static_final_declaration_list` (with
        // the `final` keyword as a separate sibling token), not as
        // `final_declaration`. The walker must descend into the function-
        // expression body of the initializer so calls like `service.loadUsers(...)`
        // inside Riverpod provider arrow functions are recorded as references.
        let code = r#"
final userManagementBootstrapProvider =
    FutureProvider<Object>((ref) async {
  final service = ref.watch(userManagementServiceProvider);
  final results = await Future.wait<Object>([
    service.loadUsers(orgId),
    service.loadDepartments(orgId),
    service.loadPendingInvitations(orgId),
    service.loadContractedHoursOptions(orgId),
  ]);
  return results;
});
"#;

        let analysis = extract_semantic_references(code).unwrap();
        let callees: Vec<&String> = analysis
            .references
            .iter()
            .filter(|(caller, _)| caller == "userManagementBootstrapProvider")
            .map(|(_, callee)| callee)
            .collect();

        for expected in &[
            "loadUsers",
            "loadDepartments",
            "loadPendingInvitations",
            "loadContractedHoursOptions",
        ] {
            assert!(
                callees.iter().any(|c| c.as_str() == *expected),
                "expected callee {} from arrow body, got {:?}",
                expected,
                callees
            );
        }
    }
}

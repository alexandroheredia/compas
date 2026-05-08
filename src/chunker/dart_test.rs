#[cfg(test)]
mod tests {
    use crate::chunker::dart::{extract_calls, extract_doc_comments, DartChunker};
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
}

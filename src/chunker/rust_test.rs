#[cfg(test)]
mod tests {
    use crate::chunker::rust::{extract_calls, RustChunker};
    use crate::chunker::{language_for_path, Chunker};
    use tree_sitter::{Node, Parser};

    fn init_parser() -> Parser {
        let mut parser = Parser::new();
        let language = unsafe {
            tree_sitter::Language::from_raw(std::mem::transmute::<
                _,
                unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage,
            >(tree_sitter_rust::LANGUAGE.into_raw())())
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
    fn debug_rust_ast() {
        let code = r#"
struct Foo;

impl Foo {
    fn bar(&self) {
        baz();
    }
}
"#;
        let mut parser = init_parser();
        let tree = parser.parse(code, None).unwrap();
        print_tree(tree.root_node(), code, 0);
    }

    #[test]
    fn test_language_for_path_detects_rust() {
        assert_eq!(
            language_for_path(std::path::Path::new("src/lib.rs")),
            Some("rust")
        );
        assert_eq!(
            language_for_path(std::path::Path::new("lib/main.dart")),
            Some("dart")
        );
        assert_eq!(language_for_path(std::path::Path::new("README.txt")), None);
    }

    #[test]
    fn test_chunk_top_level_function_with_doc_comment() {
        let code = r#"/// Builds the embedder.
pub fn build_embedder() {}
"#;
        let chunker = RustChunker;
        let chunks = chunker.chunk("src/lib.rs", code).unwrap();
        let chunk = chunks
            .iter()
            .find(|c| c.symbol == "build_embedder")
            .unwrap();
        assert_eq!(chunk.kind, "function");
        assert_eq!(chunk.language, "rust");
        assert!(chunk.content.contains("/// Builds the embedder."));
        assert!(chunk.content.contains("lib.rs build_embedder"));
    }

    #[test]
    fn test_chunk_struct_enum_trait() {
        let code = r#"
pub struct EdgeStore;

enum SearchMode {
    Fast,
}

pub trait Store {
    async fn upsert(&self);
}
"#;
        let chunker = RustChunker;
        let chunks = chunker.chunk("src/lib.rs", code).unwrap();
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "EdgeStore" && c.kind == "struct"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "SearchMode" && c.kind == "enum"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "Store" && c.kind == "trait"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "Store.upsert" && c.kind == "method"));
    }

    #[test]
    fn test_chunk_impl_methods_are_qualified() {
        let code = r#"
impl EdgeStore {
    pub fn init(&self) {}
    async fn search(&self) {}
    const DEFAULT_LIMIT: usize = 10;
    type Row = String;
}
"#;
        let chunker = RustChunker;
        let chunks = chunker.chunk("src/lib.rs", code).unwrap();
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "EdgeStore.init" && c.kind == "method"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "EdgeStore.search" && c.kind == "method"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "EdgeStore.DEFAULT_LIMIT" && c.kind == "constant"));
        assert!(chunks
            .iter()
            .any(|c| c.symbol == "EdgeStore.Row" && c.kind == "type"));
    }

    #[test]
    fn test_extract_calls_from_function_and_impl_method() {
        let code = r#"
fn run() {
    helper();
    service.search();
    crate::store::open();
}

impl EdgeStore {
    fn init(&self) {
        self.open();
        build_embedder();
    }
}

mod nested {
    fn inside() {
        helper();
    }
}
"#;
        let calls = extract_calls(code).unwrap();
        assert!(calls.contains(&("run".to_string(), "helper".to_string())));
        assert!(calls.contains(&("run".to_string(), "search".to_string())));
        assert!(calls.contains(&("run".to_string(), "open".to_string())));
        assert!(calls.contains(&("EdgeStore.init".to_string(), "open".to_string())));
        assert!(calls.contains(&("EdgeStore.init".to_string(), "build_embedder".to_string())));
        assert!(calls.contains(&("nested::inside".to_string(), "helper".to_string())));
    }

    #[test]
    fn test_file_fallback_chunk_is_file_kind() {
        let code = "use std::path::Path;\n";
        let chunker = RustChunker;
        let chunks = chunker.chunk("src/lib.rs", code).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "file");
        assert_eq!(chunks[0].symbol, "lib.rs");
        assert_eq!(chunks[0].language, "rust");
    }

    #[test]
    fn test_long_rust_chunk_splits_with_part_suffix() {
        let repeated = "    let value = \"abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\";\n".repeat(200);
        let code = format!("fn huge() {{\n{repeated}}}\n");
        let chunker = RustChunker;
        let chunks = chunker.chunk("src/lib.rs", &code).unwrap();
        assert!(chunks.iter().any(|c| c.symbol == "huge_p1"));
        assert!(chunks.iter().any(|c| c.symbol == "huge_p2"));
        assert!(chunks.iter().all(|c| c.language == "rust"));
    }
}

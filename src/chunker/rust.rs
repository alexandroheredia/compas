#![allow(clippy::missing_transmute_annotations)]

use crate::chunker::Chunker;
use crate::models::Chunk;
use anyhow::Result;
use tree_sitter::{Node, Parser};
use uuid::Uuid;

const MAX_CHUNK_CHARS: usize = 6000;

pub struct RustChunker;

impl Chunker for RustChunker {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn chunk(&self, file_path: &str, content: &str) -> Result<Vec<Chunk>> {
        let mut parser = init_parser()?;
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        let root = tree.root_node();
        let mut chunks = vec![];

        walk_items(root, content, file_path, None, &mut chunks);

        if chunks.is_empty() {
            let lines: Vec<&str> = content.lines().collect();
            chunks.push(Chunk {
                id: Uuid::new_v4().to_string(),
                content: truncate_content(content, MAX_CHUNK_CHARS),
                language: "rust".into(),
                file_path: file_path.into(),
                symbol: path_filename(file_path),
                line_start: 1,
                line_end: lines.len().max(1),
                kind: "file".into(),
                meta: Default::default(),
            });
        }

        Ok(chunks)
    }
}

fn init_parser() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = unsafe {
        tree_sitter::Language::from_raw(std::mem::transmute::<
            _,
            unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage,
        >(tree_sitter_rust::LANGUAGE.into_raw())())
    };
    parser.set_language(&language)?;
    Ok(parser)
}

fn walk_items(
    node: Node,
    content: &str,
    file_path: &str,
    module_prefix: Option<&str>,
    chunks: &mut Vec<Chunk>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }

        match child.kind() {
            "function_item" => {
                push_named_item(content, file_path, child, module_prefix, "function", chunks)
            }
            "struct_item" => {
                push_named_item(content, file_path, child, module_prefix, "struct", chunks)
            }
            "enum_item" => {
                push_named_item(content, file_path, child, module_prefix, "enum", chunks)
            }
            "const_item" => {
                push_named_item(content, file_path, child, module_prefix, "constant", chunks)
            }
            "static_item" => {
                push_named_item(content, file_path, child, module_prefix, "static", chunks)
            }
            "type_item" => {
                push_named_item(content, file_path, child, module_prefix, "type", chunks)
            }
            "macro_definition" => {
                push_named_item(content, file_path, child, module_prefix, "macro", chunks)
            }
            "trait_item" => extract_trait_item(child, content, file_path, module_prefix, chunks),
            "impl_item" => extract_impl_item(child, content, file_path, module_prefix, chunks),
            "mod_item" => extract_mod_item(child, content, file_path, module_prefix, chunks),
            _ => {}
        }
    }
}

fn push_named_item(
    content: &str,
    file_path: &str,
    node: Node,
    module_prefix: Option<&str>,
    kind: &str,
    chunks: &mut Vec<Chunk>,
) {
    let Some(name) = extract_item_name(node, content) else {
        return;
    };
    let symbol = apply_module_prefix(module_prefix, &name);
    push_chunk(
        node_text(node, content),
        content,
        file_path,
        &symbol,
        kind,
        node.start_byte(),
        node.end_byte(),
        chunks,
    );
}

fn extract_trait_item(
    node: Node,
    content: &str,
    file_path: &str,
    module_prefix: Option<&str>,
    chunks: &mut Vec<Chunk>,
) {
    let Some(name) = extract_item_name(node, content) else {
        return;
    };
    let symbol = apply_module_prefix(module_prefix, &name);
    push_chunk(
        node_text(node, content),
        content,
        file_path,
        &symbol,
        "trait",
        node.start_byte(),
        node.end_byte(),
        chunks,
    );

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "declaration_list" {
            continue;
        }
        let mut decl_cursor = child.walk();
        for decl in child.children(&mut decl_cursor) {
            if decl.kind() != "function_signature_item" {
                continue;
            }
            let Some(method_name) = extract_item_name(decl, content) else {
                continue;
            };
            let qualified = format!("{}.{}", symbol, method_name);
            push_chunk(
                node_text(decl, content),
                content,
                file_path,
                &qualified,
                "method",
                decl.start_byte(),
                decl.end_byte(),
                chunks,
            );
        }
    }
}

fn extract_impl_item(
    node: Node,
    content: &str,
    file_path: &str,
    module_prefix: Option<&str>,
    chunks: &mut Vec<Chunk>,
) {
    let Some(type_name) = extract_impl_type(node, content) else {
        return;
    };
    let impl_symbol = apply_module_prefix(module_prefix, &format!("impl {}", type_name));
    push_chunk(
        node_text(node, content),
        content,
        file_path,
        &impl_symbol,
        "impl",
        node.start_byte(),
        node.end_byte(),
        chunks,
    );

    let type_symbol = apply_module_prefix(module_prefix, &type_name);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "declaration_list" {
            continue;
        }
        let mut decl_cursor = child.walk();
        for decl in child.children(&mut decl_cursor) {
            match decl.kind() {
                "function_item" => {
                    let Some(method_name) = extract_item_name(decl, content) else {
                        continue;
                    };
                    let qualified = format!("{}.{}", type_symbol, method_name);
                    push_chunk(
                        node_text(decl, content),
                        content,
                        file_path,
                        &qualified,
                        "method",
                        decl.start_byte(),
                        decl.end_byte(),
                        chunks,
                    );
                }
                "const_item" => {
                    let Some(const_name) = extract_item_name(decl, content) else {
                        continue;
                    };
                    let qualified = format!("{}.{}", type_symbol, const_name);
                    push_chunk(
                        node_text(decl, content),
                        content,
                        file_path,
                        &qualified,
                        "constant",
                        decl.start_byte(),
                        decl.end_byte(),
                        chunks,
                    );
                }
                "type_item" => {
                    let Some(type_alias_name) = extract_item_name(decl, content) else {
                        continue;
                    };
                    let qualified = format!("{}.{}", type_symbol, type_alias_name);
                    push_chunk(
                        node_text(decl, content),
                        content,
                        file_path,
                        &qualified,
                        "type",
                        decl.start_byte(),
                        decl.end_byte(),
                        chunks,
                    );
                }
                _ => {}
            }
        }
    }
}

fn extract_mod_item(
    node: Node,
    content: &str,
    file_path: &str,
    module_prefix: Option<&str>,
    chunks: &mut Vec<Chunk>,
) {
    let Some(name) = extract_item_name(node, content) else {
        return;
    };
    let symbol = apply_module_prefix(module_prefix, &name);
    push_chunk(
        node_text(node, content),
        content,
        file_path,
        &symbol,
        "module",
        node.start_byte(),
        node.end_byte(),
        chunks,
    );

    if let Some(body) = find_child(node, "declaration_list") {
        let next_prefix = format!("{}::", symbol);
        walk_items(body, content, file_path, Some(&next_prefix), chunks);
    }
}

fn apply_module_prefix(module_prefix: Option<&str>, name: &str) -> String {
    match module_prefix {
        Some(prefix) => format!("{}{}", prefix, name),
        None => name.to_string(),
    }
}

fn extract_item_name(node: Node, content: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(compact_node_text(name, content));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "type_identifier" | "field_identifier" => {
                return Some(compact_node_text(child, content));
            }
            _ => {}
        }
    }
    None
}

fn extract_impl_type(node: Node, content: &str) -> Option<String> {
    let mut saw_for = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            if node_text(child, content).trim() == "for" {
                saw_for = true;
            }
            continue;
        }

        if saw_for {
            return Some(compact_node_text(child, content));
        }

        match child.kind() {
            "type_identifier"
            | "scoped_type_identifier"
            | "generic_type"
            | "reference_type"
            | "primitive_type" => {
                return Some(compact_node_text(child, content));
            }
            _ => {}
        }
    }
    None
}

pub fn extract_doc_comments(content: &str, start_byte: usize) -> String {
    let prefix = &content[..start_byte.min(content.len())];
    let lines: Vec<&str> = prefix.lines().collect();

    let mut comments = Vec::new();
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with("///") || trimmed.starts_with("//!") {
            comments.push(trimmed);
        } else if !trimmed.is_empty() {
            break;
        }
    }

    comments.reverse();
    comments.join("\n")
}

pub fn extract_calls(content: &str) -> Result<Vec<(String, String)>> {
    let mut parser = init_parser()?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let root = tree.root_node();
    let mut calls = vec![];

    collect_calls_in_items(root, content, None, &mut calls);

    Ok(calls)
}

fn collect_calls_in_items(
    node: Node,
    content: &str,
    module_prefix: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }

        match child.kind() {
            "function_item" => {
                let Some(name) = extract_item_name(child, content) else {
                    continue;
                };
                let caller = apply_module_prefix(module_prefix, &name);
                collect_calls_for_function(child, content, &caller, out);
            }
            "impl_item" => collect_calls_in_impl(child, content, module_prefix, out),
            "mod_item" => {
                let Some(name) = extract_item_name(child, content) else {
                    continue;
                };
                if let Some(body) = find_child(child, "declaration_list") {
                    let next_prefix = apply_module_prefix(module_prefix, &format!("{}::", name));
                    collect_calls_in_items(body, content, Some(&next_prefix), out);
                }
            }
            _ => {}
        }
    }
}

fn collect_calls_in_impl(
    node: Node,
    content: &str,
    module_prefix: Option<&str>,
    out: &mut Vec<(String, String)>,
) {
    let Some(type_name) = extract_impl_type(node, content) else {
        return;
    };
    let type_symbol = apply_module_prefix(module_prefix, &type_name);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "declaration_list" {
            continue;
        }
        let mut decl_cursor = child.walk();
        for decl in child.children(&mut decl_cursor) {
            if decl.kind() != "function_item" {
                continue;
            }
            let Some(method_name) = extract_item_name(decl, content) else {
                continue;
            };
            let caller = format!("{}.{}", type_symbol, method_name);
            collect_calls_for_function(decl, content, &caller, out);
        }
    }
}

fn collect_calls_for_function(
    function_node: Node,
    content: &str,
    caller: &str,
    out: &mut Vec<(String, String)>,
) {
    let body = function_node
        .child_by_field_name("body")
        .or_else(|| find_child(function_node, "block"));
    let Some(body) = body else {
        return;
    };

    let mut callees = Vec::new();
    collect_callees_in_scope(body, content, &mut callees);
    for callee in callees {
        out.push((caller.to_string(), callee));
    }
}

fn collect_callees_in_scope(node: Node, content: &str, out: &mut Vec<String>) {
    match node.kind() {
        "call_expression" => {
            if let Some(function) = node.child_by_field_name("function") {
                let callee = final_callee_segment(&compact_node_text(function, content));
                push_unique(out, &callee);
            }
        }
        "method_call_expression" => {
            if let Some(name) = node.child_by_field_name("name") {
                let callee = compact_node_text(name, content);
                push_unique(out, &callee);
            } else if let Some(last) = last_identifier_text(node, content) {
                push_unique(out, &last);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        if matches!(
            child.kind(),
            "function_item"
                | "closure_expression"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
        ) {
            continue;
        }
        collect_callees_in_scope(child, content, out);
    }
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !out.iter().any(|existing| existing == value) {
        out.push(value.to_string());
    }
}

fn final_callee_segment(text: &str) -> String {
    text.rsplit("::")
        .next()
        .unwrap_or(text)
        .rsplit('.')
        .next()
        .unwrap_or(text)
        .trim()
        .to_string()
}

fn last_identifier_text(node: Node, content: &str) -> Option<String> {
    let mut last = None;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "identifier" | "field_identifier") {
            last = Some(compact_node_text(current, content));
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            if child.is_named() {
                stack.push(child);
            }
        }
    }
    last
}

fn compact_node_text(node: Node, content: &str) -> String {
    content[node.start_byte()..node.end_byte()]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn node_text(node: Node, content: &str) -> String {
    content[node.start_byte()..node.end_byte()].to_string()
}

fn path_filename(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string())
}

#[allow(clippy::manual_find)]
fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn push_chunk(
    text: String,
    content: &str,
    file_path: &str,
    symbol: &str,
    kind: &str,
    start_byte: usize,
    end_byte: usize,
    chunks: &mut Vec<Chunk>,
) {
    let start_line = byte_to_line(content, start_byte);
    let end_line = byte_to_line(content, end_byte.min(content.len()));
    let rel_path = path_filename(file_path);
    let doc_comment = extract_doc_comments(content, start_byte);
    let enriched = if doc_comment.is_empty() {
        format!("{} {}\n{}", rel_path, symbol, text)
    } else {
        format!("{}\n{} {}\n{}", doc_comment, rel_path, symbol, text)
    };

    if enriched.len() <= MAX_CHUNK_CHARS {
        chunks.push(Chunk {
            id: Uuid::new_v4().to_string(),
            content: enriched,
            language: "rust".into(),
            file_path: file_path.into(),
            symbol: symbol.into(),
            line_start: start_line,
            line_end: end_line,
            kind: kind.into(),
            meta: Default::default(),
        });
        return;
    }

    let lines: Vec<&str> = enriched.lines().collect();
    let chunk_line_count = MAX_CHUNK_CHARS / 40;
    let mut offset = 0;
    let mut part = 1u32;
    while offset < lines.len() {
        let end = (offset + chunk_line_count).min(lines.len());
        let slice = lines[offset..end].join("\n");
        let clamped = truncate_content(&slice, MAX_CHUNK_CHARS);
        chunks.push(Chunk {
            id: Uuid::new_v4().to_string(),
            content: clamped,
            language: "rust".into(),
            file_path: file_path.into(),
            symbol: format!("{}_p{}", symbol, part),
            line_start: start_line + offset,
            line_end: start_line + end,
            kind: kind.into(),
            meta: Default::default(),
        });
        offset = end;
        part += 1;
    }
}

fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    let mut end = max_chars;
    while end < content.len() && !content[..end].ends_with('\n') {
        end += 1;
    }
    if end >= content.len() {
        end = content.len();
    }
    content[..end].to_string()
}

fn byte_to_line(content: &str, byte_pos: usize) -> usize {
    content[..byte_pos.min(content.len())]
        .lines()
        .count()
        .max(1)
}

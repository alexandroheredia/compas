#![allow(clippy::missing_transmute_annotations)]

use crate::chunker::Chunker;
use crate::models::Chunk;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::OnceLock;
use tree_sitter::{Node, Parser};
use uuid::Uuid;

const MAX_CHUNK_CHARS: usize = 6000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticReferenceAnalysis {
    pub references: Vec<(String, String)>,
    pub key_types: Vec<String>,
    pub import_uris: Vec<String>,
}

pub struct DartChunker;

impl Chunker for DartChunker {
    fn language(&self) -> &'static str {
        "dart"
    }

    fn chunk(&self, file_path: &str, content: &str) -> Result<Vec<Chunk>> {
        let mut parser = Parser::new();
        let language = unsafe {
            tree_sitter::Language::from_raw(std::mem::transmute::<
                _,
                unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage,
            >(tree_sitter_dart::LANGUAGE.into_raw())())
        };
        parser.set_language(&language)?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        let root = tree.root_node();
        let mut chunks = vec![];

        walk_top_level(&root, content, file_path, &mut chunks);

        if chunks.is_empty() {
            let lines: Vec<&str> = content.lines().collect();
            chunks.push(Chunk {
                id: Uuid::new_v4().to_string(),
                content: truncate_content(content, MAX_CHUNK_CHARS),
                language: "dart".into(),
                file_path: file_path.into(),
                symbol: path_filename(file_path),
                line_start: 1,
                line_end: lines.len(),
                kind: "file".into(),
                meta: Default::default(),
            });
        }

        Ok(chunks)
    }
}

fn path_filename(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string())
}

fn init_parser() -> Result<Parser> {
    let mut parser = Parser::new();
    let language = unsafe {
        tree_sitter::Language::from_raw(std::mem::transmute::<
            _,
            unsafe extern "C" fn() -> *const tree_sitter::ffi::TSLanguage,
        >(tree_sitter_dart::LANGUAGE.into_raw())())
    };
    parser.set_language(&language)?;
    Ok(parser)
}

fn walk_top_level<'a>(root: &Node<'a>, content: &'a str, file_path: &str, chunks: &mut Vec<Chunk>) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "class_definition"
            | "class_declaration"
            | "mixin_declaration"
            | "extension_declaration"
            | "enum_declaration" => {
                extract_class_members(child, content, file_path, chunks);
            }
            "function_signature" => {
                if let Some(body) = child.next_named_sibling() {
                    if body.kind() == "function_body" {
                        let combined = span_nodes(content, &[child, body]);
                        let name = extract_name(child, content).unwrap_or_else(|| "unknown".into());
                        push_chunk(
                            combined,
                            content,
                            file_path,
                            &name,
                            "function",
                            child.start_byte(),
                            body.end_byte(),
                            chunks,
                        );
                        continue;
                    }
                }
                let text = node_text(child, content);
                let name = extract_name(child, content).unwrap_or_else(|| "unknown".into());
                push_chunk(
                    text,
                    content,
                    file_path,
                    &name,
                    "function",
                    child.start_byte(),
                    child.end_byte(),
                    chunks,
                );
            }
            "function_body" => {
                // already handled with function_signature above
            }
            "getter_signature" | "setter_signature" => {
                let text = node_text(child, content);
                let name = extract_name(child, content).unwrap_or_else(|| "unknown".into());
                push_chunk(
                    text,
                    content,
                    file_path,
                    &name,
                    "getter_setter",
                    child.start_byte(),
                    child.end_byte(),
                    chunks,
                );
            }
            "variable_declaration"
            | "late_declaration"
            | "final_declaration"
            | "const_declaration"
            | "static_final_declaration_list"
            | "initialized_variable_declaration"
            | "top_level_variable_declaration" => {
                let name = extract_name_from_var(child, content)
                    .unwrap_or_else(|| path_filename(file_path));
                let text = node_text(child, content);
                push_chunk(
                    text,
                    content,
                    file_path,
                    &name,
                    "variable",
                    child.start_byte(),
                    child.end_byte(),
                    chunks,
                );
            }
            "import_directive" | "export_directive" | "part_directive" | "library_directive" => {}
            _ => {
                let text = node_text(child, content);
                if text.lines().count() > 2 {
                    let name = path_filename(file_path);
                    push_chunk(
                        text,
                        content,
                        file_path,
                        &name,
                        "declaration",
                        child.start_byte(),
                        child.end_byte(),
                        chunks,
                    );
                }
            }
        }
    }
}

fn extract_class_members<'a>(
    class_node: Node<'a>,
    content: &'a str,
    file_path: &str,
    chunks: &mut Vec<Chunk>,
) {
    let class_name = extract_name(class_node, content).unwrap_or_else(|| "AnonymousClass".into());

    let body = match find_child(&class_node, "class_body") {
        Some(b) => b,
        None => {
            let header_end = find_header_end(&class_node, content);
            let header = content[class_node.start_byte()..header_end].to_string();
            push_chunk(
                header,
                content,
                file_path,
                &class_name,
                "class",
                class_node.start_byte(),
                header_end,
                chunks,
            );
            return;
        }
    };

    let header_end = body.start_byte();
    let header = content[class_node.start_byte()..header_end.min(content.len())].to_string();
    let header_lines: Vec<&str> = header.lines().collect();
    let header_str = if header_lines.len() > 3 {
        header_lines[..3].join("\n") + "\n  // ...\n"
    } else {
        header.clone()
    };

    // Emit a standalone class chunk that includes fields and method signatures
    // for semantic richness. This ensures the class itself is searchable.
    let mut member_summaries = vec![];
    let mut body_cursor = body.walk();
    for member in body.children(&mut body_cursor) {
        if member.kind() != "class_member" {
            continue;
        }
        let mut member_cursor = member.walk();
        for mchild in member.children(&mut member_cursor) {
            match mchild.kind() {
                "field_declaration"
                | "variable_declaration"
                | "late_declaration"
                | "final_declaration"
                | "const_declaration" => {
                    member_summaries.push(node_text(mchild, content));
                }
                "declaration" => {
                    // declaration can wrap constructors too; skip those.
                    let mut is_constructor = false;
                    let mut decl_cursor = mchild.walk();
                    for decl_child in mchild.children(&mut decl_cursor) {
                        if decl_child.kind() == "constructor_signature"
                            || decl_child.kind() == "factory_constructor_signature"
                        {
                            is_constructor = true;
                            break;
                        }
                    }
                    if !is_constructor {
                        member_summaries.push(node_text(mchild, content));
                    }
                }
                "method_signature" | "getter_signature" | "setter_signature"
                | "function_signature" => {
                    // Include method signatures (without body) so the class chunk lists API surface
                    let sig_text = node_text(mchild, content);
                    // Truncate very long signatures to keep class chunk focused
                    let summary = if sig_text.len() > 200 {
                        sig_text[..200].to_string() + "..."
                    } else {
                        sig_text
                    };
                    member_summaries.push(summary);
                }
                _ => {}
            }
        }
    }
    let class_content = if member_summaries.is_empty() {
        header
    } else {
        format!("{}\n  {}", header, member_summaries.join("\n  "))
    };
    push_chunk(
        class_content,
        content,
        file_path,
        &class_name,
        "class",
        class_node.start_byte(),
        body.start_byte(),
        chunks,
    );

    let mut cursor = body.walk();
    for member in body.children(&mut cursor) {
        // Class members are wrapped in `class_member` nodes; extract the signature and body.
        if member.kind() != "class_member" {
            continue;
        }

        let mut sig_node: Option<Node> = None;
        let mut func_body: Option<Node> = None;
        let mut member_cursor = member.walk();
        for mchild in member.children(&mut member_cursor) {
            match mchild.kind() {
                "method_signature"
                | "constructor_signature"
                | "getter_signature"
                | "setter_signature"
                | "function_signature" => {
                    sig_node = Some(mchild);
                }
                "function_body" | "constructor_body" | "block" => {
                    func_body = Some(mchild);
                }
                _ => {}
            }
        }

        if let Some(sig) = sig_node {
            let (inner_sig, kind_label) = unwrap_method_signature(&sig);
            match kind_label {
                "method" => {
                    let method_name =
                        extract_name(inner_sig, content).unwrap_or_else(|| "unknown_method".into());
                    let full_method = func_body
                        .map(|body_node| span_nodes(content, &[sig, body_node]))
                        .unwrap_or_else(|| node_text(sig, content));

                    let qualified = format!("{}.{}", class_name, method_name);
                    push_chunk(
                        format!("{}\n{}", header_str, full_method),
                        content,
                        file_path,
                        &qualified,
                        "method",
                        sig.start_byte(),
                        func_body
                            .map(|b| b.end_byte())
                            .unwrap_or_else(|| sig.end_byte()),
                        chunks,
                    );
                }
                "constructor" | "factory_constructor" => {
                    let ctor_name = extract_ctor_name(inner_sig, content, &class_name);
                    let full_text = func_body
                        .map(|body_node| span_nodes(content, &[sig, body_node]))
                        .unwrap_or_else(|| node_text(sig, content));

                    let qualified = format!("{}.{}", class_name, ctor_name);
                    push_chunk(
                        format!("{}\n{}", header_str, full_text),
                        content,
                        file_path,
                        &qualified,
                        "constructor",
                        sig.start_byte(),
                        func_body
                            .map(|b| b.end_byte())
                            .unwrap_or_else(|| sig.end_byte()),
                        chunks,
                    );
                }
                "getter_setter" => {
                    let getter_name =
                        extract_name(inner_sig, content).unwrap_or_else(|| "getter".into());
                    let full_text = func_body
                        .map(|body_node| span_nodes(content, &[sig, body_node]))
                        .unwrap_or_else(|| node_text(sig, content));

                    let qualified = format!("{}.{}", class_name, getter_name);
                    push_chunk(
                        format!("{}\n{}", header_str, full_text),
                        content,
                        file_path,
                        &qualified,
                        "getter_setter",
                        sig.start_byte(),
                        func_body
                            .map(|b| b.end_byte())
                            .unwrap_or_else(|| sig.end_byte()),
                        chunks,
                    );
                }
                _ => {}
            }
        }

        // Fields may not have a separate signature node inside class_member
        let mut field_node: Option<Node> = None;
        let mut member_cursor2 = member.walk();
        for mchild in member.children(&mut member_cursor2) {
            match mchild.kind() {
                "field_declaration"
                | "variable_declaration"
                | "late_declaration"
                | "final_declaration"
                | "const_declaration" => {
                    field_node = Some(mchild);
                }
                _ => {}
            }
        }
        if let Some(field) = field_node {
            let field_name =
                extract_name_from_var(field, content).unwrap_or_else(|| "field".into());
            let qualified = format!("{}.{}", class_name, field_name);
            let text = node_text(field, content);
            push_chunk(
                format!("{}\n{}", header_str, text),
                content,
                file_path,
                &qualified,
                "field",
                field.start_byte(),
                field.end_byte(),
                chunks,
            );
        }
    }
}

/// Extract `///` doc comment lines that immediately precede `start_byte`.
/// Walks backward from the line before the declaration, collecting consecutive
/// `///` lines (skipping blank lines in between).
pub fn extract_doc_comments(content: &str, start_byte: usize) -> String {
    let prefix = &content[..start_byte.min(content.len())];
    let lines: Vec<&str> = prefix.lines().collect();

    let mut comments = Vec::new();
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with("///") {
            comments.push(trimmed);
        } else if !trimmed.is_empty() {
            break;
        }
    }

    comments.reverse();
    comments.join("\n")
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

/// Replace long string literals (>120 chars) with `'...'` so they don't dilute embeddings.
pub(crate) fn truncate_long_strings(text: &str) -> String {
    // Match Dart string literals: raw (r), multi-line (''' or """), and single-line (' or ")
    let re = regex::Regex::new(
        r#"r?(?:'''|""")[ -\x7f\s\S]*?(?:'''|""")|r?'(?:\\'|[^'])*'|r?"(?:\\"|[^"])*""#,
    )
    .unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let m = caps.get(0).unwrap().as_str();
        if m.len() > 120 {
            "'...'".to_string()
        } else {
            m.to_string()
        }
    })
    .into_owned()
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> String {
    content[node.start_byte()..node.end_byte()].to_string()
}

fn span_nodes(content: &str, nodes: &[Node]) -> String {
    if nodes.is_empty() {
        return String::new();
    }
    let start = nodes[0].start_byte();
    let end = nodes.last().unwrap().end_byte();
    content[start..end].to_string()
}

#[allow(clippy::manual_find)]
fn find_child<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn find_header_end(node: &Node, _content: &str) -> usize {
    let open_brace = find_child(node, "{");
    open_brace
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte())
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

    let rel_path = std::path::Path::new(file_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string());

    let doc_comment = extract_doc_comments(content, start_byte);

    // Truncate huge string literals so they don't dominate the embedding vector
    let text_truncated = truncate_long_strings(&text);

    let enriched = if doc_comment.is_empty() {
        format!("{} {}\n{}", rel_path, symbol, text_truncated)
    } else {
        format!(
            "{}\n{} {}\n{}",
            doc_comment, rel_path, symbol, text_truncated
        )
    };

    if enriched.len() <= MAX_CHUNK_CHARS {
        chunks.push(Chunk {
            id: Uuid::new_v4().to_string(),
            content: enriched,
            language: "dart".into(),
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
            language: "dart".into(),
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

fn byte_to_line(content: &str, byte_pos: usize) -> usize {
    content[..byte_pos.min(content.len())]
        .lines()
        .count()
        .max(1)
}

fn unwrap_method_signature<'a>(sig: &Node<'a>) -> (Node<'a>, &'static str) {
    let mut cursor = sig.walk();
    for child in sig.children(&mut cursor) {
        match child.kind() {
            "factory_constructor_signature" => return (child, "factory_constructor"),
            "constructor_signature" => return (child, "constructor"),
            "getter_signature" | "setter_signature" => return (child, "getter_setter"),
            _ => {}
        }
    }
    (*sig, "method")
}

fn extract_ctor_name(node: Node, content: &str, class_name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let name = content[child.start_byte()..child.end_byte()].to_string();
            parts.push(name);
        }
    }
    if parts.is_empty() {
        return class_name.to_string();
    }
    if parts.len() == 1 {
        if parts[0] == class_name {
            return class_name.to_string();
        }
        return parts.pop().unwrap();
    }
    parts
        .iter()
        .find(|p| *p != class_name)
        .cloned()
        .unwrap_or_else(|| class_name.to_string())
}

fn extract_name(node: Node, content: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(content[child.start_byte()..child.end_byte()].to_string());
        }
        if let Some(operator_name) = extract_operator_name(child, content) {
            return Some(operator_name);
        }
        // Recurse one level for wrapper nodes like method_signature > function_signature > identifier
        if child.kind() == "function_signature"
            || child.kind() == "method_signature"
            || child.kind() == "constructor_signature"
            || child.kind() == "factory_constructor_signature"
            || child.kind() == "getter_signature"
            || child.kind() == "setter_signature"
        {
            let mut inner = child.walk();
            for inner_child in child.children(&mut inner) {
                if inner_child.kind() == "identifier" {
                    return Some(
                        content[inner_child.start_byte()..inner_child.end_byte()].to_string(),
                    );
                }
                if let Some(operator_name) = extract_operator_name(inner_child, content) {
                    return Some(operator_name);
                }
            }
        }
    }
    None
}

fn extract_operator_name(node: Node, content: &str) -> Option<String> {
    static OPERATOR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let text = content[node.start_byte()..node.end_byte()].trim();
    if !text.contains("operator") {
        return None;
    }
    OPERATOR_RE
        .get_or_init(|| regex::Regex::new(r"\boperator\s+([^\s(]+)").unwrap())
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| format!("operator {}", m.as_str()))
}

fn extract_name_from_var(node: Node, content: &str) -> Option<String> {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "initialized_variable_definition"
            || child.kind() == "initialized_identifier"
            || child.kind() == "static_final_declaration"
            || child.kind() == "identifier"
        {
            return extract_name(child, content);
        }
        // Handle static final declarations: declaration > static_final_declaration_list > static_final_declaration > identifier
        if child.kind() == "static_final_declaration_list" {
            let mut list_cursor = child.walk();
            for list_child in child.children(&mut list_cursor) {
                if list_child.kind() == "static_final_declaration" {
                    return extract_name(list_child, content);
                }
            }
        }
        // Handle initialized identifier lists: declaration > initialized_identifier_list > initialized_identifier > identifier
        if child.kind() == "initialized_identifier_list" {
            let mut list_cursor = child.walk();
            for list_child in child.children(&mut list_cursor) {
                if list_child.kind() == "initialized_identifier" {
                    return extract_name(list_child, content);
                }
            }
        }
    }
    extract_name(node, content)
}

pub fn extract_calls(content: &str) -> Result<Vec<(String, String)>> {
    let mut parser = init_parser()?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let root = tree.root_node();
    let mut calls = vec![];

    // Walk top-level looking for function bodies
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_signature" => {
                if let Some(body) = child.next_named_sibling() {
                    if body.kind() == "function_body" || body.kind() == "block" {
                        let caller =
                            extract_name(child, content).unwrap_or_else(|| "unknown".into());
                        let mut callees = vec![];
                        collect_callees_in_scope(body, content, &mut callees);
                        for callee in callees {
                            calls.push((caller.clone(), callee));
                        }
                    }
                }
            }
            "variable_declaration"
            | "late_declaration"
            | "final_declaration"
            | "const_declaration"
            | "static_final_declaration_list"
            | "initialized_variable_declaration"
            | "top_level_variable_declaration" => {
                if let Some(name) = extract_name_from_var(child, content) {
                    let mut callees = vec![];
                    collect_callees_in_scope(child, content, &mut callees);
                    for callee in callees {
                        calls.push((name.clone(), callee));
                    }
                }
            }
            "class_declaration" | "mixin_declaration" | "extension_declaration" => {
                let class_name =
                    extract_name(child, content).unwrap_or_else(|| "AnonymousClass".into());
                if let Some(body) = find_child(&child, "class_body") {
                    let mut body_cursor = body.walk();
                    for member in body.children(&mut body_cursor) {
                        if member.kind() != "class_member" {
                            continue;
                        }
                        // Find the signature and body within class_member
                        let mut sig_node: Option<Node> = None;
                        let mut func_body: Option<Node> = None;
                        let mut member_cursor = member.walk();
                        for mchild in member.children(&mut member_cursor) {
                            match mchild.kind() {
                                "method_signature"
                                | "constructor_signature"
                                | "getter_signature"
                                | "setter_signature"
                                | "function_signature" => {
                                    sig_node = Some(mchild);
                                }
                                "function_body" | "constructor_body" => {
                                    func_body = Some(mchild);
                                }
                                _ => {}
                            }
                        }
                        if let (Some(sig), Some(body_node)) = (sig_node, func_body) {
                            let (inner_sig, _) = unwrap_method_signature(&sig);
                            let method_name = extract_name(inner_sig, content)
                                .unwrap_or_else(|| "unknown".into());
                            let qualified = format!("{}.{}", class_name, method_name);
                            let mut callees = vec![];
                            collect_callees_in_scope(body_node, content, &mut callees);
                            for callee in callees {
                                calls.push((qualified.clone(), callee));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(calls)
}

pub fn extract_semantic_references(content: &str) -> Result<SemanticReferenceAnalysis> {
    let mut parser = init_parser()?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let root = tree.root_node();
    let mut references = vec![];

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_signature" | "getter_signature" | "setter_signature" => {
                if let Some(body) = child.next_named_sibling() {
                    if matches!(body.kind(), "function_body" | "constructor_body" | "block") {
                        let caller =
                            extract_name(child, content).unwrap_or_else(|| "unknown".into());
                        for callee in collect_scoped_semantic_references(body, content) {
                            references.push((caller.clone(), callee));
                        }
                    }
                }
            }
            "variable_declaration"
            | "late_declaration"
            | "final_declaration"
            | "const_declaration"
            | "static_final_declaration_list"
            | "initialized_variable_declaration"
            | "top_level_variable_declaration" => {
                if let Some(name) = extract_name_from_var(child, content) {
                    for callee in collect_scoped_semantic_references(child, content) {
                        references.push((name.clone(), callee));
                    }
                }
            }
            "class_declaration" | "mixin_declaration" | "extension_declaration" => {
                let class_name =
                    extract_name(child, content).unwrap_or_else(|| "AnonymousClass".into());
                if let Some(body) = find_child(&child, "class_body") {
                    let mut body_cursor = body.walk();
                    for member in body.children(&mut body_cursor) {
                        if member.kind() != "class_member" {
                            continue;
                        }
                        let mut sig_node: Option<Node> = None;
                        let mut func_body: Option<Node> = None;
                        let mut member_cursor = member.walk();
                        for mchild in member.children(&mut member_cursor) {
                            match mchild.kind() {
                                "method_signature"
                                | "constructor_signature"
                                | "getter_signature"
                                | "setter_signature"
                                | "function_signature" => sig_node = Some(mchild),
                                "function_body" | "constructor_body" | "block" => {
                                    func_body = Some(mchild)
                                }
                                _ => {}
                            }
                        }
                        if let (Some(sig), Some(body_node)) = (sig_node, func_body) {
                            let (inner_sig, kind_label) = unwrap_method_signature(&sig);
                            let caller =
                                semantic_member_name(inner_sig, kind_label, &class_name, content);
                            for callee in collect_scoped_semantic_references(body_node, content) {
                                references.push((caller.clone(), callee));
                            }
                        }

                        let mut field_node: Option<Node> = None;
                        let mut member_cursor = member.walk();
                        for mchild in member.children(&mut member_cursor) {
                            match mchild.kind() {
                                "field_declaration"
                                | "variable_declaration"
                                | "late_declaration"
                                | "final_declaration"
                                | "const_declaration" => field_node = Some(mchild),
                                _ => {}
                            }
                        }
                        if let Some(field) = field_node {
                            if let Some(field_name) = extract_name_from_var(field, content) {
                                let caller = format!("{}.{}", class_name, field_name);
                                for callee in collect_scoped_semantic_references(field, content) {
                                    references.push((caller.clone(), callee));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(SemanticReferenceAnalysis {
        references,
        key_types: extract_key_types(content),
        import_uris: extract_import_uris(content),
    })
}

fn semantic_member_name(node: Node, kind_label: &str, class_name: &str, content: &str) -> String {
    match kind_label {
        "constructor" | "factory_constructor" => {
            format!(
                "{}.{}",
                class_name,
                extract_ctor_name(node, content, class_name)
            )
        }
        _ => {
            let name = extract_name(node, content).unwrap_or_else(|| "unknown".into());
            format!("{}.{}", class_name, name)
        }
    }
}

fn collect_scoped_semantic_references(node: Node, content: &str) -> Vec<String> {
    let mut refs = vec![];
    collect_semantic_references_in_scope(node, content, &mut refs);
    refs
}

fn collect_semantic_references_in_scope(node: Node, content: &str, out: &mut Vec<String>) {
    extract_property_read_from_node(node, content, out);
    extract_identifier_read_from_node(node, content, out);
    extract_call_from_node(node, content, out);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_kind = child.kind();
        if child_kind == "class_definition"
            || child_kind == "class_declaration"
            || child_kind == "mixin_declaration"
            || child_kind == "extension_declaration"
        {
            continue;
        }
        collect_semantic_references_in_scope(child, content, out);
    }
}

fn extract_property_read_from_node(node: Node, content: &str, out: &mut Vec<String>) {
    for (receiver, method_chain, has_call) in segment_call_chains(node, content) {
        if has_call || method_chain.is_empty() {
            continue;
        }
        let property_name = if let Some(recv) = receiver {
            if recv
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                format!("{}.{}", recv, method_chain.last().unwrap())
            } else {
                method_chain.last().unwrap().clone()
            }
        } else {
            method_chain.last().unwrap().clone()
        };
        push_unique_name(out, &property_name);
    }
}

fn extract_identifier_read_from_node(node: Node, content: &str, out: &mut Vec<String>) {
    if node.kind() != "identifier" || !should_capture_identifier_read(&node) {
        return;
    }

    let name = content[node.start_byte()..node.end_byte()].to_string();
    push_unique_name(out, &name);
}

fn should_capture_identifier_read(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    if matches!(
        parent.kind(),
        "function_signature"
            | "method_signature"
            | "constructor_signature"
            | "factory_constructor_signature"
            | "getter_signature"
            | "setter_signature"
            | "class_definition"
            | "class_declaration"
            | "mixin_declaration"
            | "extension_declaration"
            | "enum_declaration"
            | "type_identifier"
            | "type_arguments"
            | "type_parameter"
            | "formal_parameter"
            | "simple_formal_parameter"
            | "super_formal_parameter"
            | "field_formal_parameter"
            | "label"
            | "annotation"
            | "unconditional_assignable_selector"
            | "conditional_assignable_selector"
            | "import_directive"
            | "export_directive"
    ) {
        return false;
    }

    if matches!(
        parent.kind(),
        "initialized_variable_definition"
            | "initialized_identifier"
            | "static_final_declaration"
            | "named_argument"
    ) {
        return !is_first_named_child(node, &parent);
    }

    true
}

fn is_first_named_child(node: &Node, parent: &Node) -> bool {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.is_named() {
            return child.id() == node.id();
        }
    }
    false
}

fn push_unique_name(out: &mut Vec<String>, value: &str) {
    if !out.iter().any(|existing| existing == value) {
        out.push(value.to_string());
    }
}

fn extract_key_types(content: &str) -> Vec<String> {
    static KEY_TYPE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = KEY_TYPE_RE.get_or_init(|| {
        regex::Regex::new(
            r"\b(?:Map\s*<\s*([A-Za-z_][A-Za-z0-9_]*)\s*,|Set\s*<\s*([A-Za-z_][A-Za-z0-9_]*))",
        )
        .unwrap()
    });
    let mut key_types = HashSet::new();
    for caps in re.captures_iter(content) {
        if let Some(name) = caps.get(1).or_else(|| caps.get(2)) {
            key_types.insert(name.as_str().to_string());
        }
    }
    key_types.extend(extract_family_key_types(content));
    let mut key_types: Vec<String> = key_types.into_iter().collect();
    key_types.sort();
    key_types
}

fn extract_family_key_types(content: &str) -> HashSet<String> {
    static FAMILY_START_RE: OnceLock<regex::Regex> = OnceLock::new();
    static TYPE_NAME_RE: OnceLock<regex::Regex> = OnceLock::new();

    let family_re = FAMILY_START_RE.get_or_init(|| regex::Regex::new(r"\.family\s*<").unwrap());
    let type_name_re =
        TYPE_NAME_RE.get_or_init(|| regex::Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\??$").unwrap());

    let mut key_types = HashSet::new();
    for family_start in family_re.find_iter(content) {
        let start = family_start.end();
        let Some(end) = find_matching_angle_bracket(content, start) else {
            continue;
        };
        let generic_args = &content[start..end];
        let Some(last_arg) = split_top_level_last_arg(generic_args) else {
            continue;
        };
        if let Some(type_name) = type_name_re
            .captures(last_arg.trim())
            .and_then(|caps| caps.get(1))
        {
            key_types.insert(type_name.as_str().to_string());
        }
    }

    key_types
}

fn find_matching_angle_bracket(content: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, ch) in content[start..].char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_last_arg(args: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut last_comma = None;
    for (offset, ch) in args.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => last_comma = Some(offset),
            _ => {}
        }
    }
    let comma = last_comma?;
    Some(args[comma + 1..].trim())
}

fn extract_import_uris(content: &str) -> Vec<String> {
    static URI_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = URI_RE.get_or_init(|| {
        regex::Regex::new(r#"(?m)^\s*(?:import|export|part)\s+['\"]([^'\"]+)['\"]"#).unwrap()
    });
    let mut uris = HashSet::new();
    for caps in re.captures_iter(content) {
        if let Some(uri) = caps.get(1) {
            let value = uri.as_str();
            if !value.starts_with("dart:") {
                uris.insert(value.to_string());
            }
        }
    }
    let mut uris: Vec<String> = uris.into_iter().collect();
    uris.sort();
    uris
}

/// Collect callee identifiers within a function/method body.
/// Does NOT recurse into nested function definitions.
fn collect_callees_in_scope(node: Node, content: &str, out: &mut Vec<String>) {
    // Extract calls from ANY node that contains the call pattern.
    // In Flutter, calls appear inside expression_statement, named_argument, conditional_expression,
    // and many other expression contexts. Rather than whitelist node kinds, we check every node.
    extract_call_from_node(node, content, out);

    // Recurse into children, but skip nested function/class definitions
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_kind = child.kind();
        // Skip nested scopes
        if child_kind == "class_definition"
            || child_kind == "mixin_declaration"
            || child_kind == "extension_declaration"
        {
            continue;
        }
        if (child_kind == "function_body"
            || child_kind == "constructor_body"
            || child_kind == "block")
            && is_nested_function_body(&child)
        {
            continue;
        }
        collect_callees_in_scope(child, content, out);
    }
}

/// Extract calls from a node by partitioning sibling children into individual
/// expression segments. Handles flat sequences like `list_literal` containing
/// `identifier selector(.foo) selector((args)) identifier selector(.bar) ...`
/// where multiple call expressions are siblings rather than nested.
fn extract_call_from_node(node: Node, content: &str, out: &mut Vec<String>) {
    for (receiver, method_chain, has_call) in segment_call_chains(node, content) {
        if !has_call {
            continue;
        }
        if let Some(recv) = receiver {
            if recv
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
                && !method_chain.is_empty()
            {
                let qualified = format!("{}.{}", recv, method_chain.last().unwrap());
                if !out.contains(&qualified) {
                    out.push(qualified);
                }
            } else if !method_chain.is_empty() {
                let method = method_chain.last().unwrap();
                if !out.contains(method) {
                    out.push(method.clone());
                }
            } else {
                if !out.contains(&recv) {
                    out.push(recv);
                }
            }
        } else if !method_chain.is_empty() {
            let method = method_chain.last().unwrap();
            if !out.contains(method) {
                out.push(method.clone());
            }
        }
    }
}

/// Walk a node's direct children and partition them into call/access "segments".
/// Each segment is `(receiver, method_chain, has_call)`:
///   - `receiver`: optional leading identifier (lowercase var or uppercase type).
///     `None` for chains starting with `this`/`super` or implicit receivers.
///   - `method_chain`: ordered identifiers from `.foo.bar.baz` selectors.
///   - `has_call`: true if any selector in the chain contained `argument_part`.
///
/// A new segment begins whenever a fresh `identifier`/`this`/`super` appears
/// after we have already consumed at least one selector or seen a previous
/// receiver. This correctly splits flat AST sequences such as the children
/// of `list_literal`, where multiple independent call expressions sit
/// side-by-side without an enclosing wrapper node.
fn segment_call_chains(node: Node, content: &str) -> Vec<(Option<String>, Vec<String>, bool)> {
    let mut segments: Vec<(Option<String>, Vec<String>, bool)> = Vec::new();
    let mut current: Option<(Option<String>, Vec<String>, bool)> = None;

    let flush = |cur: &mut Option<(Option<String>, Vec<String>, bool)>,
                 segs: &mut Vec<(Option<String>, Vec<String>, bool)>| {
        if let Some(seg) = cur.take() {
            if seg.0.is_some() || !seg.1.is_empty() {
                segs.push(seg);
            }
        }
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = content[child.start_byte()..child.end_byte()].to_string();
                if current
                    .as_ref()
                    .map(|s| s.0.is_some() || !s.1.is_empty())
                    .unwrap_or(false)
                {
                    flush(&mut current, &mut segments);
                }
                current = Some((Some(name), Vec::new(), false));
            }
            "this" | "super" => {
                if current
                    .as_ref()
                    .map(|s| s.0.is_some() || !s.1.is_empty())
                    .unwrap_or(false)
                {
                    flush(&mut current, &mut segments);
                }
                current = Some((None, Vec::new(), false));
            }
            "selector" => {
                let seg = current.get_or_insert_with(|| (None, Vec::new(), false));
                if has_argument_part(child) {
                    seg.2 = true;
                }
                let mut sel_cursor = child.walk();
                for sel_child in child.children(&mut sel_cursor) {
                    if sel_child.kind() == "unconditional_assignable_selector"
                        || sel_child.kind() == "conditional_assignable_selector"
                    {
                        let mut inner = sel_child.walk();
                        for inner_child in sel_child.children(&mut inner) {
                            if inner_child.kind() == "identifier" {
                                seg.1.push(
                                    content[inner_child.start_byte()..inner_child.end_byte()]
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
                if seg.2 {
                    flush(&mut current, &mut segments);
                }
            }
            "," => {
                flush(&mut current, &mut segments);
            }
            _ => {}
        }
    }
    flush(&mut current, &mut segments);
    segments
}

fn has_argument_part(node: Node) -> bool {
    if node.kind() != "selector" {
        return false;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "argument_part" || child.kind() == "arguments" {
            return true;
        }
        // Recurse one level for selector > argument_part
        if child.kind() == "selector" {
            let mut inner = child.walk();
            for inner_child in child.children(&mut inner) {
                if inner_child.kind() == "argument_part" || inner_child.kind() == "arguments" {
                    return true;
                }
            }
        }
    }
    false
}

#[allow(dead_code)]
fn extract_assignable_selector_name(node: Node, content: &str, out: &mut Vec<String>) {
    if node.kind() != "selector" {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "unconditional_assignable_selector" | "conditional_assignable_selector" => {
                let mut inner = child.walk();
                for inner_child in child.children(&mut inner) {
                    if inner_child.kind() == "identifier" {
                        let name =
                            content[inner_child.start_byte()..inner_child.end_byte()].to_string();
                        if !out.contains(&name) {
                            out.push(name);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_nested_function_body(body_node: &Node) -> bool {
    if let Some(prev) = body_node.prev_named_sibling() {
        let kind = prev.kind();
        if kind == "function_signature"
            || kind == "method_signature"
            || kind == "constructor_signature"
            || kind == "getter_signature"
            || kind == "setter_signature"
        {
            return true;
        }
    }
    false
}

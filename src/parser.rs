use crate::config::NolspConfig;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum ChunkSymbolKind {
    Header,
    Reference,
    Terminator,
}

pub struct ChunkSymbols {
    pub name: Option<String>,
    pub kind: ChunkSymbolKind,
    pub line_num: usize,
    pub start_col: usize,
    pub full_length: usize,
    /// Explicit language tag on a terminator line (e.g. `@ % (python)` → "python").
    pub language_tag: Option<String>,
}

pub fn parse_chunk_symbols(content: &str) -> Vec<ChunkSymbols> {
    let mut symbols = Vec::new();
    let mut inside_code_block = false;

    for (line_index, line) in content.lines().enumerate() {
        if line.trim_start().starts_with('@') {
            let start_col = line.find('@').unwrap();
            let language_tag = extract_language_tag(&line[start_col + 1..]);
            symbols.push(ChunkSymbols {
                name: None,
                kind: ChunkSymbolKind::Terminator,
                line_num: line_index,
                start_col,
                full_length: 1,
                language_tag,
            });
            inside_code_block = false;
        } else if !inside_code_block {
            if let (Some(start_idx), Some(end_offset)) = (line.find("<<"), line.find(">>=")) {
                let end_idx = start_idx + end_offset;
                let name = line[(start_idx + 2)..end_idx].to_string();
                let full_length = end_offset + 3;

                symbols.push(ChunkSymbols {
                    name: Some(name),
                    kind: ChunkSymbolKind::Header,
                    line_num: line_index,
                    start_col: start_idx,
                    full_length,
                    language_tag: None,
                });

                inside_code_block = true;
            }
        } else {
            // inside_code_block is true: check for chunk references (<<name>>)
            let mut search_from = 0;
            while let Some(start_rel) = line[search_from..].find("<<") {
                let start_idx = search_from + start_rel;
                if let Some(end_rel) = line[(start_idx + 2)..].find(">>") {
                    let end_idx = start_idx + 2 + end_rel;
                    // Ensure it is not a header definition inside a code block
                    if !line[end_idx..].starts_with(">>=") {
                        let name = line[(start_idx + 2)..end_idx].to_string();
                        let full_length = end_idx + 2 - start_idx;
                        symbols.push(ChunkSymbols {
                            name: Some(name),
                            kind: ChunkSymbolKind::Reference,
                            line_num: line_index,
                            start_col: start_idx,
                            full_length,
                            language_tag: None,
                        });
                    }
                    search_from = end_idx + 2;
                } else {
                    break;
                }
            }
        }
    }

    symbols
}

/// Returns the byte range `(start, end)` of a parenthesized language tag within `s`, if any.
/// A tag is a non-empty run of `[A-Za-z0-9_+#-]` enclosed in parentheses.
fn find_language_tag(s: &str) -> Option<(usize, usize)> {
    let start = s.find('(')?;
    let rest = &s[start + 1..];
    let rel_end = rest.find(')')?;
    let inner = &rest[..rel_end];
    let valid = !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '-'));
    if valid {
        Some((start, start + 1 + rel_end + 1))
    } else {
        None
    }
}

/// Extracts a parenthesized language tag from terminator trailing text.
/// e.g. `" % (python)"` → `Some("python")`, `" <!-- (py) -->"` → `Some("py")`.
pub fn extract_language_tag(s: &str) -> Option<String> {
    let (start, end) = find_language_tag(s)?;
    Some(s[start + 1..end - 1].to_ascii_lowercase())
}

#[derive(Debug, Default)]
pub struct ChunkDependencyGraph {
    /// Maps each chunk name to the line numbers where it is defined (as headers).
    pub definitions: HashMap<String, Vec<usize>>,
    /// Maps each chunk name to the set of chunk names it references.
    pub references: HashMap<String, HashSet<String>>,
    /// Inverse map: maps each chunk name to the set of parent chunk names that reference it ("used in").
    pub used_in: HashMap<String, HashSet<String>>,
    /// Maps a chunk name to an explicit language tag written on its terminator line.
    pub explicit_languages: HashMap<String, String>,
}

pub fn build_chunk_dependency_graph(symbols: &[ChunkSymbols]) -> ChunkDependencyGraph {
    let mut graph = ChunkDependencyGraph::default();
    let mut current_chunk: Option<String> = None;

    for symbol in symbols {
        match symbol.kind {
            ChunkSymbolKind::Header => {
                if let Some(name) = &symbol.name {
                    graph
                        .definitions
                        .entry(name.clone())
                        .or_default()
                        .push(symbol.line_num);
                    current_chunk = Some(name.clone());
                }
            }
            ChunkSymbolKind::Reference => {
                if let (Some(ref_name), Some(current)) = (&symbol.name, &current_chunk) {
                    graph
                        .references
                        .entry(current.clone())
                        .or_default()
                        .insert(ref_name.clone());
                    graph
                        .used_in
                        .entry(ref_name.clone())
                        .or_default()
                        .insert(current.clone());
                }
            }
            ChunkSymbolKind::Terminator => {
                if let (Some(name), Some(tag)) = (&current_chunk, &symbol.language_tag) {
                    graph.explicit_languages.insert(name.clone(), tag.clone());
                }
                current_chunk = None;
            }
        }
    }

    graph
}

#[allow(dead_code)]
pub fn get_reachable_chunks(
    graph: &ChunkDependencyGraph,
    root_chunks: &HashSet<String>,
) -> HashSet<String> {
    let mut reachable = HashSet::new();
    let mut queue = std::collections::VecDeque::new();

    for root in root_chunks {
        reachable.insert(root.clone());
        queue.push_back(root.clone());
    }

    while let Some(parent) = queue.pop_front() {
        if let Some(children) = graph.references.get(&parent) {
            for child in children {
                if reachable.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
    }

    reachable
}

/// Infers the language for each chunk defined in the document.
///
/// Rules:
/// 1. Root file chunks inherit their language from their file extension / canonical configuration.
/// 2. For each named chunk:
///    - Traces all root file chunks that transitively reference this chunk (via `used_in`).
///    - Collects the set of distinct canonical languages of those root file chunks.
///    - If all referencing roots resolve to the exact same canonical language (e.g. both
///      `main.c` and `header.h` resolve to `"c"`), the named chunk inherits that language.
///    - If referencing roots resolve to multiple distinct languages (e.g. `"c"` and `"python"`,
///      or `"c"` and `"cpp"`), the language association is discarded (treated as syntax-agnostic / plain text).
///    - If the named chunk is never referenced by any root chunk, it is plain text.
pub fn infer_chunk_languages(
    graph: &ChunkDependencyGraph,
    config: &NolspConfig,
) -> HashMap<String, String> {
    let mut chunk_languages = HashMap::new();

    // 1. Identify all root file chunks and their canonical languages
    let mut root_languages = HashMap::new();
    for name in graph.definitions.keys() {
        if let Some(lang) = config.resolve_language(name) {
            root_languages.insert(name.clone(), lang);
        }
    }

    // 2. Infer language for every defined chunk
    for name in graph.definitions.keys() {
        // If this chunk is itself a root file chunk:
        if let Some(lang) = root_languages.get(name) {
            chunk_languages.insert(name.clone(), lang.clone());
            continue;
        }

        // For named chunks: trace backwards through `used_in` to find all ancestor root chunks
        let mut referencing_root_languages = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = std::collections::VecDeque::new();

        queue.push_back(name.clone());
        visited.insert(name.clone());

        while let Some(current) = queue.pop_front() {
            if let Some(parents) = graph.used_in.get(&current) {
                for parent in parents {
                    if let Some(root_lang) = root_languages.get(parent) {
                        referencing_root_languages.insert(root_lang.clone());
                    }
                    if visited.insert(parent.clone()) {
                        queue.push_back(parent.clone());
                    }
                }
            }
        }

        // Apply compatibility policy:
        // - Exactly 1 distinct canonical language: compatible -> inherit
        // - More than 1 distinct canonical language: conflicting -> discard (plain text)
        // - 0 distinct canonical languages: unreferenced -> plain text
        if referencing_root_languages.len() == 1 {
            let lang = referencing_root_languages.into_iter().next().unwrap();
            chunk_languages.insert(name.clone(), lang);
        }
    }

    chunk_languages
}

/// Resolves each chunk's language, honoring explicit terminator-line tags first.
///
/// Explicit tags (e.g. `@ % (python)`) win over dependency-based inference; chunks
/// without a tag fall back to [`infer_chunk_languages`]. This is the source of truth
/// for editor highlighting (semantic tokens, hover, completion, and the
/// `nolsp/chunkLanguages` notification).
pub fn resolve_chunk_languages(
    graph: &ChunkDependencyGraph,
    config: &NolspConfig,
) -> HashMap<String, String> {
    let mut chunk_languages = infer_chunk_languages(graph, config);

    for (name, tag) in &graph.explicit_languages {
        if let Some(lang) = config.resolve_language(tag) {
            chunk_languages.insert(name.clone(), lang);
        }
    }

    chunk_languages
}

/// Returns the name of the code chunk enclosing the given line (0-indexed).
/// If `line` is outside any code chunk (e.g. in documentation/prose), returns None.
pub fn get_chunk_enclosing_line(symbols: &[ChunkSymbols], line: usize) -> Option<&str> {
    let mut current_chunk: Option<(&str, usize)> = None;

    for symbol in symbols {
        match symbol.kind {
            ChunkSymbolKind::Header => {
                if let Some(name) = &symbol.name {
                    current_chunk = Some((name.as_str(), symbol.line_num));
                }
            }
            ChunkSymbolKind::Terminator => {
                if let Some((name, start_line)) = current_chunk {
                    if line >= start_line && line <= symbol.line_num {
                        return Some(name);
                    }
                }
                current_chunk = None;
            }
            ChunkSymbolKind::Reference => {}
        }
    }

    if let Some((name, start_line)) = current_chunk {
        if line >= start_line {
            return Some(name);
        }
    }

    None
}

fn get_notangle_cmd() -> PathBuf {
    if let Ok(path) = std::env::var("NOTANGLE_PATH") {
        return PathBuf::from(path);
    }
    for candidate in &[
        "notangle",
        "/usr/local/noweb/notangle",
        "/usr/local/bin/notangle",
        "/usr/bin/notangle",
    ] {
        let p = Path::new(candidate);
        if p.is_absolute() && p.exists() {
            return p.to_path_buf();
        }
    }
    PathBuf::from("notangle")
}

/**
 * Handles creating the virtual directory where all the virtual documents reside for
 * use with language servers, extracting them using `notangle -L` to build line-mapping tables.
 */
pub async fn create_virtual_docs(
    symbols: &Vec<ChunkSymbols>,
    workspace_root: &Path,
    noweb_content: &str,
) -> Result<HashMap<PathBuf, Vec<(usize, isize)>>, std::io::Error> {
    let graph = build_chunk_dependency_graph(symbols);

    // Identify root file chunks: chunks that have a file extension
    let root_file_names: HashSet<String> = graph
        .definitions
        .keys()
        .filter(|s| Path::new(s).extension().is_some())
        .cloned()
        .collect();

    // Write noweb in-memory content to a temporary file in .nolsp/tmp/temp.nw
    let temp_dir = workspace_root.join(".nolsp").join("tmp");
    fs::create_dir_all(&temp_dir).await?;
    let temp_nw_file = temp_dir.join("temp.nw");
    fs::write(&temp_nw_file, noweb_content).await?;

    let noweb_lines: Vec<&str> = noweb_content.lines().collect();

    let mut mappings = HashMap::new();

    for file_name in root_file_names {
        // Making the virtual directory where all the virtual documents will live
        let target_path = workspace_root
            .join(".nolsp")
            .join("virtual")
            .join(&file_name);

        // Create the virtual directory path if not existing
        if let Some(parent_dir) = target_path.parent() {
            fs::create_dir_all(parent_dir).await?;
        }

        let notangle_bin = get_notangle_cmd();
        let notangle_str = notangle_bin.to_string_lossy();
        let mut cmd = if cfg!(windows)
            && (notangle_str.ends_with(".cmd") || notangle_str.ends_with(".bat"))
        {
            let mut c = Command::new("cmd");
            c.arg("/c").arg(&notangle_bin);
            c
        } else {
            Command::new(&notangle_bin)
        };

        let mut child = cmd
            .arg("-L")
            .arg(format!("-R{}", file_name))
            .arg(&temp_nw_file)
            .stdout(std::process::Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "Failed to capture notangle stdout",
            )
        })?;

        let mut clean_file = fs::File::create(&target_path).await?;
        let mut reader = BufReader::new(stdout).lines();

        let mut line_mapping = Vec::new();
        let mut current_original_line = 0;

        while let Some(line) = reader.next_line().await? {
            if line.starts_with("#line ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(num) = parts[1].parse::<usize>() {
                        current_original_line = num;
                    }
                }
            } else {
                clean_file.write_all(line.as_bytes()).await?;
                clean_file.write_all(b"\n").await?;

                let original_line = if current_original_line > 0 {
                    current_original_line - 1
                } else {
                    0
                };

                let mut col_offset = 0;
                if let Some(orig_line_str) = noweb_lines.get(original_line) {
                    if orig_line_str.trim() == line.trim() {
                        let w_virt = line.chars().take_while(|c| c.is_whitespace()).count();
                        let w_orig = orig_line_str
                            .chars()
                            .take_while(|c| c.is_whitespace())
                            .count();
                        col_offset = w_virt as isize - w_orig as isize;
                    }
                }

                line_mapping.push((original_line, col_offset));

                current_original_line += 1;
            }
        }

        let status = child.wait().await?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "notangle process failed with exit code: {:?}",
                status.code()
            )));
        }

        mappings.insert(target_path, line_mapping);
    }

    Ok(mappings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chunk_references() {
        let content = r#"
<<currencies>>=
currencies = ["botcoin"]
@

<<backend.py>>=
import codecs
<<currencies>>
@
"#;
        let symbols = parse_chunk_symbols(content);

        // Find reference symbol
        let ref_sym = symbols
            .iter()
            .find(|s| s.kind == ChunkSymbolKind::Reference);
        assert!(ref_sym.is_some());
        let ref_sym = ref_sym.unwrap();
        assert_eq!(ref_sym.name.as_deref(), Some("currencies"));
    }

    #[test]
    fn test_build_chunk_dependency_graph() {
        let content = r#"
<<credentials>>=
pool="mypool"
@

<<currencies>>=
currencies = ["botcoin"]
@

<<backend.py>>=
import codecs
<<currencies>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);

        assert!(graph.definitions.contains_key("credentials"));
        assert!(graph.definitions.contains_key("currencies"));
        assert!(graph.definitions.contains_key("backend.py"));

        // backend.py references currencies
        assert!(
            graph
                .references
                .get("backend.py")
                .unwrap()
                .contains("currencies")
        );
        // currencies is used in backend.py
        assert!(
            graph
                .used_in
                .get("currencies")
                .unwrap()
                .contains("backend.py")
        );

        // credentials is not referenced
        assert!(!graph.used_in.contains_key("credentials"));
    }

    #[test]
    fn test_get_reachable_chunks() {
        let content = r#"
<<orphan>>=
x = 1
@

<<leaf>>=
y = 2
@

<<middle>>=
<<leaf>>
@

<<app.py>>=
<<middle>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);

        let mut roots = HashSet::new();
        roots.insert("app.py".to_string());

        let reachable = get_reachable_chunks(&graph, &roots);
        assert!(reachable.contains("app.py"));
        assert!(reachable.contains("middle"));
        assert!(reachable.contains("leaf"));
        assert!(!reachable.contains("orphan"));
    }

    #[test]
    fn test_example_nw_dependency_and_reachability() {
        let example_path = std::path::Path::new("../testing/example.nw");
        if !example_path.exists() {
            return;
        }
        let content = std::fs::read_to_string(example_path).unwrap();
        let symbols = parse_chunk_symbols(&content);
        let graph = build_chunk_dependency_graph(&symbols);

        assert!(graph.definitions.contains_key("currencies"));
        assert!(graph.definitions.contains_key("credentials"));
        assert!(graph.definitions.contains_key("backend.py"));

        // currencies is referenced in backend.py
        assert!(
            graph
                .references
                .get("backend.py")
                .unwrap()
                .contains("currencies")
        );
        assert!(
            graph
                .used_in
                .get("currencies")
                .unwrap()
                .contains("backend.py")
        );

        // credentials is unreferenced
        assert!(!graph.used_in.contains_key("credentials"));

        let roots: HashSet<String> = graph
            .definitions
            .keys()
            .filter(|s| Path::new(s).extension().is_some())
            .cloned()
            .collect();

        assert!(roots.contains("backend.py"));
        assert!(roots.contains("hello.c"));

        let reachable = get_reachable_chunks(&graph, &roots);
        assert!(reachable.contains("currencies"));
        assert!(!reachable.contains("credentials"));
    }

    #[tokio::test]
    async fn test_create_virtual_docs_plain_text_policy() {
        let example_path = std::path::Path::new("../testing/example.nw");
        if !example_path.exists() {
            return;
        }
        let content = std::fs::read_to_string(example_path).unwrap();
        let symbols = parse_chunk_symbols(&content);

        let temp_dir = std::env::temp_dir().join("nolsp_test_virtual_docs");
        let mappings = create_virtual_docs(&symbols, &temp_dir, &content)
            .await
            .unwrap();

        // backend.py should exist in mappings
        let backend_path = temp_dir.join(".nolsp").join("virtual").join("backend.py");
        assert!(mappings.contains_key(&backend_path));

        // credentials (unreferenced named chunk) should NOT exist in mappings
        let credentials_path = temp_dir.join(".nolsp").join("virtual").join("credentials");
        assert!(!mappings.contains_key(&credentials_path));

        // currencies lines should be included in backend.py's mapping
        let backend_mappings = mappings.get(&backend_path).unwrap();
        // line 30 of example.nw (0-indexed: 29) is currencies = [...]
        let maps_currencies = backend_mappings
            .iter()
            .any(|&(orig_line, _)| orig_line == 29);
        assert!(
            maps_currencies,
            "currencies line (29) should be mapped in virtual/backend.py"
        );

        // Clean up temp dir
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[test]
    fn test_infer_chunk_languages_compatible_c_and_h() {
        let content = r#"
<<shared_buffer>>=
char buffer[1024];
@

<<main.c>>=
<<shared_buffer>>
int main() { return 0; }
@

<<header.h>>=
<<shared_buffer>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();
        let langs = infer_chunk_languages(&graph, &config);

        assert_eq!(langs.get("main.c").map(|s| s.as_str()), Some("c"));
        assert_eq!(langs.get("header.h").map(|s| s.as_str()), Some("c"));
        // Both main.c and header.h are "c", so shared_buffer is compatible and inherits "c"
        assert_eq!(langs.get("shared_buffer").map(|s| s.as_str()), Some("c"));
    }

    #[test]
    fn test_infer_chunk_languages_conflicting_c_and_python() {
        let content = r#"
<<polyglot>>=
/* or # comment */
@

<<main.c>>=
<<polyglot>>
@

<<script.py>>=
<<polyglot>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();
        let langs = infer_chunk_languages(&graph, &config);

        assert_eq!(langs.get("main.c").map(|s| s.as_str()), Some("c"));
        assert_eq!(langs.get("script.py").map(|s| s.as_str()), Some("python"));
        // polyglot is referenced by both C and Python -> conflicting, discarded as plain text!
        assert_eq!(langs.get("polyglot"), None);
    }

    #[test]
    fn test_infer_chunk_languages_conflicting_c_and_cpp() {
        let content = r#"
<<interop_struct>>=
struct Data { int x; };
@

<<main.c>>=
<<interop_struct>>
@

<<main.cpp>>=
<<interop_struct>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();
        let langs = infer_chunk_languages(&graph, &config);

        assert_eq!(langs.get("main.c").map(|s| s.as_str()), Some("c"));
        assert_eq!(langs.get("main.cpp").map(|s| s.as_str()), Some("cpp"));
        // C and C++ are distinct languages -> discarded as plain text!
        assert_eq!(langs.get("interop_struct"), None);
    }

    #[test]
    fn test_infer_chunk_languages_transitive_and_unreferenced() {
        let content = r#"
<<orphan>>=
int x = 1;
@

<<step2>>=
y = x + 1
@

<<step1>>=
x = 10
<<step2>>
@

<<backend.py>>=
<<step1>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();
        let langs = infer_chunk_languages(&graph, &config);

        assert_eq!(langs.get("backend.py").map(|s| s.as_str()), Some("python"));
        // Transitive propagation: backend.py -> step1 -> step2
        assert_eq!(langs.get("step1").map(|s| s.as_str()), Some("python"));
        assert_eq!(langs.get("step2").map(|s| s.as_str()), Some("python"));
        // orphan is unreferenced -> plain text (None)
        assert_eq!(langs.get("orphan"), None);
    }

    #[test]
    fn test_extract_language_tag() {
        assert_eq!(extract_language_tag(" % (python)"), Some("python".into()));
        assert_eq!(
            extract_language_tag(" <!-- (py) -->"),
            Some("py".into())
        );
        assert_eq!(extract_language_tag(" // (rust)"), Some("rust".into()));
        assert_eq!(extract_language_tag(" (c_sharp)"), Some("c_sharp".into()));
        // Uppercase normalized to lowercase
        assert_eq!(extract_language_tag(" % (Python)"), Some("python".into()));
        // No tag present
        assert_eq!(extract_language_tag(" just a comment"), None);
        assert_eq!(extract_language_tag(""), None);
        // Empty parens are not a tag
        assert_eq!(extract_language_tag(" ()"), None);
    }

    #[test]
    fn test_resolve_chunk_languages_explicit_tag_wins() {
        // `shared` is referenced by both C and Python (conflicting → inference discards it),
        // but an explicit terminator tag forces it to Python.
        let content = r#"
<<shared>>=
x = 1
@ % (python)

<<main.c>>=
<<shared>>
@

<<script.py>>=
<<shared>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();

        // Pure inference would discard `shared` (conflicting languages).
        let inferred = infer_chunk_languages(&graph, &config);
        assert_eq!(inferred.get("shared"), None);

        // Resolution honors the explicit tag.
        let resolved = resolve_chunk_languages(&graph, &config);
        assert_eq!(resolved.get("shared").map(|s| s.as_str()), Some("python"));
        assert_eq!(resolved.get("main.c").map(|s| s.as_str()), Some("c"));
        assert_eq!(
            resolved.get("script.py").map(|s| s.as_str()),
            Some("python")
        );
    }

    #[test]
    fn test_resolve_chunk_languages_falls_back_to_inference() {
        // No tags: resolution is identical to inference.
        let content = r#"
<<currencies>>=
currencies = []
@

<<backend.py>>=
<<currencies>>
@
"#;
        let symbols = parse_chunk_symbols(content);
        let graph = build_chunk_dependency_graph(&symbols);
        let config = NolspConfig::default().with_defaults();

        let resolved = resolve_chunk_languages(&graph, &config);
        assert_eq!(
            resolved.get("currencies").map(|s| s.as_str()),
            Some("python")
        );
    }

    #[test]
    fn test_parse_chunk_symbols_captures_terminator_tag() {
        let content = r#"
<<currencies>>=
currencies = []
@ % (python)
"#;
        let symbols = parse_chunk_symbols(content);
        let terminator = symbols
            .iter()
            .find(|s| s.kind == ChunkSymbolKind::Terminator)
            .unwrap();
        assert_eq!(terminator.language_tag.as_deref(), Some("python"));
    }
}

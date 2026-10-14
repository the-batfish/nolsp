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
}

pub fn parse_chunk_symbols(content: &str) -> Vec<ChunkSymbols> {
    let mut symbols = Vec::new();
    let mut inside_code_block = false;

    for (line_index, line) in content.lines().enumerate() {
        if line.trim_start().starts_with('@') {
            let start_col = line.find('@').unwrap();
            symbols.push(ChunkSymbols {
                name: None,
                kind: ChunkSymbolKind::Terminator,
                line_num: line_index,
                start_col,
                full_length: 1,
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

#[derive(Debug, Default)]
pub struct ChunkDependencyGraph {
    /// Maps each chunk name to the line numbers where it is defined (as headers).
    pub definitions: HashMap<String, Vec<usize>>,
    /// Maps each chunk name to the set of chunk names it references.
    pub references: HashMap<String, HashSet<String>>,
    /// Inverse map: maps each chunk name to the set of parent chunk names that reference it ("used in").
    pub used_in: HashMap<String, HashSet<String>>,
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
        let maps_currencies = backend_mappings.iter().any(|&(orig_line, _)| orig_line == 29);
        assert!(maps_currencies, "currencies line (29) should be mapped in virtual/backend.py");

        // Clean up temp dir
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}


use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[derive(PartialEq, Clone, Copy)]
pub enum ChunkSymbolKind {
    Header,
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
        }
    }

    symbols
}

/**
 * Handles creating the virtual directory where all the virtual documents reside for
 * use with language servers, extracting them using `notangle -L` to build line-mapping tables.
 */
pub async fn create_virtual_docs(
    symbols: &Vec<ChunkSymbols>,
    workspace_root: &Path,
    noweb_file: &Path,
) -> Result<HashMap<PathBuf, Vec<(usize, isize)>>, std::io::Error> {
    // Gather up all the chunk names and keep only the unique ones
    let mut unique_names = HashSet::<String>::new();

    for symbol in symbols {
        if let (ChunkSymbolKind::Header, Some(name)) = (symbol.kind, &symbol.name) {
            unique_names.insert(name.clone());
        }
    }

    // Get rid of all the chunk names that aren't file names
    let unique_file_names: HashSet<String> = unique_names
        .into_iter()
        .filter(|s| Path::new(s).extension().is_some())
        .collect();

    // Read noweb file to compare indentation
    let noweb_content = fs::read_to_string(noweb_file).await?;
    let noweb_lines: Vec<&str> = noweb_content.lines().collect();

    let mut mappings = HashMap::new();

    for file_name in unique_file_names {
        // Making the virtual directory where all the virtual documents will live
        let target_path = workspace_root
            .join(".nolsp")
            .join("virtual")
            .join(&file_name);

        // Create the virtual directory path if not existing
        if let Some(parent_dir) = target_path.parent() {
            fs::create_dir_all(parent_dir).await?;
        }

        let mut child = Command::new("notangle")
            .arg("-L")
            .arg(format!("-R{}", file_name))
            .arg(noweb_file)
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

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use tokio::fs;
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
 * use with language servers.
 */
pub async fn create_virtual_docs(
    symbols: &Vec<ChunkSymbols>,
    workspace_root: &Path,
    noweb_file: &Path,
) -> Result<(), std::io::Error> {
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

    // List of all the child processes
    let mut children = Vec::new();

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

        let output_file = File::create(&target_path)?;

        let virtual_doc_child = Command::new("notangle")
            .arg(format!("-R{}", file_name))
            .arg(noweb_file)
            .stdout(output_file)
            .spawn()?;

        children.push(virtual_doc_child);
    }

    // Wait for all child processes to complete
    for mut child in children {
        let status = child.wait().await?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "notangle process failed with exit code: {:?}",
                status.code()
            )));
        }
    }

    Ok(())
}

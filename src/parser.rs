
#[derive(PartialEq)]
pub enum ChunkSymbolKind {
    Header,
    Terminator,
}

pub struct ChunkSymbols{
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
            if let Some(start_idx) = line.find("<<") {
                if let Some(end_offset) = line.find(">>=") {
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
    }

    symbols
}

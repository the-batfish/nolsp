mod config;
mod parser;
mod sub_lsp;

use parser::{ChunkSymbolKind, ChunkSymbols, parse_chunk_symbols};

use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

fn get_semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::EVENT,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::MACRO,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::MODIFIER,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::REGEXP,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::DECORATOR,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
            SemanticTokenModifier::DEPRECATED,
            SemanticTokenModifier::ABSTRACT,
            SemanticTokenModifier::ASYNC,
            SemanticTokenModifier::MODIFICATION,
            SemanticTokenModifier::DOCUMENTATION,
            SemanticTokenModifier::DEFAULT_LIBRARY,
        ],
    }
}

pub enum ChunkLanguagesNotification {}

impl tower_lsp::lsp_types::notification::Notification for ChunkLanguagesNotification {
    const METHOD: &'static str = "nolsp/chunkLanguages";
    type Params = serde_json::Value;
}

struct Backend {
    client: Client,
    document_map: DashMap<Url, String>,
    workspace_root: std::sync::Mutex<Option<std::path::PathBuf>>,
    config: std::sync::RwLock<config::NolspConfig>,
    sub_lsp_manager: Arc<sub_lsp::SubLspManager>,
    client_capabilities: std::sync::RwLock<Option<ClientCapabilities>>,
}

fn translate_completion_item(
    item: &mut CompletionItem,
    sub_client: &sub_lsp::SubLspClient,
    doc_uri: &Url,
) {
    if let Some(text_edit) = &mut item.text_edit {
        match text_edit {
            CompletionTextEdit::Edit(edit) => {
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    edit.range.start.line as usize,
                    edit.range.start.character as usize,
                ) {
                    edit.range.start.line = orig_line as u32;
                    edit.range.start.character = orig_col as u32;
                }
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    edit.range.end.line as usize,
                    edit.range.end.character as usize,
                ) {
                    edit.range.end.line = orig_line as u32;
                    edit.range.end.character = orig_col as u32;
                }
            }
            CompletionTextEdit::InsertAndReplace(ir) => {
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    ir.insert.start.line as usize,
                    ir.insert.start.character as usize,
                ) {
                    ir.insert.start.line = orig_line as u32;
                    ir.insert.start.character = orig_col as u32;
                }
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    ir.insert.end.line as usize,
                    ir.insert.end.character as usize,
                ) {
                    ir.insert.end.line = orig_line as u32;
                    ir.insert.end.character = orig_col as u32;
                }
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    ir.replace.start.line as usize,
                    ir.replace.start.character as usize,
                ) {
                    ir.replace.start.line = orig_line as u32;
                    ir.replace.start.character = orig_col as u32;
                }
                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                    doc_uri,
                    ir.replace.end.line as usize,
                    ir.replace.end.character as usize,
                ) {
                    ir.replace.end.line = orig_line as u32;
                    ir.replace.end.character = orig_col as u32;
                }
            }
        }
    }

    if let Some(ref mut edits) = item.additional_text_edits {
        for edit in edits {
            if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                doc_uri,
                edit.range.start.line as usize,
                edit.range.start.character as usize,
            ) {
                edit.range.start.line = orig_line as u32;
                edit.range.start.character = orig_col as u32;
            }
            if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                doc_uri,
                edit.range.end.line as usize,
                edit.range.end.character as usize,
            ) {
                edit.range.end.line = orig_line as u32;
                edit.range.end.character = orig_col as u32;
            }
        }
    }

    let original_data = item.data.clone();
    item.data = Some(serde_json::json!({
        "sub_lsp_key": sub_client.ext,
        "doc_uri": doc_uri,
        "original_data": original_data,
    }));
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root_path = if let Some(folders) = &params.workspace_folders {
            if let Some(first_folder) = folders.first() {
                first_folder.uri.to_file_path().ok()
            } else {
                None
            }
        } else {
            params.root_uri.and_then(|uri| uri.to_file_path().ok())
        }
        .or_else(|| std::env::current_dir().ok());

        // Workspace root detection
        if let Some(path) = &root_path {
            let mut root = self.workspace_root.lock().unwrap();
            *root = Some(path.clone());
        }

        // Load config and store it in state
        let loaded_config = config::load_config(root_path.as_deref());
        {
            let mut cfg = self.config.write().unwrap();
            *cfg = loaded_config;
        }

        {
            let mut cap = self.client_capabilities.write().unwrap();
            *cap = Some(params.capabilities);
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),

                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(SaveOptions::default().into()),
                        ..Default::default()
                    },
                )),

                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: get_semantic_tokens_legend(),
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(true),
                    // Tell the editor to trigger completion on these common chars
                    trigger_characters: Some(vec![
                        "<".to_string(),
                        ".".to_string(),
                        ":".to_string(),
                        "-".to_string(),
                        ">".to_string(),
                        "/".to_string(),
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "nolsp server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        self.sub_lsp_manager.shutdown().await;

        let root_path = {
            let guard = self.workspace_root.lock().unwrap();
            guard.clone()
        };

        if let Some(root) = root_path {
            let virtual_dir = root.join(".nolsp");
            if virtual_dir.exists() {
                tokio::fs::remove_dir_all(virtual_dir).await.ok();
            }
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        self.document_map.insert(uri.clone(), text.clone());
        self.client
            .log_message(
                MessageType::INFO,
                "File has been opened and saved into memory!",
            )
            .await;

        let workspace_root = self.workspace_root.lock().unwrap().clone();
        let client = self.client.clone();
        let manager = self.sub_lsp_manager.clone();
        let config = self.config.read().unwrap().clone();
        let capabilities = self
            .client_capabilities
            .read()
            .unwrap()
            .clone()
            .unwrap_or_default();

        tokio::spawn(async move {
            if let (Some(root), Ok(_noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                let graph = parser::build_chunk_dependency_graph(&symbols);
                let chunk_languages = parser::infer_chunk_languages(&graph, &config);

                let _ = client
                    .send_notification::<ChunkLanguagesNotification>(serde_json::json!({
                        "uri": uri.to_string(),
                        "languages": chunk_languages,
                    }))
                    .await;

                match parser::create_virtual_docs(&symbols, &root, &text).await {
                    Err(e) => {
                        client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to create virtual docs: {}", e),
                            )
                            .await;
                    }
                    Ok(mappings) => {
                        client
                            .log_message(MessageType::INFO, "Virtual docs created successfully!")
                            .await;
                        manager
                            .sync_virtual_docs(
                                &symbols,
                                &root,
                                client,
                                &config,
                                capabilities,
                                mappings,
                                uri.clone(),
                                false,
                            )
                            .await;
                    }
                }
            }
        });
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if let Some(change) = params.content_changes.first() {
            let new_text = change.text.clone();
            self.document_map.insert(uri.clone(), new_text.clone());

            let workspace_root = self.workspace_root.lock().unwrap().clone();
            let client = self.client.clone();
            let manager = self.sub_lsp_manager.clone();
            let config = self.config.read().unwrap().clone();
            let capabilities = self
                .client_capabilities
                .read()
                .unwrap()
                .clone()
                .unwrap_or_default();

            tokio::spawn(async move {
                if let (Some(root), Ok(_noweb_file)) = (workspace_root, uri.to_file_path()) {
                    let symbols = parse_chunk_symbols(&new_text);
                    let graph = parser::build_chunk_dependency_graph(&symbols);
                    let chunk_languages = parser::infer_chunk_languages(&graph, &config);

                    let _ = client
                        .send_notification::<ChunkLanguagesNotification>(serde_json::json!({
                            "uri": uri.to_string(),
                            "languages": chunk_languages,
                        }))
                        .await;

                    match parser::create_virtual_docs(&symbols, &root, &new_text).await {
                        Err(e) => {
                            client
                                .log_message(
                                    MessageType::ERROR,
                                    format!("Failed to create virtual docs: {}", e),
                                )
                                .await;
                        }
                        Ok(mappings) => {
                            client
                                .log_message(
                                    MessageType::INFO,
                                    "Virtual docs updated successfully!",
                                )
                                .await;
                            manager
                                .sync_virtual_docs(
                                    &symbols,
                                    &root,
                                    client,
                                    &config,
                                    capabilities,
                                    mappings,
                                    uri.clone(),
                                    false,
                                )
                                .await;
                        }
                    }
                }
            });
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;

        // When saved to disk, read directly from disk or params.text to ensure we have the latest content
        let text = if let Some(t) = params.text {
            t
        } else if let Ok(path) = uri.to_file_path() {
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(_) => match self.document_map.get(&uri) {
                    Some(doc) => doc.clone(),
                    None => return,
                },
            }
        } else {
            match self.document_map.get(&uri) {
                Some(doc) => doc.clone(),
                None => return,
            }
        };

        self.document_map.insert(uri.clone(), text.clone());

        let workspace_root = self.workspace_root.lock().unwrap().clone();
        let client = self.client.clone();
        let manager = self.sub_lsp_manager.clone();
        let config = self.config.read().unwrap().clone();
        let capabilities = self
            .client_capabilities
            .read()
            .unwrap()
            .clone()
            .unwrap_or_default();

        tokio::spawn(async move {
            if let (Some(root), Ok(_noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                let graph = parser::build_chunk_dependency_graph(&symbols);
                let chunk_languages = parser::infer_chunk_languages(&graph, &config);

                let _ = client
                    .send_notification::<ChunkLanguagesNotification>(serde_json::json!({
                        "uri": uri.to_string(),
                        "languages": chunk_languages,
                    }))
                    .await;

                match parser::create_virtual_docs(&symbols, &root, &text).await {
                    Err(e) => {
                        client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to create virtual docs: {}", e),
                            )
                            .await;
                    }
                    Ok(mappings) => {
                        client
                            .log_message(MessageType::INFO, "Virtual docs updated successfully!")
                            .await;
                        manager
                            .sync_virtual_docs(
                                &symbols,
                                &root,
                                client,
                                &config,
                                capabilities,
                                mappings,
                                uri.clone(),
                                true,
                            )
                            .await;
                    }
                }
            }
        });
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let line = params.text_document_position_params.position.line;
        let character = params.text_document_position_params.position.character;
        let uri = &params.text_document_position_params.text_document.uri;

        let doc_text = match self.document_map.get(uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        let symbols = parse_chunk_symbols(&doc_text);
        let graph = parser::build_chunk_dependency_graph(&symbols);
        let config = self.config.read().unwrap().clone();
        let chunk_languages = parser::infer_chunk_languages(&graph, &config);

        // Check if hover is on a noweb chunk reference or header
        for symbol in &symbols {
            if symbol.line_num == line as usize {
                let start = symbol.start_col;
                let end = symbol.start_col + symbol.full_length;
                if (character as usize) >= start
                    && (character as usize) <= end
                    && let Some(name) = &symbol.name
                {
                    let mut info = format!("### Noweb Code Chunk: `{}`\n", name);
                    if let Some(lang) = chunk_languages.get(name) {
                        info.push_str(&format!("- **Language**: `{}`\n", lang));
                    } else {
                        info.push_str("- **Language**: *(plain text)*\n");
                    }
                    if let Some(defs) = graph.definitions.get(name) {
                        let lines: Vec<String> =
                            defs.iter().map(|l| format!("line {}", l + 1)).collect();
                        info.push_str(&format!("- **Defined at**: {}\n", lines.join(", ")));
                    }
                    if let Some(users) = graph.used_in.get(name) {
                        let mut user_list: Vec<String> = users.iter().cloned().collect();
                        user_list.sort();
                        info.push_str(&format!("- **Used in**: `{}`\n", user_list.join("`, `")));
                    } else {
                        info.push_str("- **Used in**: *(not referenced - plain text)*\n");
                    }
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: info,
                        }),
                        range: Some(Range {
                            start: Position {
                                line,
                                character: start as u32,
                            },
                            end: Position {
                                line,
                                character: end as u32,
                            },
                        }),
                    }));
                }
            }
        }

        // Code-level hover: check if line is inside an active language chunk
        let enclosing_chunk = parser::get_chunk_enclosing_line(&symbols, line as usize);
        let chunk_lang = match enclosing_chunk {
            Some(name) => match chunk_languages.get(name) {
                Some(lang) => Some(lang.as_str()),
                None => return Ok(None), // Discarded/plain-text chunk: do not forward to sub-LSP
            },
            None => return Ok(None), // Outside code chunks (prose): do not forward to sub-LSP
        };

        let virtual_pos = self.sub_lsp_manager.find_virtual_pos_for_lang(
            line as usize,
            character as usize,
            chunk_lang,
        );
        let (sub_client, doc_uri, virtual_line, virtual_char) = match virtual_pos {
            Some(pos) => pos,
            None => {
                return Ok(None);
            }
        };

        let hover_params = serde_json::json!({
            "textDocument": {
                "uri": doc_uri
            },
            "position": {
                "line": virtual_line,
                "character": virtual_char
            }
        });

        let response = sub_client
            .send_request("textDocument/hover", hover_params)
            .await
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e))?;

        if let Some(_error) = response.get("error") {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        }

        let result = response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if result.is_null() {
            return Ok(None);
        }

        let mut hover: Hover = serde_json::from_value(result)
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e.to_string()))?;

        if let Some(ref mut range) = hover.range {
            if let Some((orig_start_line, orig_start_col)) = sub_client.map_to_original(
                &doc_uri,
                range.start.line as usize,
                range.start.character as usize,
            ) {
                range.start.line = orig_start_line as u32;
                range.start.character = orig_start_col as u32;
            }
            if let Some((orig_end_line, orig_end_col)) = sub_client.map_to_original(
                &doc_uri,
                range.end.line as usize,
                range.end.character as usize,
            ) {
                range.end.line = orig_end_line as u32;
                range.end.character = orig_end_col as u32;
            }
        }

        Ok(Some(hover))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        crate::sub_lsp::log_debug(&format!("semantic_tokens_full called for uri: {}", uri));

        let text = match self.document_map.get(&uri) {
            Some(doc) => doc.clone(),
            None => {
                crate::sub_lsp::log_debug("Document not found in document_map");
                return Ok(None);
            }
        };

        let symbols: Vec<ChunkSymbols> = parse_chunk_symbols(&text);
        crate::sub_lsp::log_debug(&format!("Found {} noweb symbols", symbols.len()));

        let config = self.config.read().unwrap().clone();
        let graph = parser::build_chunk_dependency_graph(&symbols);
        let chunk_languages = parser::infer_chunk_languages(&graph, &config);

        struct AbsoluteToken {
            line: usize,
            col: usize,
            length: usize,
            token_type: u32,
            token_modifiers: u32,
        }

        let mut absolute_tokens = Vec::new();

        // 1. Gather noweb symbols
        for symbol in &symbols {
            let token_type = match symbol.kind {
                ChunkSymbolKind::Header => 14,     // MACRO
                ChunkSymbolKind::Reference => 14,  // MACRO
                ChunkSymbolKind::Terminator => 15, // KEYWORD
            };
            absolute_tokens.push(AbsoluteToken {
                line: symbol.line_num,
                col: symbol.start_col,
                length: symbol.full_length,
                token_type,
                token_modifiers: 0,
            });
        }

        // 2. Query and merge semantic tokens from relevant sub-LSPs
        let legend = get_semantic_tokens_legend();

        for entry in self.sub_lsp_manager.active_clients.iter() {
            let sub_client = entry.value();
            for doc_entry in sub_client.line_mappings.iter() {
                let virtual_uri = doc_entry.key();

                // Only query if this virtual file belongs to the current noweb file
                let belongs = sub_client
                    .virtual_to_original_uri
                    .get(virtual_uri)
                    .map(|orig| orig.value() == &uri)
                    .unwrap_or(false);

                if !belongs {
                    continue;
                }

                let has_legend = sub_client.legend.read().unwrap().is_some();
                if !has_legend {
                    continue;
                }

                // Send request to sub-LSP
                let sub_params = SemanticTokensParams {
                    partial_result_params: PartialResultParams::default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    text_document: TextDocumentIdentifier {
                        uri: virtual_uri.clone(),
                    },
                };

                let request_val = match serde_json::to_value(sub_params) {
                    Ok(val) => val,
                    Err(_) => continue,
                };

                let response_res = tokio::time::timeout(
                    std::time::Duration::from_millis(2000),
                    sub_client.send_request("textDocument/semanticTokens/full", request_val),
                )
                .await;

                if let Ok(Ok(response)) = response_res {
                    let sub_legend = sub_client.legend.read().unwrap();
                    let sub_legend_opt = sub_legend.as_ref();
                    if let Some(result) = response.get("result") {
                        if let Some(data_val) = result.get("data").and_then(|d| d.as_array()) {
                            // Decode delta-encoded tokens
                            let mut current_line = 0;
                            let mut current_col = 0;

                            for chunk in data_val.chunks_exact(5) {
                                let delta_line = chunk[0].as_u64().unwrap_or(0) as usize;
                                let delta_start = chunk[1].as_u64().unwrap_or(0) as usize;
                                let length = chunk[2].as_u64().unwrap_or(0) as usize;
                                let token_type = chunk[3].as_u64().unwrap_or(0) as usize;
                                let token_modifiers = chunk[4].as_u64().unwrap_or(0) as u32;

                                current_line += delta_line;
                                if delta_line > 0 {
                                    current_col = delta_start;
                                } else {
                                    current_col += delta_start;
                                }

                                // Map coordinates back to the original noweb document
                                if let Some((orig_line, orig_col)) = sub_client.map_to_original(
                                    virtual_uri,
                                    current_line,
                                    current_col,
                                ) {
                                    // Ensure token belongs to an active chunk matching this sub_client
                                    if let Some(enclosing) =
                                        parser::get_chunk_enclosing_line(&symbols, orig_line)
                                    {
                                        if let Some(chunk_lang) = chunk_languages.get(enclosing) {
                                            if sub_client.language_id != *chunk_lang
                                                && sub_client.ext != *chunk_lang
                                            {
                                                continue;
                                            }
                                        } else {
                                            // Discarded / plain text chunk: do not highlight with sub-LSP
                                            continue;
                                        }
                                    } else {
                                        continue;
                                    }

                                    // Map token type to the unified legend
                                    let mut mapped_type = 8; // fallback to VARIABLE (index 8)
                                    if let Some(sub_leg) = sub_legend_opt {
                                        if let Some(sub_type_name) =
                                            sub_leg.token_types.get(token_type)
                                        {
                                            if let Some(pos) = legend
                                                .token_types
                                                .iter()
                                                .position(|t| t == sub_type_name)
                                            {
                                                mapped_type = pos as u32;
                                            }
                                        }
                                    }

                                    // Map token modifiers
                                    let mut mapped_modifiers = 0;
                                    if let Some(sub_leg) = sub_legend_opt {
                                        for i in 0..32 {
                                            if (token_modifiers & (1 << i)) != 0 {
                                                if let Some(mod_name) =
                                                    sub_leg.token_modifiers.get(i)
                                                {
                                                    if let Some(pos) = legend
                                                        .token_modifiers
                                                        .iter()
                                                        .position(|m| m == mod_name)
                                                    {
                                                        mapped_modifiers |= 1 << pos;
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    absolute_tokens.push(AbsoluteToken {
                                        line: orig_line,
                                        col: orig_col,
                                        length,
                                        token_type: mapped_type,
                                        token_modifiers: mapped_modifiers,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. Sort tokens by line, then by column
        absolute_tokens.sort_by(|a, b| {
            if a.line != b.line {
                a.line.cmp(&b.line)
            } else {
                a.col.cmp(&b.col)
            }
        });

        // 4. Remove duplicate tokens at the same location
        absolute_tokens.dedup_by(|a, b| a.line == b.line && a.col == b.col);

        // 5. Delta-encode the tokens
        let mut delta_tokens = Vec::new();
        let mut prev_line = 0;
        let mut prev_col = 0;

        for token in absolute_tokens {
            let delta_line = (token.line - prev_line) as u32;
            let delta_start = if delta_line == 0 {
                (token.col - prev_col) as u32
            } else {
                token.col as u32
            };

            delta_tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length: token.length as u32,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers,
            });

            prev_line = token.line;
            prev_col = token.col;
        }

        crate::sub_lsp::log_debug(&format!(
            "semantic_tokens_full returning {} tokens",
            delta_tokens.len()
        ));

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: delta_tokens,
        })))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;

        let text = match self.document_map.get(&uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        let line = params.text_document_position.position.line;
        let character = params.text_document_position.position.character;

        let doc_lines: Vec<&str> = text.lines().collect();
        let current_line = doc_lines.get(line as usize).copied().unwrap_or("");

        let char_idx = character as usize;
        let is_noweb_trigger = if char_idx >= 2 {
            &current_line[char_idx.saturating_sub(2)..char_idx] == "<<"
        } else {
            false
        };

        if is_noweb_trigger {
            let symbols = parse_chunk_symbols(&text);
            let mut completion_items = Vec::new();
            let mut unique_names = HashSet::new();

            for symbol in symbols {
                if let (ChunkSymbolKind::Header, Some(name)) = (symbol.kind, symbol.name) {
                    if unique_names.insert(name.clone()) {
                        completion_items.push(CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::REFERENCE),
                            detail: Some("Noweb code chunk".to_string()),
                            insert_text: Some(format!("{}>>", name)),
                            ..Default::default()
                        });
                    }
                }
            }
            return Ok(Some(CompletionResponse::Array(completion_items)));
        }

        // Standard code chunk completion forwarding to sub-LSP
        let symbols = parse_chunk_symbols(&text);
        let graph = parser::build_chunk_dependency_graph(&symbols);
        let config = self.config.read().unwrap().clone();
        let chunk_languages = parser::infer_chunk_languages(&graph, &config);

        let enclosing_chunk = parser::get_chunk_enclosing_line(&symbols, line as usize);
        let chunk_lang = match enclosing_chunk {
            Some(name) => match chunk_languages.get(name) {
                Some(lang) => Some(lang.as_str()),
                None => return Ok(None), // Discarded/plain-text chunk: do not forward to sub-LSP
            },
            None => return Ok(None), // Outside code chunks (prose): do not forward to sub-LSP
        };

        let virtual_pos = self.sub_lsp_manager.find_virtual_pos_for_lang(
            line as usize,
            character as usize,
            chunk_lang,
        );
        let (sub_client, doc_uri, virtual_line, virtual_char) = match virtual_pos {
            Some(pos) => pos,
            None => {
                return Ok(None);
            }
        };

        let comp_params = serde_json::json!({
            "textDocument": {
                "uri": doc_uri
            },
            "position": {
                "line": virtual_line,
                "character": virtual_char
            },
            "context": params.context
        });

        let response = sub_client
            .send_request("textDocument/completion", comp_params)
            .await
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e))?;

        if let Some(_error) = response.get("error") {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        }

        let result = response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if result.is_null() {
            return Ok(None);
        }

        let mut comp_response: CompletionResponse = serde_json::from_value(result)
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e.to_string()))?;

        match comp_response {
            CompletionResponse::Array(ref mut items) => {
                for item in items {
                    translate_completion_item(item, &sub_client, &doc_uri);
                }
            }
            CompletionResponse::List(ref mut list) => {
                for item in &mut list.items {
                    translate_completion_item(item, &sub_client, &doc_uri);
                }
            }
        }

        Ok(Some(comp_response))
    }

    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        let metadata = match item.data.as_ref().and_then(|d| d.as_object()) {
            Some(obj) => obj,
            None => return Ok(item),
        };

        let sub_lsp_key = match metadata.get("sub_lsp_key").and_then(|v| v.as_str()) {
            Some(key) => key.to_string(),
            None => return Ok(item),
        };

        let doc_uri_str = match metadata.get("doc_uri").and_then(|v| v.as_str()) {
            Some(uri) => uri.to_string(),
            None => return Ok(item),
        };

        let doc_uri = match Url::parse(&doc_uri_str) {
            Ok(url) => url,
            Err(_) => return Ok(item),
        };

        let original_data = metadata
            .get("original_data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let sub_client = match self.sub_lsp_manager.active_clients.get(&sub_lsp_key) {
            Some(entry) => entry.value().clone(),
            None => return Ok(item),
        };

        // Restore original sub-LSP data payload
        item.data = if original_data.is_null() {
            None
        } else {
            Some(original_data)
        };

        let response = sub_client
            .send_request(
                "completionItem/resolve",
                serde_json::to_value(&item).unwrap(),
            )
            .await
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e))?;

        if let Some(_error) = response.get("error") {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        }

        let result = response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if result.is_null() {
            return Ok(item);
        }

        let mut resolved_item: CompletionItem = serde_json::from_value(result)
            .map_err(|e| tower_lsp::jsonrpc::Error::invalid_params(e.to_string()))?;

        translate_completion_item(&mut resolved_item, &sub_client, &doc_uri);

        Ok(resolved_item)
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        document_map: DashMap::new(),
        workspace_root: std::sync::Mutex::new(None),
        config: std::sync::RwLock::new(config::NolspConfig::default()),
        sub_lsp_manager: Arc::new(sub_lsp::SubLspManager::new()),
        client_capabilities: std::sync::RwLock::new(None),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod main_tests {
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_translate_completion_item() {
        let mut item = CompletionItem {
            label: "test".to_string(),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 10,
                    },
                    end: Position {
                        line: 0,
                        character: 15,
                    },
                },
                new_text: "replacement".to_string(),
            })),
            ..Default::default()
        };

        // Create a dummy SubLspClient
        let (stdin_tx, _) = tokio::sync::mpsc::channel(1);
        let process = Arc::new(tokio::sync::Mutex::new(
            tokio::process::Command::new("true").spawn().unwrap(),
        ));
        let line_mappings = DashMap::new();
        let doc_uri = Url::parse("file:///virtual/main.rs").unwrap();
        // Line 0 maps to line 10 in original noweb file, with offset 4
        line_mappings.insert(doc_uri.clone(), vec![(10, 4)]);

        let virtual_to_original_uri = DashMap::new();
        let diagnostics_cache = Arc::new(DashMap::new());

        let sub_client = sub_lsp::SubLspClient {
            ext: "rs".to_string(),
            language_id: "rust".to_string(),
            process,
            stdin_tx,
            pending_requests: DashMap::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
            opened_documents: DashMap::new(),
            line_mappings,
            virtual_to_original_uri,
            diagnostics_cache,
            legend: std::sync::RwLock::new(None),
        };

        translate_completion_item(&mut item, &sub_client, &doc_uri);

        // Verify edit coordinates mapped back correctly
        if let Some(CompletionTextEdit::Edit(edit)) = item.text_edit {
            assert_eq!(edit.range.start.line, 10);
            assert_eq!(edit.range.start.character, 6); // 10 - 4 = 6
            assert_eq!(edit.range.end.line, 10);
            assert_eq!(edit.range.end.character, 11); // 15 - 4 = 11
        } else {
            panic!("Expected edit");
        }

        // Verify data wrapped
        let data = item.data.unwrap();
        assert_eq!(data["sub_lsp_key"], "rs");
        assert_eq!(data["doc_uri"], "file:///virtual/main.rs");
    }

    #[test]
    fn test_example_nw_chunk_languages() {
        let example_path = std::path::Path::new("../testing/example.nw");
        if !example_path.exists() {
            return;
        }
        let content = std::fs::read_to_string(example_path).unwrap();
        let symbols = parse_chunk_symbols(&content);
        let graph = parser::build_chunk_dependency_graph(&symbols);
        let config = config::NolspConfig::default().with_defaults();
        let langs = parser::infer_chunk_languages(&graph, &config);

        // Referenced in backend.py:
        assert_eq!(langs.get("currencies").map(|s| s.as_str()), Some("python"));
        assert_eq!(
            langs.get("data file opening").map(|s| s.as_str()),
            Some("python")
        );

        // Unreferenced named chunk:
        assert_eq!(langs.get("credentials"), None);

        // Root file chunks:
        assert_eq!(langs.get("backend.py").map(|s| s.as_str()), Some("python"));
        assert_eq!(langs.get("hello.c").map(|s| s.as_str()), Some("c"));
    }
}

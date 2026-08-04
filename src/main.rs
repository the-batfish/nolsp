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
        token_types: vec![SemanticTokenType::MACRO, SemanticTokenType::KEYWORD],
        token_modifiers: vec![], // We won't use modifiers for now
    }
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
                    resolve_provider: Some(false),
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
            if let (Some(root), Ok(noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                match parser::create_virtual_docs(&symbols, &root, &noweb_file).await {
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
                if let (Some(root), Ok(noweb_file)) = (workspace_root, uri.to_file_path()) {
                    let symbols = parse_chunk_symbols(&new_text);
                    match parser::create_virtual_docs(&symbols, &root, &noweb_file).await {
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

        let text = match self.document_map.get(&uri) {
            Some(doc) => doc.clone(),
            None => return,
        };

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
            if let (Some(root), Ok(noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                match parser::create_virtual_docs(&symbols, &root, &noweb_file).await {
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

        let virtual_pos = self
            .sub_lsp_manager
            .find_virtual_pos(line as usize, character as usize);
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

        let text = match self.document_map.get(&uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        let symbols: Vec<ChunkSymbols> = parse_chunk_symbols(&text);

        let mut tokens = Vec::new();
        let mut previous_line = 0;
        let mut previous_col = 0;

        for symbol in symbols {
            let delta_line = (symbol.line_num - previous_line) as u32;
            let delta_start = if delta_line == 0 {
                (symbol.start_col - previous_col) as u32
            } else {
                symbol.start_col as u32
            };

            if symbol.kind == ChunkSymbolKind::Header {
                tokens.push(SemanticToken {
                    delta_line,
                    delta_start,
                    length: symbol.full_length as u32,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                });
            } else {
                tokens.push(SemanticToken {
                    delta_line,
                    delta_start,
                    length: symbol.full_length as u32,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                });
            }

            previous_line = symbol.line_num;
            previous_col = symbol.start_col;
        }
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
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
        let virtual_pos = self
            .sub_lsp_manager
            .find_virtual_pos(line as usize, character as usize);
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

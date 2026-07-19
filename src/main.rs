mod parser;

use parser::{ChunkSymbolKind, ChunkSymbols, parse_chunk_symbols};

use dashmap::DashMap;
use std::collections::HashSet;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

fn get_semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![SemanticTokenType::MACRO, SemanticTokenType::KEYWORD],
        token_modifiers: vec![], // We won't use modifiers for now
    }
}

#[derive(Debug)]
struct Backend {
    client: Client,

    document_map: DashMap<Url, String>,
    workspace_root: std::sync::Mutex<Option<std::path::PathBuf>>,
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
        }.or_else(|| std::env::current_dir().ok());

        if let Some(path) = root_path {
            let mut root = self.workspace_root.lock().unwrap();
            *root = Some(path);
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
                    }
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
                    // Tell the editor to trigger completion when the user types '<'
                    trigger_characters: Some(vec!["<<".to_string()]),
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
        tokio::spawn(async move {
            if let (Some(root), Ok(noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                if let Err(e) = parser::create_virtual_docs(&symbols, &root, &noweb_file).await {
                    client.log_message(MessageType::ERROR, format!("Failed to create virtual docs: {}", e)).await;
                } else {
                    client.log_message(MessageType::INFO, "Virtual docs created successfully!").await;
                }
            }
        });
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if let Some(change) = params.content_changes.first() {
            let new_text = change.text.clone();

            self.document_map.insert(uri, new_text);
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
        tokio::spawn(async move {
            if let (Some(root), Ok(noweb_file)) = (workspace_root, uri.to_file_path()) {
                let symbols = parse_chunk_symbols(&text);
                if let Err(e) = parser::create_virtual_docs(&symbols, &root, &noweb_file).await {
                    client.log_message(MessageType::ERROR, format!("Failed to create virtual docs: {}", e)).await;
                } else {
                    client.log_message(MessageType::INFO, "Virtual docs updated successfully!").await;
                }
            }
        });
    }

    async fn hover(&self, _params: HoverParams) -> Result<Option<Hover>> {
        Ok(Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(
                "Lorem ipsum type shi".to_string(),
            )),
            range: None,
        }))
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
        Ok(Some(CompletionResponse::Array(completion_items)))
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
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

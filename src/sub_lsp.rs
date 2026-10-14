use dashmap::DashMap;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tower_lsp::Client;
use tower_lsp::lsp_types::{
    ClientCapabilities, Diagnostic, MessageType, SemanticTokensLegend, Url,
};

pub type DiagnosticsCache = Arc<DashMap<Url, DashMap<Url, Vec<Diagnostic>>>>;

pub struct SubLspClient {
    pub ext: String,
    pub language_id: String,
    pub process: Arc<Mutex<Child>>,
    pub stdin_tx: mpsc::Sender<Value>,
    pub pending_requests: DashMap<Value, oneshot::Sender<Value>>,
    pub next_id: std::sync::atomic::AtomicU64,
    pub opened_documents: DashMap<Url, i32>,
    pub line_mappings: DashMap<Url, Vec<(usize, isize)>>,
    pub virtual_to_original_uri: DashMap<Url, Url>,
    pub diagnostics_cache: DiagnosticsCache,
    pub legend: std::sync::RwLock<Option<SemanticTokensLegend>>,
}

pub struct SubLspManager {
    pub active_clients: DashMap<String, Arc<SubLspClient>>,
    pub diagnostics_cache: DiagnosticsCache,
}

impl Default for SubLspManager {
    fn default() -> Self {
        Self {
            active_clients: DashMap::new(),
            diagnostics_cache: Arc::new(DashMap::new()),
        }
    }
}

async fn read_message<R>(reader: &mut BufReader<R>) -> std::io::Result<Option<serde_json::Value>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut content_length = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None); // EOF
        }

        if line == "\r\n" || line == "\n" {
            break;
        }

        let line_lower = line.to_lowercase();
        if line_lower.starts_with("content-length:") {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 2 {
                if let Ok(len) = parts[1].trim().parse::<usize>() {
                    content_length = Some(len);
                }
            }
        }
    }

    let len = match content_length {
        Some(l) => l,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Missing Content-Length header",
            ));
        }
    };

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;

    let json: serde_json::Value = serde_json::from_slice(&body)?;
    Ok(Some(json))
}

async fn write_message<W>(writer: &mut W, value: &serde_json::Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

impl SubLspClient {
    pub fn spawn(
        ext: String,
        language_id: String,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        client: Client,
        _root_uri: Url,
        _capabilities: ClientCapabilities,
        diagnostics_cache: DiagnosticsCache,
    ) -> Result<Arc<Self>, std::io::Error> {
        let mut cmd = if cfg!(windows) && (command.ends_with(".cmd") || command.ends_with(".bat")) {
            let mut c = Command::new("cmd");
            c.arg("/c").arg(command).args(args).envs(env);
            c
        } else {
            let mut c = Command::new(command);
            c.args(args).envs(env);
            c
        };

        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("Failed to open stdin");
        let stdout = child.stdout.take().expect("Failed to open stdout");
        let stderr = child.stderr.take().expect("Failed to open stderr");

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Value>(64);
        let pending_requests = DashMap::new();
        let opened_documents = DashMap::new();

        let process = Arc::new(Mutex::new(child));
        let next_id = std::sync::atomic::AtomicU64::new(1);
        let line_mappings = DashMap::new();
        let virtual_to_original_uri = DashMap::new();

        let sub_client = Arc::new(Self {
            ext: ext.clone(),
            language_id,
            process,
            stdin_tx,
            pending_requests,
            next_id,
            opened_documents,
            line_mappings,
            virtual_to_original_uri,
            diagnostics_cache,
            legend: std::sync::RwLock::new(None),
        });

        // Spawn writer loop
        tokio::spawn(async move {
            while let Some(msg) = stdin_rx.recv().await {
                if let Err(e) = write_message(&mut stdin, &msg).await {
                    eprintln!("Error writing to sub-LSP stdin: {}", e);
                    break;
                }
            }
        });

        // Spawn stderr log loop
        let client_err = client.clone();
        let sub_client_err = sub_client.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                log_debug(&format!("[sub-lsp-{}-stderr] {}", sub_client_err.ext, line));
                client_err
                    .log_message(
                        MessageType::LOG,
                        format!("[sub-lsp-{}] {}", sub_client_err.ext, line),
                    )
                    .await;
            }
        });

        // Spawn reader loop
        let sub_client_read = sub_client.clone();
        let client_read = client.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader).await {
                    Ok(Some(msg)) => {
                        log_debug(&format!("Raw JSON from sub-LSP: {}", msg));
                        if let Some(id) = msg.get("id") {
                            if msg.get("method").is_none() {
                                // Response
                                if let Some((_, tx)) = sub_client_read.pending_requests.remove(id) {
                                    let _ = tx.send(msg);
                                }
                            } else {
                                // Request from the sub-LSP (e.g. workspace/configuration)
                                let method =
                                    msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                                if method == "workspace/semanticTokens/refresh" {
                                    let sub_client_resp = sub_client_read.clone();
                                    let client_resp = client_read.clone();
                                    let id_val = id.clone();
                                    tokio::spawn(async move {
                                        let res = client_resp
                                            .send_request::<tower_lsp::lsp_types::request::SemanticTokensRefresh>(())
                                            .await;
                                        let response = match res {
                                            Ok(_) => serde_json::json!({
                                                "jsonrpc": "2.0",
                                                "id": id_val,
                                                "result": null,
                                            }),
                                            Err(err) => serde_json::json!({
                                                "jsonrpc": "2.0",
                                                "id": id_val,
                                                "error": {
                                                    "code": -32603,
                                                    "message": format!("Internal error: {}", err),
                                                }
                                            }),
                                        };
                                        let _ = sub_client_resp.stdin_tx.send(response).await;
                                    });
                                } else if method == "workspace/configuration" {
                                    // Return a list of nulls matching the number of requested items
                                    let num_items = msg
                                        .get("params")
                                        .and_then(|p| p.get("items"))
                                        .and_then(|i| i.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(1);
                                    let result = serde_json::Value::Array(vec![
                                        serde_json::Value::Null;
                                        num_items
                                    ]);
                                    let response = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": result,
                                    });
                                    let _ = sub_client_read.stdin_tx.send(response).await;
                                } else if method == "workspace/diagnostic/refresh" {
                                    let response = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": null,
                                    });
                                    let _ = sub_client_read.stdin_tx.send(response).await;
                                } else {
                                    // Fallback to null result for other unsupported requests to prevent crashes
                                    let response = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": null,
                                    });
                                    let _ = sub_client_read.stdin_tx.send(response).await;
                                }
                            }
                        } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                            log_debug(&format!("Received notification: {}", method));
                            // Notification from sub-LSP
                            if method == "textDocument/publishDiagnostics" {
                                if let Some(params) = msg.get("params") {
                                    if let Err(e) =
                                        handle_diagnostics(&client_read, &sub_client_read, params)
                                            .await
                                    {
                                        let _ = client_read
                                            .log_message(
                                                MessageType::ERROR,
                                                format!("Error mapping diagnostics: {}", e),
                                            )
                                            .await;
                                    }
                                }
                            } else {
                                client_read
                                    .log_message(
                                        MessageType::LOG,
                                        format!(
                                            "[sub-lsp-{}] Notification: {}",
                                            sub_client_read.ext, method
                                        ),
                                    )
                                    .await;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        client_read
                            .log_message(
                                MessageType::ERROR,
                                format!(
                                    "[sub-lsp-{}] Error reading message: {}",
                                    sub_client_read.ext, e
                                ),
                            )
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(sub_client)
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let next_id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Value::Number(next_id.into())
        };

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(id.clone(), tx);

        if self.stdin_tx.send(request).await.is_err() {
            self.pending_requests.remove(&id);
            return Err("Failed to send request to sub-LSP stdin".to_string());
        }

        rx.await.map_err(|e| format!("Request cancelled: {}", e))
    }

    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        self.stdin_tx
            .send(notification)
            .await
            .map_err(|e| format!("Failed to send notification to sub-LSP: {}", e))
    }

    pub async fn initialize(
        &self,
        root_uri: Url,
        capabilities: ClientCapabilities,
    ) -> Result<(), String> {
        let root_path = root_uri
            .to_file_path()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        let root_name = root_uri
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .last()
            .unwrap_or("root")
            .to_string();

        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootPath": root_path,
            "rootUri": root_uri,
            "workspaceFolders": [
                {
                    "uri": root_uri,
                    "name": root_name,
                }
            ],
            "capabilities": capabilities,
            "initializationOptions": serde_json::Value::Null,
        });

        let response = self.send_request("initialize", params).await?;
        if let Some(err) = response.get("error") {
            return Err(format!("Server returned initialization error: {:?}", err));
        }

        if let Some(result) = response.get("result") {
            if let Some(caps) = result.get("capabilities") {
                if let Some(sem_tokens) = caps.get("semanticTokensProvider") {
                    if let Some(legend_val) = sem_tokens.get("legend") {
                        if let Ok(legend) =
                            serde_json::from_value::<SemanticTokensLegend>(legend_val.clone())
                        {
                            let mut self_legend = self.legend.write().unwrap();
                            *self_legend = Some(legend);
                        }
                    }
                }
            }
        }

        self.send_notification("initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    pub async fn sync_document(
        &self,
        doc_uri: Url,
        doc_path: &Path,
        mapping: Vec<(usize, isize)>,
        original_uri: Url,
    ) -> Result<(), String> {
        self.line_mappings.insert(doc_uri.clone(), mapping);
        self.virtual_to_original_uri
            .insert(doc_uri.clone(), original_uri);
        let content = tokio::fs::read_to_string(doc_path)
            .await
            .map_err(|e| format!("Failed to read virtual doc from disk: {}", e))?;

        let entry = self.opened_documents.entry(doc_uri.clone());
        match entry {
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                let version = 1;
                vacant.insert(version);

                let params = serde_json::json!({
                    "textDocument": {
                        "uri": doc_uri,
                        "languageId": self.language_id,
                        "version": version,
                        "text": content,
                    }
                });

                self.send_notification("textDocument/didOpen", params)
                    .await?;
            }
            dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
                let version = occupied.get() + 1;
                *occupied.get_mut() = version;

                let params = serde_json::json!({
                    "textDocument": {
                        "uri": doc_uri,
                        "version": version,
                    },
                    "contentChanges": [
                        {
                            "text": content,
                        }
                    ]
                });

                self.send_notification("textDocument/didChange", params)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn save_document(&self, doc_uri: Url) -> Result<(), String> {
        let params = serde_json::json!({
            "textDocument": {
                "uri": doc_uri
            }
        });
        self.send_notification("textDocument/didSave", params).await
    }

    pub fn map_to_virtual(
        &self,
        doc_uri: &Url,
        original_line: usize,
        original_col: usize,
    ) -> Option<(usize, usize)> {
        let mapping = self.line_mappings.get(doc_uri)?;
        let pos = mapping.iter().position(|&(l, _)| l == original_line)?;
        let (_, offset) = mapping.get(pos)?;
        let virt_col = (original_col as isize + offset).max(0) as usize;
        Some((pos, virt_col))
    }

    pub fn map_to_original(
        &self,
        doc_uri: &Url,
        virtual_line: usize,
        virtual_col: usize,
    ) -> Option<(usize, usize)> {
        let mapping = self.line_mappings.get(doc_uri)?;
        let &(orig_line, offset) = mapping.get(virtual_line)?;
        let orig_col = (virtual_col as isize - offset).max(0) as usize;
        Some((orig_line, orig_col))
    }
}

impl SubLspManager {
    pub fn new() -> Self {
        Self {
            active_clients: DashMap::new(),
            diagnostics_cache: Arc::new(DashMap::new()),
        }
    }

    pub async fn get_or_spawn(
        &self,
        ext: String,
        config: &crate::config::NolspConfig,
        client: Client,
        root_path: Option<PathBuf>,
        capabilities: ClientCapabilities,
    ) -> Option<Arc<SubLspClient>> {
        if let Some(c) = self.active_clients.get(&ext) {
            return Some(c.clone());
        }

        let server_config = config.get_server_for_ext(&ext)?;

        let root_uri = if let Some(path) = root_path {
            Url::from_directory_path(path).ok()?
        } else {
            Url::parse("file:///").ok()?
        };

        let language_id = server_config
            .language_id
            .clone()
            .unwrap_or_else(|| ext.clone());

        match SubLspClient::spawn(
            ext.clone(),
            language_id,
            &server_config.command,
            &server_config.args,
            &server_config.env,
            client.clone(),
            root_uri.clone(),
            capabilities.clone(),
            self.diagnostics_cache.clone(),
        ) {
            Ok(c) => {
                if let Err(e) = c.initialize(root_uri, capabilities).await {
                    let _ = client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to initialize sub-LSP for {}: {}", ext, e),
                        )
                        .await;
                    None
                } else {
                    let _ = client
                        .log_message(
                            MessageType::INFO,
                            format!("Successfully initialized sub-LSP for {}", ext),
                        )
                        .await;
                    self.active_clients.insert(ext.clone(), c.clone());

                    // Request the editor client to refresh semantic tokens
                    let client_cb = client.clone();
                    tokio::spawn(async move {
                        let _ = client_cb
                            .send_request::<tower_lsp::lsp_types::request::SemanticTokensRefresh>(())
                            .await;
                    });

                    Some(c)
                }
            }
            Err(e) => {
                let _ = client
                    .log_message(
                        MessageType::ERROR,
                        format!(
                            "Failed to spawn sub-LSP for command {}: {}",
                            server_config.command, e
                        ),
                    )
                    .await;
                None
            }
        }
    }

    pub async fn sync_virtual_docs(
        self: &Arc<Self>,
        _symbols: &[crate::parser::ChunkSymbols],
        workspace_root: &Path,
        client: Client,
        config: &crate::config::NolspConfig,
        capabilities: ClientCapabilities,
        mappings: HashMap<PathBuf, Vec<(usize, isize)>>,
        original_uri: Url,
        is_save: bool,
    ) {
        log_debug(&format!(
            "sync_virtual_docs called for original_uri: {}, is_save: {}",
            original_uri, is_save
        ));

        for (target_path, mapping) in &mappings {
            if !target_path.exists() {
                continue;
            }

            let ext = match target_path.extension().and_then(|e| e.to_str()) {
                Some(e) => e.to_string(),
                None => continue,
            };

            let sub_client = self
                .get_or_spawn(
                    ext.clone(),
                    config,
                    client.clone(),
                    Some(workspace_root.join(".nolsp").join("virtual")),
                    capabilities.clone(),
                )
                .await;

            if let Some(sub_client) = sub_client {
                log_debug(&format!("Found sub_client for ext: {}", ext));
                if let Ok(doc_uri) = Url::from_file_path(target_path) {
                    log_debug(&format!("Syncing document: {}", doc_uri));
                    if let Err(e) = sub_client
                        .sync_document(
                            doc_uri.clone(),
                            target_path,
                            mapping.clone(),
                            original_uri.clone(),
                        )
                        .await
                    {
                        let _ = client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to sync virtual doc {:?}: {}", target_path, e),
                            )
                            .await;
                    }

                    if is_save {
                        log_debug(&format!("Saving document: {}", doc_uri));
                        if let Err(e) = sub_client.save_document(doc_uri).await {
                            let _ = client
                                .log_message(
                                    MessageType::ERROR,
                                    format!("Failed to save virtual doc {:?}: {}", target_path, e),
                                )
                                .await;
                        }
                    }
                }
            }
        }
    }

    pub fn find_virtual_pos(
        &self,
        original_line: usize,
        original_col: usize,
    ) -> Option<(Arc<SubLspClient>, Url, usize, usize)> {
        for entry in self.active_clients.iter() {
            let client = entry.value();
            for doc_entry in client.line_mappings.iter() {
                let doc_uri = doc_entry.key();
                if let Some((v_line, v_col)) =
                    client.map_to_virtual(doc_uri, original_line, original_col)
                {
                    return Some((client.clone(), doc_uri.clone(), v_line, v_col));
                }
            }
        }
        None
    }

    pub async fn shutdown(&self) {
        for entry in self.active_clients.iter() {
            let client = entry.value();
            let mut proc = client.process.lock().await;
            let _ = proc.kill().await;
        }
        self.active_clients.clear();
    }
}

pub fn log_debug(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/home/alwin/Documents/2026_sem1/REIT4841/testing/.nolsp/debug.log")
    {
        let _ = writeln!(file, "{}", msg);
    }
}

async fn handle_diagnostics(
    client: &tower_lsp::Client,
    sub_client: &SubLspClient,
    params: &Value,
) -> Result<(), String> {
    let virtual_uri_str = params
        .get("uri")
        .and_then(|u| u.as_str())
        .ok_or("Missing uri in diagnostics params")?;
    let virtual_uri =
        Url::parse(virtual_uri_str).map_err(|e| format!("Invalid uri in diagnostics: {}", e))?;

    log_debug(&format!(
        "handle_diagnostics for virtual_uri: {}",
        virtual_uri
    ));

    let original_uri = sub_client
        .virtual_to_original_uri
        .get(&virtual_uri)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| format!("No original URI found for virtual URI: {}", virtual_uri))?;

    log_debug(&format!(
        "mapped virtual_uri to original_uri: {}",
        original_uri
    ));

    let diagnostics_val = params
        .get("diagnostics")
        .and_then(|d| d.as_array())
        .ok_or("Missing diagnostics array")?;

    log_debug(&format!(
        "Raw diagnostics count from sub-LSP: {}",
        diagnostics_val.len()
    ));

    let mut original_diagnostics = Vec::new();

    for diag_val in diagnostics_val {
        let mut diag: tower_lsp::lsp_types::Diagnostic =
            serde_json::from_value(diag_val.clone())
                .map_err(|e| format!("Failed to parse diagnostic: {}", e))?;

        if let Some((orig_start_line, orig_start_col)) = sub_client.map_to_original(
            &virtual_uri,
            diag.range.start.line as usize,
            diag.range.start.character as usize,
        ) {
            diag.range.start.line = orig_start_line as u32;
            diag.range.start.character = orig_start_col as u32;

            if let Some((orig_end_line, orig_end_col)) = sub_client.map_to_original(
                &virtual_uri,
                diag.range.end.line as usize,
                diag.range.end.character as usize,
            ) {
                diag.range.end.line = orig_end_line as u32;
                diag.range.end.character = orig_end_col as u32;

                original_diagnostics.push(diag);
            }
        }
    }

    // Update diagnostics cache
    let inner_map = sub_client
        .diagnostics_cache
        .entry(original_uri.clone())
        .or_insert_with(DashMap::new);
    inner_map.insert(virtual_uri, original_diagnostics);

    // Merge and publish
    let mut merged = Vec::new();
    for entry in inner_map.iter() {
        merged.extend(entry.value().clone());
    }

    log_debug(&format!(
        "Publishing {} merged diagnostics to {}",
        merged.len(),
        original_uri
    ));

    client.publish_diagnostics(original_uri, merged, None).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn test_read_write_message() {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "test",
            "params": {}
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let read = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(read, msg);
    }
}

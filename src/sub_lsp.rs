use dashmap::DashMap;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tower_lsp::Client;
use tower_lsp::lsp_types::{ClientCapabilities, MessageType, Url};

pub struct SubLspClient {
    pub ext: String,
    pub language_id: String,
    pub process: Arc<Mutex<Child>>,
    pub stdin_tx: mpsc::Sender<Value>,
    pub pending_requests: DashMap<Value, oneshot::Sender<Value>>,
    pub next_id: std::sync::atomic::AtomicU64,
    pub opened_documents: DashMap<Url, i32>,
    pub line_mappings: DashMap<Url, Vec<usize>>,
}

#[derive(Default)]
pub struct SubLspManager {
    pub active_clients: DashMap<String, Arc<SubLspClient>>,
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
        root_uri: Url,
        capabilities: ClientCapabilities,
    ) -> Result<Arc<Self>, std::io::Error> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
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

        let sub_client = Arc::new(Self {
            ext: ext.clone(),
            language_id,
            process,
            stdin_tx,
            pending_requests,
            next_id,
            opened_documents,
            line_mappings,
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
                        if let Some(id) = msg.get("id") {
                            if msg.get("method").is_none() {
                                // Response
                                if let Some((_, tx)) = sub_client_read.pending_requests.remove(id) {
                                    let _ = tx.send(msg);
                                }
                            } else {
                                // Request from the sub-LSP (e.g. workspace/configuration)
                                let response = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {
                                        "code": -32601,
                                        "message": "Method not found"
                                    }
                                });
                                let _ = sub_client_read.stdin_tx.send(response).await;
                            }
                        } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                            // Notification from sub-LSP
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

        // Perform initialization handshake asynchronously
        let sub_client_init = sub_client.clone();
        tokio::spawn(async move {
            if let Err(e) = sub_client_init.initialize(root_uri, capabilities).await {
                client
                    .log_message(
                        MessageType::ERROR,
                        format!(
                            "Failed to initialize sub-LSP for {}: {}",
                            sub_client_init.ext, e
                        ),
                    )
                    .await;
            } else {
                client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Successfully initialized sub-LSP for {}",
                            sub_client_init.ext
                        ),
                    )
                    .await;
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
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": capabilities,
            "initializationOptions": serde_json::Value::Null,
        });

        let response = self.send_request("initialize", params).await?;
        if let Some(err) = response.get("error") {
            return Err(format!("Server returned initialization error: {:?}", err));
        }

        self.send_notification("initialized", serde_json::json!({}))
            .await?;
        Ok(())
    }

    pub async fn sync_document(
        &self,
        doc_uri: Url,
        doc_path: &Path,
        mapping: Vec<usize>,
    ) -> Result<(), String> {
        self.line_mappings.insert(doc_uri.clone(), mapping);
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
}

impl SubLspManager {
    pub fn new() -> Self {
        Self {
            active_clients: DashMap::new(),
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
            root_uri,
            capabilities,
        ) {
            Ok(c) => {
                self.active_clients.insert(ext, c.clone());
                Some(c)
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
        symbols: &[crate::parser::ChunkSymbols],
        workspace_root: &Path,
        client: Client,
        config: &crate::config::NolspConfig,
        capabilities: ClientCapabilities,
        mappings: HashMap<PathBuf, Vec<usize>>,
    ) {
        use std::collections::HashSet;

        let mut unique_names = HashSet::<String>::new();
        for symbol in symbols {
            if let (crate::parser::ChunkSymbolKind::Header, Some(name)) =
                (symbol.kind, &symbol.name)
            {
                unique_names.insert(name.clone());
            }
        }

        let unique_file_names: Vec<String> = unique_names
            .into_iter()
            .filter(|s| Path::new(s).extension().is_some())
            .collect();

        for file_name in unique_file_names {
            let target_path = workspace_root
                .join(".nolsp")
                .join("virtual")
                .join(&file_name);

            if !target_path.exists() {
                continue;
            }

            let ext = match Path::new(&file_name).extension().and_then(|e| e.to_str()) {
                Some(e) => e.to_string(),
                None => continue,
            };

            let sub_client = self
                .get_or_spawn(
                    ext,
                    config,
                    client.clone(),
                    Some(workspace_root.to_path_buf()),
                    capabilities.clone(),
                )
                .await;

            if let Some(sub_client) = sub_client {
                let mapping = mappings.get(&target_path).cloned().unwrap_or_default();
                if let Ok(doc_uri) = Url::from_file_path(&target_path) {
                    if let Err(e) = sub_client
                        .sync_document(doc_uri, &target_path, mapping)
                        .await
                    {
                        let _ = client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to sync virtual doc {}: {}", file_name, e),
                            )
                            .await;
                    }
                }
            }
        }
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

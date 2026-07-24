use serde::Deserialize;
use std::collections::HashMap;
use std::default;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String,String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NolspConfig {
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
}

// Providing built-in default mappings if no config file is specified
impl NolspConfig {
    
    pub fn with_defaults(mut self) -> Self {
        let default_servers = [
                ("c", ServerConfig { command: "clangd".into(), args: vec![], env: HashMap::new() }),
                ("cpp", ServerConfig { command: "clangd".into(), args: vec![], env: HashMap::new() }),
                ("h", ServerConfig { command: "clangd".into(), args: vec![], env: HashMap::new() }),
                ("rs", ServerConfig { command: "rust-analyzer".into(), args: vec![], env:HashMap::new() }),
                ("py", ServerConfig { command: "pylsp".into(), args: vec![], env: HashMap::new() }),
                ("ts", ServerConfig { command: "typescript-language-server".into(), args: vec!["--stdio".into()], env: HashMap::new() }),
                ("go", ServerConfig { command: "gopls".into(), args: vec![], env: HashMap::new() }),
        ];

        for (ext, config) in default_servers {
            self.servers.entry(ext.to_string()).or_insert(config);
        }

        self
    }

    // getter function for client LSP for a given file extension
    pub fn get_server_for_ext(&self, ext: &str) -> Option<&ServerConfig> {
        self.servers.get(ext)
    }
}

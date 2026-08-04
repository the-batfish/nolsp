use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub language_id: Option<String>,
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
            (
                "c",
                ServerConfig {
                    command: "clangd".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("c".into()),
                },
            ),
            (
                "cpp",
                ServerConfig {
                    command: "clangd".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("cpp".into()),
                },
            ),
            (
                "h",
                ServerConfig {
                    command: "clangd".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("c".into()),
                },
            ),
            (
                "rs",
                ServerConfig {
                    command: "rust-analyzer".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("rust".into()),
                },
            ),
            (
                "py",
                ServerConfig {
                    command: "pylsp".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("python".into()),
                },
            ),
            (
                "ts",
                ServerConfig {
                    command: "typescript-language-server".into(),
                    args: vec!["--stdio".into()],
                    env: HashMap::new(),
                    language_id: Some("typescript".into()),
                },
            ),
            (
                "go",
                ServerConfig {
                    command: "gopls".into(),
                    args: vec![],
                    env: HashMap::new(),
                    language_id: Some("go".into()),
                },
            ),
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

pub fn load_config(workspace_root: Option<&Path>) -> NolspConfig {
    // Search for a workspace config first
    if let Some(root) = workspace_root {
        for filename in &[".nolsp.toml", "nolsp.toml"] {
            let path = root.join(filename);
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(config) = toml::from_str::<NolspConfig>(&content) {
                        return config.with_defaults();
                    }
                }
            }
        }
    }

    // Search and try for a global config
    if let Ok(home) = std::env::var("HOME") {
        let global_path = Path::new(&home)
            .join(".config")
            .join("nolsp")
            .join("config.toml");
        if global_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&global_path) {
                if let Ok(config) = toml::from_str::<NolspConfig>(&content) {
                    return config.with_defaults();
                }
            }
        }
    }

    // Fallback to defaults
    NolspConfig::default().with_defaults()
}

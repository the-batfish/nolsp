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
#[allow(dead_code)]
pub struct DocumentConfig {
    #[serde(default)]
    pub doc_language: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NolspConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub document: Option<DocumentConfig>,
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

    // getter function for document language (fallback to latex)
    #[allow(dead_code)]
    pub fn get_doc_language(&self) -> &str {
        self.document
            .as_ref()
            .and_then(|d| d.doc_language.as_deref())
            .unwrap_or("latex")
    }

    /// Resolves a file name or extension to a canonical language identifier.
    /// Checks explicit user server configs first, then falls back to built-in canonical mappings.
    pub fn resolve_language(&self, name_or_ext: &str) -> Option<String> {
        let ext = Path::new(name_or_ext)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(name_or_ext);

        if let Some(server) = self.servers.get(ext) {
            if let Some(lang_id) = &server.language_id {
                return Some(lang_id.clone());
            }
        }

        get_canonical_language(name_or_ext).map(|s| s.to_string())
    }
}

/// Built-in canonical language mapping.
/// Groups compatible extensions (e.g. .c and .h -> "c", .cpp and .hpp -> "cpp")
/// while keeping distinct languages strictly separated.
pub fn get_canonical_language(name_or_ext: &str) -> Option<&'static str> {
    let lower_name = name_or_ext.to_ascii_lowercase();

    // Check exact filenames first
    match lower_name.as_str() {
        "makefile" => return Some("make"),
        "cmakelists.txt" => return Some("cmake"),
        "dockerfile" => return Some("dockerfile"),
        "pipfile" => return Some("toml"),
        _ => {}
    }

    let ext = Path::new(&lower_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or(&lower_name);

    match ext {
        // C family (strictly C)
        "c" | "h" => Some("c"),

        // C++ family (strictly C++)
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "c++" | "h++" => Some("cpp"),

        // Rust
        "rs" | "rust" => Some("rust"),

        // Python
        "py" | "python" | "pyw" | "pyi" => Some("python"),

        // Go
        "go" => Some("go"),

        // TypeScript & TSX
        "ts" | "typescript" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),

        // JavaScript & JSX
        "js" | "javascript" | "mjs" | "cjs" | "jsx" => Some("javascript"),

        // Shell
        "sh" | "bash" | "zsh" => Some("bash"),

        // Web / Markup / Data
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "scss" | "sass" => Some("scss"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "xml" => Some("xml"),
        "sql" => Some("sql"),
        "r" => Some("r"),
        "java" => Some("java"),
        "kt" | "kts" | "kotlin" => Some("kotlin"),
        "cs" | "csharp" | "c_sharp" => Some("c_sharp"),
        "lua" => Some("lua"),
        "zig" => Some("zig"),
        "f" | "f90" | "f95" | "for" | "fortran" => Some("fortran"),
        "jl" | "julia" => Some("julia"),
        "hs" | "haskell" => Some("haskell"),
        "ml" | "mli" | "ocaml" => Some("ocaml"),
        "ex" | "exs" | "elixir" => Some("elixir"),
        "erl" | "hrl" | "erlang" => Some("erlang"),
        "clj" | "cljs" | "cljc" | "clojure" => Some("clojure"),
        "nix" => Some("nix"),
        "cmake" => Some("cmake"),
        "mk" => Some("make"),
        "typ" | "typst" => Some("typst"),
        "md" | "markdown" => Some("markdown"),
        "tex" | "latex" | "sty" | "cls" => Some("latex"),
        _ => None,
    }
}

pub fn load_config(workspace_root: Option<&Path>) -> NolspConfig {
    // 1. Search for project/workspace config first (nolsp.toml takes precedence)
    if let Some(root) = workspace_root {
        for filename in &["nolsp.toml", ".nolsp.toml"] {
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

    // 2. Search for user/global config (~/.config/nolsp/nolsp.toml, etc.)
    let mut config_dirs = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        config_dirs.push(std::path::PathBuf::from(xdg).join("nolsp"));
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        config_dirs.push(Path::new(&home).join(".config").join("nolsp"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        config_dirs.push(Path::new(&appdata).join("nolsp"));
    }

    for dir in config_dirs {
        for filename in &["nolsp.toml", "config.toml", ".nolsp.toml"] {
            let global_path = dir.join(filename);
            if global_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&global_path) {
                    if let Ok(config) = toml::from_str::<NolspConfig>(&content) {
                        return config.with_defaults();
                    }
                }
            }
        }
    }

    // 3. Fallback to defaults
    NolspConfig::default().with_defaults()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NolspConfig::default().with_defaults();
        assert_eq!(config.get_doc_language(), "latex");
        assert!(config.get_server_for_ext("py").is_some());
        assert!(config.get_server_for_ext("c").is_some());
        assert!(config.get_server_for_ext("rs").is_some());
    }

    #[test]
    fn test_custom_doc_language() {
        let toml_str = r#"
            [document]
            doc_language = "markdown"

            [servers.custom]
            command = "custom-lsp"
        "#;
        let config: NolspConfig = toml::from_str(toml_str).unwrap();
        let config = config.with_defaults();
        assert_eq!(config.get_doc_language(), "markdown");
        assert_eq!(
            config.get_server_for_ext("custom").unwrap().command,
            "custom-lsp"
        );
        // Default servers should still be populated
        assert!(config.get_server_for_ext("py").is_some());
    }

    #[test]
    fn test_canonical_language_resolution() {
        let config = NolspConfig::default().with_defaults();

        // C and H are compatible under "c"
        assert_eq!(config.resolve_language("main.c"), Some("c".into()));
        assert_eq!(config.resolve_language("header.h"), Some("c".into()));

        // C++ is strictly "cpp", not "c"
        assert_eq!(config.resolve_language("main.cpp"), Some("cpp".into()));
        assert_eq!(config.resolve_language("header.hpp"), Some("cpp".into()));

        // Python
        assert_eq!(config.resolve_language("backend.py"), Some("python".into()));

        // Rust
        assert_eq!(config.resolve_language("lib.rs"), Some("rust".into()));

        // Special filenames
        assert_eq!(config.resolve_language("Makefile"), Some("make".into()));
        assert_eq!(config.resolve_language("Pipfile"), Some("toml".into()));

        // Unknown
        assert_eq!(config.resolve_language("some_chunk_without_ext"), None);
    }
}

import * as path from "path";
import * as fs from "fs";
import * as vscode from "vscode";
import { workspace, ExtensionContext, window } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

function findServerPath(context: ExtensionContext): string | null {
  const config = workspace.getConfiguration("nolsp");
  const configuredPath = config.get<string>("serverPath");
  if (configuredPath && fs.existsSync(configuredPath)) {
    return configuredPath;
  }

  const binaryName = process.platform === "win32" ? "nolsp.exe" : "nolsp";
  const bundledPath = path.join(context.extensionPath, "bin", binaryName);
  if (fs.existsSync(bundledPath)) {
    if (process.platform !== "win32") {
      try {
        fs.chmodSync(bundledPath, 0o755);
      } catch (e) {
        console.error("Failed to chmod binary:", e);
      }
    }
    return bundledPath;
  }

  return binaryName;
}

function getDocLanguage(documentUri: vscode.Uri): string {
  const candidates: string[] = [];

  // 1. Workspace / Project-level candidates (highest priority)
  const workspaceFolder = workspace.getWorkspaceFolder(documentUri);
  if (workspaceFolder) {
    candidates.push(path.join(workspaceFolder.uri.fsPath, "nolsp.toml"));
    candidates.push(path.join(workspaceFolder.uri.fsPath, ".nolsp.toml"));
  } else if (documentUri.fsPath) {
    const dir = path.dirname(documentUri.fsPath);
    candidates.push(path.join(dir, "nolsp.toml"));
    candidates.push(path.join(dir, ".nolsp.toml"));
  }

  // 2. User / Global candidates (~/.config/nolsp/nolsp.toml, etc.)
  const userHome = process.env.USERPROFILE || process.env.HOME || "";
  if (userHome) {
    candidates.push(path.join(userHome, ".config", "nolsp", "nolsp.toml"));
    candidates.push(path.join(userHome, ".config", "nolsp", "config.toml"));
    candidates.push(path.join(userHome, ".config", "nolsp", ".nolsp.toml"));
  }
  if (process.env.APPDATA) {
    candidates.push(path.join(process.env.APPDATA, "nolsp", "nolsp.toml"));
  }

  for (const configPath of candidates) {
    if (fs.existsSync(configPath)) {
      try {
        const content = fs.readFileSync(configPath, "utf-8");
        for (const line of content.split(/\r?\n/)) {
          const match = line.match(/^\s*doc_language\s*=\s*["']([^"']+)["']/);
          if (match && match[1]) {
            return match[1].trim();
          }
        }
      } catch (e) {
        console.error("Error reading nolsp config:", e);
      }
    }
  }

  return "latex"; // default fallback
}

async function syncDocumentLanguage(document: vscode.TextDocument) {
  if (!document.fileName.endsWith(".nw") && !document.fileName.endsWith(".noweb")) {
    return;
  }

  const targetLang = getDocLanguage(document.uri);
  if (document.languageId !== targetLang) {
    try {
      await vscode.languages.setTextDocumentLanguage(document, targetLang);
    } catch (e) {
      console.error(`Failed to set document language to ${targetLang}:`, e);
    }
  }
}

export async function activate(context: ExtensionContext) {
  const serverCommand = findServerPath(context);

  if (!serverCommand) {
    window.showErrorMessage("Could not locate nolsp executable.");
    return;
  }

  const isWindows = process.platform === "win32";
  const userHome = process.env.USERPROFILE || process.env.HOME || "";

  let additionalPaths: string[] = [];

  if (isWindows) {
    additionalPaths = [
      path.join(userHome, ".cargo", "bin"),
      path.join(userHome, "scoop", "shims"),
      "C:\\ProgramData\\chocolatey\\bin",
      path.join(userHome, "AppData", "Roaming", "npm"),
    ];

    // Detect Python user Scripts directories (e.g. AppData/Local/Programs/Python/Python*/Scripts)
    const localAppData = process.env.LOCALAPPDATA || path.join(userHome, "AppData", "Local");
    const pythonProgramsDir = path.join(localAppData, "Programs", "Python");
    if (fs.existsSync(pythonProgramsDir)) {
      try {
        const entries = fs.readdirSync(pythonProgramsDir);
        for (const entry of entries) {
          const scriptsPath = path.join(pythonProgramsDir, entry, "Scripts");
          if (fs.existsSync(scriptsPath)) {
            additionalPaths.push(scriptsPath);
          }
        }
      } catch {
        // Ignore read errors
      }
    }

    const appData = process.env.APPDATA || path.join(userHome, "AppData", "Roaming");
    const roamingPythonDir = path.join(appData, "Python");
    if (fs.existsSync(roamingPythonDir)) {
      try {
        const entries = fs.readdirSync(roamingPythonDir);
        for (const entry of entries) {
          const scriptsPath = path.join(roamingPythonDir, entry, "Scripts");
          if (fs.existsSync(scriptsPath)) {
            additionalPaths.push(scriptsPath);
          }
        }
      } catch {
        // Ignore read errors
      }
    }
  } else {
    additionalPaths = [
      "/usr/local/noweb",
      "/usr/local/bin",
      "/usr/bin",
      path.join(userHome, ".local", "bin"),
      path.join(userHome, ".cargo", "bin"),
      path.join(userHome, "bin"),
    ];
  }

  const validPaths = additionalPaths.filter((p) => p && fs.existsSync(p));

  // On Windows, the environment variable can be keyed as "Path" or "PATH"
  const currentPath = process.env.PATH || process.env.Path || "";
  const extendedPath = Array.from(
    new Set([...currentPath.split(path.delimiter), ...validPaths])
  ).join(path.delimiter);

  // Set both PATH and Path so all child process spawn mechanisms on Windows find tools
  const env: Record<string, string | undefined> = { ...process.env, PATH: extendedPath };
  if (isWindows) {
    env.Path = extendedPath;
  }

  const serverOptions: ServerOptions = {
    run: {
      command: serverCommand,
      transport: TransportKind.stdio,
      options: { env },
    },
    debug: {
      command: serverCommand,
      transport: TransportKind.stdio,
      options: { env },
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "noweb" },
      { scheme: "file", pattern: "**/*.{nw,noweb}" },
    ],
    synchronize: {
      fileEvents: [
        workspace.createFileSystemWatcher("**/nolsp.toml"),
        workspace.createFileSystemWatcher("**/.nolsp.toml"),
      ],
    },
  };

  client = new LanguageClient(
    "nolsp",
    "nolsp Language Server",
    serverOptions,
    clientOptions
  );

  await client.start();

  // Synchronize documentation language for currently active and newly opened noweb documents
  context.subscriptions.push(
    workspace.onDidOpenTextDocument((doc) => {
      syncDocumentLanguage(doc);
    }),
    window.onDidChangeActiveTextEditor((editor) => {
      if (editor) {
        syncDocumentLanguage(editor.document);
      }
    })
  );

  if (window.activeTextEditor) {
    syncDocumentLanguage(window.activeTextEditor.document);
  }
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
  }
}

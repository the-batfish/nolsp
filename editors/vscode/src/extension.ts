import * as path from "path";
import * as fs from "fs";
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

export async function activate(context: ExtensionContext) {
  const serverCommand = findServerPath(context);

  if (!serverCommand) {
    window.showErrorMessage("Could not locate nolsp executable.");
    return;
  }

  const home = process.env.HOME || "";
  const additionalPaths = [
    "/usr/local/noweb",
    "/usr/local/bin",
    "/usr/bin",
    path.join(home, ".local", "bin"),
    path.join(home, ".cargo", "bin"),
    path.join(home, "bin"),
  ].filter((p) => fs.existsSync(p));

  const currentPath = process.env.PATH || "";
  const extendedPath = Array.from(
    new Set([...currentPath.split(path.delimiter), ...additionalPaths])
  ).join(path.delimiter);

  const serverOptions: ServerOptions = {
    run: {
      command: serverCommand,
      transport: TransportKind.stdio,
      options: { env: { ...process.env, PATH: extendedPath } },
    },
    debug: {
      command: serverCommand,
      transport: TransportKind.stdio,
      options: { env: { ...process.env, PATH: extendedPath } },
    },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "noweb" }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher("**/.nolsp.toml"),
    },
  };

  client = new LanguageClient(
    "nolsp",
    "nolsp Language Server",
    serverOptions,
    clientOptions
  );

  await client.start();
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
  }
}

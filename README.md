# nolsp

**Language Server Multiplexer Engine for [noweb](https://www.cs.tufts.edu/~nr/noweb/) literate programming.**

![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange?logo=rust&logoColor=white)
![Version](https://img.shields.io/badge/version-1.0.0-blue)

One `.nw` file, prose and code interleaved, code written in *many* languages — with full
IDE features in *all* of them at once.

## What is nolsp?

[nolsp](https://github.com/the-batfish/nolsp) is a single LSP server for
[noweb](https://www.cs.tufts.edu/~nr/noweb/) files (`.nw` / `.noweb`). A noweb document
mixes documentation with named **code chunks**, and each chunk can be written in a
*different* language. nolsp makes a normal language server aware of every chunk:

1. **Parse** the noweb file to find the chunks.
2. **Tangle** each root file chunk into a virtual file (via `notangle`).
3. **Spawn** the right language server per language (clangd, rust-analyzer, pylsp, …).
4. **Translate** positions, diagnostics, completions and hovers between the virtual
   files and your original `.nw` document.

The result: open a single noweb file and get native diagnostics, completion, hover and
semantic highlighting for every language it contains — no switching, no generated files
in your repo, no manual wiring.

## Features

- **Multi-language LSP in one file** — each chunk talks to the language server for its own language.
- **Diagnostics** — sub-server diagnostics are remapped to noweb positions, merged, and republished for the `.nw` file.
- **Completion** — `<<` triggers chunk-name completion; other characters are forwarded to the chunk's language server.
- **Hover** — chunk metadata over noweb symbols, code hovers over chunks.
- **Semantic tokens** — noweb structure (headers, references, terminators) *plus* each language's own tokens, merged into one legend.
- **Automatic language inference** — chunk languages are derived from the dependency graph; no annotations required.
- **Explicit language tags** — opt a chunk into a specific language with a one-line comment on its terminator.
- **Formatting** — a formatter action that writes/refreshes those tags idempotently.

## How it works

```
 .nw file
   │  parse
   ▼
 chunks ──tangle──► virtual files (.nolsp/virtual/)
   │                    │
   │  infer languages   │  spawn sub-LSPs (clangd, pylsp, …)
   ▼                    ▼
 language map      diagnostics/completions/hovers (virtual coords)
   │                    │
   └────── remap to .nw positions ───────────► editor
```

Sub-LSPs only ever see the **tangled** code; nolsp keeps a line map (built from
`notangle -L`'s `#line` markers) so every position and diagnostic is translated back to
the original document. All of this is transparent — you just edit the `.nw` file.

## The noweb format

A noweb document interleaves prose with **code chunks**:

```
This is prose (LaTeX, Markdown, Typst, …).

<<currencies>>=
currencies = ["AUD", "USD"]
@

<<backend.py>>=
import json
<<currencies>>
print(json.dumps(currencies))
@
```

- A **chunk** is `<<name>>=` … `@`.
- A **reference** is `<<name>>` — it pastes another chunk's code inline.
- A **root chunk** is a chunk whose name looks like a file (`<<backend.py>>=`); these are
  what get tangled into standalone virtual files.

## Language inference

nolsp decides each chunk's language automatically:

1. **Root chunks** inherit their language from their filename extension (`backend.py` → Python).
2. **Named chunks** inherit the language of the root chunks that transitively reference them.
3. A chunk referenced by roots of **different** languages (e.g. both `.c` and `.py`) is
   treated as plain text — no sub-server, no highlighting.
4. Unreferenced chunks are plain text.

Compatible extensions are grouped into canonical families — `.c` and `.h` are both `c`,
`.cpp`/`.hpp` are both `cpp` — so two C-family roots referencing the same chunk still agree.

## Terminator language tags

You can override inference with an **explicit tag** on a named chunk's `@` line, written in
your documentation language's comment syntax so it's invisible in rendered output:

| Documentation language | Tag example |
|---|---|
| LaTeX (default) | `@ % (python)` |
| Markdown | `@ <!-- (python) -->` |
| Typst | `@ // (python)` |

```latex
<<shared>>=
x = 1
@ % (python)      % forces Python even if referenced from C too
```

Tags are the source of truth for named chunks (**explicit tag > inference**), are stripped
by `notangle` (so they never appear in tangled output), and are written/refreshed
automatically by nolsp's **formatter**.

## Syntax highlighting

nolsp computes each chunk's language once and publishes it two ways:

- **Neovim (dynamic)** — nolsp sends a `nolsp/chunkLanguages` notification; the
  [`tree-sitter-noweb`](https://github.com/the-batfish/tree-sitter-noweb) plugin stores it and injects each chunk's language.
- **Other tree-sitter editors (static)** — the formatter writes explicit terminator tags
  (above) that any editor can read, so Helix, Zed, Emacs, etc. highlight correctly with zero per-editor glue.

## LSP features

| Feature | Notes |
|---|---|
| Document sync | Full-text sync (`open`/`change`/`save`) |
| Diagnostics | Remapped, merged, republished for the `.nw` URI |
| Completion | Chunk names on `<<`; otherwise forwarded to the sub-server |
| Hover | Noweb chunk metadata + code hovers |
| Semantic tokens | Noweb structure merged with sub-server tokens |
| Formatting | Generates/refreshes terminator language tags (idempotent) |
| `nolsp/chunkLanguages` | Custom notification: `{ chunkName → languageId }` |

## Configuration

nolsp is configured with `nolsp.toml` (or `.nolsp.toml`). It's discovered from the
workspace root first, then global config directories, falling back to built-in defaults.

```toml
[document]
doc_language = "latex"          # "latex" (default) | "markdown" | "typst"
                                # also selects the comment syntax for terminator tags

[servers.py]                    # keyed by file extension
command = "pylsp"
args = []
env = {}                        # extra environment variables
language_id = "python"          # LSP languageId sent to the sub-server
```

`[servers.<ext>]` overrides the server for a given extension. Built-in defaults:

| Extension | Server | Language id |
|---|---|---|
| `c`, `h` | `clangd` | `c` |
| `cpp` | `clangd` | `cpp` |
| `rs` | `rust-analyzer` | `rust` |
| `py` | `pylsp` | `python` |
| `ts` | `typescript-language-server` | `typescript` |
| `go` | `gopls` | `go` |

## Supported languages

**With built-in servers:** C, C++, Rust, Python, TypeScript, Go.

**Recognized for inference & highlighting** (add a `[servers.<ext>]` entry to enable a
server): C/C++, Rust, Python, Go, TypeScript/TSX, JavaScript/JSX, Bash, HTML, CSS/SCSS,
JSON, TOML, YAML, XML, SQL, R, Java, Kotlin, C#, Lua, Zig, Fortran, Julia, Haskell,
OCaml, Elixir, Erlang, Clojure, Nix, CMake, Make, Typst, Markdown, and LaTeX.

## Companion project

Syntax highlighting is handled by
[**tree-sitter-noweb**](https://github.com/the-batfish/tree-sitter-noweb), a tree-sitter
grammar for noweb. nolsp is the source of truth for chunk languages; the grammar just
reads a tag or does a dict lookup. Keep the two together for full highlighting.

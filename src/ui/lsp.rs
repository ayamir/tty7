//! Minimal LSP client for the code-editor panel.
//!
//! One `LspClient` per (server, workspace root): a spawned server process
//! speaking JSON-RPC over stdio. A std reader thread parses `Content-Length`
//! frames and routes responses to per-request channels, `publishDiagnostics`
//! notifications to a UI-side loop, and answers the handful of server→client
//! requests (`workspace/configuration` &co.) with benign defaults so servers
//! like rust-analyzer don't stall waiting on us.
//!
//! The editor integrates through gpui-component's provider traits: completion
//! (popup menu), hover (popover) and definition (⌘-hover underline + ⌘-click)
//! are handled by [`FileLsp`], one per open file. The component's ⌘-click jump
//! only works within the current buffer, so `definitions` filters to same-file
//! links; the app-level Go to Definition action (F12, `code_editor.rs`) does
//! the full cross-file open + jump itself.
//!
//! Servers are discovered on PATH per language (rust-analyzer, gopls, pyright,
//! typescript-language-server, clangd); a missing binary just means no LSP for
//! that language — the editor works fine without it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Context, Task, Window};
use gpui_component::input::InputState;
use gpui_component::input::{CompletionProvider, DefinitionProvider, HoverProvider, RopeExt as _};
use ropey::Rope;
use serde_json::{Value, json};

use crate::ui::app::Tty7App;

/// How long a single request may wait on the server before the provider gives
/// up (a hung server must not wedge hover/completion forever).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// The command line and LSP `languageId` for a tree-sitter language name.
/// `None` → no server configured for that language.
pub(crate) fn server_for_language(lang: &str) -> Option<(&'static [&'static str], &'static str)> {
    Some(match lang {
        "rust" => (&["rust-analyzer"], "rust"),
        "go" => (&["gopls"], "go"),
        "python" => (&["pyright-langserver", "--stdio"], "python"),
        "typescript" => (&["typescript-language-server", "--stdio"], "typescript"),
        "tsx" => (
            &["typescript-language-server", "--stdio"],
            "typescriptreact",
        ),
        "javascript" => (&["typescript-language-server", "--stdio"], "javascript"),
        "c" => (&["clangd"], "c"),
        "cpp" => (&["clangd"], "cpp"),
        _ => return None,
    })
}

/// `file://` URI for a local path (percent-encoded via the `url` crate).
pub(crate) fn uri_for_path(path: &Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(|u| u.to_string())
}

/// Local path for a `file://` URI string; `None` for non-file schemes.
pub(crate) fn path_for_uri(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

// ---------------------------------------------------------------------------
// Client.
// ---------------------------------------------------------------------------

/// Thread-shared client internals (UI thread + reader thread).
struct Inner {
    name: String,
    stdin: Mutex<ChildStdin>,
    /// In-flight requests by id; the reader thread resolves them.
    pending: Mutex<HashMap<i64, smol::channel::Sender<Value>>>,
    /// Flips true when the `initialize` response lands; frames sent before
    /// that wait in `queued`.
    ready: AtomicBool,
    queued: Mutex<Vec<String>>,
    next_id: AtomicI64,
}

impl Inner {
    fn write_frame(stdin: &mut ChildStdin, body: &str) {
        let _ = write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = stdin.flush();
    }

    /// Send a frame now, or park it until the server finished initializing.
    ///
    /// The `ready` check happens under the `queued` lock, and `flush_queued`
    /// takes the same lock — otherwise a frame could read `ready == false`,
    /// lose the race to the reader thread flipping it and draining the queue,
    /// and then park itself behind a handshake that already finished, where
    /// nothing would ever send it (a `didOpen` lost that way costs the file its
    /// diagnostics for the whole session).
    fn send(&self, body: String) {
        let mut queued = self.queued.lock().unwrap();
        if self.ready.load(Ordering::SeqCst) {
            drop(queued);
            let mut stdin = self.stdin.lock().unwrap();
            Self::write_frame(&mut stdin, &body);
        } else {
            queued.push(body);
        }
    }

    /// Initialize finished: mark ready and flush everything parked behind the
    /// handshake. `ready` flips under the `queued` lock, so no sender can slip a
    /// frame out ahead of the ones already parked (a `didChange` overtaking its
    /// `didOpen` desynchronizes the server for the rest of the session).
    fn mark_ready_and_flush(&self) {
        let mut queued = self.queued.lock().unwrap();
        self.ready.store(true, Ordering::SeqCst);
        let parked: Vec<String> = std::mem::take(&mut *queued);
        let mut stdin = self.stdin.lock().unwrap();
        for body in parked {
            Self::write_frame(&mut stdin, &body);
        }
    }
}

/// A running language server bound to one workspace root.
pub(crate) struct LspClient {
    inner: Arc<Inner>,
    child: std::cell::RefCell<Child>,
    #[allow(dead_code)] // identifies the client in future workspace-level requests
    pub(crate) root: PathBuf,
    /// didOpen version counter per document.
    versions: std::cell::RefCell<HashMap<PathBuf, i64>>,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // No graceful shutdown round-trip — the app is closing the panel or
        // exiting; killing the child reaps it without waiting on a wedged one.
        let _ = self.child.borrow_mut().kill();
    }
}

impl LspClient {
    /// Spawn `cmd` rooted at `root` and start the handshake + reader thread.
    /// Diagnostics flow out through `diag_tx` as `(path, diagnostics)`.
    pub(crate) fn spawn(
        cmd: &[&str],
        root: &Path,
        diag_tx: smol::channel::Sender<(PathBuf, Vec<lsp_types::Diagnostic>)>,
    ) -> Result<Self> {
        let mut child = Command::new(cmd[0])
            .args(&cmd[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {}", cmd[0]))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let inner = Arc::new(Inner {
            name: cmd[0].to_string(),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            ready: AtomicBool::new(false),
            queued: Mutex::new(Vec::new()),
            next_id: AtomicI64::new(2), // 1 is reserved for `initialize`
        });

        // The initialize request goes out immediately (bypassing the queue).
        let root_uri = uri_for_path(root).unwrap_or_else(|| "file:///".into());
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": root.file_name().map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "root".into()),
                }],
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "publishDiagnostics": { "relatedInformation": false },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "completion": {
                            "completionItem": {
                                "snippetSupport": false,
                                "documentationFormat": ["markdown", "plaintext"],
                            }
                        },
                        "definition": { "linkSupport": true },
                        "references": {},
                    },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "window": { "workDoneProgress": true },
                },
            },
        });
        {
            let mut stdin = inner.stdin.lock().unwrap();
            Inner::write_frame(&mut stdin, &init.to_string());
        }

        // Reader thread: frame parser + dispatcher.
        let reader_inner = inner.clone();
        std::thread::Builder::new()
            .name(format!("lsp-{}", cmd[0]))
            .spawn(move || reader_loop(stdout, reader_inner, diag_tx))
            .context("spawning lsp reader thread")?;

        Ok(Self {
            inner,
            child: std::cell::RefCell::new(child),
            root: root.to_path_buf(),
            versions: std::cell::RefCell::new(HashMap::new()),
        })
    }

    /// The server binary's name, for the status bar.
    pub(crate) fn name(&self) -> &str {
        &self.inner.name
    }

    fn notify(&self, method: &str, params: Value) {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        self.inner.send(body);
    }

    /// Fire a request; the returned channel yields the `result` value (or
    /// `Null` on a server-side error). Await it on a background executor.
    pub(crate) fn request(&self, method: &str, params: Value) -> smol::channel::Receiver<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = smol::channel::bounded(1);
        self.inner.pending.lock().unwrap().insert(id, tx);
        let body =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        self.inner.send(body);
        rx
    }

    pub(crate) fn did_open(&self, path: &Path, language_id: &str, text: &str) {
        let Some(uri) = uri_for_path(path) else {
            return;
        };
        self.versions.borrow_mut().insert(path.to_path_buf(), 1);
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text,
            }}),
        );
    }

    /// Full-document sync (the simplest correct thing at this scale).
    pub(crate) fn did_change(&self, path: &Path, text: &str) {
        let Some(uri) = uri_for_path(path) else {
            return;
        };
        let mut versions = self.versions.borrow_mut();
        let v = versions.entry(path.to_path_buf()).or_insert(1);
        *v += 1;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": *v },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    pub(crate) fn did_save(&self, path: &Path) {
        let Some(uri) = uri_for_path(path) else {
            return;
        };
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        );
    }

    pub(crate) fn did_close(&self, path: &Path) {
        let Some(uri) = uri_for_path(path) else {
            return;
        };
        self.versions.borrow_mut().remove(path);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
    }

    /// Standard text-document position params for a rope offset.
    pub(crate) fn position_params(path: &Path, text: &Rope, offset: usize) -> Option<Value> {
        let uri = uri_for_path(path)?;
        let pos = text.offset_to_position(offset);
        Some(json!({
            "textDocument": { "uri": uri },
            "position": { "line": pos.line, "character": pos.character },
        }))
    }
}

/// Parse `Content-Length`-framed JSON-RPC from the server and dispatch.
fn reader_loop(
    stdout: std::process::ChildStdout,
    inner: Arc<Inner>,
    diag_tx: smol::channel::Sender<(PathBuf, Vec<lsp_types::Diagnostic>)>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        // Headers.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return, // EOF: server exited
                Ok(_) => {}
                Err(_) => return,
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_length = rest.trim().parse().ok();
            }
        }
        let Some(len) = content_length else { continue };
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };

        let id = msg.get("id").and_then(|v| v.as_i64());
        let method = msg.get("method").and_then(|v| v.as_str());
        match (id, method) {
            // Server → client request: answer with a benign default so the
            // server never blocks on us.
            (Some(id), Some(method)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let n = msg
                            .pointer("/params/items")
                            .and_then(|v| v.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0);
                        Value::Array(vec![Value::Null; n])
                    }
                    _ => Value::Null,
                };
                let body = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                let mut stdin = inner.stdin.lock().unwrap();
                Inner::write_frame(&mut stdin, &body);
            }
            // Response.
            (Some(id), None) => {
                if id == 1 {
                    // The initialize response: complete the handshake, then
                    // release everything parked behind it.
                    let initialized =
                        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })
                            .to_string();
                    {
                        let mut stdin = inner.stdin.lock().unwrap();
                        Inner::write_frame(&mut stdin, &initialized);
                    }
                    inner.mark_ready_and_flush();
                    log::info!("lsp: {} initialized", inner.name);
                    continue;
                }
                if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.try_send(result);
                }
            }
            // Notification.
            (None, Some("textDocument/publishDiagnostics")) => {
                let uri = msg
                    .pointer("/params/uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let Some(path) = path_for_uri(uri) else {
                    continue;
                };
                let diags: Vec<lsp_types::Diagnostic> = msg
                    .pointer("/params/diagnostics")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let _ = diag_tx.try_send((path, diags));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Registry.
// ---------------------------------------------------------------------------

/// Lazily-spawned clients keyed by (server binary, workspace root). A spawn
/// failure is cached as `None` so a missing binary logs once, not per file.
pub(crate) struct LspRegistry {
    clients: HashMap<(String, PathBuf), Option<Rc<LspClient>>>,
    diag_tx: smol::channel::Sender<(PathBuf, Vec<lsp_types::Diagnostic>)>,
}

impl LspRegistry {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        // Diagnostics loop: reader threads push (path, diags); this applies
        // them to the matching open editor on the UI thread.
        let (tx, rx) = smol::channel::unbounded::<(PathBuf, Vec<lsp_types::Diagnostic>)>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok((path, diags)) = rx.recv().await {
                let ok = app.update_in(cx, |app, window, cx| {
                    app.editor_apply_diagnostics(&path, diags, window, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            clients: HashMap::new(),
            diag_tx: tx,
        }
    }

    /// The client for a language at a workspace root, spawning on first use.
    /// Returns the client plus the LSP `languageId` for didOpen.
    pub(crate) fn client_for(
        &mut self,
        language: &str,
        root: &Path,
    ) -> Option<(Rc<LspClient>, &'static str)> {
        let (cmd, language_id) = server_for_language(language)?;
        let key = (cmd[0].to_string(), root.to_path_buf());
        let slot = self.clients.entry(key).or_insert_with(|| {
            match LspClient::spawn(cmd, root, self.diag_tx.clone()) {
                Ok(client) => Some(Rc::new(client)),
                Err(e) => {
                    log::info!("lsp: {} unavailable: {e:#}", cmd[0]);
                    None
                }
            }
        });
        slot.clone().map(|c| (c, language_id))
    }
}

// ---------------------------------------------------------------------------
// Per-file provider bridging to gpui-component's LSP traits.
// ---------------------------------------------------------------------------

/// The provider object installed on an open file's `InputState`.
pub(crate) struct FileLsp {
    pub(crate) client: Rc<LspClient>,
    pub(crate) path: PathBuf,
}

/// Await one LSP response with a timeout, off the UI thread.
async fn recv_with_timeout(rx: smol::channel::Receiver<Value>) -> Result<Value> {
    let timeout = async {
        smol::Timer::after(REQUEST_TIMEOUT).await;
        Err(anyhow!("lsp request timed out"))
    };
    let recv = async { rx.recv().await.map_err(|_| anyhow!("lsp server gone")) };
    smol::future::or(recv, timeout).await
}

impl CompletionProvider for FileLsp {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<lsp_types::CompletionResponse>> {
        let Some(mut params) = LspClient::position_params(&self.path, text, offset) else {
            return Task::ready(Err(anyhow!("bad path")));
        };
        params["context"] = serde_json::to_value(&trigger).unwrap_or(Value::Null);
        let rx = self.client.request("textDocument/completion", params);
        cx.background_executor().spawn(async move {
            let v = recv_with_timeout(rx).await?;
            if v.is_null() {
                return Ok(lsp_types::CompletionResponse::Array(vec![]));
            }
            Ok(serde_json::from_value(v)?)
        })
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'))
    }
}

impl HoverProvider for FileLsp {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>> {
        let Some(params) = LspClient::position_params(&self.path, text, offset) else {
            return Task::ready(Ok(None));
        };
        let rx = self.client.request("textDocument/hover", params);
        cx.background_executor().spawn(async move {
            let v = recv_with_timeout(rx).await?;
            if v.is_null() {
                return Ok(None);
            }
            Ok(serde_json::from_value(v).ok())
        })
    }
}

impl DefinitionProvider for FileLsp {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<lsp_types::LocationLink>>> {
        let Some(params) = LspClient::position_params(&self.path, text, offset) else {
            return Task::ready(Ok(vec![]));
        };
        let Some(this_uri) = uri_for_path(&self.path) else {
            return Task::ready(Ok(vec![]));
        };
        let rx = self.client.request("textDocument/definition", params);
        cx.background_executor().spawn(async move {
            let v = recv_with_timeout(rx).await?;
            let links = normalize_definitions(v);
            // The component's ⌘-click jump applies target offsets to the
            // *current* buffer, so only same-file links are safe to hand it;
            // cross-file jumps go through the F12 action instead.
            Ok(links
                .into_iter()
                .filter(|l| l.target_uri.as_str() == this_uri)
                .collect())
        })
    }
}

/// `textDocument/definition` may answer `Location`, `Location[]` or
/// `LocationLink[]`; normalize all three to links.
pub(crate) fn normalize_definitions(v: Value) -> Vec<lsp_types::LocationLink> {
    if v.is_null() {
        return vec![];
    }
    if let Ok(links) = serde_json::from_value::<Vec<lsp_types::LocationLink>>(v.clone()) {
        return links;
    }
    let to_link = |loc: lsp_types::Location| lsp_types::LocationLink {
        origin_selection_range: None,
        target_uri: loc.uri,
        target_range: loc.range,
        target_selection_range: loc.range,
    };
    if let Ok(locs) = serde_json::from_value::<Vec<lsp_types::Location>>(v.clone()) {
        return locs.into_iter().map(to_link).collect();
    }
    if let Ok(loc) = serde_json::from_value::<lsp_types::Location>(v) {
        return vec![to_link(loc)];
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_paths_with_spaces() {
        let p = Path::new("/tmp/a dir/file.rs");
        let uri = uri_for_path(p).unwrap();
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("a%20dir"));
        assert_eq!(path_for_uri(&uri), Some(p.to_path_buf()));
    }

    #[test]
    fn definition_responses_normalize_all_three_shapes() {
        let loc = json!({ "uri": "file:///a.rs", "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 5 } } });
        // Single Location.
        assert_eq!(normalize_definitions(loc.clone()).len(), 1);
        // Location[].
        assert_eq!(
            normalize_definitions(json!([loc.clone(), loc.clone()])).len(),
            2
        );
        // LocationLink[].
        let link = json!([{ "targetUri": "file:///b.rs",
            "targetRange": loc["range"], "targetSelectionRange": loc["range"] }]);
        let links = normalize_definitions(link);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri.as_str(), "file:///b.rs");
        // Null.
        assert!(normalize_definitions(Value::Null).is_empty());
    }

    #[test]
    fn server_map_covers_the_big_five() {
        for lang in ["rust", "go", "python", "typescript", "cpp"] {
            assert!(server_for_language(lang).is_some(), "{lang}");
        }
        assert!(server_for_language("markdown").is_none());
    }
}

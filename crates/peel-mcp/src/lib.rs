//! MCP over stdio, with all session work dispatched to its owning thread.
//!
//! No network listener is opened. The desktop UI and a headless host use the
//! same tools, transactions, engine synchronization and export lifecycle.

mod actions;
mod compose;
mod control;
mod expression_control;
mod io_control;

pub use actions::{Action, decode};
pub use control::Control;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use peel_session::Session;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

struct Request {
    action: Action,
    reply: oneshot::Sender<Result<Value, String>>,
    context: RequestContext<RoleServer>,
}

/// 要求が届いたことを持ち主のスレッドに知らせる関数。
type WakeFn = Box<dyn Fn() + Send + Sync>;

/// プロトコル側と [`Bridge`] で共有する、後から一度だけ設定できる起こし役。
#[derive(Clone, Default)]
struct WakeSlot(Arc<OnceLock<WakeFn>>);

impl WakeSlot {
    fn wake(&self) {
        if let Some(wake) = self.0.get() {
            wake();
        }
    }
}

/// The UI owns this value and pumps it once per frame. Session and plugin
/// objects never cross to the protocol thread or the realtime audio thread.
pub struct Bridge {
    rx: mpsc::Receiver<Request>,
    control: Control,
    finished: Arc<AtomicBool>,
    disconnected: bool,
    waker: WakeSlot,
}

impl Bridge {
    pub fn start_stdio() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel(32);
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let waker = WakeSlot::default();
        let server_waker = waker.clone();
        std::thread::Builder::new()
            .name("peel-mcp".into())
            .spawn(move || {
                let closed = server_waker.clone();
                let result = (|| -> Result<(), String> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| e.to_string())?;
                    runtime.block_on(async {
                        let service = Server {
                            tx,
                            waker: server_waker,
                        }
                        .serve(rmcp::transport::stdio())
                        .await
                        .map_err(|e| e.to_string())?;
                        service.waiting().await.map_err(|e| e.to_string())?;
                        Ok(())
                    })
                })();
                if let Err(error) = result {
                    eprintln!("Peel MCP: {error}");
                }
                done.store(true, Ordering::Release);
                // 切断の後始末（再生停止など）も、操作を待たずに済ませる。
                closed.wake();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            rx,
            control: Control::default(),
            finished,
            disconnected: false,
            waker,
        })
    }

    /// 要求が届くたび（と切断時）に呼ぶ関数を登録する。描画フレームが
    /// 来ないと [`Bridge::pump`] が呼ばれないデスクトップ版で、イベント
    /// ループを起こすために使う。二度目以降の登録は無視される。
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.waker.0.set(Box::new(wake));
    }

    /// Drain a bounded number of calls so a busy agent cannot starve UI frames.
    pub fn pump(&mut self, session: &mut Session) {
        self.control.tick();
        if self.finished.load(Ordering::Acquire) {
            if !self.disconnected {
                self.rx.close();
                while self.rx.try_recv().is_ok() {}
                self.control.disconnect(session);
                self.disconnected = true;
            }
            return;
        }
        for _ in 0..8 {
            let Ok(request) = self.rx.try_recv() else {
                break;
            };
            if request.reply.is_closed() || request.context.ct.is_cancelled() {
                continue;
            }
            let result = self.control.execute(session, request.action);
            // Synchronize before acknowledging an edit or starting playback.
            session.tick();
            let _ = request.reply.send(result);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.disconnected
    }
}

struct Server {
    tx: mpsc::Sender<Request>,
    waker: WakeSlot,
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("peel-studio", env!("CARGO_PKG_VERSION")))
            .with_instructions("Peel Studio music workstation. Start with peel_help, peel_status and peel_read_project. Times are exact strings in quarter notes (e.g. \"1/3\"). IDs are stable UUIDs. Use peel_devices to discover the current instrument/effect catalog. All edits use the live session's undo history. Read a fresh revision before peel_edit_project. File paths must be absolute. Audio export is asynchronous: poll peel_export_job until done. Save your work before disconnecting.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if !tools().iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params(
                format!("Unknown tool {}", request.name),
                None,
            ));
        }
        let action = match decode(&request.name, request.arguments.unwrap_or_default()) {
            Ok(action) => action,
            Err(error) => {
                return Ok(CallToolResult::structured_error(json!({"error":error})).into());
            }
        };
        let (reply, receive) = oneshot::channel();
        self.tx
            .send(Request {
                action,
                reply,
                context,
            })
            .await
            .map_err(|_| ErrorData::internal_error("The Peel session has closed", None))?;
        self.waker.wake();
        let result = receive
            .await
            .map_err(|_| ErrorData::internal_error("The Peel session has closed", None))?;
        Ok(match result {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(json!({"error":error})),
        }
        .into())
    }
}

/// Derive the advertised schemas and runtime decoding from the same enum.
/// Every variant is a separate, named tool; nested definitions stay local to
/// its schema so clients can resolve $refs without fetching external files.
pub fn tools() -> Vec<Tool> {
    static TOOLS: std::sync::OnceLock<Vec<Tool>> = std::sync::OnceLock::new();
    TOOLS.get_or_init(build_tools).clone()
}

fn build_tools() -> Vec<Tool> {
    let root = serde_json::to_value(schemars::schema_for!(Action)).expect("action schema");
    root["oneOf"]
        .as_array()
        .expect("tagged actions")
        .iter()
        .map(|variant| {
            let name = variant["properties"]["action"]["const"]
                .as_str()
                .expect("action name");
            let description = variant["description"].as_str().unwrap_or(name);
            let mut schema = variant.clone();
            schema.as_object_mut().unwrap().remove("description");
            schema["properties"]
                .as_object_mut()
                .unwrap()
                .remove("action");
            schema["required"]
                .as_array_mut()
                .unwrap()
                .retain(|v| v != "action");
            let mut definitions = serde_json::Map::new();
            collect_definitions(&schema, &root, &mut definitions);
            if !definitions.is_empty() {
                schema["$defs"] = Value::Object(definitions);
            }
            let read_only = matches!(
                name,
                "help"
                    | "status"
                    | "read_project"
                    | "devices"
                    | "new_ids"
                    | "audio_devices"
                    | "output_routes"
                    | "midi_controls"
                    | "browse_files"
                    | "performance_slots"
            );
            let mut tool = Tool::new(
                format!("peel_{name}"),
                description.to_owned(),
                schema.as_object().unwrap().clone(),
            );
            tool.annotations = Some(
                ToolAnnotations::new()
                    .read_only(read_only)
                    .destructive(matches!(
                        name,
                        "edit_project"
                            | "set_notes"
                            | "set_device"
                            | "transform_notes"
                            | "history"
                            | "new_project"
                            | "open_project"
                            | "save_project"
                            | "export_audio"
                            | "export_midi"
                    ))
                    .open_world(false),
            );
            tool
        })
        .collect()
}

// Include only definitions reachable from this tool. Copying the enum's
// entire definition table into every schema wastes a client's context.
fn collect_definitions(value: &Value, root: &Value, output: &mut serde_json::Map<String, Value>) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref").and_then(Value::as_str)
                && let Some(key) = reference.strip_prefix("#/$defs/")
                && !output.contains_key(key)
                && let Some(definition) = root.get("$defs").and_then(|defs| defs.get(key))
            {
                output.insert(key.to_string(), definition.clone());
                collect_definitions(definition, root, output);
            }
            for v in fields.values() {
                collect_definitions(v, root, output);
            }
        }
        Value::Array(values) => {
            for v in values {
                collect_definitions(v, root, output);
            }
        }
        _ => {}
    }
}

/// Silent, display-free host. `audible` explicitly opts into hardware audio.
/// EOF ends the host; no stdin-reading thread is joined during shutdown.
pub fn run_headless(project: Option<std::path::PathBuf>, audible: bool) -> Result<(), String> {
    let project_data = peel_document::Project::empty("Untitled");
    let mut session = if audible {
        Session::new(project_data)
    } else {
        Session::new_silent(project_data)
    };
    if let Some(path) = project {
        session.open(&path)?;
    }
    let mut bridge = Bridge::start_stdio()?;
    while !bridge.is_finished() {
        bridge.pump(&mut session);
        session.tick();
        session.poll_midi();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

//! F46 — driving the fleet.
//!
//! "Start my VR stack" and "what's broken" are already implemented, tested and
//! shipped: they are `nx stack run` and `nx doctor`. This module **wraps the
//! CLI and adds no logic of its own** — no reimplementation of stacks, no
//! second opinion about what a healthy install looks like. If the answer would
//! ever differ from what the operator gets in a terminal, that is a bug here.
//!
//! What this module does add is the three things a tool needs before a language
//! model is allowed near it:
//!
//! * a **descriptor** — name, description, JSON-Schema parameters, consent
//!   level (SPEC §3.7) — that `wisp-mind` can register without knowing anything
//!   about `nx`;
//! * a **record** of every invocation, because SPEC §0.4 says "why did you do
//!   that?" is answerable from data;
//! * **failing safe**: no `nx` on this machine is an ordinary, quiet outcome,
//!   not an error path. She works alone.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wisp_proto::Consent;

/// Nothing the CLI does should take longer than this. `nx stack run` launches
/// apps and returns; it does not wait for them to be useful.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
const LOG_CAP: usize = 64;

/// SPEC §3.7's shapes moved to `wisp-proto` (they are contract, not fleet
/// behaviour); re-exported here so existing imports keep working.
pub use wisp_proto::{ToolDescriptor, ToolFn, ToolFuture, ToolInvocation, ToolOutcome};

type Recorder = Arc<dyn Fn(ToolInvocation) + Send + Sync>;

struct Inner {
    nx: PathBuf,
    timeout: Duration,
    log: Mutex<VecDeque<ToolInvocation>>,
    recorder: Mutex<Option<Recorder>>,
}

/// The `nx` CLI, as a set of tools.
#[derive(Clone)]
pub struct NxTools {
    inner: Arc<Inner>,
}

impl Default for NxTools {
    fn default() -> Self {
        Self::new(crate::hub::nx_binary())
    }
}

impl NxTools {
    pub fn new(nx: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                nx: nx.into(),
                timeout: DEFAULT_TIMEOUT,
                log: Mutex::new(VecDeque::new()),
                recorder: Mutex::new(None),
            }),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        // Only ever called before the tools are shared out.
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.timeout = timeout;
        }
        self
    }

    /// Every invocation is handed to this before the caller sees the outcome.
    pub fn on_record(&self, f: impl Fn(ToolInvocation) + Send + Sync + 'static) {
        *self.inner.recorder.lock().expect("tool recorder poisoned") = Some(Arc::new(f));
    }

    pub fn binary(&self) -> &Path {
        &self.inner.nx
    }

    /// Is the CLI there at all? Cheap, and re-checked on every call anyway —
    /// NX Hub may be installed while she is running.
    pub fn available(&self) -> bool {
        self.inner.nx.is_file()
    }

    /// The most recent invocations, oldest first.
    pub fn recent(&self) -> Vec<ToolInvocation> {
        self.inner.log.lock().expect("tool log poisoned").iter().cloned().collect()
    }

    /// The tools, ready for `wisp-mind` to register with the model.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor {
                name: "nx_status",
                description: "What NX apps are running right now, and their live status \
                              (heart rate, VR session, and so on). Wraps `nx status --json`.",
                consent: Consent::Ambient,
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                invoke: self.bind(Tool::Status),
            },
            ToolDescriptor {
                name: "nx_stack_list",
                description: "List the NX stacks the operator has defined, so a spoken name \
                              like \"my VR stack\" can be resolved to a stack id. \
                              Wraps `nx stack ls --json`.",
                consent: Consent::Ambient,
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                invoke: self.bind(Tool::StackList),
            },
            ToolDescriptor {
                name: "nx_stack_start",
                description: "Start an NX stack — several apps launched together in order. \
                              Use nx_stack_list first to get the id. Wraps `nx stack run <id>`.",
                consent: Consent::Explicit,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "stack": {
                            "type": "string",
                            "description": "Stack id or name, e.g. \"vr\".",
                            "maxLength": 64
                        }
                    },
                    "required": ["stack"],
                    "additionalProperties": false
                }),
                invoke: self.bind(Tool::StackStart),
            },
            ToolDescriptor {
                name: "nx_doctor",
                description: "Check the NX install for problems — what is broken and why. \
                              Wraps `nx doctor --json`. Offline by default; pass \
                              offline=false to let it also refresh from GitHub.",
                consent: Consent::Explicit,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "offline": {
                            "type": "boolean",
                            "description": "Stay off the network (default true).",
                            "default": true
                        }
                    },
                    "additionalProperties": false
                }),
                invoke: self.bind(Tool::Doctor),
            },
        ]
    }

    fn bind(&self, tool: Tool) -> ToolFn {
        let me = self.clone();
        Arc::new(move |args: Value| {
            let me = me.clone();
            Box::pin(async move { me.invoke_tool(tool, args).await }) as ToolFuture
        })
    }

    /// Invoke by name — the path `wisp-mind` takes when the model names a tool.
    pub async fn invoke(&self, name: &str, args: Value) -> ToolOutcome {
        match Tool::from_name(name) {
            Some(tool) => self.invoke_tool(tool, args).await,
            None => ToolOutcome::failed(format!("no such tool: {name}"), None),
        }
    }

    async fn invoke_tool(&self, tool: Tool, args: Value) -> ToolOutcome {
        let argv = match tool.argv(&args) {
            Ok(argv) => argv,
            Err(why) => {
                let outcome = ToolOutcome::failed(why, None);
                self.record(tool.name(), Vec::new(), &outcome);
                return outcome;
            }
        };
        let outcome = self.run(tool, &argv).await;
        self.record(tool.name(), argv, &outcome);
        outcome
    }

    async fn run(&self, tool: Tool, argv: &[String]) -> ToolOutcome {
        // Fail safe, twice: the file may be missing, and it may vanish between
        // the check and the spawn.
        if !self.available() {
            return ToolOutcome::unavailable(format!(
                "NX Hub's `nx` CLI is not installed here ({}), so I can't reach the fleet.",
                self.inner.nx.display()
            ));
        }
        let mut cmd = tokio::process::Command::new(&self.inner.nx);
        cmd.args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The CLI paints a spinner and colours unless told otherwise.
            .env("NO_COLOR", "1")
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutcome::unavailable(
                    "NX Hub's `nx` CLI is not installed here, so I can't reach the fleet."
                        .to_string(),
                );
            }
            Err(e) => return ToolOutcome::failed(format!("could not run nx: {e}"), None),
        };

        let output = match tokio::time::timeout(self.inner.timeout, child.wait_with_output()).await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return ToolOutcome::failed(format!("nx failed: {e}"), None),
            Err(_) => {
                return ToolOutcome::failed(
                    format!("`nx {}` took too long and I gave up on it.", argv.join(" ")),
                    None,
                )
            }
        };

        let code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let json = serde_json::from_str::<Value>(stdout.trim()).ok();
        let ok = output.status.success();
        let summary = if ok {
            tool.summarise(json.as_ref(), argv)
        } else {
            let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if line.is_empty() {
                format!("`nx {}` failed (exit {}).", argv.join(" "), code.unwrap_or(-1))
            } else {
                line.to_string()
            }
        };
        ToolOutcome { ok, unavailable: false, summary, json, exit_code: code }
    }

    fn record(&self, name: &str, argv: Vec<String>, outcome: &ToolOutcome) {
        let entry = ToolInvocation {
            name: name.to_string(),
            argv,
            ok: outcome.ok,
            unavailable: outcome.unavailable,
            detail: outcome.summary.clone(),
        };
        {
            let mut log = self.inner.log.lock().expect("tool log poisoned");
            if log.len() == LOG_CAP {
                log.pop_front();
            }
            log.push_back(entry.clone());
        }
        let recorder = self.inner.recorder.lock().expect("tool recorder poisoned").clone();
        if let Some(f) = recorder {
            f(entry);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Status,
    StackList,
    StackStart,
    Doctor,
}

impl Tool {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "nx_status" => Tool::Status,
            "nx_stack_list" => Tool::StackList,
            "nx_stack_start" => Tool::StackStart,
            "nx_doctor" => Tool::Doctor,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Tool::Status => "nx_status",
            Tool::StackList => "nx_stack_list",
            Tool::StackStart => "nx_stack_start",
            Tool::Doctor => "nx_doctor",
        }
    }

    fn argv(self, args: &Value) -> Result<Vec<String>, String> {
        let s = |v: &str| v.to_string();
        Ok(match self {
            Tool::Status => vec![s("status"), s("--json")],
            Tool::StackList => vec![s("stack"), s("ls"), s("--json")],
            Tool::StackStart => {
                let name = args
                    .get("stack")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "which stack? I need its name.".to_string())?;
                // The model chose this string. It never becomes a shell word —
                // there is no shell — but it must not become a flag either.
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " -_.".contains(c))
                    || name.starts_with('-')
                    || name.len() > 64
                {
                    return Err(format!("\"{name}\" is not a stack name I will pass to nx."));
                }
                // Note: the CLI verb is `run`, not `start`.
                vec![s("stack"), s("run"), name.to_string(), s("--json")]
            }
            Tool::Doctor => {
                let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(true);
                let mut argv = vec![s("doctor"), s("--json")];
                if offline {
                    // SPEC §0.2: no egress she was not asked for. `nx doctor`
                    // refreshes from GitHub unless told not to.
                    argv.push(s("--offline"));
                }
                argv
            }
        })
    }

    /// One sentence about what came back. The model still gets the raw JSON.
    fn summarise(self, json: Option<&Value>, argv: &[String]) -> String {
        let Some(json) = json else {
            return format!("`nx {}` finished.", argv.join(" "));
        };
        match self {
            Tool::Status => {
                let online = json
                    .pointer("/bus/online")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let clients = json.get("clients").and_then(Value::as_array);
                let names: Vec<String> = clients
                    .map(|c| {
                        c.iter()
                            .filter_map(|e| e.get("app").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if !online {
                    "The NX Hub bus is not running.".to_string()
                } else if names.is_empty() {
                    "The bus is up, but nothing else is on it.".to_string()
                } else {
                    format!("On the bus: {}.", names.join(", "))
                }
            }
            Tool::StackList => {
                let stacks: Vec<String> = json
                    .get("stacks")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| {
                                let id = s.get("id").and_then(Value::as_str)?;
                                Some(match s.get("name").and_then(Value::as_str) {
                                    Some(name) if name != id => format!("{id} ({name})"),
                                    _ => id.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if stacks.is_empty() {
                    "There are no stacks defined.".to_string()
                } else {
                    format!("Stacks: {}.", stacks.join(", "))
                }
            }
            Tool::StackStart => {
                let stack = argv.get(2).map(String::as_str).unwrap_or("that stack");
                format!("Started {stack}.")
            }
            Tool::Doctor => {
                let errors = json
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                let updates = json.get("updates").and_then(Value::as_u64).unwrap_or(0);
                match (errors, updates) {
                    (0, 0) => "Everything checks out.".to_string(),
                    (0, n) => format!("Nothing broken; {n} update(s) waiting."),
                    (n, _) => format!("nx doctor found {n} problem(s)."),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_names_that_could_become_flags_are_refused() {
        assert!(Tool::StackStart.argv(&json!({"stack": "--force"})).is_err());
        assert!(Tool::StackStart.argv(&json!({"stack": "vr; rm -rf ~"})).is_err());
        assert!(Tool::StackStart.argv(&json!({"stack": "$(id)"})).is_err());
        assert!(Tool::StackStart.argv(&json!({})).is_err());
        assert_eq!(
            Tool::StackStart.argv(&json!({"stack": "vr"})).unwrap(),
            vec!["stack", "run", "vr", "--json"]
        );
    }

    #[test]
    fn doctor_stays_offline_unless_asked() {
        assert!(Tool::Doctor.argv(&json!({})).unwrap().contains(&"--offline".to_string()));
        assert!(!Tool::Doctor
            .argv(&json!({"offline": false}))
            .unwrap()
            .contains(&"--offline".to_string()));
    }

    #[test]
    fn descriptors_are_well_formed() {
        let tools = NxTools::new("/nonexistent/nx");
        let ds = tools.descriptors();
        assert_eq!(ds.len(), 4);
        for d in &ds {
            assert!(Tool::from_name(d.name).is_some());
            assert_eq!(d.parameters["type"], "object");
            assert!(!d.description.is_empty());
        }
        // Anything that starts other processes needs the operator's say-so.
        let start = ds.iter().find(|d| d.name == "nx_stack_start").unwrap();
        assert_eq!(start.consent, Consent::Explicit);
    }
}

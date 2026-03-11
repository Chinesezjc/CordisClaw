//! Shell plugin: provides a runtime plugin entry that can start a terminal shell.
//! It supports non-interactive mode (`command` set) and interactive mode.

use crate::core::error::RuntimeError;
use crate::plugin::abi::{PluginRequest, PluginResponse, RuntimePlugin};
use crate::plugin::invoke::PluginInvoker;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct ShellPluginRequest {
    pub action: String,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub fixtures_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellPluginResponsePayload {
    pub ok: bool,
    pub action: String,
    pub shell: Option<String>,
    pub exit_code: Option<i32>,
    pub message: String,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShellPlugin {
    fixtures_root: Option<PathBuf>,
}

impl Default for ShellPlugin {
    fn default() -> Self {
        Self { fixtures_root: None }
    }
}

impl ShellPlugin {
    pub fn with_fixtures_root(fixtures_root: impl Into<PathBuf>) -> Self {
        Self {
            fixtures_root: Some(fixtures_root.into()),
        }
    }

    pub fn handle_request(
        &mut self,
        req: ShellPluginRequest,
    ) -> Result<ShellPluginResponsePayload, RuntimeError> {
        match req.action.as_str() {
            "start_terminal" => self.start_terminal(req),
            other => Err(RuntimeError::ShellPluginUnsupportedAction {
                action: other.to_string(),
            }),
        }
    }

    fn start_terminal(
        &self,
        req: ShellPluginRequest,
    ) -> Result<ShellPluginResponsePayload, RuntimeError> {
        if let Some(shell) = &req.shell {
            if shell != "cordis" {
                return Err(RuntimeError::ShellPluginInvalidRequest {
                    message: format!(
                        "only builtin shell is supported, got shell={shell}; remove --shell or use --shell=cordis"
                    ),
                });
            }
        }

        let fixtures_root = req
            .fixtures_root
            .map(PathBuf::from)
            .or_else(|| self.fixtures_root.clone())
            .unwrap_or_else(PluginInvoker::default_fixtures_root);

        let mut shell = BuiltinShell::new(req.cwd, fixtures_root)?;
        if let Some(script) = req.command {
            let run = shell.run_script(&script);
            let ok = run.exit_code == 0;
            Ok(ShellPluginResponsePayload {
                ok,
                action: "start_terminal".to_string(),
                shell: Some("cordis".to_string()),
                exit_code: Some(run.exit_code),
                message: if ok {
                    "builtin shell command completed".to_string()
                } else {
                    format!("builtin shell command failed with exit {}", run.exit_code)
                },
                output: if run.output.is_empty() {
                    None
                } else {
                    Some(run.output)
                },
            })
        } else {
            let code = shell.run_repl()?;
            Ok(ShellPluginResponsePayload {
                ok: code == 0,
                action: "start_terminal".to_string(),
                shell: Some("cordis".to_string()),
                exit_code: Some(code),
                message: "builtin shell session ended".to_string(),
                output: None,
            })
        }
    }
}

impl RuntimePlugin for ShellPlugin {
    fn handle(&mut self, req: PluginRequest) -> PluginResponse {
        let parsed = serde_json::from_str::<ShellPluginRequest>(&req.payload).map_err(|e| {
            RuntimeError::ShellPluginInvalidRequest {
                message: e.to_string(),
            }
        });

        let payload = match parsed.and_then(|x| self.handle_request(x)) {
            Ok(resp) => serde_json::to_string(&resp).unwrap_or_else(|e| {
                format!(
                    "{{\"ok\":false,\"action\":\"serialize\",\"message\":\"{}\"}}",
                    e
                )
            }),
            Err(err) => {
                let resp = ShellPluginResponsePayload {
                    ok: false,
                    action: "error".to_string(),
                    shell: None,
                    exit_code: None,
                    message: err.to_string(),
                    output: None,
                };
                serde_json::to_string(&resp).unwrap_or_else(|e| {
                    format!(
                        "{{\"ok\":false,\"action\":\"serialize\",\"message\":\"{}\"}}",
                        e
                    )
                })
            }
        };

        PluginResponse { payload }
    }
}

#[derive(Debug, Clone)]
struct ScriptRunResult {
    exit_code: i32,
    output: String,
}

#[derive(Debug, Clone)]
struct BuiltinShell {
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    fixtures_root: PathBuf,
}

#[derive(Debug, Clone)]
enum CommandOutcome {
    Continue { exit_code: i32, output: String },
    Exit(i32),
}

impl BuiltinShell {
    fn new(cwd: Option<String>, fixtures_root: PathBuf) -> Result<Self, RuntimeError> {
        let cwd = match cwd {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir().map_err(|e| RuntimeError::Io {
                path: PathBuf::from("."),
                message: e.to_string(),
            })?,
        };

        let mut env = BTreeMap::new();
        env.insert("USER".to_string(), "CordisClaw".to_string());
        env.insert("LOGNAME".to_string(), "CordisClaw".to_string());
        env.insert("USERNAME".to_string(), "CordisClaw".to_string());

        Ok(Self {
            cwd,
            env,
            fixtures_root,
        })
    }

    fn run_script(&mut self, script: &str) -> ScriptRunResult {
        let mut collected = Vec::new();
        for line in split_script_commands(script) {
            let command = line.trim();
            if command.is_empty() {
                continue;
            }
            match self.run_single(command) {
                CommandOutcome::Continue { exit_code, output } => {
                    if !output.is_empty() {
                        collected.push(output);
                    }
                    if exit_code != 0 {
                        return ScriptRunResult {
                            exit_code,
                            output: collected.join("\n"),
                        };
                    }
                }
                CommandOutcome::Exit(code) => {
                    return ScriptRunResult {
                        exit_code: code,
                        output: collected.join("\n"),
                    };
                }
            }
        }
        ScriptRunResult {
            exit_code: 0,
            output: collected.join("\n"),
        }
    }

    fn run_repl(&mut self) -> Result<i32, RuntimeError> {
        let stdin = io::stdin();
        loop {
            print!("CordisClaw@runtime:{}$ ", self.cwd.display());
            io::stdout()
                .flush()
                .map_err(|e| RuntimeError::Io {
                    path: PathBuf::from("<stdout>"),
                    message: e.to_string(),
                })?;

            let mut line = String::new();
            let read = stdin.read_line(&mut line).map_err(|e| RuntimeError::Io {
                path: PathBuf::from("<stdin>"),
                message: e.to_string(),
            })?;
            if read == 0 {
                return Ok(0);
            }

            let command = line.trim();
            if command.is_empty() {
                continue;
            }
            match self.run_single(command) {
                CommandOutcome::Continue { exit_code: _, output } => {
                    if !output.is_empty() {
                        println!("{output}");
                    }
                }
                CommandOutcome::Exit(code) => return Ok(code),
            }
        }
    }

    fn run_single(&mut self, line: &str) -> CommandOutcome {
        let tokens = split_tokens(line);
        if tokens.is_empty() {
            return CommandOutcome::Continue {
                exit_code: 0,
                output: String::new(),
            };
        }

        match tokens[0].as_str() {
            "help" => CommandOutcome::Continue {
                exit_code: 0,
                output: self.help_output(),
            },
            "pwd" => CommandOutcome::Continue {
                exit_code: 0,
                output: self.cwd.display().to_string(),
            },
            "cd" => {
                let Some(path) = tokens.get(1) else {
                    return CommandOutcome::Continue {
                        exit_code: 1,
                        output: "cd: missing path".to_string(),
                    };
                };
                let target = {
                    let p = PathBuf::from(path);
                    if p.is_absolute() {
                        p
                    } else {
                        self.cwd.join(p)
                    }
                };
                if target.is_dir() {
                    self.cwd = target;
                    CommandOutcome::Continue {
                        exit_code: 0,
                        output: String::new(),
                    }
                } else {
                    CommandOutcome::Continue {
                        exit_code: 1,
                        output: format!("cd: no such directory: {}", target.display()),
                    }
                }
            }
            "echo" => CommandOutcome::Continue {
                exit_code: 0,
                output: tokens[1..].join(" "),
            },
            "whoami" => CommandOutcome::Continue {
                exit_code: 0,
                output: self
                    .env
                    .get("USER")
                    .cloned()
                    .unwrap_or_else(|| "CordisClaw".to_string()),
            },
            "env" => {
                let mut out = self
                    .env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>();
                out.sort();
                CommandOutcome::Continue {
                    exit_code: 0,
                    output: out.join("\n"),
                }
            }
            "exit" => {
                let code = tokens
                    .get(1)
                    .and_then(|x| x.parse::<i32>().ok())
                    .unwrap_or(0);
                CommandOutcome::Exit(code)
            }
            other => self.run_plugin_command(other, &tokens[1..]),
        }
    }

    fn help_output(&self) -> String {
        let mut commands = vec![
            "help".to_string(),
            "pwd".to_string(),
            "cd".to_string(),
            "echo".to_string(),
            "whoami".to_string(),
            "env".to_string(),
        ];

        if let Ok(invoker) = PluginInvoker::load(&self.fixtures_root) {
            commands.extend(invoker.available_shell_commands());
        }
        commands.push("exit".to_string());

        format!("builtins: {}", commands.join(", "))
    }

    fn run_plugin_command(&self, command: &str, args: &[String]) -> CommandOutcome {
        let invoker = match PluginInvoker::load(&self.fixtures_root) {
            Ok(invoker) => invoker,
            Err(err) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: format!("{command} error: {err}"),
                };
            }
        };

        let Some(binding) = (match invoker.resolve_shell_command(command) {
            Ok(binding) => binding,
            Err(RuntimeError::ShellPluginInvalidRequest { message }) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: message,
                };
            }
            Err(err) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: format!("{command} error: {err}"),
                };
            }
        }) else {
            return CommandOutcome::Continue {
                exit_code: 127,
                output: format!("command not found: {command}"),
            };
        };

        let payload = match invoker.build_shell_payload(&binding, command, &args.join(" ")) {
            Ok(payload) => payload,
            Err(RuntimeError::ShellPluginInvalidRequest { message }) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: message,
                };
            }
            Err(err) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: format!("{command} error: {err}"),
                };
            }
        };

        let response = match invoker.invoke(&binding.plugin_path, &binding.node.id, payload) {
            Ok(response) => response,
            Err(err) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: format!("{command} error: {err}"),
                };
            }
        };

        format_plugin_response(command, &binding.node.output_schema, &response.payload)
    }
}

fn split_script_commands(script: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in script.lines() {
        for part in line.split(';') {
            out.push(part);
        }
    }
    out
}

fn split_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(current.clone());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            },
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn format_plugin_response(
    command: &str,
    output_schema: &serde_json::Value,
    payload: &str,
) -> CommandOutcome {
    let parsed = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(parsed) => parsed,
        Err(err) => {
            return CommandOutcome::Continue {
                exit_code: 1,
                output: format!("{command} error: invalid plugin response: {err}"),
            };
        }
    };

    if let Some(error) = parsed
        .get("error")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
    {
        return CommandOutcome::Continue {
            exit_code: 1,
            output: format!("{command} error: {error}"),
        };
    }

    let output_fields = schema_property_names(output_schema)
        .into_iter()
        .filter(|field| field != "error")
        .collect::<Vec<_>>();

    if let [field] = output_fields.as_slice() {
        if let Some(value) = parsed.get(field) {
            return CommandOutcome::Continue {
                exit_code: 0,
                output: format!("{}: {}", display_field_name(field), format_json_value(value)),
            };
        }
    }

    if let Some(object) = parsed.as_object() {
        let visible = object
            .iter()
            .filter(|(key, value)| key.as_str() != "error" && !value.is_null())
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return CommandOutcome::Continue {
                exit_code: 1,
                output: format!("{command} error: plugin returned no value"),
            };
        }
        if visible.len() == 1 {
            let (field, value) = visible[0];
            return CommandOutcome::Continue {
                exit_code: 0,
                output: format!("{}: {}", display_field_name(field), format_json_value(value)),
            };
        }
    }

    CommandOutcome::Continue {
        exit_code: 0,
        output: format_json_value(&parsed),
    }
}

fn schema_property_names(schema: &serde_json::Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut names = properties.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names
}

fn display_field_name(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number
            .as_f64()
            .map(format_number)
            .unwrap_or_else(|| number.to_string()),
        serde_json::Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

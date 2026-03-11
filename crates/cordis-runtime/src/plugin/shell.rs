//! Shell plugin: provides a runtime plugin entry that can start a terminal shell.
//! It supports non-interactive mode (`command` set) and interactive mode.

use crate::core::error::RuntimeError;
use crate::plugin::abi::{PluginRequest, PluginResponse, RuntimePlugin};
use cordis_expr_plugin::evaluate_expression;
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

#[derive(Debug, Default, Clone)]
pub struct ShellPlugin;

impl ShellPlugin {
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

        let mut shell = BuiltinShell::new(req.cwd)?;
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
}

#[derive(Debug, Clone)]
enum CommandOutcome {
    Continue { exit_code: i32, output: String },
    Exit(i32),
}

impl BuiltinShell {
    fn new(cwd: Option<String>) -> Result<Self, RuntimeError> {
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

        Ok(Self { cwd, env })
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
                output: "builtins: help, pwd, cd, echo, whoami, env, Expr, exit".to_string(),
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
            "Expr" | "expr" => {
                if tokens.len() < 2 {
                    return CommandOutcome::Continue {
                        exit_code: 1,
                        output: "Expr: missing expression".to_string(),
                    };
                }
                let expression = tokens[1..].join(" ");
                match evaluate_expression(&expression) {
                    Ok(value) => CommandOutcome::Continue {
                        exit_code: 0,
                        output: format!("Value: {}", format_number(value)),
                    },
                    Err(err) => CommandOutcome::Continue {
                        exit_code: 1,
                        output: format!("Expr error: {err}"),
                    },
                }
            }
            "exit" => {
                let code = tokens
                    .get(1)
                    .and_then(|x| x.parse::<i32>().ok())
                    .unwrap_or(0);
                CommandOutcome::Exit(code)
            }
            other => CommandOutcome::Continue {
                exit_code: 127,
                output: format!("command not found: {other}"),
            },
        }
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

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

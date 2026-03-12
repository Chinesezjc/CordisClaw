use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, NodeDoc, PluginDocs,
    PluginRequest, PluginResponse,
};
use cordis_plugin_host::{default_fixtures_root, CatalogPlugin, PluginCatalog};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
struct ShellPluginRequest {
    action: String,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    fixtures_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ShellPluginResponsePayload {
    ok: bool,
    action: String,
    shell: Option<String>,
    exit_code: Option<i32>,
    message: String,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Clone)]
struct ShellPlugin {
    fixtures_root: Option<PathBuf>,
}

impl Default for ShellPlugin {
    fn default() -> Self {
        Self { fixtures_root: None }
    }
}

impl ShellPlugin {
    fn handle_request(
        &mut self,
        req: ShellPluginRequest,
    ) -> Result<ShellPluginResponsePayload, String> {
        match req.action.as_str() {
            "start_terminal" => self.start_terminal(req),
            other => Err(format!("shell plugin unsupported action: {other}")),
        }
    }

    fn start_terminal(&self, req: ShellPluginRequest) -> Result<ShellPluginResponsePayload, String> {
        if let Some(shell) = &req.shell {
            if shell != "cordis" {
                return Err(format!(
                    "shell plugin invalid request: only builtin shell is supported, got shell={shell}; remove --shell or use --shell=cordis"
                ));
            }
        }

        let fixtures_root = req
            .fixtures_root
            .map(PathBuf::from)
            .or_else(|| self.fixtures_root.clone())
            .unwrap_or_else(default_fixtures_root);

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

#[derive(Debug, Clone)]
struct ScriptRunResult {
    exit_code: i32,
    output: String,
}

#[derive(Debug, Clone)]
struct BuiltinShell {
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    plugin_catalog: Option<PluginCatalog>,
    plugin_catalog_error: Option<String>,
}

#[derive(Debug, Clone)]
enum CommandOutcome {
    Continue { exit_code: i32, output: String },
    Exit(i32),
}

impl BuiltinShell {
    fn new(cwd: Option<String>, fixtures_root: PathBuf) -> Result<Self, String> {
        let cwd = match cwd {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir().map_err(|e| format!("I/O at .: {e}"))?,
        };

        let mut env = BTreeMap::new();
        env.insert("USER".to_string(), "CordisClaw".to_string());
        env.insert("LOGNAME".to_string(), "CordisClaw".to_string());
        env.insert("USERNAME".to_string(), "CordisClaw".to_string());

        let (plugin_catalog, plugin_catalog_error) = match PluginCatalog::load(&fixtures_root) {
            Ok(catalog) => (Some(catalog), None),
            Err(err) => (None, Some(err.to_string())),
        };

        Ok(Self {
            cwd,
            env,
            plugin_catalog,
            plugin_catalog_error,
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

    fn run_repl(&mut self) -> Result<i32, String> {
        let stdin = io::stdin();
        loop {
            print!("CordisClaw@runtime:{}$ ", self.cwd.display());
            io::stdout()
                .flush()
                .map_err(|e| format!("I/O at <stdout>: {e}"))?;

            let mut line = String::new();
            let read = stdin
                .read_line(&mut line)
                .map_err(|e| format!("I/O at <stdin>: {e}"))?;
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

        if let Some(catalog) = &self.plugin_catalog {
            commands.extend(available_shell_commands(catalog));
        }
        commands.push("exit".to_string());

        format!("builtins: {}", commands.join(", "))
    }

    fn run_plugin_command(&self, command: &str, args: &[String]) -> CommandOutcome {
        let Some(catalog) = &self.plugin_catalog else {
            let message = self
                .plugin_catalog_error
                .clone()
                .unwrap_or_else(|| "plugin catalog unavailable".to_string());
            return CommandOutcome::Continue {
                exit_code: 1,
                output: format!("{command} error: {message}"),
            };
        };

        let Some(binding) = (match resolve_shell_command(catalog, command) {
            Ok(binding) => binding,
            Err(message) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: message,
                };
            }
        }) else {
            return CommandOutcome::Continue {
                exit_code: 127,
                output: format!("command not found: {command}"),
            };
        };

        let payload = match build_shell_payload(&binding.node, command, &args.join(" ")) {
            Ok(payload) => payload,
            Err(message) => {
                return CommandOutcome::Continue {
                    exit_code: 1,
                    output: message,
                };
            }
        };

        let response = match catalog.invoke(&binding.plugin_path, &binding.node.id, payload) {
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

#[derive(Debug, Clone)]
struct ShellCommandBinding {
    plugin_path: String,
    display_name: String,
    node: NodeDoc,
}

fn available_shell_commands(plugin_catalog: &PluginCatalog) -> Vec<String> {
    let mut commands = plugin_catalog
        .plugins()
        .filter_map(shell_command_binding)
        .map(|binding| binding.display_name)
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

fn resolve_shell_command(plugin_catalog: &PluginCatalog, command: &str) -> Result<Option<ShellCommandBinding>, String> {
    let mut matches = Vec::new();
    for plugin in plugin_catalog.plugins() {
        if let Some(binding) = shell_command_binding(plugin) {
            if binding.display_name.eq_ignore_ascii_case(command) {
                matches.push(binding);
            }
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            let plugin_paths = matches
                .iter()
                .map(|binding| binding.plugin_path.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "{command}: command is ambiguous across plugins: {plugin_paths}"
            ))
        }
    }
}

fn shell_command_binding(plugin: &CatalogPlugin) -> Option<ShellCommandBinding> {
    if plugin.plugin_path == "shell" {
        return None;
    }
    let display_name = plugin.docs.command_name.clone()?;
    if plugin.docs.nodes.len() != 1 {
        return None;
    }

    Some(ShellCommandBinding {
        plugin_path: plugin.plugin_path.clone(),
        display_name,
        node: plugin.docs.nodes[0].clone(),
    })
}

fn build_shell_payload(node: &NodeDoc, display_name: &str, raw_args: &str) -> Result<String, String> {
    let input_fields = schema_property_names(&node.input_schema);
    let required_fields = required_field_names(&node.input_schema);
    let trimmed = raw_args.trim();

    match input_fields.as_slice() {
        [] => {
            if trimmed.is_empty() {
                Ok("{}".to_string())
            } else {
                Err(format!("{display_name}: unexpected arguments"))
            }
        }
        [field] => {
            if trimmed.is_empty() {
                if required_fields.contains(field) {
                    Err(format!("{display_name}: missing {field}"))
                } else {
                    Ok("{}".to_string())
                }
            } else {
                Ok(json!({ field: trimmed }).to_string())
            }
        }
        _ => Err(format!(
            "{display_name}: plugin command requires {} input fields; builtin shell supports only one",
            input_fields.len()
        )),
    }
}

fn format_plugin_response(command: &str, output_schema: &Value, payload: &str) -> CommandOutcome {
    let parsed = match serde_json::from_str::<Value>(payload) {
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

fn schema_property_names(schema: &Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut names = properties.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names
}

fn required_field_names(schema: &Value) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut names = required
        .iter()
        .filter_map(|value| value.as_str().map(ToString::to_string))
        .collect::<Vec<_>>();
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

fn format_json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number
            .as_f64()
            .map(format_number)
            .unwrap_or_else(|| number.to_string()),
        Value::String(text) => text.clone(),
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

fn docs_value() -> PluginDocs {
    plugin_docs(
        "shell",
        "shell",
        "0.1.0",
        None,
        vec![node_doc(
            "shell_entry",
            "Start the CordisClaw terminal in interactive or scripted mode.",
            json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string" },
                    "shell": { "type": "string" },
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "fixtures_root": { "type": "string" }
                }
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "action": { "type": "string" },
                    "shell": { "type": ["string", "null"] },
                    "exit_code": { "type": ["integer", "null"] },
                    "message": { "type": "string" },
                    "output": { "type": ["string", "null"] }
                }
            }),
            &["reads stdin", "writes stdout"],
            &["unsupported action", "invalid shell backend", "command not found"],
        )],
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_shell_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let parsed = serde_json::from_str::<ShellPluginRequest>(&req.payload)
        .map_err(|e| format!("shell plugin invalid request: {e}"));

    match parsed.and_then(|request| ShellPlugin::default().handle_request(request)) {
        Ok(resp) => json_response(&resp),
        Err(message) => {
            let resp = ShellPluginResponsePayload {
                ok: false,
                action: "error".to_string(),
                shell: None,
                exit_code: None,
                message,
                output: None,
            };
            json_response(&resp)
        }
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}

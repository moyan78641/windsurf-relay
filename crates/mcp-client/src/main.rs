mod protocol;
mod windsurf;
mod prompt;
mod executor;

use std::path::PathBuf;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Catch panics so the process doesn't silently die
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[mcp-client] PANIC: {}", info);
    }));
    run_mcp_server().await
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum TransportMode { Lsp, Line }

fn is_header_line(line: &str) -> bool {
    match line.split_once(':') {
        Some((name, _)) => {
            let name = name.trim();
            name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("content-type")
        }
        None => false,
    }
}

/// Read LSP-framed message (Content-Length header + body)
async fn read_lsp_message(reader: &mut BufReader<tokio::io::Stdin>, first_line: Option<&str>) -> anyhow::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    let mut seen_header = false;

    // Parse first_line if provided (from auto-detection)
    if let Some(fl) = first_line {
        seen_header = true;
        if let Some((name, value)) = fl.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(len) = value.trim().parse::<usize>() {
                    content_length = Some(len);
                }
            }
        }
    }

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 { return Ok(None); }
        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            if seen_header { break; }
            continue;
        }
        seen_header = true;
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(len) = value.trim().parse::<usize>() {
                    content_length = Some(len);
                }
            }
        }
    }

    let length = content_length.ok_or_else(|| anyhow::anyhow!("Missing Content-Length"))?;
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf).await?;
    Ok(Some(String::from_utf8(buf)?))
}

/// Read a single line JSON message (Line mode)
async fn read_line_message(reader: &mut BufReader<tokio::io::Stdin>) -> anyhow::Result<Option<String>> {
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 { return Ok(None); }
        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() { continue; }
        return Ok(Some(trimmed.to_string()));
    }
}

/// Auto-detect transport mode and read message
async fn read_message(reader: &mut BufReader<tokio::io::Stdin>, mode: &mut Option<TransportMode>) -> anyhow::Result<Option<String>> {
    match mode {
        Some(TransportMode::Line) => read_line_message(reader).await,
        Some(TransportMode::Lsp) => read_lsp_message(reader, None).await,
        None => {
            // Auto-detect: read first non-empty line
            loop {
                let mut line = String::new();
                let bytes = reader.read_line(&mut line).await?;
                if bytes == 0 { return Ok(None); }
                let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                if trimmed.is_empty() { continue; }

                if is_header_line(trimmed) {
                    // LSP mode
                    *mode = Some(TransportMode::Lsp);
                    return read_lsp_message(reader, Some(trimmed)).await;
                } else {
                    // Line mode — this line IS the JSON message
                    *mode = Some(TransportMode::Line);
                    return Ok(Some(trimmed.to_string()));
                }
            }
        }
    }
}

async fn write_message(stdout: &mut tokio::io::Stdout, mode: TransportMode, payload: &str) -> anyhow::Result<()> {
    match mode {
        TransportMode::Lsp => {
            let header = format!("Content-Length: {}\r\n\r\n", payload.len());
            stdout.write_all(header.as_bytes()).await?;
            stdout.write_all(payload.as_bytes()).await?;
        }
        TransportMode::Line => {
            stdout.write_all(payload.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
        }
    }
    stdout.flush().await?;
    Ok(())
}

async fn run_mcp_server() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut transport_mode: Option<TransportMode> = None;

    let relay_url = std::env::var("RELAY_URL")
        .unwrap_or_else(|_| "http://localhost:3000".into());
    let access_token = std::env::var("ACCESS_TOKEN")
        .or_else(|_| std::env::var("WINDSURF_API_KEY"))
        .unwrap_or_default();
    let client = reqwest::Client::builder()
        .build()?;

    loop {
        let message = match read_message(&mut reader, &mut transport_mode).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                eprintln!("[mcp-client] stdin EOF, exiting");
                break;
            }
            Err(e) => {
                eprintln!("[mcp-client] read error: {}", e);
                continue;
            }
        };

        if message.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&message) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[mcp-client] JSON parse error: {}", e);
                continue;
            }
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let id = request.get("id").cloned();

        // Notifications (no id) — don't respond
        if id.is_none() {
            continue;
        }

        let response = match method.as_str() {
            "initialize" => handle_initialize(&request),
            "tools/list" => handle_tools_list(&request),
            "tools/call" => {
                handle_tools_call(&request, &client, &relay_url, &access_token).await
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {}", method) }
            }),
        };

        // Write response — if this fails, log but don't exit
        match serde_json::to_string(&response) {
            Ok(resp_json) => {
                let mode = transport_mode.unwrap_or(TransportMode::Line);
                if let Err(e) = write_message(&mut stdout, mode, &resp_json).await {
                    eprintln!("[mcp-client] write error: {}, but continuing...", e);
                }
            }
            Err(e) => {
                eprintln!("[mcp-client] serialize error: {}", e);
            }
        }
        eprintln!("[mcp-client] responded to method={}, loop continues", method);
    }

    Ok(())
}

fn handle_initialize(msg: &Value) -> Value {
    let id = msg.get("id").cloned().unwrap_or(json!(null));
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "windsurf-relay-mcp",
                "version": "0.1.0"
            }
        }
    })
}

fn handle_tools_list(msg: &Value) -> Value {
    let id = msg.get("id").cloned().unwrap_or(json!(null));

    let tools = vec![json!({
        "name": "fast_context_search",
        "description": "AI-driven semantic code search. Searches a codebase with natural language and returns relevant file paths with line ranges, plus suggested grep keywords.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural language search query" },
                "project_path": { "type": "string", "description": "Absolute path to project root. Empty = cwd.", "default": "" },
                "tree_depth": { "type": "integer", "description": "Directory tree depth (1-6, default 3)", "default": 3, "minimum": 1, "maximum": 6 },
                "max_turns": { "type": "integer", "description": "Search rounds (1-5, default 5)", "default": 5, "minimum": 1, "maximum": 5 },
                "max_results": { "type": "integer", "description": "Max files to return (1-30, default 10)", "default": 10, "minimum": 1, "maximum": 30 }
            },
            "required": ["query"]
        }
    })];

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tools }
    })
}

async fn handle_tools_call(
    msg: &Value,
    client: &reqwest::Client,
    relay_url: &str,
    access_token: &str,
) -> Value {
    let id = msg.get("id").cloned().unwrap_or(json!(null));
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    if tool_name != "fast_context_search" {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": format!("Unknown tool: {}", tool_name) }
        });
    }

    let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let project_path = args.get("project_path").and_then(|p| p.as_str()).unwrap_or("");
    let tree_depth = args.get("tree_depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
    let max_turns = args.get("max_turns").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
    let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10) as u32;

    let project_root = if project_path.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).to_string_lossy().to_string()
    } else {
        project_path.to_string()
    };

    match do_search(client, relay_url, access_token, query, &project_root, tree_depth, max_turns, max_results).await {
        Ok(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": text }] }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": format!("Error: {}", e) }], "isError": true }
        }),
    }
}

/// Report search log to relay server (fire-and-forget)
async fn report_log(
    client: &reqwest::Client,
    relay_url: &str,
    access_token: &str,
    query: &str,
    status: &str,
    error_msg: &str,
    duration_ms: i64,
) {
    let _ = client
        .post(&format!("{}/api/windsurf/log", relay_url))
        .bearer_auth(access_token)
        .json(&json!({
            "query": query,
            "status": status,
            "error_msg": error_msg,
            "duration_ms": duration_ms,
            "provider": "windsurf",
        }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

async fn do_search(
    client: &reqwest::Client,
    relay_url: &str,
    access_token: &str,
    query: &str,
    project_root: &str,
    tree_depth: u32,
    max_turns: u32,
    max_results: u32,
) -> anyhow::Result<String> {
    let max_commands: u32 = 8;
    let start = std::time::Instant::now();

    let creds: Value = client
        .post(&format!("{}/api/windsurf/credentials", relay_url))
        .bearer_auth(access_token)
        .send()
        .await?
        .json()
        .await?;

    if let Some(err) = creds.get("error") {
        let msg = err.as_str().unwrap_or("Authentication failed");
        report_log(client, relay_url, access_token, query, "error", msg, start.elapsed().as_millis() as i64).await;
        anyhow::bail!("{}", msg);
    }

    let api_key = creds["api_key"].as_str().ok_or_else(|| anyhow::anyhow!("No api_key"))?;
    let jwt = creds["jwt"].as_str().ok_or_else(|| anyhow::anyhow!("No jwt"))?;
    let ws_cfg = windsurf::WindsurfConfig {
        api_base: creds["windsurf_config"]["api_base"].as_str().unwrap_or("").into(),
        auth_base: creds["windsurf_config"]["auth_base"].as_str().unwrap_or("").into(),
        app_version: creds["windsurf_config"]["app_version"].as_str().unwrap_or("").into(),
        ls_version: creds["windsurf_config"]["ls_version"].as_str().unwrap_or("").into(),
        model: creds["windsurf_config"]["model"].as_str().unwrap_or("").into(),
        timeout_ms: creds["windsurf_config"]["timeout_ms"].as_u64().unwrap_or(30000),
    };

    let repo_map = generate_repo_map(project_root, tree_depth);
    let system_prompt = prompt::build_system_prompt(max_turns, max_commands, max_results);
    let user_content = format!(
        "Problem Statement: {}\n\nRepo Map (tree -L {} /codebase):\n```text\n{}\n```",
        query, tree_depth, repo_map
    );
    let tool_defs = prompt::get_tool_definitions(max_commands);

    let mut messages = vec![
        windsurf::ChatMessage { role: 5, content: system_prompt, tool_call_id: None, tool_name: None, tool_args_json: None, ref_call_id: None },
        windsurf::ChatMessage { role: 1, content: user_content, tool_call_id: None, tool_name: None, tool_args_json: None, ref_call_id: None },
    ];

    let mut exec = executor::ToolExecutor::new(project_root);
    let total_api_calls = max_turns + 1;

    for turn in 0..total_api_calls {
        let proto = windsurf::build_request(&ws_cfg, api_key, jwt, &messages, &tool_defs);
        let resp_data = match windsurf::streaming_request(client, &ws_cfg, &proto).await {
            Ok(data) => data,
            Err(e) => {
                let msg = format!("Windsurf API error: {}", e);
                report_log(client, relay_url, access_token, query, "error", &msg, start.elapsed().as_millis() as i64).await;
                anyhow::bail!("{}", msg);
            }
        };

        let (thinking, tool_info) = windsurf::parse_response(&resp_data);

        match tool_info {
            None => {
                if thinking.starts_with("[Error]") {
                    report_log(client, relay_url, access_token, query, "error", &thinking, start.elapsed().as_millis() as i64).await;
                    anyhow::bail!("{}", thinking);
                }
                report_log(client, relay_url, access_token, query, "success", "", start.elapsed().as_millis() as i64).await;
                return Ok(format!("No relevant files found.\n\nRaw: {}", thinking));
            }
            Some((name, args)) => {
                if name == "answer" {
                    let answer_xml = args.get("answer").and_then(|v| v.as_str()).unwrap_or("");
                    let result = format_answer(answer_xml, project_root, &exec.collected_rg_patterns, tree_depth, max_turns);
                    report_log(client, relay_url, access_token, query, "success", "", start.elapsed().as_millis() as i64).await;
                    return Ok(result);
                }
                if name == "restricted_exec" {
                    let call_id = uuid::Uuid::new_v4().to_string();
                    let args_json = serde_json::to_string(&args)?;
                    let results = exec.exec_tool_call(&args).await;

                    messages.push(windsurf::ChatMessage {
                        role: 2, content: thinking,
                        tool_call_id: Some(call_id.clone()),
                        tool_name: Some("restricted_exec".into()),
                        tool_args_json: Some(args_json),
                        ref_call_id: None,
                    });
                    messages.push(windsurf::ChatMessage {
                        role: 4, content: results,
                        tool_call_id: None, tool_name: None, tool_args_json: None,
                        ref_call_id: Some(call_id),
                    });

                    if turn >= max_turns - 1 {
                        messages.push(windsurf::ChatMessage {
                            role: 1, content: prompt::FINAL_FORCE_ANSWER.into(),
                            tool_call_id: None, tool_name: None, tool_args_json: None, ref_call_id: None,
                        });
                    }
                }
            }
        }
    }

    report_log(client, relay_url, access_token, query, "timeout", "max turns", start.elapsed().as_millis() as i64).await;

    // Fallback: build answer from files the AI read during search
    if !exec.collected_files.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let mut parts = Vec::new();
        let files: Vec<&String> = exec.collected_files.iter()
            .filter(|f| seen.insert(f.to_string()))
            .collect();
        let n = files.len();
        parts.push(format!("Found {} files (max turns reached, partial result).", n));
        parts.push(String::new());
        for (i, f) in files.iter().enumerate() {
            let rel = f.replace("/codebase/", "");
            let full = PathBuf::from(project_root).join(&rel);
            parts.push(format!("  [{}/{}] {}", i + 1, n, full.to_string_lossy()));
        }
        let unique_rg: Vec<&String> = exec.collected_rg_patterns.iter()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .filter(|p| p.len() >= 3)
            .collect();
        if !unique_rg.is_empty() {
            parts.push(String::new());
            let kw: Vec<&str> = unique_rg.iter().map(|s| s.as_str()).collect();
            parts.push(format!("grep keywords: {}", kw.join(", ")));
        }
        parts.push(String::new());
        parts.push(format!("[config] tree_depth={}, max_turns={} (timeout fallback)", tree_depth, max_turns));
        return Ok(parts.join("\n"));
    }

    Ok("Max turns reached without answer".into())
}

fn generate_repo_map(project_root: &str, target_depth: u32) -> String {
    let root = PathBuf::from(project_root);
    let mut lines = vec!["/codebase".to_string()];
    tree_walk_for_map(&root, "", target_depth as usize, 0, &mut lines);
    let result = lines.join("\n");
    if result.len() > 250 * 1024 && target_depth > 1 {
        return generate_repo_map(project_root, target_depth - 1);
    }
    result
}

fn tree_walk_for_map(dir: &std::path::Path, prefix: &str, max_depth: usize, depth: usize, lines: &mut Vec<String>) {
    if depth >= max_depth || lines.len() > 2000 { return; }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    let skip = ["node_modules", ".git", "dist", "build", "target", ".venv", "__pycache__", "vendor", ".cache"];
    let filtered: Vec<_> = entries.into_iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            !name.starts_with('.') && !skip.contains(&name.as_str())
        })
        .collect();
    let count = filtered.len();
    for (i, entry) in filtered.iter().enumerate() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_last = i == count - 1;
        let connector = if is_last { "└── " } else { "├── " };
        lines.push(format!("{}{}{}", prefix, connector, name));
        if entry.path().is_dir() {
            let new_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
            tree_walk_for_map(&entry.path(), &new_prefix, max_depth, depth + 1, lines);
        }
    }
}

fn format_answer(xml: &str, project_root: &str, rg_patterns: &[String], tree_depth: u32, max_turns: u32) -> String {
    let file_re = regex_lite::Regex::new(r#"<file\s+path="([^"]+)">([\s\S]*?)</file>"#).unwrap();
    let range_re = regex_lite::Regex::new(r"<range>(\d+)-(\d+)</range>").unwrap();
    let mut files = Vec::new();
    for cap in file_re.captures_iter(xml) {
        let rel = cap[1].replace("/codebase/", "");
        let full_path = PathBuf::from(project_root).join(&rel);
        let ranges: Vec<String> = range_re.captures_iter(&cap[2])
            .map(|rc| format!("L{}-{}", &rc[1], &rc[2]))
            .collect();
        files.push((full_path.to_string_lossy().to_string(), ranges.join(", ")));
    }
    let mut parts = Vec::new();
    let n = files.len();
    if n > 0 {
        parts.push(format!("Found {} relevant files.", n));
        parts.push(String::new());
        for (i, (path, ranges)) in files.iter().enumerate() {
            parts.push(format!("  [{}/{}] {} ({})", i + 1, n, path, ranges));
        }
    } else {
        parts.push("No relevant files found.".into());
    }
    let unique: Vec<&String> = rg_patterns.iter()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .filter(|p| p.len() >= 3)
        .collect();
    if !unique.is_empty() {
        parts.push(String::new());
        let kw: Vec<&str> = unique.iter().map(|s| s.as_str()).collect();
        parts.push(format!("grep keywords: {}", kw.join(", ")));
    }
    parts.push(String::new());
    parts.push(format!("[config] tree_depth={}, max_turns={}", tree_depth, max_turns));
    parts.join("\n")
}

//! Windsurf API 交互层 (standalone, no server deps)

use anyhow::Result;
use uuid::Uuid;
use super::protocol::*;

const WS_APP: &str = "windsurf";

#[derive(Debug, Clone)]
pub struct WindsurfConfig {
    pub api_base: String,
    pub auth_base: String,
    pub app_version: String,
    pub ls_version: String,
    pub model: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: u64,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args_json: Option<String>,
    pub ref_call_id: Option<String>,
}

pub fn build_metadata(cfg: &WindsurfConfig, api_key: &str, jwt: &str) -> ProtobufEncoder {
    let mut meta = ProtobufEncoder::new();
    meta.write_string(1, WS_APP);
    meta.write_string(2, &cfg.app_version);
    meta.write_string(3, api_key);
    meta.write_string(4, "zh-cn");

    let sys_info = serde_json::json!({
        "Os": std::env::consts::OS,
        "Arch": std::env::consts::ARCH,
        "Release": "", "Version": "", "Machine": std::env::consts::ARCH,
        "Nodename": "relay",
        "Sysname": if cfg!(target_os = "macos") { "Darwin" }
                   else if cfg!(target_os = "windows") { "Windows_NT" }
                   else { "Linux" },
        "ProductVersion": "",
    });
    meta.write_string(5, &sys_info.to_string());
    meta.write_string(7, &cfg.ls_version);

    let cpu_info = serde_json::json!({
        "NumSockets": 1, "NumCores": num_cpus(), "NumThreads": num_cpus(),
        "VendorID": "", "Family": "0", "Model": "0", "ModelName": "Unknown", "Memory": 0,
    });
    meta.write_string(8, &cpu_info.to_string());
    meta.write_string(12, WS_APP);
    meta.write_string(21, jwt);
    meta.write_bytes(30, &[0x00, 0x01]);
    meta
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

pub fn build_request(
    cfg: &WindsurfConfig,
    api_key: &str,
    jwt: &str,
    messages: &[ChatMessage],
    tool_defs: &str,
) -> Vec<u8> {
    let mut req = ProtobufEncoder::new();
    let meta = build_metadata(cfg, api_key, jwt);
    req.write_message(1, &meta);

    for m in messages {
        let msg = build_chat_message(
            m.role, &m.content,
            m.tool_call_id.as_deref(), m.tool_name.as_deref(),
            m.tool_args_json.as_deref(), m.ref_call_id.as_deref(),
        );
        req.write_message(2, &msg);
    }

    req.write_string(3, tool_defs);
    req.to_vec()
}

fn build_chat_message(
    role: u64, content: &str,
    tool_call_id: Option<&str>, tool_name: Option<&str>,
    tool_args_json: Option<&str>, ref_call_id: Option<&str>,
) -> ProtobufEncoder {
    let mut msg = ProtobufEncoder::new();
    msg.write_varint(2, role);
    msg.write_string(3, content);
    if let (Some(tc_id), Some(tn), Some(ta)) = (tool_call_id, tool_name, tool_args_json) {
        let mut tc = ProtobufEncoder::new();
        tc.write_string(1, tc_id);
        tc.write_string(2, tn);
        tc.write_string(3, ta);
        msg.write_message(6, &tc);
    }
    if let Some(ref_id) = ref_call_id {
        msg.write_string(7, ref_id);
    }
    msg
}

pub async fn streaming_request(
    client: &reqwest::Client,
    cfg: &WindsurfConfig,
    proto_bytes: &[u8],
) -> Result<Vec<u8>> {
    let frame = connect_frame_encode(proto_bytes);
    let url = format!("{}/GetDevstralStream", cfg.api_base);
    let trace_id = Uuid::new_v4().to_string().replace("-", "");
    let span_id = &Uuid::new_v4().to_string().replace("-", "")[..16];

    let resp = client
        .post(&url)
        .header("Content-Type", "application/connect+proto")
        .header("Connect-Protocol-Version", "1")
        .header("Connect-Accept-Encoding", "gzip")
        .header("Connect-Content-Encoding", "gzip")
        .header("Connect-Timeout-Ms", cfg.timeout_ms.to_string())
        .header("User-Agent", "connect-go/1.18.1 (go1.25.5)")
        .header("Accept-Encoding", "identity")
        .header("Baggage", format!(
            "sentry-release=language-server-windsurf@{},sentry-environment=stable,sentry-sampled=false,sentry-trace_id={},sentry-public_key=b813f73488da69eedec534dba1029111",
            cfg.ls_version, trace_id
        ))
        .header("Sentry-Trace", format!("{}-{}-0", trace_id, span_id))
        .timeout(std::time::Duration::from_millis(cfg.timeout_ms + 5000))
        .body(frame)
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status().as_u16());
    }
    let data = resp.bytes().await?;
    Ok(data.to_vec())
}

pub fn parse_response(data: &[u8]) -> (String, Option<(String, serde_json::Value)>) {
    let frames = connect_frame_decode(data);
    let mut all_text = String::new();

    for frame_data in &frames {
        if let Ok(text) = std::str::from_utf8(frame_data) {
            if text.starts_with('{') {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(text) {
                    if let Some(err) = obj.get("error") {
                        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
                        return (format!("[Error] {}: {}", code, msg), None);
                    }
                }
            }
        }

        let raw_text = String::from_utf8_lossy(frame_data).replace('\u{FFFD}', "");
        if raw_text.contains("[TOOL_CALLS]") {
            all_text = raw_text.to_string();
            break;
        }
        for s in extract_strings(frame_data) {
            if s.len() > 10 { all_text.push_str(&s); }
        }
    }

    if let Some(parsed) = parse_tool_call(&all_text) {
        return (parsed.0, Some((parsed.1, parsed.2)));
    }
    eprintln!("[mcp-client] no tool call parsed. all_text length={}, has [TOOL_CALLS]={}", all_text.len(), all_text.contains("[TOOL_CALLS]"));
    if all_text.len() < 2000 {
        eprintln!("[mcp-client] all_text: {}", all_text);
    }
    (all_text, None)
}

fn parse_tool_call(text: &str) -> Option<(String, String, serde_json::Value)> {
    let text = text.replace("</s>", "");
    let idx = text.find("[TOOL_CALLS]")?;
    let after = &text[idx + 12..];
    let args_idx = after.find("[ARGS]")?;
    let name = after[..args_idx].trim().to_string();
    let raw = after[args_idx + 6..].trim();

    let mut depth = 0i32;
    let mut end = 0;
    for (i, c) in raw.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => { depth -= 1; if depth == 0 { end = i + 1; break; } }
            _ => {}
        }
    }
    if end == 0 { end = raw.len(); }

    let json_str = &raw[..end];

    // Try parsing as-is first
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(json_str) {
        return Some((text[..idx].trim().to_string(), name, args));
    }

    // JSON repair: try appending missing closing braces
    let mut repaired = json_str.to_string();
    let open = repaired.chars().filter(|c| *c == '{').count();
    let close = repaired.chars().filter(|c| *c == '}').count();
    if open > close {
        for _ in 0..(open - close) {
            repaired.push('}');
        }
        eprintln!("[mcp-client] repaired JSON: added {} closing braces", open - close);
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&repaired) {
            return Some((text[..idx].trim().to_string(), name, args));
        }
    }

    // Last resort: extract individual commandN objects that are valid JSON
    let mut salvaged = serde_json::Map::new();
    let cmd_re = regex_lite::Regex::new(r#""(command\d+)"\s*:\s*\{"#).ok()?;
    for m in cmd_re.find_iter(json_str) {
        let start = json_str[m.start()..].find('{').map(|i| i + m.start())?;
        let mut d = 0i32;
        let mut e = start;
        for (i, c) in json_str[start..].char_indices() {
            match c {
                '{' => d += 1,
                '}' => { d -= 1; if d == 0 { e = start + i + 1; break; } }
                _ => {}
            }
        }
        if d == 0 {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&json_str[start..e]) {
                let key_end = json_str[m.start()..].find('"').unwrap_or(0) + m.start() + 1;
                let key_start = m.start() + 1;
                if key_start < key_end && key_end < json_str.len() {
                    let key = &json_str[key_start..key_end - 1];
                    if key.starts_with("command") {
                        salvaged.insert(key.to_string(), obj);
                    }
                }
            }
        }
    }
    if !salvaged.is_empty() {
        eprintln!("[mcp-client] salvaged {} commands from malformed JSON", salvaged.len());
        return Some((text[..idx].trim().to_string(), name, serde_json::Value::Object(salvaged)));
    }

    eprintln!("[mcp-client] tool call JSON parse failed after all repairs");
    eprintln!("[mcp-client] raw (first 500): {}", &json_str[..json_str.len().min(500)]);
    None
}

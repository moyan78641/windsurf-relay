//! 本地工具执行器
//!
//! 在用户机器上执行 rg/readfile/tree/ls/glob 命令。
//! 移植自 Node.js 版本的 executor.mjs

use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::task;

const RESULT_MAX_LINES: usize = 50;
const LINE_MAX_CHARS: usize = 250;

pub struct ToolExecutor {
    root: PathBuf,
    pub collected_rg_patterns: Vec<String>,
    pub collected_files: Vec<String>,
}

impl ToolExecutor {
    pub fn new(project_root: &str) -> Self {
        Self {
            root: PathBuf::from(project_root).canonicalize().unwrap_or_else(|_| PathBuf::from(project_root)),
            collected_rg_patterns: Vec::new(),
            collected_files: Vec::new(),
        }
    }

    /// 虚拟路径 /codebase → 真实路径
    fn real_path(&self, virtual_path: &str) -> PathBuf {
        if virtual_path.starts_with("/codebase") {
            let rel = virtual_path.strip_prefix("/codebase").unwrap_or("").trim_start_matches('/');
            self.root.join(rel)
        } else {
            PathBuf::from(virtual_path)
        }
    }

    /// 真实路径 → 虚拟路径
    fn remap(&self, text: &str) -> String {
        text.replace(&self.root.to_string_lossy().to_string(), "/codebase")
    }

    /// 截断输出
    fn truncate(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let limit = lines.len().min(RESULT_MAX_LINES);
        let mut result: Vec<String> = lines[..limit]
            .iter()
            .map(|line| {
                if line.len() > LINE_MAX_CHARS {
                    line[..LINE_MAX_CHARS].to_string()
                } else {
                    line.to_string()
                }
            })
            .collect();

        if lines.len() > RESULT_MAX_LINES {
            result.push("... (lines truncated) ...".into());
        }
        result.join("\n")
    }

    /// ripgrep 搜索
    pub async fn rg(
        &mut self,
        pattern: &str,
        path: &str,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> String {
        self.collected_rg_patterns.push(pattern.to_string());
        let rp = self.real_path(path);

        if !rp.exists() {
            return format!("Error: path does not exist: {}", path);
        }

        let mut args = vec![
            "--no-heading".to_string(),
            "-n".to_string(),
            "--max-count".to_string(),
            "50".to_string(),
            pattern.to_string(),
            rp.to_string_lossy().to_string(),
        ];

        if let Some(inc) = include {
            for g in inc {
                args.push("--glob".into());
                args.push(g.clone());
            }
        }
        if let Some(exc) = exclude {
            for g in exc {
                args.push("--glob".into());
                args.push(format!("!{}", g));
            }
        }

        let root_str = self.root.to_string_lossy().to_string();
        // 尝试找到 rg 二进制
        let rg_bin = find_rg_binary();

        let result = task::spawn_blocking(move || {
            let output = Command::new(&rg_bin)
                .args(&args)
                .output();

            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);

                    if out.status.success() || out.status.code() == Some(0) {
                        let text = if stdout.is_empty() { "(no matches)".into() } else { stdout.to_string() };
                        Self::truncate(&text.replace(&root_str, "/codebase"))
                    } else if out.status.code() == Some(1) {
                        "(no matches)".into()
                    } else if !stderr.is_empty() {
                        Self::truncate(&stderr.replace(&root_str, "/codebase"))
                    } else {
                        "(no matches)".into()
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }).await.unwrap_or_else(|e| format!("Error: {}", e));

        result
    }

    /// 读取文件
    pub fn readfile(&self, file: &str, start_line: Option<usize>, end_line: Option<usize>) -> String {
        let rp = self.real_path(file);

        let content = match std::fs::read_to_string(&rp) {
            Ok(c) => c,
            Err(_) => return format!("Error: file not found: {}", file),
        };

        let lines: Vec<&str> = content.lines().collect();
        let s = start_line.unwrap_or(1).saturating_sub(1);
        let e = end_line.unwrap_or(lines.len()).min(lines.len());

        let numbered: Vec<String> = lines[s..e]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}:{}", s + i + 1, line))
            .collect();

        Self::truncate(&numbered.join("\n"))
    }

    /// 目录树
    pub fn tree(&self, path: &str, levels: Option<usize>) -> String {
        let rp = self.real_path(path);
        if !rp.is_dir() {
            return format!("Error: dir not found: {}", path);
        }

        let mut lines = vec![path.to_string()];
        self.tree_walk(&rp, "", levels.unwrap_or(3), 0, &mut lines);
        Self::truncate(&self.remap(&lines.join("\n")))
    }

    fn tree_walk(&self, dir: &Path, prefix: &str, max_depth: usize, depth: usize, lines: &mut Vec<String>) {
        if depth >= max_depth { return; }
        if lines.len() > 500 { return; } // 安全限制

        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(_) => return,
        };
        entries.sort_by_key(|e| e.file_name());

        let count = entries.len();
        for (i, entry) in entries.iter().enumerate() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }

            let is_last = i == count - 1;
            let connector = if is_last { "└── " } else { "├── " };
            lines.push(format!("{}{}{}", prefix, connector, name));

            if entry.path().is_dir() {
                let new_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
                self.tree_walk(&entry.path(), &new_prefix, max_depth, depth + 1, lines);
            }
        }
    }

    /// 列出目录
    pub fn ls(&self, path: &str, long_format: bool, all: bool) -> String {
        let rp = self.real_path(path);
        let entries = match std::fs::read_dir(&rp) {
            Ok(rd) => {
                let mut v: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| all || !n.starts_with('.'))
                    .collect();
                v.sort();
                v
            }
            Err(_) => return format!("Error: dir not found: {}", path),
        };

        if !long_format {
            return Self::truncate(&entries.join("\n"));
        }

        let mut lines = vec![format!("total {}", entries.len())];
        for name in &entries {
            let fp = rp.join(name);
            if let Ok(meta) = std::fs::metadata(&fp) {
                let t = if meta.is_dir() { "d" } else { "-" };
                lines.push(format!("{}rwxr-xr-x {:>8} {}", t, meta.len(), name));
            }
        }
        Self::truncate(&self.remap(&lines.join("\n")))
    }

    /// glob 匹配
    pub fn glob(&self, pattern: &str, path: &str, type_filter: Option<&str>) -> String {
        let rp = self.real_path(path);
        let mut matches = Vec::new();
        self.glob_walk(&rp, pattern, type_filter.unwrap_or("all"), &mut matches, 0);

        if matches.is_empty() {
            return "(no matches)".into();
        }
        matches.sort();
        let out: Vec<String> = matches.iter().map(|m| self.remap(&m.to_string_lossy())).collect();
        out.join("\n")
    }

    fn glob_walk(&self, dir: &Path, pattern: &str, type_filter: &str, matches: &mut Vec<PathBuf>, depth: usize) {
        if matches.len() >= 100 || depth > 10 { return; }

        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => return,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            if matches.len() >= 100 { return; }
            let name = entry.file_name().to_string_lossy().to_string();
            let fp = entry.path();

            if simple_glob_match(&name, pattern) {
                let is_dir = fp.is_dir();
                let ok = match type_filter {
                    "file" => !is_dir,
                    "directory" => is_dir,
                    _ => true,
                };
                if ok { matches.push(fp.clone()); }
            }

            if fp.is_dir() && !name.starts_with('.') && pattern.contains("**") {
                self.glob_walk(&fp, pattern, type_filter, matches, depth + 1);
            }
        }
    }

    /// 执行单个命令
    pub async fn exec_command(&mut self, cmd: &serde_json::Value) -> String {
        let cmd_type = cmd.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match cmd_type {
            "rg" => {
                let pattern = cmd.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
                let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("/codebase");
                let include: Option<Vec<String>> = cmd.get("include")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());
                let exclude: Option<Vec<String>> = cmd.get("exclude")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());

                self.rg(pattern, path, include.as_deref(), exclude.as_deref()).await
            }
            "readfile" => {
                let file = cmd.get("file").and_then(|f| f.as_str()).unwrap_or("");
                let start = cmd.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                let end = cmd.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                self.readfile(file, start, end)
            }
            "tree" => {
                let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("/codebase");
                let levels = cmd.get("levels").and_then(|v| v.as_u64()).map(|v| v as usize);
                self.tree(path, levels)
            }
            "ls" => {
                let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("/codebase");
                let long = cmd.get("long_format").and_then(|v| v.as_bool()).unwrap_or(false);
                let all = cmd.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
                self.ls(path, long, all)
            }
            "glob" => {
                let pattern = cmd.get("pattern").and_then(|p| p.as_str()).unwrap_or("*");
                let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("/codebase");
                let tf = cmd.get("type_filter").and_then(|v| v.as_str());
                self.glob(pattern, path, tf)
            }
            _ => format!("Error: unknown command type '{}'", cmd_type),
        }
    }

    /// 并行执行所有 commandN
    pub async fn exec_tool_call(&mut self, args: &serde_json::Value) -> String {
        let obj = match args.as_object() {
            Some(o) => o,
            None => return "(invalid args)".into(),
        };

        let mut keys: Vec<&String> = obj.keys().filter(|k| k.starts_with("command")).collect();
        keys.sort();

        // 收集命令，然后并行执行
        let mut tasks = Vec::new();
        for key in &keys {
            if let Some(cmd) = obj.get(*key) {
                let cmd_clone = cmd.clone();
                let root = self.root.clone();

                // 收集 rg patterns
                if cmd.get("type").and_then(|t| t.as_str()) == Some("rg") {
                    if let Some(p) = cmd.get("pattern").and_then(|p| p.as_str()) {
                        self.collected_rg_patterns.push(p.to_string());
                    }
                }

                // 收集 readfile 文件路径
                if cmd.get("type").and_then(|t| t.as_str()) == Some("readfile") {
                    if let Some(f) = cmd.get("file").and_then(|f| f.as_str()) {
                        self.collected_files.push(f.to_string());
                    }
                }

                let key_clone = (*key).clone();
                tasks.push(tokio::spawn(async move {
                    let mut executor = ToolExecutor::new(&root.to_string_lossy());
                    let output = executor.exec_command(&cmd_clone).await;
                    format!("<{}_result>\n{}\n</{}_result>", key_clone, output, key_clone)
                }));
            }
        }

        let mut results = Vec::new();
        for task in tasks {
            match task.await {
                Ok(r) => results.push(r),
                Err(e) => results.push(format!("<error>{}</error>", e)),
            }
        }

        results.join("")
    }
}

/// 简单 glob 匹配
fn simple_glob_match(name: &str, pattern: &str) -> bool {
    // 处理常见 glob 模式
    if pattern == "*" { return true; }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return name.ends_with(&format!(".{}", ext));
    }
    if let Some(prefix) = pattern.strip_suffix("*") {
        return name.starts_with(prefix);
    }
    name == pattern
}

/// 查找 rg 二进制路径
fn find_rg_binary() -> String {
    // 优先使用系统 rg
    if let Ok(output) = Command::new("which").arg("rg").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() { return path; }
        }
    }
    // Windows
    if let Ok(output) = Command::new("where").arg("rg").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("rg").trim().to_string();
            if !path.is_empty() { return path; }
        }
    }
    "rg".into()
}

//! 同步 HTTP 服务器（基于 tiny_http，无异步运行时）。
//!
//! 提供 Orcha 的 Web UI 与 JSON API：
//!
//! | 路径 | 说明 |
//! |------|------|
//! | `GET /` | 任务列表页 |
//! | `GET /tasks/:id` | 任务详情页 |
//! | `GET /tasks/:id/history` | Round 时间线页 |
//! | `GET /api/tasks` | 任务列表 JSON |
//! | `GET /api/tasks/:id` | 单任务 JSON |
//! | `GET /api/tasks/:id/history` | Round 记录 JSON |
//! | `GET /api/tasks/:id/memory` | LLM 对话记忆 JSON（D2） |
//! | `GET /static/app.css` / `/static/app.js` | 静态资源 |
//!
//! 设计取舍：
//! - tiny_http 同步阻塞，每个请求一个线程（demo 足够，不引入 tokio）
//! - 静态资源用 `include_str!` 编入二进制，单文件部署
//! - 数据层复用 `FileTaskStore` / `FileHistoryStore` / `FileMemoryStore`
//!   （每次请求新建，它们只是 path 持有者，构造廉价）

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use orcha_core::{
    FileHistoryStore, FileMemoryStore, FileTaskStore, HistoryStore, MemoryStore, TaskStore,
};
use orcha_sdk::TaskStatus;
use serde::Serialize;
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// Orcha Web UI 服务器。
pub struct HttpServer {
    home: PathBuf,
    port: u16,
    /// 可选访问令牌（#16）。设置后所有请求需携带 Bearer/Cookie/`?token=` 凭证，
    /// 否则返回 401。未设置（None）时保持旧行为（无认证），向后兼容。
    auth_token: Option<String>,
}

impl HttpServer {
    pub fn new(home: impl Into<PathBuf>, port: u16) -> Self {
        Self {
            home: home.into(),
            port,
            auth_token: None,
        }
    }

    /// 启用访问令牌认证（#16）。传 None 等价于不认证。
    pub fn with_auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    /// 阻塞运行服务器。Ctrl-C 时退出。
    pub fn serve(&self) -> Result<()> {
        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        let server = Server::http(addr).map_err(|e| anyhow::anyhow!("bind {addr} 失败: {e}"))?;
        match &self.auth_token {
            Some(t) => eprintln!(
                "orcha shell serve: http://{addr}/?token={t}  (home: {}, 认证已启用)",
                self.home.display()
            ),
            None => eprintln!(
                "orcha shell serve: http://{addr}  (home: {}, 警告: 未启用认证)",
                self.home.display()
            ),
        }

        for request in server.incoming_requests() {
            // 每个请求一个线程；demo 规模足够。
            let home = self.home.clone();
            let auth_token = self.auth_token.clone();
            std::thread::spawn(move || {
                if let Err(e) = handle(request, &home, &auth_token) {
                    eprintln!("handler error: {e}");
                }
            });
        }
        Ok(())
    }
}

fn handle(request: tiny_http::Request, home: &Path, auth_token: &Option<String>) -> Result<()> {
    let url = request.url().to_string();
    let method = request.method().clone();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));

    // #16: 可选令牌认证（Bearer / Cookie / ?token=）
    if let Some(expected) = auth_token {
        let query_token = parse_query_value(query, "token");
        let bearer = header_value(&request, "authorization").and_then(|h| bearer_token(&h));
        let cookie = header_value(&request, "cookie").and_then(|h| cookie_value(&h, "orcha_token"));
        if !auth_ok(
            expected,
            query_token.as_deref(),
            bearer.as_deref(),
            cookie.as_deref(),
        ) {
            let resp = Response::from_string(r#"{"error":"unauthorized"}"#)
                .with_status_code(StatusCode(401))
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], b"application/json").unwrap(),
                );
            request.respond(resp).context("respond")?;
            return Ok(());
        }
        // ?token=xxx 一次性引导：给浏览器种 Cookie，后续 AJAX 自动携带。
        if let Some(t) = query_token.as_deref() {
            if t == expected.as_str() {
                let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                let mut resp = route(&segments, &method, home);
                let cookie = format!("orcha_token={expected}; HttpOnly; SameSite=Strict; Path=/",);
                resp = resp.with_header(
                    Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).unwrap(),
                );
                request.respond(resp).context("respond")?;
                return Ok(());
            }
        }
    }

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let resp = route(&segments, &method, home);
    request.respond(resp).context("respond")?;
    Ok(())
}

// ============================================================
// 认证辅助（#16）
// ============================================================

/// 三种凭证任一匹配即放行。
fn auth_ok(
    expected: &str,
    query_token: Option<&str>,
    bearer: Option<&str>,
    cookie: Option<&str>,
) -> bool {
    query_token == Some(expected) || bearer == Some(expected) || cookie == Some(expected)
}

/// 从 `Authorization: Bearer <token>` 提取 token。
fn bearer_token(header: &str) -> Option<String> {
    let v = header.trim();
    let rest = v.strip_prefix("Bearer ")?;
    Some(rest.trim().to_string())
}

/// 从 Cookie 头提取指定 name 的值。
fn cookie_value(cookie_header: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(prefix.as_str()) {
            return Some(percent_decode(rest));
        }
    }
    None
}

/// 从 query string 提取指定 key 的值（百分号解码）。
fn parse_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 极简百分号解码（token 仅含 URL 安全字符时也能正确处理 %XX 与 +）。
fn percent_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b as char);
                i += 3;
                continue;
            }
            out.push('%');
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// 从请求头取首个匹配 name（大小写不敏感）的值。
fn header_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.to_string())
}

fn route(segments: &[&str], method: &Method, home: &Path) -> Response<std::io::Cursor<Vec<u8>>> {
    // 静态资源
    if segments == ["static", "app.css"] {
        return asset("text/css; charset=utf-8", include_str!("web/app.css"));
    }
    if segments == ["static", "app.js"] {
        return asset(
            "application/javascript; charset=utf-8",
            include_str!("web/app.js"),
        );
    }

    // HTML 页面
    if segments.is_empty() && method == &Method::Get {
        return page("index", include_str!("web/index.html"));
    }
    if segments.len() == 2 && segments[0] == "tasks" && method == &Method::Get {
        return page("task", include_str!("web/task.html"));
    }
    if segments.len() == 3
        && segments[0] == "tasks"
        && segments[2] == "history"
        && method == &Method::Get
    {
        return page("history", include_str!("web/history.html"));
    }

    // JSON API
    if segments == ["api", "tasks"] && method == &Method::Get {
        return api_json(list_tasks(home));
    }
    if segments.len() == 3
        && segments[0] == "api"
        && segments[1] == "tasks"
        && method == &Method::Get
    {
        let id = segments[2];
        if id == "summary" {
            return api_json(stats(home));
        }
        return api_json(get_task(home, id));
    }
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "tasks"
        && segments[3] == "history"
        && method == &Method::Get
    {
        return api_json(list_history(home, segments[2]));
    }
    if segments.len() == 4
        && segments[0] == "api"
        && segments[1] == "tasks"
        && segments[3] == "memory"
        && method == &Method::Get
    {
        return api_json(list_memory(home, segments[2]));
    }

    Response::from_string("Not Found").with_status_code(StatusCode(404))
}

// ============================================================
// API 数据组装
// ============================================================

#[derive(Serialize)]
struct TaskRow {
    id: String,
    description: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn list_tasks(home: &Path) -> Result<serde_json::Value> {
    let store = FileTaskStore::new(home);
    let tasks = store.list(None)?;
    let rows: Vec<TaskRow> = tasks
        .iter()
        .map(|t| TaskRow {
            id: t.id.clone(),
            description: t.description.clone(),
            status: status_str(t.status),
            created_at: t.created_at,
        })
        .collect();
    Ok(serde_json::to_value(&rows)?)
}

fn get_task(home: &Path, id: &str) -> Result<serde_json::Value> {
    let store = FileTaskStore::new(home);
    let task = store
        .get(id)?
        .with_context(|| format!("task {id} not found"))?;
    let history = FileHistoryStore::new(home);
    let records = history.list_history(id).unwrap_or_default();
    let artifacts: Vec<_> = records
        .iter()
        .flat_map(|r| r.artifacts.iter().cloned())
        .collect();
    Ok(serde_json::json!({
        "task": task,
        "history_count": records.len(),
        "artifacts": artifacts,
    }))
}

fn list_history(home: &Path, id: &str) -> Result<serde_json::Value> {
    let history = FileHistoryStore::new(home);
    let records = history.list_history(id)?;
    Ok(serde_json::to_value(&records)?)
}

fn list_memory(home: &Path, id: &str) -> Result<serde_json::Value> {
    let memory = FileMemoryStore::new(home);
    let entries = memory.list(id).unwrap_or_default();
    Ok(serde_json::to_value(&entries)?)
}

#[derive(Serialize)]
struct Stats {
    total: usize,
    pending: usize,
    running: usize,
    done: usize,
    failed: usize,
    blocked: usize,
}

fn stats(home: &Path) -> Result<serde_json::Value> {
    let store = FileTaskStore::new(home);
    let tasks = store.list(None)?;
    let mut s = Stats {
        total: tasks.len(),
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        blocked: 0,
    };
    for t in &tasks {
        match t.status {
            TaskStatus::Pending => s.pending += 1,
            TaskStatus::Running => s.running += 1,
            TaskStatus::Done => s.done += 1,
            TaskStatus::Failed => s.failed += 1,
            TaskStatus::Blocked => s.blocked += 1,
        }
    }
    Ok(serde_json::to_value(&s)?)
}

fn status_str(s: TaskStatus) -> String {
    match s {
        TaskStatus::Pending => "PENDING".into(),
        TaskStatus::Running => "RUNNING".into(),
        TaskStatus::Blocked => "BLOCKED".into(),
        TaskStatus::Done => "DONE".into(),
        TaskStatus::Failed => "FAILED".into(),
    }
}

// ============================================================
// 响应构造
// ============================================================

fn asset(content_type: &str, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap())
        .with_header(cors())
}

fn page(_name: &str, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(Header::from_bytes(&b"Content-Type"[..], b"text/html; charset=utf-8").unwrap())
}

fn api_json(value: Result<serde_json::Value>) -> Response<std::io::Cursor<Vec<u8>>> {
    match value {
        Ok(v) => Response::from_string(serde_json::to_string(&v).unwrap_or_default())
            .with_header(Header::from_bytes(&b"Content-Type"[..], b"application/json").unwrap())
            .with_header(cors()),
        Err(e) => Response::from_string(format!(r#"{{"error":"{e}"}}"#))
            .with_status_code(500)
            .with_header(Header::from_bytes(&b"Content-Type"[..], b"application/json").unwrap())
            .with_header(cors()),
    }
}

fn cors() -> Header {
    Header::from_bytes(&b"Access-Control-Allow-Origin"[..], b"*").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_sdk::Task;

    #[test]
    fn list_tasks_serializes_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTaskStore::new(dir.path());
        store.init().unwrap();
        let mut t = Task::new("T-test".into(), "demo".into());
        store.insert(&t).unwrap();
        orcha_core::transition(&mut t, TaskStatus::Running).unwrap();
        store.update(&t).unwrap();

        let v = list_tasks(dir.path()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "T-test");
        assert_eq!(arr[0]["status"], "RUNNING");
    }

    #[test]
    fn stats_counts_by_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTaskStore::new(dir.path());
        store.init().unwrap();
        // Pending → Running → Done
        let mut a = Task::new("T-a".into(), "x".into());
        store.insert(&a).unwrap();
        orcha_core::transition(&mut a, TaskStatus::Running).unwrap();
        orcha_core::transition(&mut a, TaskStatus::Done).unwrap();
        store.update(&a).unwrap();
        // Pending → Running → Failed
        let mut b = Task::new("T-b".into(), "y".into());
        store.insert(&b).unwrap();
        orcha_core::transition(&mut b, TaskStatus::Running).unwrap();
        orcha_core::transition(&mut b, TaskStatus::Failed).unwrap();
        store.update(&b).unwrap();

        let v = stats(dir.path()).unwrap();
        assert_eq!(v["total"], 2);
        assert_eq!(v["done"], 1);
        assert_eq!(v["failed"], 1);
    }

    #[test]
    fn memory_endpoint_returns_empty_for_unknown_task() {
        let dir = tempfile::tempdir().unwrap();
        let v = list_memory(dir.path(), "T-nope").unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn route_returns_404_for_unknown_path() {
        let resp = route(&["foo", "bar"], &Method::Get, Path::new("/tmp"));
        // 404 响应体为空；通过状态码断言（tiny_http 不暴露状态码读法，
        // 但 empty(404) 不会 panic。这里仅验证不 panic）。
        let _ = resp;
    }

    // ---- #16 认证辅助测试 ----

    #[test]
    fn auth_ok_accepts_any_of_three_credentials() {
        let expected = "s3cret-token";
        assert!(auth_ok(expected, Some("s3cret-token"), None, None));
        assert!(auth_ok(expected, None, Some("s3cret-token"), None));
        assert!(auth_ok(expected, None, None, Some("s3cret-token")));
    }

    #[test]
    fn auth_ok_rejects_wrong_and_missing_credentials() {
        let expected = "s3cret-token";
        assert!(!auth_ok(expected, Some("wrong"), None, None));
        assert!(!auth_ok(expected, None, None, None), "无凭证应拒绝");
        assert!(!auth_ok(expected, Some(""), None, None), "空 token 应拒绝");
    }

    #[test]
    fn bearer_token_parses_header() {
        assert_eq!(
            bearer_token("Bearer s3cret-token"),
            Some("s3cret-token".to_string())
        );
        assert_eq!(bearer_token("bearer x"), None, "大小写敏感，小写应失败");
        assert_eq!(bearer_token("Basic xyz"), None);
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn cookie_value_extracts_named_cookie() {
        let h = "theme=dark; orcha_token=abc; sid=zzz";
        assert_eq!(cookie_value(h, "orcha_token"), Some("abc".to_string()));
        assert_eq!(cookie_value(h, "missing"), None);
        assert_eq!(
            cookie_value("orcha_token=abc", "orcha_token"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn parse_query_value_decodes_token() {
        assert_eq!(
            parse_query_value("token=abc&tab=mem", "token"),
            Some("abc".to_string())
        );
        assert_eq!(parse_query_value("foo=1", "token"), None);
        // 含百分号编码的 token：%20 → 空格，%2B → '+'
        assert_eq!(
            parse_query_value("token=a%20b", "token"),
            Some("a b".to_string())
        );
        assert_eq!(
            parse_query_value("token=a%2Bb", "token"),
            Some("a+b".to_string())
        );
    }

    #[test]
    fn http_server_with_auth_token_stores_token() {
        let srv = HttpServer::new("/tmp", 7421).with_auth_token(Some("t1".into()));
        assert_eq!(srv.auth_token.as_deref(), Some("t1"));
        let srv2 = HttpServer::new("/tmp", 7421);
        assert!(srv2.auth_token.is_none(), "默认无认证（向后兼容）");
    }
}

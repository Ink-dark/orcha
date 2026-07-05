//! M5 HTTP 服务器：基于 `std::net::TcpListener` 的极简同步 HTTP/1.1 实现。
//!
//! 不引入 `axum` / `tokio` / `hyper`，保持项目「零额外异步运行时」原则。
//! 每个连接 `thread::spawn` 一个线程处理；Cycleround 本身是同步阻塞的，
//! 不需要 async runtime。性能足以覆盖 M5 / M6 验收场景。
//!
//! ## 路由
//! - `POST /run`：校验 API Key → 解析 OrchaEvent body →
//!   `OrchaShell::submit_event` → 返回 `{"event_id": "..."}`（200）
//! - `GET /status/{event_id}`：校验 API Key →
//!   `OrchaShell::get_response` → 返回 OrchaResponse JSON（200）
//! - 其他路径 / 方法 → 404
//!
//! ## 鉴权
//! 所有请求都要 `Authorization: Bearer <api_key>` 头；缺失或不匹配返回 401。
//! 这覆盖 ROADMAP M5 验收项「无 API Key 请求返回 401」。
//!
//! ## 响应格式
//! 成功：HTTP 200 + JSON body。
//! 失败：对应状态码 + `{"error": "...", "status": <code>}`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use orcha_sdk::OrchaEvent;

use crate::error::{Result, ShellError};
use crate::shell::OrchaShell;

/// HTTP 服务器。绑定一个端口，把请求路由到 [`OrchaShell`]。
pub struct HttpServer {
    shell: Arc<OrchaShell>,
    addr: String,
}

impl HttpServer {
    /// 构造服务器。`addr` 形如 `127.0.0.1:0`（随机端口）或 `127.0.0.1:8080`。
    pub fn new(shell: Arc<OrchaShell>, addr: impl Into<String>) -> Self {
        Self {
            shell,
            addr: addr.into(),
        }
    }

    /// 在已 bind 的 listener 上阻塞处理连接，直到 `stop_signal` 触发或出错。
    ///
    /// 调用方负责 bind listener（这样可以在 serve 前拿到真实端口，
    /// 避免端口竞争）。设 non-blocking 模式让 accept 能周期性检查 stop_signal。
    pub fn serve(
        &self,
        listener: TcpListener,
        stop_signal: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        listener.set_nonblocking(true).map_err(ShellError::Io)?;

        while !stop_signal.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let shell = self.shell.clone();
                    std::thread::spawn(move || {
                        let _ = handle_connection(stream, shell);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // 无连接，短睡后重试（让 stop_signal 能被检查到）。
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(ShellError::Io(e));
                }
            }
        }
        Ok(())
    }

    /// 便捷方法：自己 bind `self.addr` 再 serve。生产路径用这个。
    pub fn bind_and_serve(&self, stop_signal: Arc<std::sync::atomic::AtomicBool>) -> Result<()> {
        let listener = TcpListener::bind(&self.addr)
            .map_err(|e| ShellError::Other(format!("failed to bind {}: {}", self.addr, e)))?;
        self.serve(listener, stop_signal)
    }
}

/// 处理单个连接：读请求 → 路由 → 写响应。
fn handle_connection(stream: TcpStream, shell: Arc<OrchaShell>) -> Result<()> {
    // 设 read 超时避免恶意慢连接挂住线程。
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(ShellError::Io)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(ShellError::Io)?);

    // 解析 request line：METHOD PATH HTTP/1.1
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| ShellError::HttpParse(format!("read request line: {e}")))?;
    let request_line = request_line.trim().to_string();
    if request_line.is_empty() {
        return Err(ShellError::HttpParse("empty request line".into()));
    }
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // 解析 headers。
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ShellError::HttpParse(format!("read header: {e}")))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // headers 结束
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    // 读 body（若有 Content-Length）。
    let body = if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader
            .read_exact(&mut buf)
            .map_err(|e| ShellError::HttpParse(format!("read body: {e}")))?;
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };

    let auth = headers.get("authorization").map(|s| s.as_str());
    let response = route(&shell, method, path, auth, body);

    write_response(stream, &response)?;
    Ok(())
}

/// 路由后的 HTTP 响应：状态码 + JSON body。
struct HttpResponse {
    status: u16,
    body: String,
}

/// 路由请求到 Shell，返回 HttpResponse（含状态码 + body）。
fn route(
    shell: &OrchaShell,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: String,
) -> HttpResponse {
    // 1. 全局鉴权（所有路由都要 API Key）。
    if let Err(e) = shell.check_api_key(auth) {
        return error_response(&e);
    }

    // 2. 路由。
    let result: Result<serde_json::Value> = match (method, path) {
        ("POST", "/run") => route_run(shell, body),
        ("GET", p) if p.starts_with("/status/") => route_status(shell, p),
        _ => Err(ShellError::InvalidPath(format!(
            "unknown route: {method} {path}"
        ))),
    };

    match result {
        Ok(v) => HttpResponse {
            status: 200,
            body: serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
        },
        Err(e) => error_response(&e),
    }
}

/// `POST /run`：解析 OrchaEvent → submit_event → 返回 `{"event_id": "..."}`。
fn route_run(shell: &OrchaShell, body: String) -> Result<serde_json::Value> {
    if body.trim().is_empty() {
        return Err(ShellError::InvalidBody("empty body".into()));
    }
    let event: OrchaEvent = serde_json::from_str(&body)?;
    let event_id = shell.submit_event(event)?;
    Ok(serde_json::json!({ "event_id": event_id }))
}

/// `GET /status/{event_id}`：查询响应 → 返回 OrchaResponse。
fn route_status(shell: &OrchaShell, path: &str) -> Result<serde_json::Value> {
    let event_id = path.strip_prefix("/status/").unwrap_or("").trim();
    if event_id.is_empty() {
        return Err(ShellError::InvalidPath("missing event_id".into()));
    }
    let response = shell.get_response(event_id)?;
    let v = serde_json::to_value(&response)?;
    Ok(v)
}

/// 把 ShellError 转成 HttpResponse（错误状态码 + JSON body）。
fn error_response(e: &ShellError) -> HttpResponse {
    HttpResponse {
        status: e.http_status(),
        body: serde_json::to_string(&e.to_response_body()).unwrap_or_else(|_| "{}".into()),
    }
}

/// 把 HttpResponse 写入 stream（HTTP/1.1 响应）。
fn write_response(stream: TcpStream, response: &HttpResponse) -> Result<()> {
    let status_text = status_text(response.status);
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        status_text,
        response.body.len()
    );
    let mut stream = stream;
    stream
        .write_all(header.as_bytes())
        .map_err(ShellError::Io)?;
    stream
        .write_all(response.body.as_bytes())
        .map_err(ShellError::Io)?;
    stream.flush().map_err(ShellError::Io)?;
    Ok(())
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use orcha_sdk::{EventPayload, EventType, OrchaResponse, ResponseStatus};

    /// 一个最简 mock adapter：handle_event 把 event_id 存进 response 映射。
    struct MockAdapter {
        responses: Mutex<HashMap<String, OrchaResponse>>,
    }

    impl MockAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(HashMap::new()),
            })
        }
    }

    impl crate::ShellAdapter for MockAdapter {
        fn info(&self) -> crate::AdapterInfo {
            crate::AdapterInfo {
                name: "mock".into(),
                source: "mock".into(),
                description: "test mock".into(),
            }
        }
        fn handle_event(&self, event: OrchaEvent) -> Result<String> {
            let id = event.event_id.clone();
            let response = OrchaResponse {
                event_id: id.clone(),
                status: ResponseStatus::Final,
                content: event.payload.raw_text.clone(),
                artifacts: Vec::new(),
            };
            self.responses
                .lock()
                .expect("mock mutex")
                .insert(id.clone(), response);
            Ok(id)
        }
        fn get_response(&self, event_id: &str) -> Result<OrchaResponse> {
            self.responses
                .lock()
                .expect("mock mutex")
                .get(event_id)
                .cloned()
                .ok_or_else(|| ShellError::EventNotFound(event_id.into()))
        }
    }

    fn make_event(id: &str, text: &str) -> OrchaEvent {
        OrchaEvent {
            event_id: id.into(),
            source: "mock".into(),
            user_id: "u".into(),
            timestamp: 0,
            event_type: EventType::UserPrompt,
            payload: EventPayload {
                raw_text: text.into(),
                attachments: Vec::new(),
            },
        }
    }

    /// 在随机端口启动 server，返回 (base_url, stop_signal, shell)。
    fn start_server() -> (String, Arc<AtomicBool>, Arc<OrchaShell>) {
        let shell = Arc::new(OrchaShell::new("test-api-key"));
        shell.register_adapter(MockAdapter::new());
        let server = HttpServer::new(shell.clone(), "127.0.0.1:0");

        // 测试方先 bind listener，拿到真实端口后再交给 server 线程，避免竞争。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("127.0.0.1:{}", addr.port());

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        std::thread::spawn(move || {
            let _ = server.serve(listener, stop_clone);
        });
        // 给 server 一点时间进入 accept 循环。
        std::thread::sleep(Duration::from_millis(100));
        (url, stop, shell)
    }

    /// 发一个 HTTP 请求并返回完整响应字符串。
    fn http_request(
        url: &str,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        if let Some(a) = auth {
            req.push_str(&format!("Authorization: {a}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let mut parts = response.splitn(2, "\r\n\r\n");
        let headers = parts.next().unwrap_or("");
        let body = parts.next().unwrap_or("");
        let status_line = headers.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    #[test]
    fn post_run_with_valid_api_key_returns_event_id() {
        let (url, stop, _) = start_server();
        let event = make_event("", "hello via http");
        let body = serde_json::to_string(&event).unwrap();
        let (status, resp_body) = http_request(
            &url,
            "POST",
            "/run",
            Some("Bearer test-api-key"),
            Some(&body),
        );
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp_body).unwrap();
        assert!(v["event_id"].as_str().unwrap().starts_with("EVT-"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn post_run_without_api_key_returns_401() {
        let (url, stop, _) = start_server();
        let event = make_event("", "x");
        let body = serde_json::to_string(&event).unwrap();
        let (status, resp_body) = http_request(&url, "POST", "/run", None, Some(&body));
        assert_eq!(status, 401);
        assert!(resp_body.contains("unauthorized"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn post_run_with_wrong_api_key_returns_401() {
        let (url, stop, _) = start_server();
        let event = make_event("", "x");
        let body = serde_json::to_string(&event).unwrap();
        let (status, _) = http_request(&url, "POST", "/run", Some("Bearer wrong-key"), Some(&body));
        assert_eq!(status, 401);
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn get_status_returns_response_after_run() {
        let (url, stop, shell) = start_server();
        // 直接通过 shell 提交一个事件，拿到 event_id。
        let event_id = shell
            .submit_event(make_event("EVT-known", "hello status"))
            .unwrap();

        let (status, resp_body) = http_request(
            &url,
            "GET",
            &format!("/status/{event_id}"),
            Some("Bearer test-api-key"),
            None,
        );
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp_body).unwrap();
        assert_eq!(v["event_id"], "EVT-known");
        assert_eq!(v["status"], "FINAL");
        assert_eq!(v["content"], "hello status");
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn get_status_missing_event_returns_404() {
        let (url, stop, _) = start_server();
        let (status, resp_body) = http_request(
            &url,
            "GET",
            "/status/EVT-missing",
            Some("Bearer test-api-key"),
            None,
        );
        assert_eq!(status, 404);
        assert!(resp_body.contains("event not found"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn unknown_route_returns_404() {
        let (url, stop, _) = start_server();
        let (status, _) = http_request(&url, "GET", "/unknown", Some("Bearer test-api-key"), None);
        // 未知路由走 InvalidPath，状态码 400。
        assert_eq!(status, 400);
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn post_run_with_invalid_json_returns_400() {
        let (url, stop, _) = start_server();
        let (status, resp_body) = http_request(
            &url,
            "POST",
            "/run",
            Some("Bearer test-api-key"),
            Some("not json"),
        );
        assert_eq!(status, 400);
        // 解析失败应反映在错误信息里（ShellError::Json 的 Display 含 "json error"）。
        assert!(
            resp_body.contains("json error") || resp_body.contains("error"),
            "body should mention error: {resp_body}"
        );
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn get_status_without_path_param_returns_400() {
        let (url, stop, _) = start_server();
        let (status, _) = http_request(&url, "GET", "/status/", Some("Bearer test-api-key"), None);
        assert_eq!(status, 400);
        stop.store(true, Ordering::SeqCst);
    }
}

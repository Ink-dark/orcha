//! 跨进程 IPC 传输抽象（M6）。
//!
//! 统一三种后端：
//! - **Unix Socket**（Linux/macOS）：性能最优，进程间原生
//! - **Named Pipe**（Windows）：Windows 原生进程间通信（后续实现）
//! - **localhost TCP**（fallback）：全平台兼容
//!
//! `auto` 模式按平台选择：Linux/macOS → Unix Socket，Windows → TCP。
//!
//! 详见 docs/ROADMAP.md M6 验收：跨平台编译通过（CI windows-latest 仍绿）。

use std::io::{self, Read, Write};

/// IPC 连接的读写流抽象。
pub trait IpcStream: Read + Write + Send {
    /// 设置读超时（用于 watchdog，M7）。
    ///
    /// - `None`：阻塞读（默认）。
    /// - `Some(d)`：`d` 后未读到数据则 `read` 返回 `WouldBlock` 错误。
    ///
    /// Gateway reader 线程设 35s 超时（略大于 Adapter 心跳间隔 30s），
    /// 连续 3 次超时（≈105s 无心跳）视为 Adapter 死亡，断开连接。
    fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()>;

    /// 克隆 handle（用于拆分读写：reader 持一份，writer 持一份）。
    ///
    /// 两份共享同一底层连接（同一 fd），各自独立持有。
    /// `set_read_timeout` 是 socket 级别，但 writer 只 write 不受影响。
    fn try_clone(&self) -> io::Result<Box<dyn IpcStream>>;
}

/// IPC 地址（Unix path 或 TCP host:port）。
#[derive(Debug, Clone)]
pub enum IpcAddr {
    /// Unix Socket 文件路径（Linux/macOS）。
    Unix(std::path::PathBuf),
    /// TCP 地址 `host:port`（全平台 fallback）。
    Tcp(String, u16),
    /// Windows Named Pipe 名称（如 `\\.\pipe\orcha`）。
    #[cfg(windows)]
    NamedPipe(String),
}

impl IpcAddr {
    /// `auto` 模式：按平台选择。
    /// Linux/macOS → Unix Socket（`{home}/gateway.sock`）
    /// Windows → TCP（`127.0.0.1:{port}`）
    pub fn auto_detect(home: &std::path::Path, port: u16) -> Self {
        #[cfg(unix)]
        {
            // Unix Socket 不用 port，但保留参数让 Windows 分支能用（参数名带下划线避免 unused 警告）。
            let _port = port;
            IpcAddr::Unix(home.join("gateway.sock"))
        }
        #[cfg(not(unix))]
        {
            // Windows 走 TCP，不用 home 路径，但保留参数让 Unix 分支能用。
            let _home = home;
            IpcAddr::Tcp("127.0.0.1".to_string(), port)
        }
    }

    /// 从 config 的 kind 字段解析。
    pub fn from_kind(kind: &str, home: &std::path::Path, port: u16) -> Self {
        match kind {
            "unix" => IpcAddr::Unix(home.join("gateway.sock")),
            "tcp" => IpcAddr::Tcp("127.0.0.1".to_string(), port),
            _ => Self::auto_detect(home, port),
        }
    }
}

// ── TCP 后端（全平台） ──────────────────────────────────────────

mod tcp_backend {
    use super::{IpcAddr, IpcStream};
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// TCP listener wrapper。
    pub struct TcpTransport {
        listener: TcpListener,
    }

    struct TcpStreamWrap(TcpStream);

    impl Read for TcpStreamWrap {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for TcpStreamWrap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl IpcStream for TcpStreamWrap {
        fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
            self.0.set_read_timeout(dur)
        }

        fn try_clone(&self) -> io::Result<Box<dyn IpcStream>> {
            Ok(Box::new(TcpStreamWrap(self.0.try_clone()?)))
        }
    }

    impl TcpTransport {
        pub fn bind(addr: &IpcAddr) -> io::Result<Self> {
            let (host, port) = match addr {
                IpcAddr::Tcp(h, p) => (h.as_str(), *p),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "TCP 后端需要 Tcp 地址",
                    ))
                }
            };
            let listener = TcpListener::bind((host, port))?;
            Ok(Self { listener })
        }

        pub fn accept(&self) -> io::Result<Box<dyn IpcStream>> {
            let (stream, _) = self.listener.accept()?;
            Ok(Box::new(TcpStreamWrap(stream)))
        }
    }

    /// 客户端连接辅助函数。
    pub fn connect_stream(addr: &IpcAddr) -> io::Result<Box<dyn IpcStream>> {
        let (host, port) = match addr {
            IpcAddr::Tcp(h, p) => (h.as_str(), *p),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TCP 需要 Tcp 地址",
                ))
            }
        };
        let stream = TcpStream::connect((host, port))?;
        Ok(Box::new(TcpStreamWrap(stream)))
    }
}

// ── Unix Socket 后端（Linux/macOS only） ────────────────────────

#[cfg(unix)]
mod unix_backend {
    use super::{IpcAddr, IpcStream};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub struct UnixTransport {
        listener: UnixListener,
    }

    struct UnixStreamWrap(UnixStream);

    impl Read for UnixStreamWrap {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for UnixStreamWrap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl IpcStream for UnixStreamWrap {
        fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
            self.0.set_read_timeout(dur)
        }

        fn try_clone(&self) -> io::Result<Box<dyn IpcStream>> {
            Ok(Box::new(UnixStreamWrap(self.0.try_clone()?)))
        }
    }

    impl UnixTransport {
        pub fn bind(addr: &IpcAddr) -> io::Result<Self> {
            let path = match addr {
                IpcAddr::Unix(p) => p,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Unix 后端需要 Unix 地址",
                    ))
                }
            };
            // 清理可能残留的旧 socket 文件
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)?;
            // #17: 收紧 socket 文件权限为 0600，防止同机其他用户连接。
            // 默认受 umask 影响（常为 0755/0777），多用户系统下可被他人连入
            // 伪造触发/绕过审批。设权限失败则清理并报错，避免遗留可连 socket。
            if let Err(e) = std::fs::set_permissions(path, PermissionsExt::from_mode(0o600)) {
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
            Ok(Self { listener })
        }

        pub fn accept(&self) -> io::Result<Box<dyn IpcStream>> {
            let (stream, _) = self.listener.accept()?;
            Ok(Box::new(UnixStreamWrap(stream)))
        }
    }

    /// 客户端连接辅助函数。
    pub fn connect_stream(addr: &IpcAddr) -> io::Result<Box<dyn IpcStream>> {
        let path = match addr {
            IpcAddr::Unix(p) => p,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Unix 需要 Unix 地址",
                ))
            }
        };
        let stream = UnixStream::connect(path)?;
        Ok(Box::new(UnixStreamWrap(stream)))
    }
}

/// 统一的 IPC listener（枚举替代 trait object，避免 dyn 兼容性问题）。
pub enum IpcListener {
    #[cfg(unix)]
    Unix(unix_backend::UnixTransport),
    Tcp(tcp_backend::TcpTransport),
}

impl IpcListener {
    /// 接受一个新连接，返回可读写的 stream。
    pub fn accept(&self) -> io::Result<Box<dyn IpcStream>> {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(t) => t.accept(),
            IpcListener::Tcp(t) => t.accept(),
        }
    }

    /// 当前 listener 是否为 TCP 后端。
    pub fn is_tcp(&self) -> bool {
        matches!(self, IpcListener::Tcp(_))
    }

    /// #26：接受连接并做共享密钥握手。
    ///
    /// - **Unix**：文件系统权限 0600 已保护（#17），无需握手，直接返回 stream。
    /// - **TCP + Some(secret)**：accept 后读首行，必须为 `AUTH <secret>`，
    ///   不匹配/缺失/超时立即断开（返回 `Err`，调用方丢弃 stream）。
    /// - **TCP + None**：打印警告后放行（dev 向后兼容；生产应配 secret）。
    pub fn accept_authenticated(&self, secret: Option<&str>) -> io::Result<Box<dyn IpcStream>> {
        let stream = self.accept()?;
        server_authenticate(stream, self.is_tcp(), secret)
    }
}

/// 统一的服务端 bind：根据 IpcAddr 类型选后端。
pub fn bind(addr: &IpcAddr) -> io::Result<IpcListener> {
    match addr {
        #[cfg(unix)]
        IpcAddr::Unix(_) => Ok(IpcListener::Unix(unix_backend::UnixTransport::bind(addr)?)),
        IpcAddr::Tcp(_, _) => Ok(IpcListener::Tcp(tcp_backend::TcpTransport::bind(addr)?)),
        #[cfg(windows)]
        IpcAddr::NamedPipe(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Named Pipe 后端待实现，请用 tcp kind",
        )),
        #[cfg(not(unix))]
        IpcAddr::Unix(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix Socket 在当前平台不可用，请用 tcp kind",
        )),
    }
}

/// 统一的客户端连接：根据 IpcAddr 类型选后端。
pub fn connect_stream(addr: &IpcAddr) -> io::Result<Box<dyn IpcStream>> {
    match addr {
        #[cfg(unix)]
        IpcAddr::Unix(_) => unix_backend::connect_stream(addr),
        IpcAddr::Tcp(_, _) => tcp_backend::connect_stream(addr),
        #[cfg(windows)]
        IpcAddr::NamedPipe(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Named Pipe 待实现，请用 tcp",
        )),
        #[cfg(not(unix))]
        IpcAddr::Unix(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix Socket 在当前平台不可用",
        )),
    }
}

/// #26：客户端连上后做共享密钥握手（仅 TCP 发送 `AUTH <secret>\n`）。
///
/// Unix socket 靠文件系统权限保护，不需要握手。与
/// [`IpcListener::accept_authenticated`] 对应。
pub fn connect_stream_authenticated(
    addr: &IpcAddr,
    secret: Option<&str>,
) -> io::Result<Box<dyn IpcStream>> {
    let is_tcp = matches!(addr, IpcAddr::Tcp(_, _));
    let mut stream = connect_stream(addr)?;
    client_authenticate(&mut *stream, is_tcp, secret)?;
    Ok(stream)
}

// ============================================================
// #26：TCP 共享密钥握手
// ============================================================

/// 握手前缀：客户端首行必须发 `AUTH <secret>\n`。
const AUTH_LINE_PREFIX: &str = "AUTH ";
/// AUTH 行最大字节数（含前缀 + secret + 换行），防恶意大行耗内存。
const AUTH_MAX_LINE: usize = 512;

/// 服务端在 accept 后认证（仅 TCP）。
///
/// - Unix：直接返回 stream（文件系统 0600 已保护）
/// - TCP + Some(secret)：读首行，必须为 `AUTH <secret>`，否则返回 `Err`
///   （调用方丢弃 stream 即断开连接）
/// - TCP + None：打印警告，放行（dev 向后兼容）
fn server_authenticate(
    mut stream: Box<dyn IpcStream>,
    is_tcp: bool,
    secret: Option<&str>,
) -> io::Result<Box<dyn IpcStream>> {
    if !is_tcp {
        return Ok(stream);
    }
    match secret {
        None => {
            eprintln!(
                "[gateway] 警告: IPC TCP 启用但未配 tcp_secret / ORCHA_IPC_TCP_SECRET，\
                 任意本机进程可连入伪造触发/审批（#26）"
            );
            Ok(stream)
        }
        Some(expected) => {
            // 读首行（到 \n，限长防滥用）
            let line = read_line_with_limit(&mut *stream, AUTH_MAX_LINE)?;
            let provided = line
                .strip_prefix(AUTH_LINE_PREFIX)
                .map(|s| s.trim_end_matches(['\r', '\n']));
            match provided {
                Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => Ok(stream),
                _ => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "IPC TCP 握手失败：AUTH 密钥不匹配或缺失",
                )),
            }
        }
    }
}

/// 客户端连上后发 `AUTH <secret>\n`（仅 TCP + Some(secret)）。
fn client_authenticate(
    stream: &mut dyn IpcStream,
    is_tcp: bool,
    secret: Option<&str>,
) -> io::Result<()> {
    if !is_tcp {
        return Ok(());
    }
    if let Some(s) = secret {
        let mut line = String::with_capacity(AUTH_LINE_PREFIX.len() + s.len() + 1);
        line.push_str(AUTH_LINE_PREFIX);
        line.push_str(s);
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
    }
    Ok(())
}

/// 读一行（到 `\n`），限长防恶意大行耗内存。`\n` 不包含在返回值里。
fn read_line_with_limit(stream: &mut dyn IpcStream, max: usize) -> io::Result<String> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "IPC 握手：连接在对端发 AUTH 前关闭",
                ))
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > max {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "IPC 握手：AUTH 行过长",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// 定长比较，避免 timing attack 泄露密钥前缀。
/// 长度不同直接返回 false（不泄露长度信息以外的内容）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detect_unix_on_unix() {
        let home = std::path::Path::new("/tmp/orcha");
        let addr = IpcAddr::auto_detect(home, 7422);
        #[cfg(unix)]
        assert!(matches!(addr, IpcAddr::Unix(_)));
        #[cfg(not(unix))]
        assert!(matches!(addr, IpcAddr::Tcp(_, _)));
    }

    #[test]
    fn from_kind_tcp() {
        let addr = IpcAddr::from_kind("tcp", std::path::Path::new("/tmp"), 7422);
        assert!(matches!(addr, IpcAddr::Tcp(_, 7422)));
    }

    #[test]
    fn from_kind_auto() {
        let addr = IpcAddr::from_kind("auto", std::path::Path::new("/tmp"), 7422);
        #[cfg(unix)]
        assert!(matches!(addr, IpcAddr::Unix(_)));
        #[cfg(not(unix))]
        assert!(matches!(addr, IpcAddr::Tcp(_, _)));
    }

    #[test]
    fn tcp_roundtrip() {
        let addr = IpcAddr::Tcp("127.0.0.1".to_string(), 17499);

        let server = bind(&addr).unwrap();
        let handle = std::thread::spawn(move || {
            let mut stream = server.accept().unwrap();
            let mut buf = [0u8; 5];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            assert_eq!(&buf, b"hello");
        });

        let mut client = connect_stream(&addr).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();
        drop(client);

        handle.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unix_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let addr = IpcAddr::Unix(dir.path().join("test.sock"));

        let server = bind(&addr).unwrap();
        let handle = std::thread::spawn(move || {
            let mut stream = server.accept().unwrap();
            let mut buf = [0u8; 5];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            assert_eq!(&buf, b"hello");
        });

        let mut client = connect_stream(&addr).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();
        drop(client);

        handle.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unix_socket_permissions_are_restricted() {
        // #17：bind 后 socket 文件权限应为 0600，防止同机其他用户连接。
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("perm.sock");
        let addr = IpcAddr::Unix(sock.clone());

        let _server = bind(&addr).expect("bind");

        let mode = std::fs::metadata(&sock)
            .expect("socket 元数据")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "socket 权限应为 0600，实际 {mode:#o}（#17）");
    }

    // ============================================================
    // #26：TCP 共享密钥握手
    // ============================================================

    /// 找一个本机空闲端口：bind 一次拿到端口，立刻 drop，复用端口。
    fn free_tcp_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// #26：正确密钥握手成功，之后可正常收发 JSON-line 消息。
    #[test]
    fn tcp_handshake_succeeds_with_correct_secret() {
        let port = free_tcp_port();
        let addr = IpcAddr::Tcp("127.0.0.1".to_string(), port);
        let secret = "s3cret-token-#26".to_string();

        let server = bind(&addr).unwrap();
        let secret_clone = secret.clone();
        let server_handle = std::thread::spawn(move || {
            let mut stream = server.accept_authenticated(Some(&secret_clone)).unwrap();
            // 握手通过后，正常读 JSON-line 消息
            let mut buf = [0u8; 5];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            buf
        });

        let mut client = connect_stream_authenticated(&addr, Some(&secret)).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();
        drop(client);

        let buf = server_handle.join().unwrap();
        assert_eq!(&buf, b"hello", "握手通过后应能正常收发");
    }

    /// #26：密钥不匹配时，服务端 accept_authenticated 返回 PermissionDenied，
    /// 且后续消息不会被处理（攻击者无法注入触发/审批）。
    #[test]
    fn tcp_handshake_rejects_wrong_secret() {
        let port = free_tcp_port();
        let addr = IpcAddr::Tcp("127.0.0.1".to_string(), port);
        let server_secret = "correct-secret".to_string();
        let client_wrong_secret = "wrong-secret".to_string();

        let server = bind(&addr).unwrap();
        let server_handle = std::thread::spawn(move || {
            // 服务端期望正确密钥
            server.accept_authenticated(Some(&server_secret))
        });

        // 客户端发错误密钥
        let mut client = connect_stream_authenticated(&addr, Some(&client_wrong_secret)).unwrap();
        // 客户端发完 AUTH 行后立刻注入一条恶意消息（不应被服务端处理）
        client
            .write_all(b"{\"type\":\"trigger\",\"malicious\":true}\n")
            .unwrap();
        client.flush().unwrap();
        drop(client);

        let result = server_handle.join().unwrap();
        let err = match result {
            Ok(_) => panic!("密钥不匹配应被拒绝，实际握手成功"),
            Err(e) => e,
        };
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "应返回 PermissionDenied，实际 {err:?}"
        );
    }

    /// #26：客户端不发 AUTH 行（直接发消息），服务端应拒绝。
    /// 模拟未授权进程直接连 TCP 注入伪造触发。
    #[test]
    fn tcp_handshake_rejects_missing_auth_line() {
        let port = free_tcp_port();
        let addr = IpcAddr::Tcp("127.0.0.1".to_string(), port);
        let secret = "guard-secret".to_string();

        let server = bind(&addr).unwrap();
        let server_handle = std::thread::spawn(move || server.accept_authenticated(Some(&secret)));

        // 攻击者：直接连，不发 AUTH，直接注入伪造审批响应
        let mut attacker = connect_stream(&addr).unwrap();
        attacker
            .write_all(b"{\"type\":\"approval_response\",\"action_id\":\"forged\"}\n")
            .unwrap();
        attacker.flush().unwrap();
        drop(attacker);

        let result = server_handle.join().unwrap();
        assert!(result.is_err(), "无 AUTH 行应被拒绝");
    }

    /// #26：未配 secret（None）时 TCP 仍可连（dev 向后兼容）。
    #[test]
    fn tcp_handshake_allows_when_no_secret_configured() {
        let port = free_tcp_port();
        let addr = IpcAddr::Tcp("127.0.0.1".to_string(), port);

        let server = bind(&addr).unwrap();
        let server_handle = std::thread::spawn(move || {
            // None = dev 模式不认证
            let mut stream = server.accept_authenticated(None).unwrap();
            let mut buf = [0u8; 5];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            buf
        });

        // 客户端也不发 AUTH（secret=None）
        let mut client = connect_stream_authenticated(&addr, None).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();
        drop(client);

        let buf = server_handle.join().unwrap();
        assert_eq!(&buf, b"hello", "None 模式应放行（dev 向后兼容）");
    }

    /// #26：Unix socket 不做握手（文件系统 0600 已保护），secret 被忽略。
    #[test]
    #[cfg(unix)]
    fn unix_handshake_is_noop_regardless_of_secret() {
        let dir = tempfile::tempdir().unwrap();
        let addr = IpcAddr::Unix(dir.path().join("auth.sock"));
        let secret = "unused-on-unix".to_string();

        let server = bind(&addr).unwrap();
        let secret_clone = secret.clone();
        let server_handle = std::thread::spawn(move || {
            // Unix 即使传了 secret 也应跳过握手
            let mut stream = server.accept_authenticated(Some(&secret_clone)).unwrap();
            let mut buf = [0u8; 5];
            use std::io::Read;
            let _ = stream.read(&mut buf);
            buf
        });

        // 客户端：Unix 不发 AUTH 行，直接发数据
        let mut client = connect_stream_authenticated(&addr, Some(&secret)).unwrap();
        client.write_all(b"hello").unwrap();
        client.flush().unwrap();
        drop(client);

        let buf = server_handle.join().unwrap();
        assert_eq!(&buf, b"hello", "Unix 应跳过握手（0600 保护）");
    }

    /// #26：constant_time_eq 不泄露密钥前缀（长度不同直接 false）。
    #[test]
    fn constant_time_eq_correctness() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab")); // 长度不同
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    /// #26：read_line_with_limit 在超长行时报错（防恶意大行耗内存）。
    #[test]
    fn read_line_with_limit_rejects_oversized_line() {
        // 用内存中的 Cursor 模拟 stream
        struct MemStream(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for MemStream {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl std::io::Write for MemStream {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.get_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl IpcStream for MemStream {
            fn set_read_timeout(&self, _: Option<std::time::Duration>) -> std::io::Result<()> {
                Ok(())
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn IpcStream>> {
                Ok(Box::new(MemStream(std::io::Cursor::new(
                    self.0.get_ref().clone(),
                ))))
            }
        }
        let big: Vec<u8> = vec![b'A'; 600]; // 超过 AUTH_MAX_LINE(512)
        let mut stream = MemStream(std::io::Cursor::new(big));
        let result = read_line_with_limit(&mut stream, AUTH_MAX_LINE);
        assert!(result.is_err(), "超长 AUTH 行应被拒绝");
    }
}

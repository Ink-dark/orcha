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
}

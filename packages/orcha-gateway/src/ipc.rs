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
pub trait IpcStream: Read + Write + Send {}

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

    impl IpcStream for TcpStreamWrap {}

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

    impl IpcStream for UnixStreamWrap {}

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
}

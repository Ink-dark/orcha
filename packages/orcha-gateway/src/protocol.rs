//! Gateway ↔ Adapter 之间的 IPC 协议（M7）。
//!
//! 传输层用 [`crate::ipc::IpcStream`]（字节流），协议层用 **JSON line**：
//! 每条消息是一个紧凑 JSON 对象 + `\n` 分隔。优点：
//! - 跨语言（Rust / TypeScript）天然兼容；
//! - 可读，`nc` / `socat` 即可调试；
//! - 不依赖二进制 schema（如 protobuf），少一个依赖。
//!
//! ## 消息方向
//!
//! - **Adapter → Gateway**：[`AdapterToGateway`]——IM 触发、用户回复、鉴权询问、心跳。
//! - **Gateway → Adapter**：[`GatewayToAdapter`]——卡片更新、降级通知、任务结果、鉴权回复、心跳 ack。
//!
//! ## 协议约定
//!
//! - Adapter 主动 connect 到 Gateway（Gateway 是 IPC 服务端）；
//! - 一条 TCP/Unix 连接内**多路复用**多个会话（用 `session` 字段区分不同 IM 会话）；
//! - Gateway 不主动断开连接，除非 Adapter 发非法消息或心跳超时；
//! - 心跳：Adapter 每 30s 发 [`AdapterToGateway::Heartbeat`]，Gateway 回
//!   [`GatewayToAdapter::HeartbeatAck`]；连续 3 次未收到心跳，Gateway 视为 Adapter 死亡。
//!
//! 详见 docs/ROADMAP.md M7 验收：飞书通过 Adapter 长连接接收 `@Orcha fix <issue>` 并触发 Cycleround。

use std::io::{self, Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

// ============================================================
// 消息类型
// ============================================================

/// Adapter → Gateway 的消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterToGateway {
    /// 用户 @Orcha 触发任务（或回复"继续/取消"等命令）。
    Trigger {
        /// 任务 ID（Adapter 侧生成；Gateway 收到后用它做 trace_id）。
        /// 若为空，Gateway 会生成 `Task::generate_id()`。
        task_id: String,
        /// 任务描述（如 "fix the login bug"）。
        description: String,
        /// IM 会话标识（飞书 chat_id / QQ group_openid），用于回推卡片。
        session: String,
        /// 触发来源元信息。
        source: TriggerSource,
    },
    /// 用户在已有任务上回复（如 Reviewer 拒绝后用户决定"放弃" / "重试"）。
    Reply {
        task_id: String,
        /// 用户输入原文。
        content: String,
        session: String,
    },
    /// Adapter 启动时询问 Gateway 当前用户/群是否在白名单。
    AuthCheck { user: String, group: Option<String> },
    /// 心跳，Adapter 每 30s 发一次。
    Heartbeat { ts_ms: u64 },
}

/// 触发来源元信息，用于鉴权与审计。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriggerSource {
    /// IM 平台：`feishu` / `qq` / `slack`...
    pub platform: String,
    /// 触发用户标识（飞书 open_id / QQ openid）。
    pub user: String,
    /// 触发群标识（私聊为 None）。
    pub group: Option<String>,
    /// 触发消息原文（含 @Orcha 前缀），用于审计。
    pub raw: String,
}

/// Gateway → Adapter 的消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayToAdapter {
    /// 卡片状态实时更新（patch 同一张卡片：观察中→规划中→写代码中→测试中→审核中）。
    CardUpdate {
        task_id: String,
        session: String,
        /// 当前阶段标签（"🔍 观察中" / "📋 规划中" / "💻 写代码中" / ...）。
        phase: String,
        /// 详细进度（如 "已发现 3 个文件"）。
        detail: String,
        /// 进度 0-100，方便卡片进度条。
        progress: u8,
    },
    /// 降级链通知（如 LLM 超时通知用户"AI 卡住，正在恢复"）。
    Notify {
        task_id: String,
        session: String,
        /// `info` / `warn` / `error`，影响卡片图标。
        level: NotifyLevel,
        message: String,
    },
    /// 任务终态结果。
    TaskResult {
        task_id: String,
        session: String,
        /// `success` / `failed` / `panic`。
        outcome: String,
        /// 一句话总结。
        summary: String,
        /// 产物 URL 列表（如 `file:///...`），用于卡片可点击展开。
        artifacts: Vec<String>,
    },
    /// 鉴权回复。
    AuthResult {
        allowed: bool,
        /// 拒绝原因（allowed=false 时填）。
        reason: Option<String>,
    },
    /// 心跳 ack。
    HeartbeatAck { ts_ms: u64 },
}

/// 通知级别。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    Info,
    Warn,
    Error,
}

// ============================================================
// 读写辅助（JSON line 协议）
// ============================================================

/// 写一条消息到流：序列化为紧凑 JSON + 追加 `\n`。
///
/// 调用方负责在写完后调用 `flush()`。
pub fn write_msg<W: Write, M: Serialize>(w: &mut W, msg: &M) -> io::Result<()> {
    let mut json = serde_json::to_string(msg).map_err(json_to_io)?;
    json.push('\n');
    w.write_all(json.as_bytes())
}

/// 读一条消息：读到 `\n` 为止，反序列化为 `M`。
///
/// 返回 `Ok(None)` 表示对端关闭连接（EOF 且缓冲区为空）。
pub fn read_msg<R: Read, M: DeserializeOwned>(r: &mut R) -> io::Result<Option<M>> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte)?;
        if n == 0 {
            // EOF
            if buf.is_empty() {
                return Ok(None);
            }
            // 缓冲区还有数据但没有换行——视为不完整消息
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete message (no trailing newline)",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message exceeds 1MB (no newline found)",
            ));
        }
    }
    let msg: M = serde_json::from_slice(&buf).map_err(json_to_io)?;
    Ok(Some(msg))
}

fn json_to_io(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// 把多条消息序列化后追加到 `Vec<u8>`，模拟 IPC 流。
    fn encode_all<M: Serialize>(msgs: &[M]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in msgs {
            write_msg(&mut out, m).unwrap();
        }
        out
    }

    /// 从字节流逐条反序列化，返回所有消息（直到 EOF）。
    fn decode_all<M: DeserializeOwned>(bytes: &[u8]) -> Vec<M> {
        let mut cursor = std::io::Cursor::new(bytes);
        let mut out = Vec::new();
        while let Some(m) = read_msg(&mut cursor).unwrap() {
            out.push(m);
        }
        out
    }

    #[test]
    fn roundtrip_trigger_message() {
        let msg = AdapterToGateway::Trigger {
            task_id: "T-1".into(),
            description: "fix login bug".into(),
            session: "im-session-1".into(),
            source: TriggerSource {
                platform: "feishu".into(),
                user: "ou_xxx".into(),
                group: Some("oc_yyy".into()),
                raw: "@Orcha fix login bug".into(),
            },
        };
        let bytes = encode_all(std::slice::from_ref(&msg));
        let decoded: Vec<AdapterToGateway> = decode_all(&bytes);
        assert_eq!(decoded, vec![msg]);
    }

    #[test]
    fn roundtrip_card_update_with_emoji() {
        // 验证中文/emoji 在 JSON line 协议下不会出问题。
        let msg = GatewayToAdapter::CardUpdate {
            task_id: "T-2".into(),
            session: "s".into(),
            phase: "🔍 观察中".into(),
            detail: "已发现 3 个文件".into(),
            progress: 30,
        };
        let bytes = encode_all(std::slice::from_ref(&msg));
        let decoded: Vec<GatewayToAdapter> = decode_all(&bytes);
        assert_eq!(decoded, vec![msg]);
    }

    #[test]
    fn multiple_messages_in_one_stream() {
        let msgs = vec![
            GatewayToAdapter::HeartbeatAck { ts_ms: 1 },
            GatewayToAdapter::Notify {
                task_id: "T-3".into(),
                session: "s".into(),
                level: NotifyLevel::Warn,
                message: "AI 卡住".into(),
            },
            GatewayToAdapter::TaskResult {
                task_id: "T-3".into(),
                session: "s".into(),
                outcome: "success".into(),
                summary: "done".into(),
                artifacts: vec!["file:///a.py".into()],
            },
        ];
        let bytes = encode_all(&msgs);
        let decoded: Vec<GatewayToAdapter> = decode_all(&bytes);
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded, msgs);
    }

    #[test]
    fn read_msg_returns_none_on_clean_eof() {
        let empty: &[u8] = &[];
        let mut cursor = std::io::Cursor::new(empty);
        let m: Option<GatewayToAdapter> = read_msg(&mut cursor).unwrap();
        assert!(m.is_none(), "EOF 且无缓冲应返回 None");
    }

    #[test]
    fn read_msg_errors_on_incomplete_message() {
        // 没有换行符结尾，EOF 时应报错。
        let bytes = b"{\"type\":\"heartbeat_ack\",\"ts_ms\":1}";
        let mut cursor = std::io::Cursor::new(bytes);
        let result: io::Result<Option<GatewayToAdapter>> = read_msg(&mut cursor);
        assert!(result.is_err(), "无换行的不完整消息应报错");
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
            "应报 UnexpectedEof"
        );
    }

    #[test]
    fn write_msg_appends_newline() {
        let mut out = Vec::new();
        write_msg(&mut out, &GatewayToAdapter::HeartbeatAck { ts_ms: 1 }).unwrap();
        assert!(out.ends_with(b"\n"), "写出的消息必须以 \\n 结尾");
        assert_eq!(
            out.iter().filter(|&&b| b == b'\n').count(),
            1,
            "JSON 内部不应出现裸 \\n（被转义为 \\\\n）"
        );
    }

    #[test]
    fn message_too_large_rejected() {
        // 构造一个 >1MB 的消息（description 字段填超长字符串）。
        let big = "x".repeat(2 * 1024 * 1024);
        let msg = AdapterToGateway::Trigger {
            task_id: "T".into(),
            description: big,
            session: "s".into(),
            source: TriggerSource {
                platform: "p".into(),
                user: "u".into(),
                group: None,
                raw: String::new(),
            },
        };
        let bytes = encode_all(&[msg]);
        let mut cursor = std::io::Cursor::new(&bytes);
        let result: io::Result<Option<AdapterToGateway>> = read_msg(&mut cursor);
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "应拒绝超长消息");
    }

    /// 模拟真实 IPC 场景：写 5 条消息 → 全部读回 → 验证顺序与内容。
    #[test]
    fn stream_roundtrip_preserves_order() {
        let queue: VecDeque<GatewayToAdapter> = vec![
            GatewayToAdapter::CardUpdate {
                task_id: "T".into(),
                session: "s".into(),
                phase: "🔍 观察中".into(),
                detail: "start".into(),
                progress: 10,
            },
            GatewayToAdapter::CardUpdate {
                task_id: "T".into(),
                session: "s".into(),
                phase: "📋 规划中".into(),
                detail: "planning".into(),
                progress: 30,
            },
            GatewayToAdapter::CardUpdate {
                task_id: "T".into(),
                session: "s".into(),
                phase: "💻 写代码中".into(),
                detail: "writing".into(),
                progress: 60,
            },
            GatewayToAdapter::Notify {
                task_id: "T".into(),
                session: "s".into(),
                level: NotifyLevel::Info,
                message: "继续".into(),
            },
            GatewayToAdapter::TaskResult {
                task_id: "T".into(),
                session: "s".into(),
                outcome: "success".into(),
                summary: "完成".into(),
                artifacts: vec![],
            },
        ]
        .into_iter()
        .collect();

        let bytes = {
            let mut out = Vec::new();
            for m in &queue {
                write_msg(&mut out, m).unwrap();
            }
            out
        };

        let decoded: Vec<GatewayToAdapter> = decode_all(&bytes);
        assert_eq!(decoded.len(), queue.len());
        for (i, expected) in queue.iter().enumerate() {
            assert_eq!(&decoded[i], expected, "第 {i} 条消息应保留原序");
        }
    }

    #[test]
    fn notify_level_serializes_to_snake_case() {
        // 验证 #[serde(rename_all = "snake_case")] 生效（避免客户端误判大小写）。
        let m = GatewayToAdapter::Notify {
            task_id: "T".into(),
            session: "s".into(),
            level: NotifyLevel::Warn,
            message: "x".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains("\"level\":\"warn\""),
            "NotifyLevel 应序列化为 snake_case，实际: {json}"
        );
    }

    #[test]
    fn message_tag_serializes_to_snake_case() {
        // 验证 enum tag 用 snake_case（card_update 而非 CardUpdate），
        // 与 TS 客户端的命名约定一致。
        let m = GatewayToAdapter::CardUpdate {
            task_id: "T".into(),
            session: "s".into(),
            phase: "p".into(),
            detail: "d".into(),
            progress: 1,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains("\"type\":\"card_update\""),
            "enum tag 应为 snake_case，实际: {json}"
        );
    }
}

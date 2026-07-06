use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

/// 单条审计日志。
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// ISO 8601 时间戳
    pub timestamp: String,
    /// 执行操作的 agent（"planner" / "worker" / "reviewer" / "fixer" 等）
    pub agent: String,
    /// 操作目标相对路径
    pub path: String,
    /// 操作类型
    pub action: AuditAction,
    /// canonical 后的绝对路径
    pub canonical_path: String,
    /// 操作是否被允许
    pub approved: bool,
    /// 拒绝原因（approved=true 时为空）
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditAction {
    Read,
    Write,
    Delete,
    Create,
}

/// 审计日志写入器。
///
/// 线程安全：内部用 `Mutex<BufWriter<File>>` 保护。
/// 每条日志 JSON 一行（JSONL 格式），追加写入。
pub struct AuditLogger {
    writer: Mutex<BufWriter<File>>,
}

impl AuditLogger {
    /// 在 `history_dir` 下为 `task_id` 创建审计日志文件。
    ///
    /// 文件路径：`history_dir/{task_id}.audit.jsonl`。
    /// 若目录不存在则自动创建。
    pub fn new(history_dir: &Path, task_id: &str) -> Result<Self, String> {
        fs::create_dir_all(history_dir).map_err(|e| format!("创建审计目录失败: {e}"))?;
        let file_path = history_dir.join(format!("{task_id}.audit.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .map_err(|e| format!("打开审计文件失败 ({file_path:?}): {e}"))?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// 记录一条审计日志。
    pub fn log(&self, entry: &AuditEntry) {
        let mut w = match self.writer.lock() {
            Ok(w) => w,
            Err(_) => return,
        };
        let line = serde_json::to_string(entry).unwrap_or_default();
        let _ = writeln!(w, "{line}");
        let _ = w.flush();
    }

    /// 便捷方法：记录一条 reads 操作。
    pub fn log_read(
        &self,
        agent: &str,
        path: &str,
        canonical_path: &Path,
        approved: bool,
        reason: &str,
    ) {
        self.log(&AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: agent.into(),
            path: path.into(),
            action: AuditAction::Read,
            canonical_path: canonical_path.display().to_string(),
            approved,
            reason: reason.into(),
        });
    }

    /// 便捷方法：记录一条 write 操作。
    pub fn log_write(
        &self,
        agent: &str,
        path: &str,
        canonical_path: &Path,
        approved: bool,
        reason: &str,
    ) {
        self.log(&AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: agent.into(),
            path: path.into(),
            action: AuditAction::Write,
            canonical_path: canonical_path.display().to_string(),
            approved,
            reason: reason.into(),
        });
    }

    /// 便捷方法：记录一条 delete 操作。
    pub fn log_delete(
        &self,
        agent: &str,
        path: &str,
        canonical_path: &Path,
        approved: bool,
        reason: &str,
    ) {
        self.log(&AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: agent.into(),
            path: path.into(),
            action: AuditAction::Delete,
            canonical_path: canonical_path.display().to_string(),
            approved,
            reason: reason.into(),
        });
    }

    /// 便捷方法：记录一条 create 操作。
    pub fn log_create(
        &self,
        agent: &str,
        path: &str,
        canonical_path: &Path,
        approved: bool,
        reason: &str,
    ) {
        self.log(&AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            agent: agent.into(),
            path: path.into(),
            action: AuditAction::Create,
            canonical_path: canonical_path.display().to_string(),
            approved,
            reason: reason.into(),
        });
    }
}

/// 无操作审计日志器（用于不启审计的路径）。
pub struct NoopAuditLogger;

impl NoopAuditLogger {
    pub fn log_read(
        &self,
        _agent: &str,
        _path: &str,
        _canonical: &Path,
        _approved: bool,
        _reason: &str,
    ) {
    }
    pub fn log_write(
        &self,
        _agent: &str,
        _path: &str,
        _canonical: &Path,
        _approved: bool,
        _reason: &str,
    ) {
    }
    pub fn log_delete(
        &self,
        _agent: &str,
        _path: &str,
        _canonical: &Path,
        _approved: bool,
        _reason: &str,
    ) {
    }
    pub fn log_create(
        &self,
        _agent: &str,
        _path: &str,
        _canonical: &Path,
        _approved: bool,
        _reason: &str,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_logger_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let logger = AuditLogger::new(dir.path(), "T-test").unwrap();

        logger.log_read(
            "planner",
            "src/main.py",
            Path::new("/tmp/ws/src/main.py"),
            true,
            "",
        );
        logger.log_write(
            "worker",
            "src/main.py",
            Path::new("/tmp/ws/src/main.py"),
            true,
            "",
        );
        logger.log_write(
            "worker",
            ".env",
            Path::new("/tmp/ws/.env"),
            false,
            "危险路径",
        );

        let content = std::fs::read_to_string(dir.path().join("T-test.audit.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 3);

        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["agent"], "planner");
        assert_eq!(entry["action"], "read");
        assert!(entry["approved"].as_bool().unwrap());

        let denied: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert!(!denied["approved"].as_bool().unwrap());
        assert_eq!(denied["path"], ".env");
    }

    #[test]
    fn audit_logger_creates_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let logger = AuditLogger::new(&nested, "T-nested").unwrap();
        logger.log_read("p", "f", Path::new("/f"), true, "");
        assert!(nested.join("T-nested.audit.jsonl").exists());
    }

    #[test]
    fn noop_logger_does_not_panic() {
        let noop = NoopAuditLogger;
        noop.log_read("a", "p", Path::new("/p"), true, "");
        noop.log_write("a", "p", Path::new("/p"), false, "nope");
    }
}

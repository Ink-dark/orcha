//! 任务队列 + worker 池（M6）。
//!
//! 非阻塞设计：任务入队后后台 worker 跑 Cycleround，Gateway 不被单个任务卡死。
//! M6 阶段先搭骨架（入队 + 串行执行），worker 池多线程见 M7 IM 接入时完善。

use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use orcha_sdk::Task;

/// 任务队列：入队 + 后台执行。
///
/// M6 用 mpsc channel + 单 worker 线程（串行执行）。
/// M7 扩展为 worker 池（N 个线程并行消费）。
pub struct TaskQueue {
    tx: mpsc::Sender<TaskMessage>,
    handle: Option<thread::JoinHandle<()>>,
}

enum TaskMessage {
    Run { task: Task },
    Shutdown,
}

impl TaskQueue {
    /// 构建队列并启动后台 worker。
    ///
    /// M6 阶段用单 worker 串行执行；`workers` 参数暂用于 M7 扩展。
    pub fn new(_cycle_config: orcha_core::CycleConfig) -> Self {
        let (tx, rx) = mpsc::channel::<TaskMessage>();

        let handle = thread::spawn(move || {
            for msg in rx {
                match msg {
                    TaskMessage::Run { task } => {
                        // trace ID = task.id，贯穿 Gateway → Core → history
                        eprintln!(
                            "[gateway] worker 收到任务 trace_id={} desc=\"{}\" (M6 skeleton，暂不执行 Cycleround)",
                            task.id, task.description
                        );
                        // M6 骨架：打印日志即可。
                        // M7 接入后：store.update → RUNNING → cycle.run_streaming → 消费 RoundEvent → DONE/FAILED
                    }
                    TaskMessage::Shutdown => break,
                }
            }
            eprintln!("[gateway] worker 已退出");
        });

        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// 入队一个任务（非阻塞）。
    pub fn enqueue(&self, task: Task) -> Result<()> {
        self.tx
            .send(TaskMessage::Run { task })
            .map_err(|_| anyhow::anyhow!("worker 线程已关闭"))?;
        Ok(())
    }

    /// 阻塞主线程（M6 阶段用，M7 改为事件驱动）。
    pub fn blocking_serve(self) {
        eprintln!("[gateway] 任务队列已启动（M6 skeleton，Ctrl-C 退出）");
        // M6 阶段：阻塞直到收到 Ctrl-C
        // M7 会接入 IM 事件驱动，不再阻塞
        loop {
            thread::park();
        }
    }

    /// 优雅关闭。
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(TaskMessage::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 任务存储后端 trait（让 queue 不直接依赖具体 store 实现）。
///
/// M6 阶段是 marker trait；M7 接入后 queue 会调 store.update 改任务状态。
pub trait TaskStoreBackend: Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_accepts_tasks() {
        let mut queue = TaskQueue::new(orcha_core::CycleConfig::default());

        let task = Task::new("T-test".to_string(), "test task".to_string());
        queue.enqueue(task).unwrap();

        // 给 worker 一点时间处理
        thread::sleep(std::time::Duration::from_millis(100));

        queue.shutdown();
    }
}

//! 多核心优化的**基础框架**：固定线程数的工作池 + 顺序保持的并行映射。
//!
//! # 为什么自研而不上 rayon
//!
//! 本项目的并行需求很窄：**任务少而重**（一屏十几张图的解码）、结果必须
//! **按下标回填**（并行不能改变任何可见顺序）、单个任务失败**不能带走主线程**。
//! rayon 的 work-stealing 与全局共享池在这里是负资产：多一个重依赖，线程数
//! 不可控，且它的 `par_iter().collect()` 在任务 panic 时同样会传播。
//!
//! # 三条保证
//!
//! 1. **线程数固定、懒启动**：工作线程只在 `Pool::new` 时创建一次，之后睡在
//!    条件变量上——空闲时 **0 CPU**，不像 `std::thread::scope` 那样每次调用
//!    都要创建/销毁 N 个线程（每个线程在 Windows 上是几十微秒 + 1MB 栈）。
//! 2. **结果按序**：`parallel_map` 的返回值与入参按下标一一对应，与串行执行
//!    的结果逐位相同。
//! 3. **单任务失败不外溢**：任一任务 `panic!` 只把**自己那一格**留成 `None`，
//!    其余照常完成；等待侧有超时兜底，池子出问题也不会让主线程永久挂起。
//!
//! # 怎么接入一个新的并行消费者
//!
//! ```text
//! 主线程：读数据（可能非 Send，例如 rusqlite 连接）
//!        ↓ 把「字节 + 键」这类 **owned + Send** 的负载装成 Vec<T>
//! parallel::global().parallel_map(jobs, |job| -> R { 纯计算 })
//!        ↓ Vec<Option<R>>（按下标对齐）
//! 主线程：按序写回缓存 / 界面
//! ```
//!
//! 只有「纯计算」那一段能进池子——`Service`（rusqlite）**不是 `Send`**，
//! 任何需要它的工作必须留在主线程做完再分发（本项目图片缩略图就是这么接的）。
//!
//! # 诊断
//!
//! - 线程数可用环境变量 `MC_WORKER_THREADS` 覆盖（默认 `min(核数, 8)`），
//!   设 **0** 则完全退化为串行，用于 A/B 对照；
//! - [`Pool::stats`] 给出提交数 / 完成数 / panic 数，配合
//!   `RUST_LOG=modular_clipboard_core=debug` 观察。

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Duration;

/// 工作线程的默认上限。
///
/// 取 8 而不是 `available_parallelism()` 的裸值：本项目的任务是**重 IO/解码**
/// 型，几十个线程只会互相抢 L3 与内存带宽，收益曲线在 4~8 之间就平了。
const DEFAULT_MAX_THREADS: usize = 8;

/// 环境变量：覆盖工作线程数（0 = 完全串行）。
pub const ENV_WORKER_THREADS: &str = "MC_WORKER_THREADS";

/// 等待结果的超时（池子异常时不能让主线程永久挂起）。
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 池子的运行时统计（诊断用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// 提交给池子的任务块数。
    pub submitted: usize,
    /// 已完成的任务块数。
    pub completed: usize,
    /// 发生 panic 的**单项**任务数（每格只留 `None`，不外溢）。
    pub panics: usize,
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Shared {
    queue: Mutex<VecDeque<Task>>,
    /// 新任务到达时唤醒一个工作线程。
    cv: Condvar,
    /// 停机标志：置位后工作线程排空队列即退出。
    stop: AtomicBool,
    submitted: AtomicUsize,
    completed: AtomicUsize,
    panics: AtomicUsize,
}

/// 固定线程数的工作池。
///
/// 不实现 `Clone`：池子应当被共享（`Arc<Pool>`）而不是复制语义。
pub struct Pool {
    shared: Arc<Shared>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    threads: usize,
}

impl Pool {
    /// 创建 `threads` 个工作线程的池子。
    ///
    /// `threads == 0` 时不建任何线程：之后所有 `parallel_map` 都在调用线程
    /// 内串行执行（`spawn` 的火焰任务也同样就地执行），用于 A/B 对照。
    pub fn new(threads: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            submitted: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            panics: AtomicUsize::new(0),
        });
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let sh = Arc::clone(&shared);
            let h = std::thread::Builder::new()
                .name(format!("mc-worker-{i}"))
                .spawn(move || worker_loop(sh))
                .expect("创建工作线程失败");
            handles.push(h);
        }
        Self {
            shared,
            handles: Mutex::new(handles),
            threads,
        }
    }

    /// 工作线程数（0 = 纯串行池）。
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// 诊断统计。
    pub fn stats(&self) -> Stats {
        Stats {
            submitted: self.shared.submitted.load(Ordering::Relaxed),
            completed: self.shared.completed.load(Ordering::Relaxed),
            panics: self.shared.panics.load(Ordering::Relaxed),
        }
    }

    /// 进程级共享池：第一次调用时创建，空闲时所有工作线程睡在条件变量上。
    ///
    /// 用 `OnceLock` 而不是 `lazy_static`：标准库就够，不必为单例再引依赖。
    /// 这个池子**故意不关闭**（它是进程生命周期的资源）；`Drop` 的停机逻辑
    /// 只服务于测试与将来可能的局部池。
    pub fn global() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        POOL.get_or_init(|| {
            let n = std::env::var(ENV_WORKER_THREADS)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|p| p.get().min(DEFAULT_MAX_THREADS))
                        .unwrap_or(2)
                });
            Pool::new(n)
        })
    }

    /// 火焰任务（不等结果）。
    ///
    /// 线程数为 0 时就地执行——A/B 对照时不该改变「这个任务会不会跑」。
    pub fn spawn(&self, task: impl FnOnce() + Send + 'static) {
        if self.threads == 0 {
            task();
            return;
        }
        self.submit(Box::new(task));
    }

    /// 顺序保持的并行映射：结果与入参**按下标一一对应**。
    ///
    /// - `jobs` 为空 ⇒ 返回空；
    /// - `jobs.len() == 1` 或线程数为 0 ⇒ **就地串行**，不进队列（省一次
    ///   跨线程往返，小批量时并行本身就不划算）；
    /// - 单项 panic ⇒ 那一格为 `None`，其余照常（见 [`Stats::panics`]）。
    ///
    /// 分块是**连续静态切块**而不是动态 work-stealing：本项目每个任务的
    /// 负载高度同质（一张图的解码），静态切块既保住了顺序与缓存局部性，
    /// 又省掉共享队列上的逐任务同步。
    pub fn parallel_map<T, R, F>(&self, jobs: Vec<T>, f: F) -> Vec<Option<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
        F: Fn(T) -> R + Send + Sync + 'static,
    {
        let n = jobs.len();
        if n == 0 {
            return Vec::new();
        }
        let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
        if n == 1 || self.threads == 0 {
            for (i, job) in jobs.into_iter().enumerate() {
                out[i] = self.run_one(|| f(job));
            }
            return out;
        }

        let workers = self.threads.min(n);
        let chunk = n.div_ceil(workers);
        let f = Arc::new(f);
        let (tx_done, rx_done) = mpsc::channel::<Vec<(usize, Option<R>)>>();
        let mut sent = 0usize;

        let mut iter = jobs.into_iter().enumerate();
        loop {
            let mut part: Vec<(usize, T)> = Vec::with_capacity(chunk.min(n));
            for _ in 0..chunk {
                match iter.next() {
                    Some(item) => part.push(item),
                    None => break,
                }
            }
            if part.is_empty() {
                break;
            }
            let f = Arc::clone(&f);
            let tx = tx_done.clone();
            let sh = Arc::clone(&self.shared);
            self.submit(Box::new(move || {
                let rows = catch_unwind(AssertUnwindSafe(|| {
                    part.into_iter()
                        .map(|(i, job)| {
                            let r = catch_unwind(AssertUnwindSafe(|| f(job)));
                            match r {
                                Ok(v) => (i, Some(v)),
                                Err(_) => {
                                    sh.panics.fetch_add(1, Ordering::Relaxed);
                                    (i, None)
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                }));
                // 整块也兜一层：panic 在「分配结果 Vec」这类地方发生时，
                // 不能让等待侧少收到一块而卡住。
                let rows = rows.unwrap_or_default();
                let _ = tx.send(rows);
            }));
            sent += 1;
        }

        for _ in 0..sent {
            match rx_done.recv_timeout(JOIN_TIMEOUT) {
                Ok(rows) => {
                    for (i, r) in rows {
                        if let Some(slot) = out.get_mut(i) {
                            *slot = r;
                        }
                    }
                }
                Err(_) => {
                    // 池子异常（线程死了 / 任务丢了）：拿多少算多少，
                    // 剩下的格子保持 `None`——不能让界面永久卡死。
                    tracing::warn!("并行任务等待超时，结果可能不完整");
                    break;
                }
            }
        }
        out
    }

    /// 单项执行 + panic 记账（串行路径也用，保证两条路径语义一致）。
    fn run_one<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(v) => Some(v),
            Err(_) => {
                self.shared.panics.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    fn submit(&self, task: Task) {
        let mut q = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        q.push_back(task);
        self.shared.submitted.fetch_add(1, Ordering::Relaxed);
        drop(q);
        self.shared.cv.notify_one();
    }

    /// 停机：排空队列后逐一 join。
    ///
    /// 只在**拥有**池子（非全局单例）时有意义；测试靠它保证不留野线程。
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 工作线程主循环：有任务就跑，没有就睡（**空闲 0 CPU**）。
fn worker_loop(shared: Arc<Shared>) {
    loop {
        let task = {
            let mut q = shared
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(t) = q.pop_front() {
                    break t;
                }
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                q = shared
                    .cv
                    .wait(q)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        task();
        shared.completed.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn empty_jobs_return_empty() {
        let p = Pool::new(2);
        let out: Vec<Option<i32>> = p.parallel_map(Vec::<i32>::new(), |x| x);
        assert!(out.is_empty());
        p.shutdown();
    }

    /// 顺序必须与串行执行逐位一致——这是框架对调用方的核心承诺。
    #[test]
    fn results_keep_input_order() {
        let p = Pool::new(4);
        let jobs: Vec<usize> = (0..97).collect();
        let out = p.parallel_map(jobs, |x| x * x + 3);
        assert_eq!(out.len(), 97);
        for (i, r) in out.iter().enumerate() {
            assert_eq!(*r, Some(i * i + 3), "下标 {i} 的结果错位");
        }
        p.shutdown();
    }

    #[test]
    fn single_item_runs_inline() {
        let p = Pool::new(4);
        let out = p.parallel_map(vec![21usize], |x| x * 2);
        assert_eq!(out, vec![Some(42)]);
        p.shutdown();
    }

    /// 线程数为 0 时也必须给出正确结果（A/B 对照的串行档）。
    #[test]
    fn zero_threads_degrades_to_serial() {
        let p = Pool::new(0);
        let out = p.parallel_map(vec![1usize, 2, 3, 4], |x| x + 10);
        assert_eq!(out, vec![Some(11), Some(12), Some(13), Some(14)]);
        p.shutdown();
    }

    /// 单项 panic 不得外溢：它那一格为 `None`，其余照常完成。
    #[test]
    fn panic_is_isolated_per_item() {
        let p = Pool::new(4);
        let out = p.parallel_map((0..40usize).collect::<Vec<_>>(), |x| {
            assert_ne!(x, 13, "这一项故意 panic");
            x + 1
        });
        assert_eq!(out.len(), 40);
        assert_eq!(out[13], None, "panic 的那一项应是 None");
        for (i, r) in out.iter().enumerate() {
            if i != 13 {
                assert_eq!(*r, Some(i + 1), "下标 {i} 被波及");
            }
        }
        assert!(p.stats().panics >= 1, "panic 必须被记账");
        p.shutdown();
    }

    /// 池子必须**复用**线程：连续多次调用不能每次都新建线程。
    #[test]
    fn threads_are_reused_across_calls() {
        let p = Pool::new(3);
        for round in 0..5 {
            let out = p.parallel_map((0..12usize).collect::<Vec<_>>(), move |x| x + round);
            assert_eq!(out.len(), 12);
        }
        assert_eq!(p.threads(), 3);
        p.shutdown();
        // 每轮 3 块（12 项 / 3 线程 ⇒ 每块 4 项）⇒ 共 15 块。
        // A worker bumps `completed` after the task sends its result, so the
        // counter is only exact once shutdown has joined every worker.
        assert_eq!(p.stats().completed, 15, "任务块应全部完成");
    }

    /// 停机后队列里的任务也应跑完（不是丢弃）。
    #[test]
    fn shutdown_drains_the_queue() {
        let p = Pool::new(2);
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let h = std::sync::Arc::clone(&hits);
            p.spawn(move || {
                h.fetch_add(1, Ordering::Relaxed);
            });
        }
        p.shutdown();
        assert_eq!(hits.load(Ordering::Relaxed), 20, "停机时队列里的任务必须跑完");
    }

    /// 全局池是同一个实例（不会每次调用新建线程）。
    #[test]
    fn global_pool_is_a_singleton() {
        let a = Pool::global() as *const Pool;
        let b = Pool::global() as *const Pool;
        assert_eq!(a, b, "全局池必须是单例");
    }
}

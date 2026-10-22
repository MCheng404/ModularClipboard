//! 把**构建时的 git 信息**注入到 `env!()` 里，供「关于」页动态显示版本。
//!
//! # 为什么版本号要「动态获取」
//!
//! 稳定版与开发版是两种东西，不能共用一个号：
//!
//! - **稳定版**有明确的语义版本（`0.2`），发出去就固定了；
//! - **cl 版**是开发过程中的构建，号上不去也下不来，但必须能唯一
//!   对应到一次提交 —— 排障时用户截图上的「cl-a1b2c3d」就是查得到的
//!   那一次代码。
//!
//! 把 commit 短 hash 用 `cargo:rustc-env` 注入后，UI 侧零运行时成本
//! （一个 `env!()` 而已），也不会因为「运行时去跑 git」而拖慢启动。
//!
//! # 拿不到 git 时怎么办
//!
//! 发布构建机可能没有 `.git`（源码 tarball 构建），或者根本没装 git。
//! 这两种情况都是**合法**的，不是错误：`GIT_SHORT_HASH` 留空即可，
//! UI 侧据此回落到稳定版号。绝不能因为拿不到 hash 就让构建失败——
//! 那等于「源码包永远编不出来」。
//!
//! # 何时重新运行
//!
//! `cargo:rerun-if-changed` 指向 `.git/HEAD` 与 `.git/index`：任何一次
//! `git commit` / `git checkout` 都会改它们，于是 hash 跟着更新，不会
//! 出现「代码变了但 About 页上的 hash 还是旧的」。
//!
//! NOTE: git 的工作目录必须是**仓库根**。`CARGO_MANIFEST_DIR` 是
//! `crates/modular-clipboard-ui`，而 `.git` 在它上两级 —— 用 `current_dir`
//! 显式指定，否则在某些调用方式（`cargo build -p`）下 git 会跑到别的
//! 目录里去找仓库，拿到空结果还查不出来。
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest.join("../..");

    // 仓库根下的 `.git`：HEAD 与 index 是「提交了没有 / 切没切分支」的
    // 最小充分信号（refs 里的对象变化会被 index/HEAD 体现）。
    let git_dir = repo.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());

    let hash = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(&repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        // 空串 = 没有 git / 不是仓库：UI 侧据此回落到稳定版号。
        .unwrap_or_default();

    // 永远输出这个变量（哪怕空）：UI 用 `env!("GIT_SHORT_HASH")` 读取，
    // 变量缺失会让**编译**失败，而那是完全没必要的门槛。
    println!("cargo:rustc-env=GIT_SHORT_HASH={hash}");
}

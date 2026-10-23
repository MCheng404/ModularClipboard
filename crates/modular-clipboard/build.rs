//! 把 `assets/icon.ico` 作为 Windows 资源链进 exe。
//!
//! # 为什么要这一步
//!
//! Rust 的 `windows_subsystem = "windows"` 只决定「不弹控制台」，
//! **不会**给 exe 带图标。没有图标资源时，资源管理器、任务栏、
//! Alt+Tab 显示的都是一张白纸——用户第一眼看到的就是这个。
//!
//! 做的只有一件事：用 Windows SDK 的 `rc.exe` 把 `.ico` 编成 `.res`，
//! 再把 `.res` 作为链接参数交给 `link.exe`。
//!
//! # 找不到 rc.exe 时怎么办
//!
//! **只警告，不报错**。构建机可能没装 Windows SDK（例如纯 Rust 的
//! 交叉环境），那时候「exe 没有图标」是可以接受的降级，
//! 而「整个项目构建不了」不是。所以这里打印一条明确的警告让人知道
//! 产物缺了什么，其余照常。
//!
//! 本项目的构建规范（见 BUILD.md）要求先 `source scripts/env.sh`，
//! 那里会带上 SDK 路径；直接 `cargo build` 时靠下面的目录扫描兜底。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("../../assets/icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    // 只给 Windows 目标的二进制加资源。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if !icon.is_file() {
        println!(
            "cargo:warning=缺少 {}，exe 将没有图标（跑 scripts/make_icons.py 生成）",
            icon.display()
        );
        return;
    }

    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=未找到 rc.exe（Windows SDK），exe 将没有图标。\
             装好 SDK 或先 source scripts/env.sh 再构建。"
        );
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("app-icon.rc");
    let res_file = out_dir.join("app-icon.res");

    // .rc 里的路径要写成反斜杠，而且**反斜杠必须双写**。
    //
    // 踩过：`..\..\assets\icon.ico` 里的 `\a` 会被 rc 当成转义序列
    // （响铃），路径变成 `..\..ssets\icon.ico`，报 RC2135 file not found。
    // 这个错误只在 rc 的 stderr 里，构建日志看不到——所以下面把
    // rc 的输出捕获下来一起打印。
    let icon_lit = icon
        .to_string_lossy()
        .replace('/', "\\")
        .replace('\\', "\\\\");
    std::fs::write(&rc_file, format!("1 ICON \"{icon_lit}\"\n"))
        .expect("写入 .rc 失败");

    let output = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            // 直接把手写的 .res 交给链接器：link.exe 接受 .res 输入。
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stdout);
            let msg = msg.trim();
            println!("cargo:warning=rc.exe 失败（{}）：{msg}", o.status);
            println!("cargo:warning=本次构建的 exe 不会有图标");
        }
        Err(e) => println!("cargo:warning=调用 rc.exe 失败（{e}），exe 将没有图标"),
    }
}

/// 找一个可用的 `rc.exe`。
///
/// 顺序：环境变量（`scripts/env.sh` 会设）→ Windows Kits 目录扫描。
/// 扫描时取**版本号最大**的那个：SDK 目录里同时留着好几个版本，
/// 随便挑一个可能撞上不完整的安装。
fn find_rc() -> Option<PathBuf> {
    for key in ["RC", "RC_x86_64_pc_windows_msvc"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            let p = PathBuf::from(v);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let kits = [
        std::env::var("ProgramFiles(x86)").unwrap_or_default(),
        std::env::var("ProgramFiles").unwrap_or_default(),
    ];
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for root in kits {
        if root.is_empty() {
            continue;
        }
        let bin = Path::new(&root).join("Windows Kits/10/bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };
        for e in entries.flatten() {
            let cand = e.path().join("x64/rc.exe");
            if !cand.is_file() {
                continue;
            }
            let ver: Vec<u64> = e
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect();
            if best.as_ref().is_none_or(|(v, _)| ver > *v) {
                best = Some((ver, cand));
            }
        }
    }
    best.map(|(_, p)| p)
}

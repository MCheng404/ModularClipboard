"""把含中文的 Windows 批处理/脚本从 UTF-8 转成 GBK。

# 为什么需要转换

`cmd.exe` 与 `wscript.exe` 都按**系统 ANSI 码页**读脚本，而中文
Windows 的 ANSI 就是 GBK。UTF-8 编码的 `.cmd` 里只要有中文注释，
双击运行时 cmd 就会报「命令语法不正确」—— 而且报错位置通常指向
一个**看起来完全正常**的命令行，极难定位。

项目约定：编写/生成 `.bat` / `.cmd` 一律存 GBK。

用法：
    python fix_script_encoding.py check  # 只检查
    python fix_script_encoding.py fix    # 转换
"""

from __future__ import annotations

import io
import sys
from pathlib import Path

# 需要检查的脚本。`.vbs` 由 wscript 读取，同样受 ANSI 码页约束。
TARGETS = ["启动剪贴板.cmd", "启动剪贴板.vbs"]


def has_cjk(text: str) -> bool:
    """是否含中日韩统一表意文字。

    只看 CJK 区间即可：ASCII 内容在两种编码下完全一致，转码无意义。
    """
    return any("一" <= c <= "鿿" for c in text)


def analyze(path: Path) -> tuple[str, str | None, bool]:
    """返回 (编码, 解码后的文本 或 None, 是否含中文)。"""
    raw = path.read_bytes()
    for enc in ("utf-8", "gbk"):
        try:
            text = raw.decode(enc)
        except UnicodeDecodeError:
            continue
        # UTF-8 能解码任何合法 UTF-8；GBK 几乎也能解��大部分字节序列。
        # 用「能否严格解码 + 重新编码是否字节等价」来消歧。
        try:
            if text.encode(enc) == raw:
                return enc, text, has_cjk(text)
        except UnicodeEncodeError:
            continue
    return "未知", None, False


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "check"
    root = Path(__file__).resolve().parent.parent
    changed = False

    for name in TARGETS:
        path = root / name
        if not path.exists():
            print(f"[跳过] {name} 不存在")
            continue

        enc, text, cn = analyze(path)
        if not cn:
            print(f"[无需] {name}：编码={enc}，不含中文")
            continue

        if enc == "gbk":
            print(f"[OK]   {name}：已是 GBK")
            continue

        if mode == "check":
            print(f"[需转] {name}：编码={enc}，含中文 → 应为 GBK")
            continue

        # UTF-8（含中文）→ GBK。
        try:
            path.write_bytes(text.encode("gbk"))
        except UnicodeEncodeError as e:
            # 个别字符（如生僻字 emoji）在 GBK 里没有对应码位。
            print(f"[失败] {name}：有字符无法编码为 GBK：{e}")
            print("       请改用纯 BMP 内、GBK 可表示的文字")
            changed = True
            continue
        print(f"[已转] {name}：UTF-8 → GBK")
        changed = True

    # 转完后立刻回读校验，确认真的能按 GBK 解出来。
    if mode == "fix":
        print("\n=== 回读校验 ===")
        for name in TARGETS:
            path = root / name
            if not path.exists():
                continue
            enc, text, cn = analyze(path)
            if cn:
                preview = next(
                    (ln for ln in (text or "").splitlines() if has_cjk(ln)), ""
                )
                print(f"{name}: 编码={enc}")
                print(f"  含中文行: {preview[:60]}")
    return 1 if changed and mode == "check" else 0


if __name__ == "__main__":
    raise SystemExit(main())
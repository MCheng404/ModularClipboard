//! 交互动效：标量 / 颜色 / 矩形的补间。
//!
//! # 为什么自研而不引 crate
//!
//! ① egui 生态的动画 crate 普遍假设 eframe/winit 的时间源，本项目
//! ash 直驱、无 eframe，时间源要自己传，crate 的时间管理反而是负担；
//! ② 需求面很窄——补间 + 缓动 + 按 Context 的注册表，引依赖的收益/风险
//! 比不成立；③ egui 自带 `animate_bool` 只覆盖 0→1 的布尔场景，缺的是
//! 「任意目标值 + 任意时长 + 颜色/矩形」这一层薄封装。
//!
//! # 三个不变量（单测逐一守着）
//!
//! 1. **retarget 连续**：目标值中途再变时，从**当前显示值**续接，不跳变；
//! 2. **完成吸附**：时长用尽后值精确等于目标（EPSILON 兜底）；
//! 3. **零每帧堆分配**：状态存在 `ctx.data` 的临时存储里，每帧每个活动
//!    动画只有几次 f32 运算 + 一次 HashMap 查找（首帧插入除外）。
//!
//! # 时间源
//!
//! 用 `ctx.input(|i| i.time)`（egui 自管的秒计时），不引 chrono/instant，
//! 也不与墙钟挂钩——补间进度因此与帧率无关：120Hz 与 60Hz 下观感一致。

use std::collections::HashMap;
use std::time::Duration;

use egui::{Color32, Context, Id, Rect, emath::lerp};

/// 缓动函数。
///
/// 取主流桌面端 motion 规范的常用子集：进出各一支、线性一支。
/// 新增曲线只改这里一处（纯函数，单测直接覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ease {
    /// 匀速。仅用于进度条这类「线性即语义」的场景。
    #[default]
    Linear,
    /// 先快后慢（出场）。悬停、展开的默认选择。
    OutCubic,
    /// 先极快后极慢（出场）。短距离位移（toast 滑入）用。
    OutExpo,
    /// 两端慢中间快（入场+出场）。主题切换、面板开合。
    InOutCubic,
}

impl Ease {
    /// 把归一化进度 `t ∈ [0,1]` 映射到 eased 进度。
    ///
    /// 纯函数：不 clamp 输入之外的任何东西（调用方负责 clamp），
    /// 便于单测直接验证单调性与端点。
    pub fn apply(self, t: f32) -> f32 {
        match self {
            Self::Linear => t,
            Self::OutCubic => 1.0 - (1.0 - t).powi(3),
            Self::OutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f32.powf(-10.0 * t)
                }
            }
            Self::InOutCubic => {
                if t < 0.5 {
                    4.0 * t.powi(3)
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// 单个补间的状态。
#[derive(Debug, Clone, Copy)]
struct Tween {
    /// 起始值（retarget 时为当时的显示值）。
    from: f32,
    /// 目标值。
    to: f32,
    /// 起始时刻（egui 秒计时）。
    t0: f64,
    /// 时长（秒）。
    dur: f32,
    ease: Ease,
}

impl Tween {
    /// 取 `now`（egui 秒）时刻的显示值。完成后精确吸附到 `to`。
    fn value_at(&self, now: f64) -> f32 {
        if self.dur <= 0.0 || now >= self.t0 + self.dur as f64 {
            return self.to;
        }
        let t = ((now - self.t0) / self.dur as f64) as f32;
        lerp(self.from..=self.to, self.ease.apply(t.clamp(0.0, 1.0)))
    }
}

/// 每个 `Context` 一份的动画注册表，存在 egui 的 data store 里。
///
/// NOTE: `Clone` 是 egui `IdTypeMap` 临时存储的硬约束（`insert_temp` /
/// `get_temp_mut_or_default` 都要求 `T: Clone + Send + Sync`）。
/// 本类型只有一张 `HashMap<Id, Tween>`（Tween 是 Copy），克隆代价
/// 仅在有动画活动时存在，且每帧至多一次。
#[derive(Default, Clone)]
struct AnimStore {
    map: HashMap<Id, Tween>,
}

/// data store 的键。一个 Context 一份，随 Context 生命周期。
fn store_id() -> Id {
    Id::new("mcb_anim_store")
}

/// 取 `id` 对应的补间当前值；`target` 变化时从当前显示值续接。
///
/// `id` 由调用方构造，必须**稳定**（同一控件每帧同一个 id）且**互异**
/// （不同控件不同 id）——例如 `Id::new(("row_hover", item_id))`。
pub fn tween_f32(ctx: &Context, id: Id, target: f32, dur: Duration, ease: Ease) -> f32 {
    let now = ctx.input(|i| i.time);
    // CAUTION: 全部逻辑必须在闭包**内**完成。从 `data_mut` 的闭包里
    // 返回 `&mut AnimStore` 会撞上「返回的引用比被借用的参数活得久」
    // （E0495 变体），egui 的 `data_mut` 签名不允许。返回值是 `f32`
    // （Copy）就没有这个问题。
    ctx.data_mut(|d| {
        let store = d.get_temp_mut_or_default::<AnimStore>(store_id());
        let t = store.map.entry(id).or_insert(Tween {
            from: target,
            to: target,
            t0: now,
            dur: dur.as_secs_f32(),
            ease,
        });
        // 目标变了：从**当前显示值**续接（retarget 连续性），避免跳变。
        // EPSILON 吸附：微小抖动不重启动画。
        if (t.to - target).abs() > f32::EPSILON {
            let cur = t.value_at(now);
            *t = Tween {
                from: cur,
                to: target,
                t0: now,
                dur: dur.as_secs_f32(),
                ease,
            };
        }
        t.value_at(now)
    })
}

/// 以显式的 `from → to` **重新开始**一段补间（覆盖既有状态），返回当前值。
///
/// # 与 [`tween_f32`] 的分工（这条是「主题切换失效」的根因）
///
/// `tween_f32` 是**惰性**的：目标值没变时它只是取当前值，已经到目标就
/// 什么都不做。对「值跟着某个状态走」的场景（悬停、开关）这是对的。
/// 但「每次触发都重播一遍固定动画」的场景（主题切换的 0→1 进度）不能
/// 这么用——目标恒为 `1.0`，第一次调用就把状态插成 `from == to == 1.0`，
/// 之后无论切多少次主题，`tween_f32` 都返回 1.0，动画**永远不会重播**。
///
/// 所以重播必须有一个显式入口：`restart(.., 0.0, 1.0, ..)` 把状态重置成
/// 「从 0 开始、目标 1」，随后每帧的 `tween_f32(ctx, id, 1.0, ..)`
/// 就能正常推进它（目标一致 ⇒ 不 retarget，只按时间前进）。
pub fn restart(ctx: &Context, id: Id, from: f32, to: f32, dur: Duration, ease: Ease) -> f32 {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| {
        let store = d.get_temp_mut_or_default::<AnimStore>(store_id());
        let t = Tween {
            from,
            to,
            t0: now,
            dur: dur.as_secs_f32(),
            ease,
        };
        let v = t.value_at(now);
        store.map.insert(id, t);
        v
    })
}

/// 颜色补间：四通道各自补间后组装。
///
/// 透明度参与补间（淡入淡出的常用轴）。
///
/// CAUTION: `to_array()` 返回的是**预乘**分量（[`Color32`] 内部按预乘
/// alpha 存储），补间后必须用 `from_rgba_premultiplied` 重组——用
/// `from_rgba_unmultiplied` 会二次预乘，半透明色在补间途中整体发暗
/// （2026-10-17 修，与 `paint::fade_color` 同一个坑）。
pub fn tween_color(ctx: &Context, id: Id, target: Color32, dur: Duration) -> Color32 {
    let [r, g, b, a] = target.to_array();
    let r = tween_f32(ctx, id.with("r"), r as f32, dur, Ease::OutCubic) as u8;
    let g = tween_f32(ctx, id.with("g"), g as f32, dur, Ease::OutCubic) as u8;
    let b = tween_f32(ctx, id.with("b"), b as f32, dur, Ease::OutCubic) as u8;
    let a = tween_f32(ctx, id.with("a"), a as f32, dur, Ease::OutCubic) as u8;
    Color32::from_rgba_premultiplied(r, g, b, a)
}

/// 矩形补间：四边各自补间。用于面板滑入、选中块位移。
pub fn tween_rect(ctx: &Context, id: Id, target: Rect, dur: Duration, ease: Ease) -> Rect {
    let x0 = tween_f32(ctx, id.with("x0"), target.min.x, dur, ease);
    let y0 = tween_f32(ctx, id.with("y0"), target.min.y, dur, ease);
    let x1 = tween_f32(ctx, id.with("x1"), target.max.x, dur, ease);
    let y1 = tween_f32(ctx, id.with("y1"), target.max.y, dur, ease);
    Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
}

/// 清空某 id 的补间状态（控件销毁时调用，避免注册表无限增长）。
pub fn clear(ctx: &Context, id: Id) {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<AnimStore>(store_id())
            .map
            .remove(&id);
    });
}

/// `id` 是否已有补间条目（不论是否已到达目标）。
///
/// 供「一次性入场动画」判断「这个控件本会话首次出现吗」——到达目标后
/// 条目仍在（完成态不是消失态），这是与 [`busy`] 的关键区别。
pub fn has(ctx: &Context, id: Id) -> bool {
    ctx.data(|d| {
        d.get_temp::<AnimStore>(store_id())
            .is_some_and(|s| s.map.contains_key(&id))
    })
}

/// 一次性入场动画：`id` **首次出现**时从 0 重播到 1，之后直接取当前值。
///
/// # 为什么需要它（与 [`tween_f32` 的分工）
///
/// `tween_f32` 是惰性的：首帧插入时 `from == to == target`，控件第一次
/// 出现就**直接落在目标上**——对「跟着状态走」的场景（悬停、选中）这是
/// 对的。但「每次出现都播一遍入场」的场景（空状态淡入、Toast 滑入）
/// 需要相反的行为：第一次出现时从 0 开始放。
///
/// 判据用 [`has`] 而不是「是否在飞」：完成后条目仍在，不会每帧重播
/// （否则空状态会无限重启动画，永远停在半透明）。
///
/// 同一 `id` 在**同一帧被多次调用**是安全的：第一调用责重启，后续
/// 调用只推进——两个区域用同一条消息时两者同步淡入，不会互相覆盖。
pub fn fade_in_once(
    ctx: &Context,
    id: Id,
    dur: Duration,
    ease: Ease,
) -> f32 {
    if !has(ctx, id) {
        restart(ctx, id, 0.0, 1.0, dur, ease);
    }
    tween_f32(ctx, id, 1.0, dur, ease)
}

/// 是否仍有补间在飞行（未达目标）。
///
/// 帧循环的**空闲闸门**用它决定「没有事件也要渲染」：否则动画只在
/// 事件驱动的帧上推进——鼠标不动时主题切换会卡在半路、hover 淡出
/// 会被剪断。判定容差 1e-4：肉眼不可见的残余不算在飞。
pub fn busy(ctx: &Context) -> bool {
    let now = ctx.input(|i| i.time);
    ctx.data(|d| {
        d.get_temp::<AnimStore>(store_id()).is_some_and(|s| {
            s.map
                .values()
                .any(|t| (t.value_at(now) - t.to).abs() > 1e-4)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_time(t: f64) -> (Context, egui::RawInput) {
        let ctx = Context::default();
        let input = egui::RawInput {
            time: Some(t),
            ..Default::default()
        };
        // 必须先跑一帧让 input.time 生效（egui 在 begin_pass 时消费）。
        let mut out = ctx.run_ui(input.clone(), |_| {});
        out.textures_delta.clear();
        (ctx, input)
    }

    #[test]
    fn easing_is_monotonic_and_bounded() {
        for ease in [Ease::Linear, Ease::OutCubic, Ease::OutExpo, Ease::InOutCubic] {
            let mut prev = -1.0;
            for i in 0..=20 {
                let t = i as f32 / 20.0;
                let v = ease.apply(t);
                assert!((0.0..=1.0).contains(&v), "{ease:?} 越界：{v}");
                assert!(v >= prev - 1e-6, "{ease:?} 非单调：{prev} -> {v}");
                prev = v;
            }
            assert_eq!(ease.apply(0.0), 0.0, "{ease:?} 起点应为 0");
            assert!((ease.apply(1.0) - 1.0).abs() < 1e-6, "{ease:?} 终点应为 1");
        }
    }

    #[test]
    fn tween_settles_exactly_on_target() {
        let (ctx, mut input) = ctx_with_time(0.0);
        let id = Id::new("settle");
        // 首帧插入：直接从目标值开始（不做「从虚空飞入」——控件第一次
        // 出现就应该在它的目标状态上）。
        assert_eq!(
            tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear),
            1.0
        );
        // 改目标为 0：从当前显示值 1.0 续接（当帧不跳变）。
        let v = tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::OutCubic);
        assert!((v - 1.0).abs() < 1e-5, "retarget 当帧应停在当前显示值：{v}");
        // 推进到时长之外：必须**精确**吸附到目标（不是「接近」）。
        input.time = Some(10.0);
        let mut out = ctx.run_ui(input, |_| {});
        out.textures_delta.clear();
        let v = tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::OutCubic);
        assert_eq!(v, 0.0, "时长用尽后必须精确等于目标");
    }

    #[test]
    fn retarget_continues_from_current_value() {
        let (ctx, mut input) = ctx_with_time(0.0);
        let id = Id::new("retarget");
        // 首帧插入即目标 0（未悬停态）。
        assert_eq!(
            tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::Linear),
            0.0
        );
        // 悬停：目标 1，从 0 续接，当帧仍是 0。
        assert_eq!(
            tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear),
            0.0
        );
        // t=50ms：线性进度一半。
        input.time = Some(0.05);
        let mut out = ctx.run_ui(input.clone(), |_| {});
        out.textures_delta.clear();
        let mid = tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear);
        assert!((mid - 0.5).abs() < 1e-5, "中点应约 0.5：{mid}");
        // 中途改目标为 0（移开鼠标）：必须从 0.5 续接，不跳变。
        let after = tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::Linear);
        assert!(
            (after - 0.5).abs() < 1e-5,
            "retarget 当帧应从当前显示值续接（0.5），实际 {after}"
        );
        // 再推进 50ms（新时长的一半）：0.5 → 0.25。
        input.time = Some(0.10);
        let mut out = ctx.run_ui(input, |_| {});
        out.textures_delta.clear();
        let v = tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::Linear);
        assert!((v - 0.25).abs() < 1e-5, "续接后应走到 0.25，实际 {v}");
    }

    #[test]
    fn clear_removes_state() {
        let (ctx, _input) = ctx_with_time(0.0);
        let id = Id::new("clear-me");
        tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear);
        clear(&ctx, id);
        // 清掉后下一次调用以新目标为起点重新开始（而不是从旧状态续接）。
        let v = tween_f32(&ctx, id, 0.0, Duration::from_millis(100), Ease::Linear);
        assert_eq!(v, 0.0, "清除后应视为全新补间");
    }

    /// 主题切换补间的重播：目标恒为 1.0 时 `tween_f32` 是惰性的，
    /// 必须靠 `restart` 才能重播——这正是「切主题没反应」的根因。
    #[test]
    fn restart_replays_a_fixed_animation() {
        let (ctx, mut input) = ctx_with_time(0.0);
        let id = Id::new("pal_mix_like");
        // 首次调用（惰性）：直接落在目标 1.0 上。
        assert_eq!(
            tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear),
            1.0
        );
        // 再次调用（目标没变）：仍然是 1.0 —— 动画不会自己重播。
        input.time = Some(5.0);
        let mut out = ctx.run_ui(input.clone(), |_| {});
        out.textures_delta.clear();
        assert_eq!(
            tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear),
            1.0,
            "目标不变时 tween_f32 必须惰性（这就是需要 restart 的理由）"
        );

        // restart：从 0 重播，当帧取到起点 0。
        let v = restart(&ctx, id, 0.0, 1.0, Duration::from_millis(100), Ease::Linear);
        assert_eq!(v, 0.0, "重启当帧应停在起点");
        // 之后每帧的 tween_f32（目标 1.0）按时间推进它。
        input.time = Some(5.05);
        let mut out = ctx.run_ui(input.clone(), |_| {});
        out.textures_delta.clear();
        let mid = tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear);
        assert!((mid - 0.5).abs() < 1e-5, "重启后应正常推进，实际 {mid}");
        input.time = Some(5.2);
        let mut out = ctx.run_ui(input, |_| {});
        out.textures_delta.clear();
        assert_eq!(
            tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear),
            1.0,
            "时长用尽后吸附到 1.0"
        );
    }

    /// `restart` 会覆盖既有状态（不是叠加、不是续接）。
    #[test]
    fn restart_overrides_previous_state() {
        let (ctx, mut input) = ctx_with_time(0.0);
        let id = Id::new("override");
        tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear);
        input.time = Some(0.05);
        let mut out = ctx.run_ui(input, |_| {});
        out.textures_delta.clear();
        // 中途重启到另一段动画：起点立刻生效。
        let v = restart(&ctx, id, 0.3, 0.9, Duration::from_millis(100), Ease::Linear);
        assert!((v - 0.3).abs() < 1e-6, "重启当帧应等于 from，实际 {v}");
    }

    /// `restart` 之后 `busy()` 必须为真——否则空闲闸门会跳过帧，
    /// 动画只在事件帧推进（主题切换会卡在半路）。
    #[test]
    fn restart_marks_animation_busy() {
        let (ctx, _input) = ctx_with_time(0.0);
        let id = Id::new("busy-restart");
        tween_f32(&ctx, id, 1.0, Duration::from_millis(100), Ease::Linear);
        assert!(!busy(&ctx), "已完成（惰性 1.0）时不该算在飞");
        restart(&ctx, id, 0.0, 1.0, Duration::from_millis(100), Ease::Linear);
        assert!(busy(&ctx), "重启后必须算在飞");
    }

    /// `fade_in_once`：首现从 0 重播、完成后停稳、**不反复重播**。
    ///
    /// 守卫的是空状态淡入的语义：每帧重播会让它永远停在半透明。
    #[test]
    fn fade_in_once_plays_once_then_settles() {
        let (ctx, mut input) = ctx_with_time(0.0);
        let id = Id::new("empty-in");
        // 首现：从 0 起播（与 tween_f32 的「直接落目标」相反）。
        assert_eq!(
            fade_in_once(&ctx, id, Duration::from_millis(100), Ease::Linear),
            0.0,
            "首次出现必须从 0 开始放"
        );
        assert!(has(&ctx, id), "重播后应留下条目");
        assert!(busy(&ctx), "入场中必须算在飞（否则空闲闸门会掐掉它）");
        // 推进到中点。
        input.time = Some(0.05);
        let mut out = ctx.run_ui(input.clone(), |_| {});
        out.textures_delta.clear();
        let mid = fade_in_once(&ctx, id, Duration::from_millis(100), Ease::Linear);
        assert!((mid - 0.5).abs() < 1e-5, "中点应约 0.5，实际 {mid}");
        // 完成后：稳定在 1，不再重播。
        input.time = Some(1.0);
        let mut out = ctx.run_ui(input, |_| {});
        out.textures_delta.clear();
        for _ in 0..3 {
            assert_eq!(
                fade_in_once(&ctx, id, Duration::from_millis(100), Ease::Linear),
                1.0,
                "完成后必须恒为 1 且不重播（每帧重播会永远停在半透明）"
            );
        }
        assert!(!busy(&ctx), "落定后不该算在飞");
        // `has` 与 `busy` 的区别：落定后条目仍在（完成态 ≠ 消失态）。
        assert!(has(&ctx, id));
        // clear 之后视为全新：下次首现重新从 0 放。
        clear(&ctx, id);
        assert!(!has(&ctx, id));
        assert_eq!(
            fade_in_once(&ctx, id, Duration::from_millis(100), Ease::Linear),
            0.0,
            "清除后应视为首次出现"
        );
    }
}

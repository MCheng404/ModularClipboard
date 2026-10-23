//! 热键录制：`App` 的录制态方法。
//!
//! 录制必须在界面进程里做（按键来自 egui 输入），录完写配置，
//! 常驻进程收到配置保存后自行重注册（`app::daemon::rebind_hotkeys`）。


impl super::App {
/// 修饰键的**物理键**变体。
///
/// egui 0.36 的 `Key` 枚举含这些（eframe 从 winit 的物理键映射而来），
/// 而修饰键的逻辑状态在 `Event::Key::modifiers` 里——两者都会来。
/// 录制时按到修饰键本身必须忽略：不过滤就会把它当成「主键」，
/// `name()` 返回 `ControlLeft` 这类多字符文本，于是弹
/// 「不支持把 ControlLeft 设为快捷键」并**退出录制态**，
/// 用户接下来的组合再也录不进去（2026-10-13 用户实测）。
fn is_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// 录制模式下捕获按键，生成新的快捷键绑定。
///
/// # 为什么要在 UI 线程里做
///
/// 全局热键的注册在托盘/常驻进程，但**录制**必须在界面这边：
/// 键是从 egui 的输入事件里读的，而输入只属于窗口所在的进程。
/// 录完写进配置，常驻进程收到配置保存后会自己重注册
/// （见 `app::daemon::rebind_hotkeys`）。
pub(super) fn capture_recording(&mut self, ctx: &egui::Context) {
    // 只关心「是否在录制」：候选组合记在 paint_state 里，
    // 槽位在确认（Op::FinishRecording）时才用到。
    let Some(_slot) = self.paint_state.hotkey_recording else {
            return;
        };

        // Esc 取消：录到一半想反悔是常态，没有退路会很烦。
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.paint_state.hotkey_recording = None;
            self.paint_state.recording_combo = None;
            // 反悔之后老键必须立刻能用。
            self.set_hotkeys_suspended(false);
            return;
        }

        // 候选组合**只记录不应用**：录制覆盖层实时显示它，用户点击
        // 任意位置确认后才落地（`Op::FinishRecording`）。按一下就改的
        // 设计会让手抖直接换掉热键。
        let captured = ctx.input(|i| {
            i.events.iter().find_map(|ev| match ev {
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if !Self::is_modifier_key(*key) => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, mods)) = captured else {
            return;
        };
        // 挡掉方向键这类非字符键：提示但**不退出录制态**——用户可能
        // 只是按错键，退出录制反而要重新点一次「录制」（旧实现在这里
        // 直接退出，被用户评为「离谱的逻辑」）。
        let name = key.name();
        let mut chars = name.chars();
        let _first = chars.next();
        if chars.next().is_some() && !name.starts_with('F') {
            self.svc.notify(modular_clipboard_core::i18n::t("{name} is not a supported shortcut").replace("{name}", name));
            return;
        }
        let hotkey = modular_clipboard_core::config::Hotkey {
            key: name.to_string(),
            ctrl: mods.ctrl,
            shift: mods.shift,
            alt: mods.alt,
            // egui 的 `command` 在 Windows 上就是 Ctrl；Win 键它不报告，
            // 因此单独问一次系统——Win+V 这类组合必须能录出来。
            win: modular_clipboard_platform::win_key_down(),
        };
        if !hotkey.ctrl && !hotkey.shift && !hotkey.alt && !hotkey.win {
            self.svc.notify(modular_clipboard_core::i18n::t("Hold at least one modifier (Ctrl/Shift/Alt/Win)").into());
            return;
        }
        // 后按的覆盖先按的：用户改主意就再按一次新的。
        self.paint_state.recording_combo = Some(hotkey);
    }

    /// 把一个槽位的绑定落地：写配置、清草稿、恢复注册、落盘。
    ///
    /// 文本输入（`Op::SetHotkeyText` 解析后）、设置卡片「重置」与
    /// 录制确认三条路共用它——分开写过三遍，必漂。
    pub(super) fn apply_hotkey(
        &mut self,
        slot: crate::paint::HotkeySlot,
        hotkey: modular_clipboard_core::config::Hotkey,
    ) {
        match slot {
            crate::paint::HotkeySlot::Toggle => {
                self.svc.state.config.hotkey_toggle = hotkey;
            }
            crate::paint::HotkeySlot::PastePrevious => {
                self.svc.state.config.hotkey_paste_previous = hotkey;
            }
        }
        // 草稿一并清掉：留着旧文本，下次回车会把上次手打的
        // 内容再应用一遍（「录成了框里却还有字」）。
        slot.draft_mut(&mut self.paint_state).clear();
        // 录制期间热键是注销状态。常驻进程模式下
        // `after_config_change` 的 SaveConfig 会让它重注册；
        // 单进程模式只会写盘，得在这儿立即恢复——
        // 用户录完很可能马上按键测试，不能让他吃空窗。
        if !self.svc.is_remote() {
            let failed = modular_clipboard_app::daemon::rebind_hotkeys(&self.svc.state.config);
            self.svc.state.hotkey_error = (!failed.is_empty()).then(|| failed.join("；"));
        }
        self.after_config_change();
    }

    /// 是否正在录制快捷键（设置窗口据此走 GPU 模糊路径）。
    pub fn hotkey_recording_open(&self) -> bool {
        self.paint_state.hotkey_recording.is_some()
    }

    /// 录制快捷键期间暂停 / 恢复全局热键。
    ///
    /// # 为什么要暂停
    ///
    /// `RegisterHotKey` 注册的组合会被 OS 从所有窗口没收：按键不再
    /// 产生 `WM_KEYDOWN`，而是直接送给注册窗口。于是用户点「录制」后
    /// 按下**当前正绑着的组合**（想把 Win+V 换掉时先按一下确认）时，
    /// 设置窗口永远收不到——录制像卡死，用户反馈「录制有问题」。
    ///
    /// 恢复由持有托盘的一侧按当前配置重注册：常驻进程走 IPC 命令，
    /// 单进程模式托盘就在本进程。
    pub(super) fn set_hotkeys_suspended(&mut self, suspend: bool) {
        if let Err(e) = self.svc.suspend_hotkeys(suspend) {
            tracing::warn!(%e, suspend, "暂停/恢复全局热键失败");
        }
    }

    /// 把「快捷键注册失败」这条反馈浮到界面上（状态栏提示）。
    ///
    /// 只在**内容变化**时提示：失败值在修复前一直挂在状态里，
    /// 每帧都弹一遍等于状态栏永远停着这条消息。
    ///
    /// 来源有两个：常驻进程把它放进快照（注册发生在托盘线程里，
    /// 只有那边拿得到结果）；单进程模式由本进程直接写入。
    /// 两者落点都是 `svc.state.hotkey_error`，这里不区分来源。
    pub(super) fn surface_hotkey_error(&mut self) {
        if self.svc.state.hotkey_error == self.last_hotkey_error {
            return;
        }
        self.last_hotkey_error = self.svc.state.hotkey_error.clone();
        if let Some(msg) = self.svc.state.hotkey_error.clone() {
            self.svc.notify(msg);
        }
    }
}

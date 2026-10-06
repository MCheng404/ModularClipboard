//! 图片缩略图：尺寸计算、解码降采样、CPU 侧缓存。
//!
//! # 现状：GPU 侧尚不能显示（不是本模块的问题）
//!
//! 本模块负责「字节 → 可显示的缩略图」这半段，全部是纯 CPU 逻辑，可单测。
//! 但把缩略图真正**画到屏幕上**还需要渲染层支持多纹理，而当前渲染器
//! 只绑定了一张纹理（字体图集）：
//!
//! - `shaders/egui.wgsl` 只声明 `@binding(2) var font_tex`，
//!   片元着色器写的是 `textureSample(font_tex, ...)`，把纹理当**单通道覆盖率**用；
//! - `pipeline.rs::DescriptorLayout` 只有 3 个绑定（uniform/sampler/纹理，计数均为 1）；
//! - `renderer.rs::upload_font_delta` 对非字体纹理 `tracing::warn!` 后 `continue`
//!   ——用户纹理被**显式丢弃**；
//! - `frame.rs` 的描述符池 `COMBINED_IMAGE_SAMPLER` 计数 =交换链图像数
//!   （每槽位恰好 1 个），放不下第二张纹理。
//!
//! 因此 `ctx.load_texture()` 产生的 `TexturesDelta` 目前会被渲染器丢掉。
//! 接线所需改动见 `PROGRESS.md` 待决区「缩略图 GPU 接线」。
//!
//! # 为什么不在本模块里做降采样之外的事
//!
//! 显存预算、纹理创建、描述符绑定都属于渲染层职责。本模块只产出
//! 「一张≤256px 的 RGBA8 位图」，恰好是 egui `ColorImage` 需要的形态，
//! 接通渲染层后无需改动即可使用。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use modular_clipboard_core::EntryId;

/// 缩略图最长边上限（像素）。
///
/// 原图保留在 blob 里按需查看，这里只给界面显示用的小图。
/// 256px 在 1xDPI 下约一个详情面板的宽度，足够看清缩略内容；
/// 再大只是浪费显存（RGBA8 下256×256 = 256KB，不缩则 4K 图要 32MB）。
pub const THUMB_MAX_EDGE: u32 = 256;

/// 缓存默认字节预算（32 MiB）。
///
/// 按每张 256KB 估算约可留 128 张。剪贴板历史里图片通常远少于这个数，
/// 预算的作用是「防止极端情况下（连续复制大图）无上限增长」。
pub const DEFAULT_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// 单个载荷的字节上限，超过则不尝试解码。
///
/// blob本身有大小限制（见存储层），但载荷超限时**只存元数据**，
/// 这里的常量是第二道防线：解码一个几百 MB 的输入会长时间阻塞 UI 线程。
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// 失败抑制表的最大条目数。
///
/// 每条只含key（id + hash 字符串）与两个整数，量级在百字节。
/// 上限存在的意义是「坏条目不会让内存无上限增长」。
const MAX_TRACKED_FAILURES: usize = 64;

/// 解码时允许的最大像素字节（RGBA8）。
///
/// 防解压炸弹：一个几百字节的压缩包可以声称自己展开成几 GB。
const MAX_DECODE_ALLOC: u64 = 64 * 1024 * 1024;

/// 失败结果的抑制时长（毫秒）。
///
/// # 为什么需要「负缓存」
///
/// `draw_image_preview` **每帧**被调用。若坏载荷每次都重新读盘 + 重新解码，
/// 一个损坏的 4K 图会让 UI 线程以 60Hz 反复做几十毫秒的解码——
/// 表现为「点开坏图后整个界面卡死」。
///
/// 但也不能**永久**缓存失败：载荷可能被修复（同步盘把占位文件换回真图），
/// 永久缓存会让界面永远停在错误提示上。折中是**短时抑制 + 到期重试**。
const FAILURE_SUPPRESS_MS: u64 = 2_000;

/// 单调时钟（Unix 毫秒）。
///
/// 失败抑制只需要「过了多久」，不需要绝对时刻。用毫秒精度足够：
/// 抑制窗口是 2 秒。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 尺寸计算
// ---------------------------------------------------------------------------

/// 计算缩放后落在 `max_edge` 内、且保持长宽比的尺寸。
///
/// # 规则
///
/// 1. `w`/`h`/`max_edge` 任一为 0 ⇒ `None`（无可用尺寸）；
/// 2. **不放大**：最长边已不超过 `max_edge` 时原样返回。
///    小图放大只会模糊并白占显存，交给 egui 在绘制时缩放即可；
/// 3. 否则按最长边等比缩放，四舍五入，且每边至少 1px
///    （极端长条图缩放后另一侧可能算出 0，那会导致上传零尺寸纹理而报错）。
///
/// 用整数运算而非浮点：`(w * max_edge) / longest` 的舍入规则是确定的，
/// 同一输入永远得到同一输出，便于断言与排查。
pub fn fit_within(w: u32, h: u32, max_edge: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 || max_edge == 0 {
        return None;
    }
    let longest = w.max(h) as u64;
    if longest <= max_edge as u64 {
        return Some((w, h));
    }
    let m = max_edge as u64;
    // +longest/2 实现「四舍五入」而非「向下取整」。
    let scale = |v: u32| -> u32 {
        let num = v as u64 * m + longest / 2;
        ((num / longest).max(1)) as u32
    };
    Some((scale(w), scale(h)))
}

// ---------------------------------------------------------------------------
// 解码
// ---------------------------------------------------------------------------

/// 缩略图解码失败。区分「不是图片」与「是图片但解不开」，
/// 因为前者通常是正常情况（如载荷已被淘汰），后者才是异常。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbError {
    /// 输入为空或超过 [`MAX_INPUT_BYTES`]，不值得尝试解码。
    TooLarge,
    /// 不是任何受支持的图片格式。
    NotAnImage,
    /// 是图片但解码失败（截断、损坏、超出解码预算）。
    DecodeFailed,
}

/// 一张已降采样的 RGBA8 位图。
///
/// 像素存`Vec<u8>` 而非 `Vec<Color32>`：内容完全相同但省不掉字节
/// （都是 4 通道），而`Vec<u8>` 便于与 `image` crate 的输出零拷贝对接，
/// 也便于精确计算缓存占用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// RGBA8，行优先，长度 = `width * height * 4`。
    pub pixels: Vec<u8>,
}

impl Thumbnail {
    /// 该缩略图占用的字节数，用于缓存预算记账。
    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }

    /// 转成 egui 的 [`egui::ColorImage`]，可直接喂给 `ctx.load_texture`。
    pub fn to_color_image(&self) -> egui::ColorImage {
        egui::ColorImage {
            size: [self.width as usize, self.height as usize],
            source_size: egui::vec2(self.width as f32, self.height as f32),
            pixels: self
                .pixels
                .chunks_exact(4)
                .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                .collect(),
        }
    }
}

/// 把图片字节解码成最长边不超过 `max_edge` 的缩略图。
///
/// # 不会 panic
///
/// 所有失败路径都返回 [`ThumbError`]。本函数由 UI 线程直接调用，
/// 一个 panic 会带崩整个帧循环，因此「非图片数据」必须是普通返回值。
pub fn decode(bytes: &[u8], max_edge: u32) -> Result<Thumbnail, ThumbError> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(ThumbError::TooLarge);
    }
    if max_edge == 0 {
        return Err(ThumbError::TooLarge);
    }

    // `with_guessed_format` 按魔数嗅探格式——载荷没有可靠的扩展名，
    // 剪贴板也不携带文件名，格式只能靠内容判断。
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ThumbError::NotAnImage)?;

    // 限制解码峰值内存。注意 `max_image_width/height` 保持 `None`：
    // 它们是**严格**限制，超过就整张拒绝，而不是缩放。
    // 真正要防的是解压炸弹（`max_alloc`），尺寸超限由 `fit_within` 缩放处理。
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    let mut reader = reader;
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| match e {
        image::ImageError::Unsupported(_) => ThumbError::NotAnImage,
        _ => ThumbError::DecodeFailed,
    })?;

    let (src_w, src_h) = (decoded.width(), decoded.height());
    // 原图已小于上限时 `resize_exact` 会做一次无意义的重采样，
    // 直接用原解码结果即可（省掉一次全图拷贝与滤波）。
    //
    // ⚠️ 输出尺寸必须取**缩放后**的 `nw/nh`。曾误用 `src_w/src_h`
    // 填进 `Thumbnail`，于是 1920×1080 的图被标成 1920×1080
    // 却只带 256×144 的像素——`to_color_image` 按 width 切分时会
    // 直接越界 panic。
    let (out_w, out_h, rgba) = match fit_within(src_w, src_h, max_edge) {
        Some((nw, nh)) if (nw, nh) == (src_w, src_h) => (nw, nh, decoded.to_rgba8()),
        Some((nw, nh)) => (
            nw,
            nh,
            decoded
                .resize_exact(nw, nh, image::imageops::FilterType::Triangle)
                .to_rgba8(),
        ),
        None => return Err(ThumbError::DecodeFailed),
    };
    let rgba = rgba.into_raw();

    // `fit_within` 已保证尺寸 > 0，但解码器返回的长度不符时不能直接索引。
    let expect = (out_w as usize)
        .saturating_mul(out_h as usize)
        .saturating_mul(4);
    if rgba.len() != expect {
        return Err(ThumbError::DecodeFailed);
    }
    Ok(Thumbnail {
        width: out_w,
        height: out_h,
        pixels: rgba,
    })
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

/// 缓存键。
///
/// # 为什么 `id` 与 `hash` 都要
///
/// `id` 是主键，但条目可以被删除后**复用**（清库后自增id 重新计数），
/// 只用 `id` 会把旧图的缩略图错配给新条目。加上 `hash`（内容指纹）
/// 后，内容变了必然 miss。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub id: EntryId,
    pub hash: String,
}

impl CacheKey {
    pub fn new(id: EntryId, hash: impl Into<String>) -> Self {
        Self {
            id,
            hash: hash.into(),
        }
    }
}

/// 按字节预算做 LRU淘汰的缩略图缓存。
///
/// # 淘汰策略
///
/// 用「访问顺序队列 + HashMap」实现 LRU：
///
/// - 命中时把key 移到队尾（最近使用）；
/// - 超预算时从队首（最久未用）丢，直到回到预算内。
///
/// 刻意**没有**用 `Vec` + `position` 线性查找做淘汰：那会让每次命中都
/// 变成 O(n)。队列 + 哈希的组合让命中是 O(1)（除 VecDeque 的中段移除，
/// 但条目数在百量级，最坏也远快于重新解码一张图）。
///
/// # 为什么不按「张数」而是按字节
///
/// 一张 16×16 的缩略图和一个 256×256 的差 256 倍。按张数限流会在
/// 前者多时看似宽松、后者多时把内存撑爆；按字节限流才是真正的约束。
#[derive(Debug)]
pub struct ThumbnailCache {
    budget: usize,
    used: usize,
    map: HashMap<CacheKey, Arc<Thumbnail>>,
    /// 访问顺序，队首最久未用。
    order: VecDeque<CacheKey>,
    /// 失败抑制表：`键 → (失败时刻, 错误)`。
    ///
    /// 与 `map` 分开，因为它存的是「**没有**结果」这件事，
    /// 而 `map` 存的是「有结果」。放进同一个 map 需要用 `Option` 表达，
    /// 可读性反而更差。
    failures: HashMap<CacheKey, (u64, ThumbError)>,
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl ThumbnailCache {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            map: HashMap::new(),
            order: VecDeque::new(),
            failures: HashMap::new(),
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 当前缓存占用的字节数（按像素数据计）。
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// 取缓存，未命中返回 `None`（**不**触发解码）。
    ///
    /// 命中会把该键移到最近使用端。
    pub fn get(&mut self, key: &CacheKey) -> Option<Arc<Thumbnail>> {
        let hit = self.map.get(key).cloned()?;
        Self::touch(&mut self.order, key);
        Some(hit)
    }

    /// 直接插入（若已存在则替换并记账）。
    pub fn insert(&mut self, key: CacheKey, thumb: Arc<Thumbnail>) {
        if let Some(old) = self.map.insert(key.clone(), thumb) {
            self.used = self.used.saturating_sub(old.byte_len());
        }
        self.used += self.map[&key].byte_len();
        Self::touch(&mut self.order, &key);
        self.evict_to_budget();
    }

    /// 取缓存，未命中则调用 `load` 取字节并解码。
    ///
    /// 这是 UI 侧唯一需要调用的入口：命中时**不会**重复解码，
    /// 因此同一条目在详情面板反复打开时不会重复付解码代价。
    ///
    /// # 失败会被短时抑制
    ///
    /// 失败结果记入 `failures` 并抑制 [`FAILURE_SUPPRESS_MS`] 毫秒，
    /// 期间直接返回同一个错误而**不再读盘**。到期后自动重试，
    /// 因此载荷被修复后能自行恢复，不必重启程序。
    ///
    /// 抑制表本身也有上限（[`MAX_TRACKED_FAILURES`]），
    /// 防止大量坏条目把内存撑大。
    ///
    /// 预算为 0 时仍会解码并返回，只是不留存——调用方拿到的
    /// [`Arc`] 在本次调用内有效，适合「只要这一帧」的用法。
    pub fn get_or_load<F>(
        &mut self,
        key: CacheKey,
        load: F,
    ) -> Result<Arc<Thumbnail>, ThumbError>
    where
        F: FnOnce() -> Result<Vec<u8>, String>,
    {
        if let Some(hit) = self.get(&key) {
            return Ok(hit);
        }
        // 仍在抑制期内：直接复用上次的错误，不再付读盘+解码的代价。
        if let Some((at, err)) = self.failures.get(&key)
            && now_ms().saturating_sub(*at) < FAILURE_SUPPRESS_MS
        {
            return Err(*err);
        }

        let result = load()
            .map_err(|_| ThumbError::DecodeFailed)
            .and_then(|bytes| decode(&bytes, THUMB_MAX_EDGE));

        match result {
            Ok(thumb) => {
                let thumb = Arc::new(thumb);
                if self.budget > 0 {
                    self.insert(key.clone(), Arc::clone(&thumb));
                }
                // 成功即解除抑制：载荷可能已从损坏恢复。
                self.failures.remove(&key);
                Ok(thumb)
            }
            Err(err) => {
                self.remember_failure(key, err);
                Err(err)
            }
        }
    }

    /// 记一次失败，并让抑制表不超过 [`MAX_TRACKED_FAILURES`] 条。
    ///
    /// 满了就丢掉**最旧**的一条（`HashMap` 无序，故取 `at` 最小者）。
    /// 淘汰旧失败是安全的：最坏后果只是某个坏条目被多试一次。
    fn remember_failure(&mut self, key: CacheKey, err: ThumbError) {
        if self.failures.len() >= MAX_TRACKED_FAILURES
            && let Some(oldest) = self
                .failures
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            self.failures.remove(&oldest);
        }
        self.failures.insert(key, (now_ms(), err));
    }

    /// 丢弃全部条目。
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.failures.clear();
        self.used = 0;
    }

    /// 淘汰直到占用回到预算内。
    fn evict_to_budget(&mut self) {
        while self.used > self.budget {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.map.remove(&oldest) {
                self.used = self.used.saturating_sub(dropped.byte_len());
            }
        }
    }

    /// 把 key 移到队尾。已存在则先移除旧位置，避免重复键让队列无限增长。
    fn touch(order: &mut VecDeque<CacheKey>, key: &CacheKey) {
        if let Some(pos) = order.iter().position(|k| k == key) {
            order.remove(pos);
        }
        order.push_back(key.clone());
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---- 尺寸计算 ----

    #[test]
    fn fit_scales_longest_edge_to_limit() {
        assert_eq!(fit_within(1920, 1080, 256), Some((256, 144)));
        assert_eq!(fit_within(1080, 1920, 256), Some((144, 256)));
    }

    #[test]
    fn fit_preserves_aspect_ratio_within_rounding() {
        // 长宽比不可能被整数缩放精确保持，正确的判据是
        // 「每一维与理想值的偏差不超过 0.5px（四舍五入的固有误差）」。
        //
        // 曾把判据写成「两边的缩放比例之差 < 1/256」——那对**极端长宽比**
        // 是错误的要求：1000×3 缩到 256 宽时，理想高度是 0.768px，
        // 只能取整到 1px，此时比例偏差 0.077 远大于 1/256，
        // 但它已经是整数缩放下的**最优解**。
        for (w, h, max) in [(1920u32, 1080u32, 256u32), (1000, 3, 256), (37, 91, 16)] {
            let (ow, oh) = fit_within(w, h, max).unwrap();
            let scale = f64::from(max) / f64::from(w.max(h));
            let ideal_w = f64::from(w) * scale;
            let ideal_h = f64::from(h) * scale;
            assert!(
                (f64::from(ow) - ideal_w).abs() <= 0.5,
                "宽 {w}->{ow}，理想 {ideal_w}"
            );
            assert!(
                (f64::from(oh) - ideal_h).abs() <= 0.5,
                "高 {h}->{oh}，理想 {ideal_h}"
            );
        }
    }

    #[test]
    fn fit_does_not_upscale_small_images() {
        // 小图放大只会模糊 + 白占显存。
        assert_eq!(fit_within(32, 32, 256), Some((32, 32)));
        assert_eq!(fit_within(1, 1, 256), Some((1, 1)));
    }

    #[test]
    fn fit_exact_boundary_is_identity() {
        // 恰好等于上限：不缩。
        assert_eq!(fit_within(256, 128, 256), Some((256, 128)));
    }

    #[test]
    fn fit_never_returns_zero_side() {
        // 极端长条：缩放后短边算出来会是 0，必须兜到 1。
        // 零尺寸纹理会让上传路径直接报错。
        let (w, h) = fit_within(100_000, 1, 256).unwrap();
        assert_eq!((w, h), (256, 1));
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn fit_rejects_degenerate_inputs() {
        assert_eq!(fit_within(0, 100, 256), None);
        assert_eq!(fit_within(100, 0, 256), None);
        assert_eq!(fit_within(0, 0, 256), None);
        assert_eq!(fit_within(100, 100, 0), None);
    }

    #[test]
    fn fit_handles_maximum_pixels_without_overflow() {
        // u32 上限尺寸：乘法必须用 u64，否则溢出。
        let (w, h) = fit_within(u32::MAX, u32::MAX / 2, 256).unwrap();
        assert!(w <= 256 && h <= 256);
    }

    // ---- 解码降级 ----

    fn encode_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("编码测试用PNG 应成功");
        buf.into_inner()
    }

    #[test]
    fn decode_downscales_large_png() {
        let png = encode_png(1920, 1080);
        let t = decode(&png, THUMB_MAX_EDGE).unwrap();
        assert_eq!((t.width, t.height), (256, 144));
        assert_eq!(t.pixels.len(), 256 * 144 * 4);
    }

    #[test]
    fn decode_keeps_small_png_intact() {
        let png = encode_png(64, 48);
        let t = decode(&png, THUMB_MAX_EDGE).unwrap();
        assert_eq!((t.width, t.height), (64, 48), "小图不该被放大也不该被缩");
        assert_eq!(t.pixels.len(), 64 * 48 * 4);
    }

    #[test]
    fn decode_preserves_pixel_content() {
        // 纯色图缩放后仍应是同一个颜色——验证通道顺序没串。
        let t = decode(&encode_png(300, 300), THUMB_MAX_EDGE).unwrap();
        assert_eq!(&t.pixels[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert_eq!(decode(&[], 256), Err(ThumbError::TooLarge));
    }

    #[test]
    fn decode_rejects_oversized_input_without_allocating() {
        // 关键：不能因为「看起来像图片」就先读一遍。
        let big = vec![0u8; MAX_INPUT_BYTES + 1];
        assert_eq!(decode(&big, 256), Err(ThumbError::TooLarge));
    }

    #[test]
    fn decode_rejects_non_image_bytes_without_panic() {
        // 载荷可能已被淘汰、或条目类型判断错误传了文本过来。
        // UI 线程上 panic 会带崩帧循环，必须是普通 Err。
        for junk in [
            b"hello world".to_vec(),
            b"{\"json\":true}".to_vec(),
            vec![0xFFu8; 512],
            b"\x89PNG\r\n\x1a\n".to_vec(), // 只有魔数，没有内容
        ] {
            let r = decode(&junk, 256);
            assert!(r.is_err(), "非图片数据必须返回 Err 而非成功");
        }
    }

    #[test]
    fn decode_rejects_truncated_png() {
        let mut png = encode_png(128, 128);
        png.truncate(png.len() / 2);
        assert!(decode(&png, 256).is_err());
    }

    #[test]
    fn decode_zero_max_edge_is_rejected() {
        assert!(decode(&encode_png(8, 8), 0).is_err());
    }

    #[test]
    fn decode_error_kinds_are_distinguishable() {
        // 「不是图片」与「是图片但坏了」要能分开，前者是正常情况。
        assert_eq!(decode(b"not an image at all", 256), Err(ThumbError::NotAnImage));
        let mut png = encode_png(64, 64);
        png.truncate(10);
        assert_eq!(decode(&png, 256), Err(ThumbError::DecodeFailed));
    }

    // ---- 缓存 ----

    fn thumb_of(n: u8) -> Arc<Thumbnail> {
        Arc::new(Thumbnail {
            width: 2,
            height: 2,
            pixels: vec![n; 16],
        })
    }

    #[test]
    fn cache_same_key_does_not_reload() {
        // 核心不变量：同一 item 反复打开详情面板不重复解码。
        let mut c = ThumbnailCache::default();
        let k = CacheKey::new(1, "h1");
        let calls = AtomicU32::new(0);
        let mut load = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(encode_png(64, 64))
        };
        c.get_or_load(k.clone(), &mut load).unwrap();
        c.get_or_load(k.clone(), &mut load).unwrap();
        c.get_or_load(k.clone(), &mut load).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "命中缓存不应再次 load");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn cache_distinguishes_different_hashes_on_same_id() {
        // 条目 id 会被复用（清库后自增重新计数），只认id 会错配。
        let mut c = ThumbnailCache::default();
        let a = CacheKey::new(7, "hash-A");
        let b = CacheKey::new(7, "hash-B");
        c.insert(a.clone(), thumb_of(1));
        c.insert(b.clone(), thumb_of(2));
        assert_eq!(c.len(), 2);
        assert_ne!(c.get(&a).unwrap().pixels[0], c.get(&b).unwrap().pixels[0]);
    }

    #[test]
    fn cache_hit_refreshes_recency_so_survivors_keep_their_own_data() {
        // LRU 的意义：常用的留下，不常用的被丢。
        let mut c = ThumbnailCache::new(32); // 每张 16 字节 → 只能留 2 张
        let k1 = CacheKey::new(1, "a");
        let k2 = CacheKey::new(2, "b");
        let k3 = CacheKey::new(3, "c");
        c.insert(k1.clone(), thumb_of(1));
        c.insert(k2.clone(), thumb_of(2));
        // 触碰 k1，使 k2 变成最久未用。
        assert!(c.get(&k1).is_some());
        c.insert(k3.clone(), thumb_of(3));

        assert_eq!(c.len(), 2, "应恰好留 2 张");
        assert!(c.get(&k1).is_some(), "刚用过的应保留");
        assert!(c.get(&k3).is_some());
        assert!(c.get(&k2).is_none(), "最久未用的应被淘汰");
    }

    #[test]
    fn cache_never_exceeds_byte_budget() {
        let mut c = ThumbnailCache::new(48); // 每张 16 字节 → 最多 3 张
        for i in 0..10 {
            c.insert(CacheKey::new(i, "h"), thumb_of(1));
        }
        assert!(c.used_bytes() <= 48, "占用 {} 超过预算", c.used_bytes());
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn cache_accounting_matches_contents() {
        let mut c = ThumbnailCache::default();
        c.insert(CacheKey::new(1, "h"), thumb_of(1));
        assert_eq!(c.used_bytes(), 16);
        // 覆盖同键不应重复记账。
        c.insert(CacheKey::new(1, "h"), thumb_of(2));
        assert_eq!(c.used_bytes(), 16, "替换不应累加");
        c.clear();
        assert_eq!(c.used_bytes(), 0);
        assert!(c.is_empty());
    }

    #[test]
    fn cache_order_has_no_duplicates() {
        // 重复 touch 若在队列里留下重复键，淘汰就会删错条目。
        let mut c = ThumbnailCache::new(1024);
        let k = CacheKey::new(1, "h");
        c.insert(k.clone(), thumb_of(1));
        c.get(&k);
        c.get(&k);
        c.insert(k.clone(), thumb_of(2));
        assert_eq!(c.order.len(), 1);
    }

    #[test]
    fn cache_suppresses_repeated_failures() {
        // 回归测试：`draw_image_preview` 每帧调用。若坏载荷每次都重新
        // 读盘 + 重新解码，点开一张损坏大图会让 UI 线程以 60Hz 反复
        // 做几十毫秒解码 —— 表现为「整个界面卡死」。
        let mut c = ThumbnailCache::default();
        let k = CacheKey::new(1, "h");
        let calls = AtomicU32::new(0);
        let mut failing = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<Vec<u8>, String>("blob 损坏".into())
        };
        for _ in 0..10 {
            assert_eq!(
                c.get_or_load(k.clone(), &mut failing),
                Err(ThumbError::DecodeFailed)
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "失败应被抑制，不应每帧重试"
        );
        assert!(c.is_empty(), "失败结果不应进入像素缓存");
    }

    #[test]
    fn cache_failure_suppression_expires_so_repairs_are_picked_up() {
        // 反向约束：不能把失败**永久**缓存。载荷可能被修复
        // （同步盘把占位文件换回真图），永久缓存会让界面永远停在错误提示。
        let mut c = ThumbnailCache::default();
        let k = CacheKey::new(1, "h");
        let bad: Result<Vec<u8>, String> = Err("损坏".into());
        assert!(c.get_or_load(k.clone(), || bad.clone()).is_err());

        // 把失败时刻改到很久以前，等价于抑制窗口已过。
        let ancient = now_ms().saturating_sub(FAILURE_SUPPRESS_MS + 1_000);
        c.failures.insert(k.clone(), (ancient, ThumbError::DecodeFailed));

        let png = encode_png(32, 32);
        assert!(c.get_or_load(k, || Ok(png)).is_ok(), "到期后应重试并成功");
        assert!(
            c.failures.is_empty(),
            "成功后应解除失败抑制，否则会永久卡在失败态"
        );
    }

    #[test]
    fn cache_success_clears_stale_failure() {
        // 载荷被修复后，旧的失败记录必须消失，否则后续调用会一直
        // 返回旧错误（即使窗口没过）。
        let mut c = ThumbnailCache::default();
        let k = CacheKey::new(1, "h");
        let bad: Result<Vec<u8>, String> = Err("损坏".into());
        assert!(c.get_or_load(k.clone(), || bad.clone()).is_err());
        // 强制让成功路径绕过抑制窗口。
        let ancient = now_ms().saturating_sub(FAILURE_SUPPRESS_MS + 1_000);
        c.failures.insert(k.clone(), (ancient, ThumbError::DecodeFailed));
        let png = encode_png(16, 16);
        assert!(c.get_or_load(k, || Ok(png)).is_ok());
        assert!(c.failures.is_empty());
    }

    #[test]
    fn cache_failure_table_is_bounded() {
        // 大量坏条目不能让抑制表无上限增长。
        let mut c = ThumbnailCache::default();
        let bad: Result<Vec<u8>, String> = Err("坏".into());
        for i in 0..(MAX_TRACKED_FAILURES * 3) {
            c.remember_failure(CacheKey::new(i as EntryId, "h"), ThumbError::NotAnImage);
        }
        assert!(
            c.failures.len() <= MAX_TRACKED_FAILURES,
            "抑制表 {} 超过上限",
            c.failures.len()
        );
        let _ = bad;
    }

    #[test]
    fn cache_clear_also_drops_failures() {
        // 残留的失败记录会让「清空全部历史」后的条目继续显示旧错误。
        let mut c = ThumbnailCache::default();
        let bad: Result<Vec<u8>, String> = Err("坏".into());
        c.get_or_load(CacheKey::new(1, "h"), || bad).unwrap_err();
        assert!(!c.failures.is_empty());
        c.clear();
        assert!(c.failures.is_empty());
    }

    #[test]
    fn cache_zero_budget_still_returns_thumbnail() {
        // 「只要这一帧」的用法：不留存但要有结果。
        let mut c = ThumbnailCache::new(0);
        let png = encode_png(64, 64);
        let t = c.get_or_load(CacheKey::new(1, "h"), || Ok(png)).unwrap();
        assert_eq!((t.width, t.height), (64, 64));
        assert!(c.is_empty());
    }

    #[test]
    fn decode_reports_post_resize_dimensions_not_source() {
        // 回归测试：曾把**原图**尺寸填进 Thumbnail，
        // 而像素是缩放后的。于是 1920×1080 的图被标成 1920×1080
        // 却只带256×144 的像素 —— `to_color_image` 按 width 切分
        // `chunks_exact(4)` 时数量对不上，`ColorImage` 与实际像素
        // 不一致，渲染层按 1920 宽读会越界。
        let t = decode(&encode_png(1920, 1080), THUMB_MAX_EDGE).unwrap();
        assert_eq!((t.width, t.height), (256, 144));
        // 自洽性：声明的尺寸必须与像素长度精确吻合。
        assert_eq!(
            t.pixels.len(),
            t.width as usize * t.height as usize * 4,
            "声明尺寸与像素长度必须自洽"
        );
        //顺带验证 egui 侧不会因此panic。
        let ci = t.to_color_image();
        assert_eq!(ci.size, [256, 144]);
        assert_eq!(ci.pixels.len(), 256 * 144);
    }

    #[test]
    fn to_color_image_shape_matches_pixels() {
        let t = Thumbnail {
            width: 2,
            height: 3,
            pixels: vec![255u8; 2 * 3 * 4],
        };
        let ci = t.to_color_image();
        assert_eq!(ci.size, [2, 3]);
        assert_eq!(ci.pixels.len(), 6);
        assert_eq!(ci.pixels[0], egui::Color32::from_rgba_unmultiplied(255, 255, 255, 255));
    }
}
//! 繁体中文（zh-TW）文案表。
//!
//! # 键是英文，不是中文
//!
//! 英文是本项目 i18n 的**主语言**：所有译文都以英文为基准书写，
//! 开发者对着英文写译文，不需要先读中文。调用点的 `t(("中文", "English"))`
//! 里，查表用的是**英文那一半**（`Msg.1`）；中文那一半只服务于
//! 「跟随系统且系统是中文」的场景，不参与查表。
//!
//! 为什么英文能当键而中文不能：中文有大量同形不同义的词
//! （「置顶」既是卡片种类也是行操作按钮），英文经过 2026-10-23
//! 一轮消歧后逐个唯一——那轮把 12 个撞键的英文串改了（见 git
//! log 里「i18n 以英文为主语言」提交），消歧前英文一键对多个中文，
//! 根本没法当键。
//!
//! # 增量覆盖
//!
//! 表允许**部分覆盖**：缺的条目自动回落英文（见 `super::Locale::pick`）。
//! 所以漏译不会显示成第三种语言混搭，也不会显示成空串。
//!
//! 由简体转换而来，港澳台用法。
pub(super) static TABLE: &[(&str, &str)] = &[
    (" (custom)", "（自訂）"),
    // 上下文：同上。
    (" chars", "字元"),
    // 上下文：同上。
    (" entries", " 筆"),
    (" entries (multi-select)", " 筆（可多選）"),
    // 上下文：数字后的单位后缀（`5 items`）。
    (" items", "筆"),
    ("'Copy & notify' only fills the clipboard; 'Copy & paste' also sends Ctrl+V to the target", "「僅貼上並提示」只寫剪貼簿由你手動貼上；「直接貼上並輸入」會向目標視窗傳送 Ctrl+V"),
    ("0 = one fixed sky, 1 = reshuffle on every rebuild", "0 = 固定一片星空，1 = 每次重建換一片"),
    ("<full path to modular-clipboard.exe>", "<modular-clipboard.exe 的完整路徑>"),
    ("A style layer on top of light / dark: paper, eye-care green, starfield purple, OLED", "疊加在明暗模式上的風格層：紙紋 / 護眼綠 / 星空紫 / OLED"),
    ("AI pairing prompt copied", "已複製 AI 配對提示詞"),
    ("About", "關於"),
    // 上下文：搜索面板相对主窗口的停靠位置（不是列表排序方向）。
    ("Above", "視窗上方"),
    ("Accent color", "主題色"),
    ("Accent color: pick from swatches, sliders, or a hex value", "界面強調色：色板、色相滑桿或十六進位任選"),
    ("Access token", "存取金鑰"),
    ("Access token; empty = no authentication", "存取金鑰；留空表示無需認證"),
    ("Activate a given window or app", "啟用指定視窗或應用"),
    ("Address the MCP service listens on", "MCP 服務監聽的位址"),
    ("Adjust", "調整"),
    ("All history cleared", "已清除全部歷史"),
    ("Alt + right-click", "Alt + 右鍵"),
    ("Always on top", "視窗置頂"),
    ("Amber", "琥珀"),
    ("App info", "軟體資訊"),
    ("App used to open file entries; empty = system file manager", "開啟檔案項目用的程式；留空則用系統檔案總管"),
    ("App used to open link entries; empty = system default browser", "開啟連結項目用的程式；留空則用系統預設瀏覽器"),
    ("Appearance", "外觀"),
    ("Author page", "作者首頁"),
    ("Auto", "自動"),
    ("Auto (discrete preferred)", "自動（獨顯優先）"),
    ("Background effect", "背景效果"),
    ("Background opacity", "背景不透明度"),
    ("Bearer token (configured in settings)", "Bearer Token（已在設定中配置）"),
    // 上下文：同上。
    ("Below", "視窗下方"),
    ("Black level", "純黑程度"),
    ("Blur", "模糊"),
    ("Blur intensity in frosted mode", "磨砂模式下的模糊程度"),
    ("Blur strength", "磨砂強度"),
    ("Borders", "分隔提亮"),
    // 上下文：同上。
    ("Bottom edge", "底部"),
    ("Brightness", "亮度"),
    ("Browser path", "瀏覽器路徑"),
    ("Brush", "畫筆"),
    ("Built with", "技術棧"),
    ("Cancel", "取消"),
    ("Cancel (Esc)", "取消修改（Esc）"),
    ("Cannot edit image: content was evicted ({e})", "無法編輯圖片：內容已被淘汰（{e}）"),
    ("Cannot edit image: missing dimensions", "無法編輯圖片：缺少尺寸資訊"),
    ("Cannot edit: content was evicted ({e})", "無法編輯：內容已被淘汰（{e}）"),
    ("Capture resolution", "抓屏解析度"),
    ("Discrete GPU", "獨立顯卡"),
    ("GPU adapter", "GPU 適配器"),
    ("Graphics", "顯卡"),
    ("In use", "目前"),
    ("Integrated GPU", "整合顯卡"),
    ("Opacity of entry cards; without a background effect they blend into the window fill", "列卡片底色的不透明度；無背景效果時與視窗底色混合"),
    ("Percentage of the window size to capture; 100% matches screen pixels exactly, lower costs less", "擷取區域佔視窗尺寸的百分比；100% 與螢幕像素一一對應，越低越省資源"),
    ("Cannot open the image editor:", "無法開啟圖片編輯視窗："),
    ("Capacity and cleanup", "容量與清理"),
    ("Capture", "擷取"),
    ("Capture the screen or a window", "截取螢幕或視窗畫面"),
    ("Card blur", "卡片磨砂"),
    ("Card meta badges", "卡片角標"),
    ("Card opacity", "卡片不透明度"),
    ("Card shadow", "卡片陰影"),
    ("Which graphics card renders the UI; takes effect after a restart", "用哪張顯卡渲染介面；重新啟動後生效"),
    ("Window and startup", "視窗與啟動"),
    ("Window background", "視窗背景"),
    ("External programs", "外部程式"),
    ("Row cards", "行卡片"),
    ("Panels and popups", "面板與彈出視窗"),
    ("Changes saved", "已儲存修改"),
    ("Check", "檢查更新"),
    ("Check for updates", "版本檢查"),
    ("Checking…", "檢查中…"),
    ("Clean up temp files", "清理臨時檔案"),
    ("Clear", "清空"),
    ("Clear all history (hold 5s)", "清空全部歷史（長按 5 秒）"),
    ("Clear failed:", "清空失敗："),
    ("Clear history", "清空歷史"),
    ("Clear search", "清空搜尋"),
    ("Click UI elements", "點擊界面元素"),
    ("Click a name to open its repository", "點擊任一名稱開啟它的官方儲存庫"),
    ("Click anywhere to stop recording", "錄製完成後，點擊任意位置停止錄製"),
    ("Click or activate elements anywhere", "在任意位置點擊或啟用元素"),
    ("Click to open in the file manager", "點擊在檔案總管中開啟"),
    ("Click to view the full text", "點擊查看協定全文"),
    ("Clipboard access", "剪貼板讀寫"),
    ("Clipboard content larger than this is not recorded", "超出該大小的剪貼內容不記錄"),
    ("Close", "關閉"),
    ("Close without saving", "關閉（不儲存）"),
    ("Collapse", "折疊"),
    ("Color", "調色"),
    ("Color fringing at the edges: 0 = none, higher = more prismatic", "邊緣彩色描邊：0 = 無，越高越呈稜鏡分光"),
    ("Colors", "顏色"),
    ("Connected", "已連線"),
    ("Connection", "通訊"),
    ("Contrast", "對比"),
    ("Copied", "已複製"),
    ("Copied — press Ctrl+V to paste", "已複製到剪貼簿，按 Ctrl+V 貼上"),
    ("Copy", "複製"),
    ("Copy & notify", "僅貼上並提示"),
    ("Copy & paste", "直接貼上並輸入"),
    ("Copy AI pairing prompt", "一鍵 AI 配對提示詞"),
    ("Copy failed:", "複製失敗："),
    ("Coral", "珊瑚"),
    ("Crop", "裁剪"),
    ("Ctrl + right-click", "Ctrl + 右鍵"),
    ("Cursor parallax", "游標互動"),
    ("Custom", "自訂"),
    ("Dark", "深色"),
    ("Data", "資料"),
    ("Data directory", "資料目錄"),
    ("Deduplicate copies", "複製去重"),
    ("Default", "預設"),
    ("Default (explorer.exe)", "預設（explorer.exe）"),
    ("Default (system browser)", "預設（系統預設瀏覽器）"),
    ("Delete", "刪除"),
    ("Delete failed:", "刪除失敗："),
    ("Details", "詳細"),
    ("Disabled", "已停用"),
    ("Display language: follow the system, or pin one from the list", "顯示語言：跟隨系統，或固定為簡體中文 / 英文"),
    ("Dock position", "停靠位置"),
    ("Done", "完成"),
    ("Done: ", "已執行："),
    ("Downloading…", "下載中…"),
    ("Drag", "拖曳"),
    ("Drag a box on the image; release to crop", "在圖片上拖出一個框，放開即裁剪"),
    ("Drag and drop", "拖曳操作"),
    ("Drag elements or perform drag-and-drop", "拖曳元素或執行拖放"),
    ("Drop shadow under entry cards; dark mode deepens it automatically", "項目卡片的投影；深色模式下自動加深"),
    ("Durations (read-only)", "時長（唯讀）"),
    ("Edit content", "修改內容"),
    ("Edit image", "編輯圖片"),
    ("Effective values of all theme tokens. Dragging writes an override, which wins over values derived from the style theme and accent color.", "全部主題令牌的目前生效值。拖曳即寫入覆寫，優先於風格主題與強調色的衍生值。"),
    ("Embedded", "內嵌"),
    ("Emerald", "翠綠"),
    ("Empty content, not saved", "內容為空，未儲存"),
    ("Enable", "啟用"),
    ("Entry count", "顯示筆數"),
    ("Expand", "展開"),
    ("Eye-care green", "護眼綠"),
    ("Failed to apply:", "設定失敗："),
    ("Failed to copy the pairing prompt:", "複製配對提示詞失敗："),
    ("Failed to load the original image", "原圖載入失敗"),
    ("Failed to open link:", "開啟連結失敗："),
    ("Failed to open the file manager:", "開啟檔案總管失敗："),
    ("Failed to save settings:", "儲存設定失敗："),
    ("Failed to start the capture thread", "無法建立擷取執行緒"),
    ("Fetch the latest tag and compare with local", "從儲存庫拉取最新 tag 並與本機比較"),
    ("File manager path", "檔案管理員路徑"),
    ("Files", "檔案"),
    ("Floating search panel; dock position and height adjustable", "懸浮搜尋面板：停靠位置與高度可調"),
    ("Floating type buttons at the window edge: click to filter; multi-select, adjustable position and size", "視窗邊緣的懸浮類型按鈕：點擊過濾，可多選、可調位置與大小"),
    ("Focus windows", "切換焦點視窗"),
    ("Follow system", "跟隨系統"),
    ("Follow the system, or force dark / light", "跟隨系統，或強制深色 / 淺色"),
    ("Font scale", "字型縮放"),
    ("Fresnel highlight along the edges; lower spreads it out", "沿邊緣的菲涅爾高光；越小越鋪開"),
    ("From cyan (cool) to yellow (warm)", "偏青（冷）到偏黃（暖）"),
    ("Frosted", "磨砂"),
    ("Frosted glass for entry cards: 0 = off, higher = blurrier", "條目卡片的毛玻璃效果：0 = 關閉，數值越大越模糊"),
    ("Gamma", "伽瑪"),
    ("General", "常規"),
    ("Glass", "玻璃"),
    ("Glass dispersion", "玻璃色散"),
    ("Glass blur", "玻璃模糊度"),
    ("Blur strength in glass mode, independent of frosted blur", "玻璃模式的模糊強度，與磨砂強度相互獨立"),
    ("Glass refraction", "玻璃折射"),
    ("Glass rim light", "玻璃邊緣光"),
    ("Exclude content from glass", "元素不參與玻璃計算"),
    (
        "Keep text and icons over a clean, unfrosted backdrop: the content area only refracts",
        "卡片內容區只保留折射，不做磨砂與混色：文字與圖示背後是乾淨可比的底圖",
    ),
    ("Graphite", "石墨"),
    ("Green tint", "綠意濃度"),
    // 上下文：HSV 取色器的 S 分量（不是整体饱和度）。
    ("HSV saturation", "飽和度"),
    ("Height of the search panel (30-56 pt)", "搜尋面板的高度（30~56pt）"),
    ("Hex", "十六進位"),
    ("Hide on focus loss", "失焦時隱藏"),
    ("Hide the main window when another app gains focus", "切換到其它程式時自動隱藏主視窗"),
    ("Hide to tray", "關閉到系統匣"),
    ("History", "歷史"),
    ("Hold at least one modifier (Ctrl/Shift/Alt/Win)", "至少按住一個修飾鍵（Ctrl/Shift/Alt/Win）"),
    ("How far the background is pushed toward pure black", "背景壓向純黑的比例"),
    ("How feedback appears: in-window toasts or a separate popup", "操作反饋的顯示：視窗內提示或獨立彈窗"),
    ("How many entries to keep; empty = size-based eviction only", "保留的項目數量；留空則只按容量淘汰"),
    ("How much star radii vary", "星點半徑的差異程度"),
    ("How often the clipboard is checked; lower is faster but heavier", "剪貼板檢查間隔；越小越靈敏、越耗資源"),
    ("How strongly stars breathe between bright and dim", "星點緩慢明暗呼吸的強度"),
    ("How strongly the glass bends what is behind it; 0 = no bending", "玻璃彎曲身後內容的強度；0 = 不彎曲"),
    ("How strongly the green base mixes in", "綠色基調的混合比例"),
    ("How strongly the purple base mixes in", "紫色基調的混合比例"),
    ("How strongly the warm paper base mixes in", "暖色紙基的混合比例"),
    ("How visible the background stars are", "背景星點的明顯程度"),
    ("How visible the fibers and grain are", "纖維與噪點的明顯程度"),
    ("Hue", "色相"),
    ("Icon", "圖示"),
    ("Icon size", "圖示大小"),
    ("Icon, type and size at the card's corner: click to toggle, drag to reorder", "卡片右下角的圖示、類型與大小：點擊顯示隱藏，拖曳排序"),
    ("Identical content is bumped to the top instead of duplicated", "內容相同時不新增，改為置頂更新"),
    ("Image", "圖片"),
    ("Image thumbnails", "圖片縮圖"),
    ("In-window toast", "視窗內提示"),
    ("Indigo", "靛藍"),
    ("Intensity and strength of the current style theme", "目前風格主題的濃度與效果強度"),
    ("Interface language", "界面語言"),
    ("Invalid shortcut:", "快速鍵無效："),
    ("Just now", "剛剛"),
    ("Keep the main window above other windows", "主視窗始終顯示在其它視窗之上"),
    ("Keystrokes", "按鍵模擬"),
    ("Large", "大"),
    ("Launch at startup", "開機自行啟動"),
    ("Lay out headings, lists and code blocks; off = monospaced source", "依標題、列表與程式碼塊排版；關閉後顯示等寬源碼"),
    // 上下文：同上。
    ("Left edge", "左側"),
    ("License", "開放原始碼授權"),
    ("Light", "淺色"),
    ("Light / dark mode", "明暗模式"),
    ("Lightness", "明度"),
    ("Liquid glass", "液態玻璃"),
    ("Listen address", "監聽位址"),
    ("Local", "本機"),
    // 上下文：上锁（不是置顶 Pin）。
    ("Lock", "固定"),
    // 上下文：卡片锁已上（不是置顶）。
    ("Locked", "已固定"),
    ("MCP service", "MCP 服務"),
    ("MCP service: ", "MCP 服務："),
    ("MCP tool access", "MCP 呼叫設定"),
    ("Masked", "密文檢視"),
    ("Max chars per item (controls width)", "每項顯示的最大字元數（控制寬度）"),
    ("Max entries", "歷史項目數上限"),
    ("Max entry size", "單條大小上限"),
    ("Mini panel", "迷你面板"),
    ("Minimize", "最小化"),
    ("Multi-select", "多選組合過濾"),
    ("No HTTP port configured; stdio mode is client-managed and cannot be probed", "未設定 HTTP 連接埠；stdio 模式由用戶端託管，無法偵測"),
    ("No clipboard history yet", "尚無剪貼簿記錄"),
    ("No data", "暫無資料"),
    ("No matching results", "沒有符合的結果"),
    ("No path", "無路徑"),
    ("No pinned entries yet", "尚未有置頂項目"),
    ("No recently changed settings", "暫無最近修改的設定"),
    ("No release notes available", "暫無更新說明"),
    ("No service detected (start it with --mcp-http)", "未偵測到服務（請確認已用 --mcp-http 啟動）"),
    ("None", "無"),
    ("None, frosted blur, translucent, or liquid glass", "無、磨砂、半透明或液態玻璃"),
    ("Not connected", "未連線"),
    ("Nothing to undo", "沒有可復原的修改"),
    ("Notifications", "通知方式"),
    ("Number of recent entries shown in the panel", "面板中顯示的最近項目數"),
    ("Numbers", "數值"),
    ("Ocean blue", "預設藍"),
    ("Off", "關閉"),
    ("Offset", "位移"),
    ("Olive", "橄欖"),
    ("Opacity", "濃度"),
    ("Open", "開啟"),
    ("Open in browser", "在瀏覽器中開啟"),
    ("Open repo", "開啟儲存庫"),
    ("Original size unknown", "原圖尺寸未知"),
    ("Override individual theme tokens: any element, any value", "逐項覆寫主題取值：任何元素、任何參數"),
    ("Own column", "獨立分欄"),
    ("Panel height", "面板高度"),
    ("Paper", "紙紋"),
    ("Paper tint", "紙基濃度"),
    ("Paste failed:", "貼上失敗："),
    ("Paste mode", "貼上方式"),
    // 上下文：热键槽位名（不是动作说明那一句）。
    ("Paste previous", "貼上一條"),
    ("Paste previous entry", "貼上上一個項目"),
    ("Paste the most recent entry into the focused app", "把最近一條歷史直接貼到目前輸入處"),
    ("Pin", "置頂"),
    ("Pin entries in the same list, or in a separate draggable column", "置頂項目與歷史同欄混排，或獨立成可拖出的分欄"),
    ("Pin failed:", "置頂失敗："),
    ("Pin window", "視窗置頂"),
    ("Pinned", "已置頂"),
    // 上下文：卡片种类（不是行操作「Pin」）。
    ("Pinned card", "置頂卡片"),
    ("Pinned layout", "置頂佈局"),
    ("Polling interval", "監聽間隔"),
    ("Port", "連接埠"),
    ("Port for the MCP service; empty = disabled", "MCP 服務連接埠；留空則不啟用"),
    ("Preset", "預設"),
    ("Press key combos and function keys (shortcuts, confirm/cancel)", "按下組合鍵與功能鍵（快速鍵、確認/取消）"),
    ("Preview length", "預覽長度"),
    ("Preview unavailable", "無法預覽"),
    ("Project", "專案"),
    ("Project page", "專案首頁"),
    ("Purple tint", "紫度"),
    ("Quit", "結束"),
    // 上下文：设置项里的退出行为（不是托盘菜单的「退出」）。
    ("Quit app", "結束程式"),
    ("Quit the app and stop recording", "結束程式並停止背景記錄"),
    ("Randomize layout", "分布隨機化"),
    ("Read UI layout", "取得元素佈局"),
    ("Read geometry and hierarchy of UI elements", "讀取界面元素的幾何與層級資訊"),
    ("Read or write system clipboard text", "讀取或寫入系統剪貼板文字"),
    ("Recently changed", "最近修改"),
    ("Record", "錄製"),
    ("Record clipboard", "記錄剪貼板"),
    ("Recording behavior", "記錄行為"),
    ("Recording shortcut", "正在錄製快速鍵"),
    ("Remove temp files left over from the previous run on startup", "啟動時刪除上次退出遗留的臨時檔案"),
    ("Render Markdown", "Markdown 算繪"),
    ("Reset", "重設"),
    ("Reset all", "全部復原"),
    // 上下文：重置令牌覆盖表（不是主题默认值）。
    ("Reset all tokens", "全部重設"),
    ("Resize and move any window", "調整任意視窗的尺寸與位置"),
    ("Resize windows", "調整視窗大小"),
    ("Restarting…", "即將重新啟動…"),
    ("Restore default", "恢復預設"),
    ("Restore this card to defaults", "恢復此卡片預設值"),
    // 上下文：同上。
    ("Right edge", "右側"),
    ("Right-click with a modifier to pop up recent entries at the cursor", "按住修飾鍵右鍵點擊，在游標處彈出最近幾筆"),
    ("Rose", "玫紅"),
    ("Saturation", "飽和"),
    ("Save", "儲存"),
    ("Save (Ctrl+Enter)", "確認修改（Ctrl+Enter）"),
    ("Save failed:", "儲存失敗："),
    ("Save failed: invalid image data", "儲存失敗：影像資料異常"),
    ("Screen capture", "螢幕截圖"),
    ("Scroll pages and lists to a target position", "捲動頁面與列表到目標位置"),
    ("Scroll views", "捲動檢視"),
    ("Search", "搜尋"),
    ("Search (expanded)", "搜尋（展開中）"),
    ("Search (filter active, click to view)", "搜尋（過濾生效中，點擊查看）"),
    ("Search history…", "搜尋歷史…"),
    ("Search in browser", "在瀏覽器中搜尋"),
    ("Search panel", "搜尋面板"),
    ("Search settings", "搜尋設定"),
    ("Select a style theme first", "先選擇一個風格主題"),
    ("Select an entry to see details", "選取一筆記錄查看詳細資訊"),
    ("Select several types at once; the list shows entries matching any of them", "開啟後可同時選擇多個類型，列表顯示任一所選的項目"),
    ("Separate layers with brighter borders on pure black", "純黑下靠描邊區分層級"),
    ("Separate popup", "獨立彈窗"),
    ("Service is running", "服務運行中"),
    ("Set to", "已設為"),
    ("Settings", "設定"),
    ("Shared column", "共用單欄"),
    ("Shift + right-click", "Shift + 右鍵"),
    ("Shortcuts", "快速鍵"),
    ("Show / hide main window", "顯示 / 隱藏主視窗"),
    ("Show all types", "始終顯示全部類型"),
    ("Show an icon and menu in the system tray while running", "背景常駐時在系統匣顯示圖示與選單"),
    ("Show image entries as thumbnails; off = size text only", "圖片項目直接顯示縮圖；關閉後僅顯示尺寸"),
    ("Show in file manager", "在檔案總管中顯示"),
    ("Show main window", "顯示主視窗"),
    ("Show tray icon", "顯示系統匣圖示"),
    ("Show type names", "顯示類型名稱"),
    ("Side length of each type button (24-48 pt)", "類型按鈕的邊長（24~48pt）"),
    ("Size", "大小"),
    ("Size variation", "大小不規則"),
    ("Skip clipboard content while passwords are being typed", "疑似輸入密碼時不記錄剪貼內容"),
    ("Skip password fields", "跳過密碼欄位"),
    ("Small", "小"),
    ("Source code, issue tracker and releases", "原始碼、問題反饋與發布版本"),
    // 上下文：字号档位名（不是「恢复默认」按钮）。
    ("Standard", "標準"),
    ("Star density", "星點密度"),
    ("Starfield purple", "星空紫"),
    ("Stars shift opposite to the cursor (parallax)", "星點隨游標位置反向偏移（視差）"),
    ("Start hidden in the tray instead of opening the window", "啟動後不顯示主視窗，直接駐留系統匣"),
    ("Start minimized", "啟動時最小化"),
    ("Start recording in the background at sign-in; open the window from the tray", "登入後僅啟動背景記錄，界面從系統匣喚起"),
    ("Start the HTTP service: modular-clipboard.exe --mcp-http\n", "啟動 HTTP 服務：modular-clipboard.exe --mcp-http\n"),
    ("Stop adding new entries when off", "關閉後不再新增歷史項目"),
    ("Storage", "儲存"),
    // 上下文：图片编辑器的笔触颜色（不是主题色）。
    ("Stroke color", "顏色"),
    ("Style parameters", "風格參數"),
    ("Style theme", "風格主題"),
    ("Table view", "表格檢視"),
    ("Teal", "天青"),
    ("Temperature", "色溫"),
    ("Text", "文字"),
    ("Text labels next to the icons: below them when vertical, to the right when horizontal", "在圖示旁顯示文字標籤；垂直停靠時在圖示下方，水平停靠時在右側"),
    ("Text size; presets apply instantly", "界面文字大小；預設檔位即改即生效"),
    ("Texture strength", "紋理強度"),
    ("The developer's GitHub profile", "開發者的 GitHub 首頁"),
    ("Theme", "主題"),
    ("This entry no longer exists", "該記錄已不存在"),
    ("Toggle the main window from anywhere", "全域喚起或隱藏主視窗"),
    ("Token overrides", "進階微調"),
    ("Token overrides · {n} overridden", "進階微調 · 已覆寫 {n} 個令牌"),
    // 上下文：类型过滤栏贴住窗口的哪条边（不是列表顶部）。
    ("Top edge", "頂部"),
    ("Translate (DeepL web)", "翻譯（DeepL 網頁版）"),
    ("Translucent", "半透明"),
    ("Transparency of the background layer; text stays opaque", "背景層的透明程度；文字始終不透明"),
    ("Trigger modifier; these can stack, keep at least one", "觸發修飾鍵，可疊加；至少保留一個"),
    ("Turn off to disable all MCP calls", "關閉後所有 MCP 呼叫不可用"),
    ("Turned off in settings", "總開關已關閉"),
    ("Twinkle", "閃爍"),
    ("Type", "類型"),
    ("Type content…", "輸入內容…"),
    ("Type filter bar", "類型過濾欄"),
    ("Type text", "輸入文字"),
    ("Type text and shortcuts into the focused control", "向焦點控件鍵入文字與快速鍵"),
    ("Undo crop", "復原裁剪"),
    ("Undo edit", "復原修改"),
    ("Undo stroke", "復原一筆"),
    ("Unpin", "取消置頂"),
    ("Unpin window", "取消視窗置頂"),
    ("Unreachable", "遠端無法連線（網路/防火牆/git）"),
    ("Up to date", "已是最新"),
    ("Update", "更新"),
    ("Update available", "發現新版本"),
    ("Version", "版本"),
    ("View", "檢視"),
    ("Violet", "紫羅蘭"),
    ("Warmth", "色溫"),
    ("What's new", "更新內容"),
    ("When off, only types present in history are shown", "關閉時僅顯示已有記錄的類型"),
    ("Whether the panel docks above or below the main window", "搜尋面板貼在主視窗的上方還是下方"),
    ("Which edge of the main window the bar sticks to", "類型欄貼在主視窗的哪一側"),
    ("Width", "粗細"),
    ("Win + right-click", "Win + 右鍵"),
    ("X-Large", "特大"),
    ("XX-Large", "超大"),
    ("\n=== Option 1: stdio (local, recommended) ===\nFor local clients such as Claude Desktop / Cursor, add this to your MCP config:\n", "\n=== 方式一：stdio（本機，推薦） ===\n適用於 Claude Desktop / Cursor 等本機用戶端，在 MCP 設定中加入：\n"),
    ("\n=== Option 2: HTTP (remote / custom address) ===\nService URL: http://{host}:{port}/mcp\nAuth: {auth}\nConfig:\n", "\n=== 方式二：HTTP（遠端 / 自訂位址） ===\n服務位址：http://{host}:{port}/mcp\n驗證：{auth}\n設定：\n"),
    ("bytes", "位元組"),
    // 上下文：同上。
    ("days ago", "天前"),
    // 上下文：同上。
    ("hours ago", "小時前"),
    ("local", "本機"),
    // 上下文：相对时间，跟在数字后（`5 min ago`）。
    ("min ago", "分鐘前"),
    ("{a} + {b} more files", "{a} 等 {b} 個檔案"),
    ("{a} tools · {b} callable · all disabled when off", "共 {a} 個能力 · {b} 個可呼叫 · 關閉後全部不可用"),
    ("{a}: {b} registration failed (possibly taken by another app)", "{a}：{b} 註冊失敗（可能已被其它程式占用）"),
    ("{name} is not a supported shortcut", "不支援將 {name} 設為快速鍵"),
    ("{n} overridden", "已覆寫 {n} 個令牌"),
    ("↺ Rotate left", "↺ 左轉"),
    ("↻ Rotate right", "↻ 右轉"),
    ("⇅ Flip vertical", "⇅ 垂直反轉"),
    ("⇋ Flip horizontal", "⇋ 水平反轉"),
    ("└ Opacity", "└ 透明度"),
    ("【modular-clipboard · MCP pairing】\nmodular-clipboard exposes a Model Context Protocol (MCP) service that can run UI automation on this machine.\n\nAvailable tools (10):\n", "【modular-clipboard · MCP 服務配對】\nmodular-clipboard 提供 Model Context Protocol（MCP）服務，可對本機執行介面自動化操作。\n\n可呼叫工具（10 項）：\n"),
];

/// 计数短语的复数形态表。**空**——本语言不分单复数，
/// 计数短语一律回落 [`TABLE`] 的通用形（见 [`super::tp`]）。
pub(super) static PLURALS: &[(&str, [&str; 4])] = &[];

#[cfg(test)]
mod tests {
    use super::TABLE;

    /// 占位符必须逐字保留：`{n}` 漏一个就是运行期格式化错位。
    /// （跨语言的同一道断言在 `super::super` 的测试里。）
    #[test]
    fn placeholders_match_the_english_key() {
        // CAUTION: 用嵌套 fn 而不是闭包。闭包写 `|s: &str| -> Vec<&str>`
        // 时返回值与入参的生命周期不会自动绑定，编译期报
        // "lifetime may not live long enough"；fn 的省略生命周期规则
        // 会正确地把输出绑到输入上。
        fn ph(s: &str) -> Vec<&str> {
            let mut v = Vec::new();
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'{' && let Some(e) = s[i..].find('}') {
                    v.push(&s[i..i + e + 1]);
                    i += e + 1;
                    continue;
                }
                i += 1;
            }
            v
        }
        for (en, tr) in TABLE {
            assert_eq!(ph(en), ph(tr), "{en} 的占位符与译文不一致");
        }
    }

    /// 表内不允许重复键——重复会让前一条永远查不到（静默失效）。
    #[test]
    fn no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for (en, _) in TABLE {
            assert!(seen.insert(*en), "键 {en} 重复");
        }
    }

    /// 空译文等同于没译——宁可回落英文，也不要显示空字符串。
    #[test]
    fn no_empty_translations() {
        for (en, tr) in TABLE {
            assert!(!tr.trim().is_empty(), "{en} 的译文为空");
            assert!(!en.trim().is_empty(), "出现了空键");
        }
    }

    /// 译文不能与英文完全相同——那样等于没翻译，还占一行。
    /// （自称词如 `HTML` / `Markdown` 不进表，正是为此。）
    #[test]
    fn translations_differ_from_english() {
        for (en, tr) in TABLE {
            assert_ne!(en, tr, "{en} 的译文与英文相同，删掉这条更省");
        }
    }
}

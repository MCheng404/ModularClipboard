//! 日语（ja-JP）文案表。
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
//! 日本語。敬体（です・ます）统一，按钮与标签用简体短语。
pub(super) static TABLE: &[(&str, &str)] = &[
    (" (custom)", "（カスタム）"),
    // 上下文：同上。
    (" chars", "文字"),
    // 上下文：同上。
    (" entries", " 件"),
    (" entries (multi-select)", " 件（複数選択可）"),
    // 上下文：数字后的单位后缀（`5 items`）。
    (" items", "件"),
    ("'Copy & notify' only fills the clipboard; 'Copy & paste' also sends Ctrl+V to the target", "「コピー＆通知」はクリップボードに書き込むだけで手動貼り付けが必要です。「コピー＆貼り付け」は対象ウィンドウに Ctrl+V を送信します"),
    ("0 = one fixed sky, 1 = reshuffle on every rebuild", "0 = 星空を固定、1 = 再構築ごとにシャッフル"),
    ("<full path to modular-clipboard.exe>", "<modular-clipboard.exe のフルパス>"),
    ("A style layer on top of light / dark: paper, eye-care green, starfield purple, OLED", "ライト/ダークの上に重なるスタイル層：紙紋・アイケアグリーン・星空紫・OLED"),
    ("AI pairing prompt copied", "AI ペアリングプロンプトをコピーしました"),
    ("About", "バージョン情報"),
    // 上下文：搜索面板相对主窗口的停靠位置（不是列表排序方向）。
    ("Above", "ウィンドウ上"),
    ("Accent color", "アクセントカラー"),
    ("Accent color: pick from swatches, sliders, or a hex value", "アクセントカラー：パレット、色相スライダー、16進数のいずれか"),
    ("Access token", "アクセストークен"),
    ("Access token; empty = no authentication", "アクセストークン。空欄なら認証不要"),
    ("Activate a given window or app", "指定のウィンドウやアプリを有効化"),
    ("Address the MCP service listens on", "MCP サービスが待ち受けるアドレス"),
    ("Adjust", "調整"),
    ("All history cleared", "履歴をすべて消去しました"),
    ("Alt + right-click", "Alt + 右クリック"),
    ("Always on top", "ウィンドウを最前面に"),
    ("Amber", "アンバー"),
    ("App info", "アプリ情報"),
    ("App used to open file entries; empty = system file manager", "ファイル項目を開くプログラム。空欄でシステムのエクスプローラー"),
    ("App used to open link entries; empty = system default browser", "リンク項目を開くプログラム。空欄でシステム既定ブラウザ"),
    ("Appearance", "外観"),
    ("Author page", "作者ページ"),
    ("Auto", "自動"),
    ("Auto (discrete preferred)", "自動（ディスクリート優先）"),
    ("Background effect", "背景効果"),
    ("Background opacity", "背景の不透明度"),
    ("Bearer token (configured in settings)", "Bearer Token（設定画面で構成済み）"),
    // 上下文：同上。
    ("Below", "ウィンドウ下"),
    ("Black level", "黒の程度"),
    ("Blur", "ぼかし"),
    ("Blur intensity in frosted mode", "磨砂モードのぼかし具合"),
    ("Blur strength", "ぼかし強度"),
    ("Borders", "境界線を明るく"),
    // 上下文：同上。
    ("Bottom edge", "下"),
    ("Brightness", "明るさ"),
    ("Browser path", "ブラウザのパス"),
    ("Brush", "ブラシ"),
    ("Built with", "使用技術"),
    ("Cancel", "キャンセル"),
    ("Cancel (Esc)", "取り消し（Esc）"),
    ("Cannot edit image: content was evicted ({e})", "画像を編集できません：内容は退避されました（{e}）"),
    ("Cannot edit image: missing dimensions", "画像を編集できません：サイズ情報がありません"),
    ("Cannot edit: content was evicted ({e})", "編集できません：内容は退避されました（{e}）"),
    ("Capture resolution", "キャプチャ解像度"),
    ("Discrete GPU", "ディスクリート GPU"),
    ("GPU adapter", "GPU アダプター"),
    ("Graphics", "グラフィックス"),
    ("In use", "使用中"),
    ("Integrated GPU", "内蔵 GPU"),
    ("Opacity of entry cards; without a background effect they blend into the window fill", "エントリカードの不透明度。背景効果がない場合はウィンドウの下地と混ざります"),
    ("Percentage of the window size to capture; 100% matches screen pixels exactly, lower costs less", "ウィンドウサイズに対するキャプチャ領域の割合。100% で画面のピクセルと一致し、低いほど負荷が下がります"),
    ("Cannot open the image editor:", "画像編集ウィンドウを開けません："),
    ("Capacity and cleanup", "容量とクリーンアップ"),
    ("Capture", "キャプチャ"),
    ("Capture the screen or a window", "画面またはウィンドウをキャプチャ"),
    ("Card blur", "カードのすりガラス"),
    ("Card meta badges", "カードのバッジ"),
    ("Card opacity", "カードの不透明度"),
    ("Card shadow", "カードの影"),
    ("Which graphics card renders the UI; takes effect after a restart", "UI を描画する GPU。再起動後に反映されます"),
    ("Window and startup", "ウィンドウと起動"),
    ("Window background", "ウィンドウ背景"),
    ("External programs", "外部プログラム"),
    ("Row cards", "行カード"),
    ("Panels and popups", "パネルとポップアップ"),
    ("Changes saved", "変更を保存しました"),
    ("Check", "確認"),
    ("Check for updates", "更新を確認"),
    ("Checking…", "確認中…"),
    ("Clean up temp files", "一時ファイルを削除"),
    ("Clear", "全消去"),
    ("Clear all history (hold 5s)", "全履歴を消去（5秒長押し）"),
    ("Clear failed:", "全消去失敗："),
    ("Clear history", "履歴を消去"),
    ("Clear search", "検索を消去"),
    ("Click UI elements", "UI 要素をクリック"),
    ("Click a name to open its repository", "名前をクリックすると公式リポジトリが開きます"),
    ("Click anywhere to stop recording", "記録完了後、任意の場所をクリックすると停止します"),
    ("Click or activate elements anywhere", "任意の位置で要素をクリック/有効化"),
    ("Click to open in the file manager", "クリックでファイルマネージャーを開く"),
    ("Click to view the full text", "クリックで全文を表示"),
    ("Clipboard access", "クリップボード読み書き"),
    ("Clipboard content larger than this is not recorded", "このサイズを超えるクリップボード内容は記録しません"),
    ("Close", "閉じる"),
    ("Close without saving", "閉じる（保存しない）"),
    ("Collapse", "折りたたむ"),
    ("Color", "色"),
    ("Color fringing at the edges: 0 = none, higher = more prismatic", "端の色にじみ：0 = なし、大きいほどプリズム状"),
    ("Colors", "色"),
    ("Connected", "接続済み"),
    ("Connection", "接続"),
    ("Contrast", "コントラスト"),
    ("Copied", "コピーしました"),
    ("Copied — press Ctrl+V to paste", "クリップボードにコピーしました。Ctrl+V で貼り付け"),
    ("Copy", "コピー"),
    ("Copy & notify", "コピーして通知"),
    ("Copy & paste", "コピーして貼り付け"),
    ("Copy AI pairing prompt", "AI ペアリングプロンプトをコピー"),
    ("Copy failed:", "コピー失敗："),
    ("Coral", "コーラル"),
    ("Crop", "トリミング"),
    ("Ctrl + right-click", "Ctrl + 右クリック"),
    ("Cursor parallax", "カーソル視差"),
    ("Custom", "カスタム"),
    ("Dark", "ダーク"),
    ("Data", "データ"),
    ("Data directory", "データディレクトリ"),
    ("Deduplicate copies", "コピーの重複除去"),
    ("Default", "既定"),
    ("Default (explorer.exe)", "既定（explorer.exe）"),
    ("Default (system browser)", "既定（システム既定ブラウザ）"),
    ("Delete", "削除"),
    ("Delete failed:", "削除失敗："),
    ("Details", "詳細"),
    ("Disabled", "無効"),
    ("Display language: follow the system, or pin one from the list", "表示言語：システムに従うか、中国語 / 英語に固定"),
    ("Dock position", "ドッキング位置"),
    ("Done", "完了"),
    ("Done: ", "実行済み："),
    ("Downloading…", "ダウンロード中…"),
    ("Drag", "ドラッグ"),
    ("Drag a box on the image; release to crop", "画像上に枠をドラッグし、離すとトリミング"),
    ("Drag and drop", "ドラッグ＆ドロップ"),
    ("Drag elements or perform drag-and-drop", "要素をドラッグ／ドラッグ＆ドロップを実行"),
    ("Drop shadow under entry cards; dark mode deepens it automatically", "項目カードの影。ダークモードでは自動的に濃くなる"),
    ("Durations (read-only)", "時間（読み取り専用）"),
    ("Edit content", "内容を編集"),
    ("Edit image", "画像を編集"),
    ("Effective values of all theme tokens. Dragging writes an override, which wins over values derived from the style theme and accent color.", "すべてのテーマトークンの現在の有効値。ドラッグすると上書きされ、スタイルテーマやアクセント色からの派生値より優先されます。"),
    ("Embedded", "埋め込み"),
    ("Emerald", "エメラルド"),
    ("Empty content, not saved", "内容が空のため保存しませんでした"),
    ("Enable", "有効"),
    ("Entry count", "表示件数"),
    ("Expand", "展開"),
    ("Eye-care green", "アイケアグリーン"),
    ("Failed to apply:", "設定失敗："),
    ("Failed to copy the pairing prompt:", "ペアリングプロンプトのコピーに失敗："),
    ("Failed to load the original image", "原図の読み込みに失敗しました"),
    ("Failed to open link:", "リンクを開けません："),
    ("Failed to open the file manager:", "エクスプローラーを開けません："),
    ("Failed to save settings:", "設定の保存に失敗："),
    ("Failed to start the capture thread", "キャプチャスレッドを起動できません"),
    ("Fetch the latest tag and compare with local", "リポジトリから最新タグを取得して本地と比較"),
    ("File manager path", "ファイルマネージャのパス"),
    ("Files", "ファイル"),
    ("Floating search panel; dock position and height adjustable", "浮遊検索パネル：ドッキング位置と高さを調整可"),
    ("Floating type buttons at the window edge: click to filter; multi-select, adjustable position and size", "ウィンドウ端の浮遊種類ボタン：クリックで絞り込み、複数選択・位置とサイズ調整可"),
    ("Focus windows", "フォーカスするウィンドウを切り替え"),
    ("Follow system", "システムに従う"),
    ("Follow the system, or force dark / light", "システムに従うか、ダーク/ライトを強制"),
    ("Font scale", "文字サイズ"),
    ("Fresnel highlight along the edges; lower spreads it out", "縁に沿うフレネル反射光；小さいほど広がる"),
    ("From cyan (cool) to yellow (warm)", "青（冷）から黄（暖）まで"),
    ("Frosted", "磨砂"),
    ("Frosted glass for entry cards: 0 = off, higher = blurrier", "カードをすりガラス風に：0 = オフ、数値が大きいほどぼかしが強い"),
    ("Gamma", "ガンマ"),
    ("General", "一般"),
    ("Glass", "ガラス"),
    ("Glass dispersion", "ガラスの分散"),
    ("Glass blur", "ガラスのぼかし"),
    ("Blur strength in glass mode, independent of frosted blur", "ガラスモードのぼかし強度。すりガラスとは独立"),
    ("Glass refraction", "ガラスの屈折"),
    ("Glass rim light", "ガラスの縁の光"),
    ("Graphite", "グラファイト"),
    ("Green tint", "緑の濃さ"),
    // 上下文：HSV 取色器的 S 分量（不是整体饱和度）。
    ("HSV saturation", "彩度"),
    ("Height of the search panel (30-56 pt)", "検索パネルの高さ（30〜56pt）"),
    ("Hex", "16進数"),
    ("Hide on focus loss", "フォーカスを失ったら隠す"),
    ("Hide the main window when another app gains focus", "他のアプリがフォーカスを得たら主ウィンドウを隠す"),
    ("Hide to tray", "トレイに隠す"),
    ("History", "履歴"),
    ("Hold at least one modifier (Ctrl/Shift/Alt/Win)", "修飾キーを最低1つ押してください（Ctrl/Shift/Alt/Win）"),
    ("How far the background is pushed toward pure black", "背景が純黒にどれだけ寄るか"),
    ("How feedback appears: in-window toasts or a separate popup", "操作フィードバックの表示：ウィンドウ内トーストか独立ポップアップ"),
    ("How many entries to keep; empty = size-based eviction only", "保持する項目数。空欄なら容量ベースの淘汰のみ"),
    ("How much star radii vary", "星の半径のばらつき具合"),
    ("How often the clipboard is checked; lower is faster but heavier", "クリップボードの確認間隔。小さいほど高速だが負荷も増える"),
    ("How strongly stars breathe between bright and dim", "星が明暗に呼吸する強さ"),
    ("How strongly the glass bends what is behind it; 0 = no bending", "ガラスが背後にあるものをどれだけ曲げるか；0 = 曲げなし"),
    ("How strongly the green base mixes in", "緑の基調がどれだけ混ざるか"),
    ("How strongly the purple base mixes in", "紫の基調がどれだけ混ざるか"),
    ("How strongly the warm paper base mixes in", "暖色の紙地がどれだけ混ざるか"),
    ("How visible the background stars are", "背景の星の目立ち具合"),
    ("How visible the fibers and grain are", "繊維と粒子の目立ち具合"),
    ("Hue", "色相"),
    ("Icon", "アイコン"),
    ("Icon size", "アイコンサイズ"),
    ("Icon, type and size at the card's corner: click to toggle, drag to reorder", "カード右下のアイコン・種類・サイズ：クリックで表示切替、ドラッグで並べ替え"),
    ("Identical content is bumped to the top instead of duplicated", "同じ内容なら追加せず最上部に更新"),
    ("Image", "画像"),
    ("Image thumbnails", "画像のサムネイル"),
    ("In-window toast", "ウィンドウ内トースト"),
    ("Indigo", "インディゴ"),
    ("Intensity and strength of the current style theme", "現在のスタイルテーマの濃さと効果の強さ"),
    ("Interface language", "インターフェース言語"),
    ("Invalid shortcut:", "ショートカットが無効："),
    ("Just now", "たった今"),
    ("Keep the main window above other windows", "メインウィンドウを常に他のウィンドウより前に"),
    ("Keystrokes", "キー入力シミュレーション"),
    ("Large", "大"),
    ("Launch at startup", "サインイン時に起動"),
    ("Lay out headings, lists and code blocks; off = monospaced source", "見出し・リスト・コードブロックを整形。オフなら等幅ソース"),
    // 上下文：同上。
    ("Left edge", "左"),
    ("License", "ライセンス"),
    ("Light", "ライト"),
    ("Light / dark mode", "ライト/ダークモード"),
    ("Lightness", "明度"),
    ("Liquid glass", "リキッドガラス"),
    ("Listen address", "待ち受けアドレス"),
    ("Local", "ローカル"),
    // 上下文：上锁（不是置顶 Pin）。
    ("Lock", "ロック"),
    // 上下文：卡片锁已上（不是置顶）。
    ("Locked", "ロック済み"),
    ("MCP service", "MCP サービス"),
    ("MCP service: ", "MCP サービス："),
    ("MCP tool access", "MCP ツールアクセス"),
    ("Masked", "マスク表示"),
    ("Max chars per item (controls width)", "1項目あたりの最大文字数（幅を決める）"),
    ("Max entries", "履歴の上限"),
    ("Max entry size", "1項目のサイズ上限"),
    ("Mini panel", "ミニパネル"),
    ("Minimize", "最小化"),
    ("Multi-select", "複数選択"),
    ("No HTTP port configured; stdio mode is client-managed and cannot be probed", "HTTP ポートが未設定です。stdio モードはクライアント管理のため検出できません"),
    ("No clipboard history yet", "クリップボード履歴はまだありません"),
    ("No data", "データがありません"),
    ("No matching results", "一致する結果がありません"),
    ("No path", "パスなし"),
    ("No pinned entries yet", "ピン留めされた項目はまだありません"),
    ("No recently changed settings", "最近変更した設定はありません"),
    ("No release notes available", "更新情報はありません"),
    ("No service detected (start it with --mcp-http)", "サービスが検出されませんでした（--mcp-http で起動したか確認してください）"),
    ("None", "なし"),
    ("None, frosted blur, translucent, or liquid glass", "なし、磨砂、半透明、またはリキッドガラス"),
    ("Not connected", "未接続"),
    ("Nothing to undo", "元に戻せる変更はありません"),
    ("Notifications", "通知方法"),
    ("Number of recent entries shown in the panel", "パネルに表示する最近の項目数"),
    ("Numbers", "数値"),
    ("Ocean blue", "オーシャンブルー"),
    ("Off", "オフ"),
    ("Offset", "オフセット"),
    ("Olive", "オリーブ"),
    ("Opacity", "不透明度"),
    ("Open", "開く"),
    ("Open in browser", "ブラウザで開く"),
    ("Open repo", "リポジトリを開く"),
    ("Original size unknown", "原寸サイズは不明です"),
    ("Override individual theme tokens: any element, any value", "テーマトークンを個別に上書き：任意の要素、任意の値"),
    ("Own column", "独立した欄"),
    ("Panel height", "パネルの高さ"),
    ("Paper", "紙紋"),
    ("Paper tint", "紙地の濃さ"),
    ("Paste failed:", "貼り付け失敗："),
    ("Paste mode", "貼り付け方法"),
    // 上下文：热键槽位名（不是动作说明那一句）。
    ("Paste previous", "前の項目を貼り付け"),
    ("Paste previous entry", "前の項目を貼り付け"),
    ("Paste the most recent entry into the focused app", "最新の履歴をフォーカス中のアプリに直接貼り付け"),
    ("Pin", "ピン留め"),
    ("Pin entries in the same list, or in a separate draggable column", "ピン留めを履歴と同一欄に混在させるか、独立してドラッグできる欄にする"),
    ("Pin failed:", "ピン留め失敗："),
    ("Pin window", "ウィンドウを最前面に固定"),
    ("Pinned", "ピン留め済み"),
    // 上下文：卡片种类（不是行操作「Pin」）。
    ("Pinned card", "固定カード"),
    ("Pinned layout", "ピン留めのレイアウト"),
    ("Polling interval", "監視間隔"),
    ("Port", "ポート"),
    ("Port for the MCP service; empty = disabled", "MCP サービスのポート。空欄で無効"),
    ("Preset", "プリセット"),
    ("Press key combos and function keys (shortcuts, confirm/cancel)", "組合せキーやファンクションキーを押す（ショートカット、決定/取消）"),
    ("Preview length", "プレビュー長"),
    ("Preview unavailable", "プレビューできません"),
    ("Project", "プロジェクト"),
    ("Project page", "プロジェクトページ"),
    ("Purple tint", "紫の濃さ"),
    ("Quit", "終了"),
    // 上下文：设置项里的退出行为（不是托盘菜单的「退出」）。
    ("Quit app", "終了"),
    ("Quit the app and stop recording", "アプリを終了して記録を停止"),
    ("Randomize layout", "配置をランダム化"),
    ("Read UI layout", "UI レイアウトを読み取る"),
    ("Read geometry and hierarchy of UI elements", "UI 要素の幾何と階層を読み取る"),
    ("Read or write system clipboard text", "システムのクリップボードテキストを読み書き"),
    ("Recently changed", "最近の変更"),
    ("Record", "記録"),
    ("Record clipboard", "クリップボードを記録"),
    ("Recording behavior", "記録の動作"),
    ("Recording shortcut", "ショートカットを記録中"),
    ("Remove temp files left over from the previous run on startup", "起動時に前回終了時の一時ファイルを削除"),
    ("Render Markdown", "Markdown 表示"),
    ("Reset", "リセット"),
    ("Reset all", "すべてリセット"),
    // 上下文：重置令牌覆盖表（不是主题默认值）。
    ("Reset all tokens", "すべてリセット"),
    ("Resize and move any window", "任意のウィンドウのサイズと位置を変更"),
    ("Resize windows", "ウィンドウサイズを変更"),
    ("Restarting…", "再起動します…"),
    ("Restore default", "既定に戻す"),
    ("Restore this card to defaults", "このカードを既定値に戻す"),
    // 上下文：同上。
    ("Right edge", "右"),
    ("Right-click with a modifier to pop up recent entries at the cursor", "修飾キーを押しながら右クリックでカーソル位置に最近の項目を表示"),
    ("Rose", "ローズ"),
    ("Saturation", "彩度"),
    ("Save", "保存"),
    ("Save (Ctrl+Enter)", "確定（Ctrl+Enter）"),
    ("Save failed:", "保存失敗："),
    ("Save failed: invalid image data", "保存失敗：画像データが異常です"),
    ("Screen capture", "スクリーンショット"),
    ("Scroll pages and lists to a target position", "ページやリストを目的位置までスクロール"),
    ("Scroll views", "ビューをスクロール"),
    ("Search", "検索"),
    ("Search (expanded)", "検索（展開中）"),
    ("Search (filter active, click to view)", "検索（絞り込み中、クリックで表示）"),
    ("Search history…", "履歴を検索…"),
    ("Search in browser", "ブラウザで検索"),
    ("Search panel", "検索パネル"),
    ("Search settings", "設定を検索"),
    ("Select a style theme first", "先にスタイルテーマを選んでください"),
    ("Select an entry to see details", "項目を選ぶと詳細が表示されます"),
    ("Select several types at once; the list shows entries matching any of them", "オンにすると複数の種類を同時に選べ、リストはいずれかに一致する項目を表示"),
    ("Separate layers with brighter borders on pure black", "純黒の下で描辺で層を区切る"),
    ("Separate popup", "独立ポップアップ"),
    ("Service is running", "サービス実行中"),
    ("Set to", "設定しました"),
    ("Settings", "設定"),
    ("Shared column", "單一欄に共通"),
    ("Shift + right-click", "Shift + 右クリック"),
    ("Shortcuts", "ショートカット"),
    ("Show / hide main window", "メインウィンドウの表示/非表示"),
    ("Show all types", "常にすべての種類を表示"),
    ("Show an icon and menu in the system tray while running", "常駐中はシステムトレイにアイコンとメニューを表示"),
    ("Show image entries as thumbnails; off = size text only", "画像項目をサムネイルで表示。オフなら寸法のみ"),
    ("Show in file manager", "エクスプローラーで表示"),
    ("Show main window", "メインウィンドウを表示"),
    ("Show tray icon", "トレイアイコンを表示"),
    ("Show type names", "種類名を表示"),
    ("Side length of each type button (24-48 pt)", "各種類ボタンの辺の長さ（24〜48pt）"),
    ("Size", "サイズ"),
    ("Size variation", "大きさのばらつき"),
    ("Skip clipboard content while passwords are being typed", "パスワード入力中はクリップボード内容を記録しません"),
    ("Skip password fields", "パスワード欄をスキップ"),
    ("Small", "小"),
    ("Source code, issue tracker and releases", "ソースコード、issue 追跡、リリース"),
    // 上下文：字号档位名（不是「恢复默认」按钮）。
    ("Standard", "標準"),
    ("Star density", "星の密度"),
    ("Starfield purple", "星空の紫"),
    ("Stars shift opposite to the cursor (parallax)", "星がカーソルと逆方向にずれる（視差）"),
    ("Start hidden in the tray instead of opening the window", "起動後に主ウィンドウを開かずトレイに常駐"),
    ("Start minimized", "最小化して起動"),
    ("Start recording in the background at sign-in; open the window from the tray", "サインイン後はバックグラウンド記録のみ起動し、UI はトレイから起動"),
    ("Start the HTTP service: modular-clipboard.exe --mcp-http\n", "HTTP サービスを起動してください：modular-clipboard.exe --mcp-http\n"),
    ("Stop adding new entries when off", "オフにすると新しい項目を追加しません"),
    ("Storage", "ストレージ"),
    // 上下文：图片编辑器的笔触颜色（不是主题色）。
    ("Stroke color", "色"),
    ("Style parameters", "スタイル設定"),
    ("Style theme", "スタイルテーマ"),
    ("Table view", "テーブル表示"),
    ("Teal", "ティール"),
    ("Temperature", "色温度"),
    ("Text", "テキスト"),
    ("Text labels next to the icons: below them when vertical, to the right when horizontal", "アイコンの横に文字ラベル。垂直時は下、水平時は右"),
    ("Text size; presets apply instantly", "UI の文字サイズ。プリセットは即時適用"),
    ("Texture strength", "テクスチャ強度"),
    ("The developer's GitHub profile", "開発者の GitHub プロフィール"),
    ("Theme", "テーマ"),
    ("This entry no longer exists", "この項目は存在しません"),
    ("Toggle the main window from anywhere", "どこからでも主ウィンドウを切り替え"),
    ("Token overrides", "トークン微調整"),
    ("Token overrides · {n} overridden", "トークン微調整 · {n} 件を上書き"),
    // 上下文：类型过滤栏贴住窗口的哪条边（不是列表顶部）。
    ("Top edge", "上"),
    ("Translate (DeepL web)", "翻訳（DeepL ウェブ版）"),
    ("Translucent", "半透明"),
    ("Transparency of the background layer; text stays opaque", "背景層の透明度。文字は常に不透明"),
    ("Trigger modifier; these can stack, keep at least one", "トリガー修飾キー。重ねられます。最低1つは残して"),
    ("Turn off to disable all MCP calls", "オフにすると MCP の呼び出しをすべて無効化"),
    ("Turned off in settings", "設定でオフになっています"),
    ("Twinkle", "瞬き"),
    ("Type", "種類"),
    ("Type content…", "内容を入力…"),
    ("Type filter bar", "種類フィルター欄"),
    ("Type text", "テキストを入力"),
    ("Type text and shortcuts into the focused control", "フォーカスされたコントロールに文字とショートカットを入力"),
    ("Undo crop", "トリミングを元に戻す"),
    ("Undo edit", "変更を元に戻す"),
    ("Undo stroke", "ひと筆を元に戻す"),
    ("Unpin", "ピン留め解除"),
    ("Unpin window", "ウィンドウの最前面を解除"),
    ("Unreachable", "リモートに到達できません"),
    ("Up to date", "最新です"),
    ("Update", "更新"),
    ("Update available", "新しい版があります"),
    ("Version", "バージョン"),
    ("View", "ビュー"),
    ("Violet", "バイオレット"),
    ("Warmth", "色温度"),
    ("What's new", "更新内容"),
    ("When off, only types present in history are shown", "オフなら履歴にある種類だけ表示"),
    ("Whether the panel docks above or below the main window", "検索パネルが主ウィンドウの上か下か"),
    ("Which edge of the main window the bar sticks to", "種類欄が主ウィンドウのどの辺に付くか"),
    ("Width", "太さ"),
    ("Win + right-click", "Win + 右クリック"),
    ("X-Large", "極大"),
    ("XX-Large", "超極大"),
    ("\n=== Option 1: stdio (local, recommended) ===\nFor local clients such as Claude Desktop / Cursor, add this to your MCP config:\n", "\n=== 方法 1：stdio（ローカル、推奨） ===\nClaude Desktop / Cursor などのローカルクライアントでは、MCP 設定に次を追加します：\n"),
    ("\n=== Option 2: HTTP (remote / custom address) ===\nService URL: http://{host}:{port}/mcp\nAuth: {auth}\nConfig:\n", "\n=== 方法 2：HTTP（リモート / カスタムアドレス） ===\nサービス URL：http://{host}:{port}/mcp\n認証：{auth}\n設定：\n"),
    ("bytes", "バイト"),
    // 上下文：同上。
    ("days ago", "日前"),
    // 上下文：同上。
    ("hours ago", "時間前"),
    ("local", "ローカル"),
    // 上下文：相对时间，跟在数字后（`5 min ago`）。
    ("min ago", "分前"),
    ("{a} + {b} more files", "{a} 他 {b} ファイル"),
    ("{a} tools · {b} callable · all disabled when off", "{a} 個の機能 · {b} 個が呼び出し可能 · オフ時はすべて無効"),
    ("{a}: {b} registration failed (possibly taken by another app)", "{a}：{b} の登録に失敗しました（他のアプリが使用中の可能性）"),
    ("{name} is not a supported shortcut", "{name} はショートカットに設定できません"),
    ("{n} overridden", "{n} 件を上書き"),
    ("↺ Rotate left", "↺ 左回転"),
    ("↻ Rotate right", "↻ 右回転"),
    ("⇅ Flip vertical", "⇅ 垂直反転"),
    ("⇋ Flip horizontal", "⇋ 水平反転"),
    ("└ Opacity", "└ 透明度"),
    ("【modular-clipboard · MCP pairing】\nmodular-clipboard exposes a Model Context Protocol (MCP) service that can run UI automation on this machine.\n\nAvailable tools (10):\n", "【modular-clipboard · MCP サービスのペアリング】\nmodular-clipboard は Model Context Protocol（MCP）サービスを提供し、このマシン上で UI 自動化操作を実行できます。\n\n呼び出し可能なツール（10 件）：\n"),
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

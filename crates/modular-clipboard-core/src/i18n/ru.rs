//! 俄语（ru-RU）文案表。
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
//! Русский. 按钮与标签用短短语，说明文字可用完整句。
pub(super) static TABLE: &[(&str, &str)] = &[
    (" (custom)", " (свой)"),
    (" MB", " МБ"),
    // 上下文：同上。
    (" chars", " символов"),
    // 上下文：同上。
    (" entries", " записей"),
    (" entries (multi-select)", " записей (мультивыбор)"),
    // 上下文：数字后的单位后缀（`5 items`）。
    (" items", " записей"),
    (" ms", " мс"),
    ("'Copy & notify' only fills the clipboard; 'Copy & paste' also sends Ctrl+V to the target", "«Копировать и уведомить» только заполняет буфер обмена, вставлять нужно вручную; «Копировать и вставить» также отправляет Ctrl+V в целевое окно"),
    ("0 = one fixed sky, 1 = reshuffle on every rebuild", "0 = одно фиксированное небо, 1 = пересборка при каждой перестройке"),
    ("<full path to modular-clipboard.exe>", "<полный путь к modular-clipboard.exe>"),
    ("A style layer on top of light / dark: paper, eye-care green, starfield purple, OLED", "Слой стиля поверх светлой/тёмной: бумага, зелёный, звёздное поле, OLED"),
    ("AI pairing prompt copied", "Промпт сопряжения ИИ скопирован"),
    ("About", "О программе"),
    // 上下文：搜索面板相对主窗口的停靠位置（不是列表排序方向）。
    ("Above", "Над окном"),
    ("Accent color", "Акцентный цвет"),
    ("Accent color: pick from swatches, sliders, or a hex value", "Акцентный цвет: палитра, слайдер оттенка или HEX"),
    ("Access token", "Ключ доступа"),
    ("Access token; empty = no authentication", "Ключ доступа; пусто — без аутентификации"),
    ("Activate a given window or app", "Активировать указанное окно или приложение"),
    ("Address the MCP service listens on", "Адрес, который прослушивает служба MCP"),
    ("Adjust", "Корректировка"),
    ("All history cleared", "Вся история очищена"),
    ("Alt + right-click", "Alt + правый клик"),
    ("Always on top", "Поверх всех окон"),
    ("Amber", "Янтарный"),
    ("App info", "О приложении"),
    ("App used to open file entries; empty = system file manager", "Программа для открытия файлов; пусто = системный проводник"),
    ("App used to open link entries; empty = system default browser", "Программа для открытия ссылок; пусто = системный браузер"),
    ("Appearance", "Внешний вид"),
    ("Author page", "Страница автора"),
    ("Background effect", "Эффект фона"),
    ("Background opacity", "Непрозрачность фона"),
    ("Bearer token (configured in settings)", "Bearer Token (настроен в параметрах)"),
    // 上下文：同上。
    ("Below", "Под окном"),
    ("Black level", "Уровень чёрного"),
    ("Blur", "Размытие"),
    ("Blur intensity in frosted mode", "Интенсивность размытия в матовом режиме"),
    ("Blur strength", "Сила размытия"),
    ("Borders", "Светлые границы"),
    // 上下文：同上。
    ("Bottom edge", "Снизу"),
    ("Brightness", "Яркость"),
    ("Browser path", "Путь к браузеру"),
    ("Brush", "Кисть"),
    ("Built with", "Технологии"),
    ("Cancel", "Отмена"),
    ("Cancel (Esc)", "Отменить (Esc)"),
    ("Cannot edit image: content was evicted ({e})", "Нельзя изменить изображение: содержимое вытеснено ({e})"),
    ("Cannot edit image: missing dimensions", "Нельзя изменить изображение: нет данных о размере"),
    ("Cannot edit: content was evicted ({e})", "Нельзя изменить: содержимое вытеснено ({e})"),
    ("Cannot open the image editor:", "Не удалось открыть редактор изображений:"),
    ("Capture", "Захват"),
    ("Capture the screen or a window", "Захватывать экран или окно"),
    ("Card blur", "Размытие карточек"),
    ("Card meta badges", "Значки на карточке"),
    ("Card opacity", "Непрозрачность карточек"),
    ("Card shadow", "Тень карточек"),
    ("Changes saved", "Изменения сохранены"),
    ("Check", "Проверить"),
    ("Check for updates", "Проверить обновления"),
    ("Checking…", "Проверка…"),
    ("Clean up temp files", "Удалять временные файлы"),
    ("Clear", "Очистить"),
    ("Clear all history (hold 5s)", "Очистить всю историю (удерживайте 5 с)"),
    ("Clear failed:", "Ошибка очистки:"),
    ("Clear history", "Очистить историю"),
    ("Clear search", "Очистить поиск"),
    ("Click UI elements", "Нажимать элементы UI"),
    ("Click a name to open its repository", "Нажмите на название, чтобы открыть репозиторий"),
    ("Click anywhere to stop recording", "После записи нажмите в любом месте для остановки"),
    ("Click or activate elements anywhere", "Нажимать или активировать элементы в любом месте"),
    ("Click to open in the file manager", "Нажмите, чтобы открыть в проводнике"),
    ("Click to view the full text", "Нажмите, чтобы прочитать полный текст"),
    ("Clipboard access", "Доступ к буферу обмена"),
    ("Clipboard content larger than this is not recorded", "Содержимое буфера больше этого размера не записывается"),
    ("Close", "Закрыть"),
    ("Close without saving", "Закрыть без сохранения"),
    ("Collapse", "Свернуть"),
    ("Color", "Цвет"),
    ("Colors", "Цвета"),
    ("Connected", "Подключено"),
    ("Connection", "Подключение"),
    ("Contrast", "Контраст"),
    ("Copied", "Скопировано"),
    ("Copied — press Ctrl+V to paste", "Скопировано — нажмите Ctrl+V, чтобы вставить"),
    ("Copy", "Копировать"),
    ("Copy & notify", "Копировать и сообщить"),
    ("Copy & paste", "Копировать и вставить"),
    ("Copy AI pairing prompt", "Скопировать промпт сопряжения ИИ"),
    ("Copy failed:", "Ошибка копирования:"),
    ("Coral", "Коралловый"),
    ("Crop", "Обрезка"),
    ("Ctrl + right-click", "Ctrl + правый клик"),
    ("Cursor parallax", "Параллакс курсора"),
    ("Custom", "Свой"),
    ("Dark", "Тёмная"),
    ("Data", "Данные"),
    ("Data directory", "Каталог данных"),
    ("Deduplicate copies", "Дедупликация копий"),
    ("Default", "По умолчанию"),
    ("Default (explorer.exe)", "По умолчанию (explorer.exe)"),
    ("Default (system browser)", "По умолчанию (системный браузер)"),
    ("Delete", "Удалить"),
    ("Delete failed:", "Ошибка удаления:"),
    ("Details", "Подробности"),
    ("Disabled", "Отключено"),
    ("Display language: follow the system, or pin one from the list", "Язык отображения: как в системе или один из списка"),
    ("Dock position", "Положение"),
    ("Done", "Готово"),
    ("Done: ", "Выполнено: "),
    ("Downloading…", "Загрузка…"),
    ("Drag", "Перетащить"),
    ("Drag a box on the image; release to crop", "Выделите рамку на изображении и отпустите для обрезки"),
    ("Drag and drop", "Перетаскивание"),
    ("Drag elements or perform drag-and-drop", "Перетаскивать элементы или выполнять drag-and-drop"),
    ("Drop shadow under entry cards; dark mode deepens it automatically", "Тень под карточками; в тёмной теме усиливается автоматически"),
    ("Durations (read-only)", "Длительности (только чтение)"),
    ("Edit content", "Изменить содержимое"),
    ("Edit image", "Изменить изображение"),
    ("Effective values of all theme tokens. Dragging writes an override, which wins over values derived from the style theme and accent color.", "Текущие действующие значения всех токенов темы. Перетаскивание записывает переопределение, которое имеет приоритет над значениями из стиля оформления и акцентного цвета."),
    ("Embedded", "Встроено"),
    ("Emerald", "Изумрудный"),
    ("Empty content, not saved", "Содержимое пусто, не сохранено"),
    ("Enable", "Включить"),
    ("Entry count", "Число записей"),
    ("Expand", "Развернуть"),
    ("Eye-care green", "Зелёный (для глаз)"),
    ("Failed to apply:", "Ошибка применения:"),
    ("Failed to copy the pairing prompt:", "Не удалось скопировать промпт сопряжения:"),
    ("Failed to load the original image", "Не удалось загрузить оригинал"),
    ("Failed to open link:", "Не удалось открыть ссылку:"),
    ("Failed to open the file manager:", "Не удалось открыть проводник:"),
    ("Failed to save settings:", "Ошибка сохранения настроек:"),
    ("Failed to start the capture thread", "Не удалось запустить поток захвата"),
    ("Fetch the latest tag and compare with local", "Получить последний тег из репозитория и сравнить с локальным"),
    ("File manager path", "Путь к файловому менеджеру"),
    ("Files", "Файлы"),
    ("Floating search panel; dock position and height adjustable", "Плавающая панель поиска: настраиваемые положение и высота"),
    ("Floating type buttons at the window edge: click to filter; multi-select, adjustable position and size", "Плавающие кнопки типов у края окна: клик — фильтр, мультивыбор, настраиваемые положение и размер"),
    ("Focus windows", "Переключать фокус окон"),
    ("Follow system", "Как в системе"),
    ("Follow the system, or force dark / light", "Как в системе или принудительно тёмная/светлая"),
    ("Font scale", "Масштаб шрифта"),
    ("From cyan (cool) to yellow (warm)", "От циана (хол.) до жёлтого (тепл.)"),
    ("Frosted", "Матовое"),
    ("Frosted glass for entry cards: 0 = off, higher = blurrier", "Матовое стекло для карточек: 0 = выкл., больше — сильнее размытие"),
    ("Gamma", "Гамма"),
    ("General", "Общие"),
    ("Graphite", "Графитовый"),
    ("Green tint", "Плотность зелёного"),
    // 上下文：HSV 取色器的 S 分量（不是整体饱和度）。
    ("HSV saturation", "Насыщенность"),
    ("Height of the search panel (30-56 pt)", "Высота панели поиска (30–56 pt)"),
    ("Hex", "HEX"),
    ("Hide on focus loss", "Скрывать при потере фокуса"),
    ("Hide the main window when another app gains focus", "Скрывать главное окно, когда фокус уходит в другое приложение"),
    ("Hide to tray", "Свернуть в трей"),
    ("History", "История"),
    ("Hold at least one modifier (Ctrl/Shift/Alt/Win)", "Удерживайте хотя бы один модификатор (Ctrl/Shift/Alt/Win)"),
    ("How far the background is pushed toward pure black", "Насколько фон приближен к чистому чёрному"),
    ("How feedback appears: in-window toasts or a separate popup", "Как показывать отклик: всплывающие подсказки в окне или отдельное окно"),
    ("How many entries to keep; empty = size-based eviction only", "Сколько записей хранить; пусто — вытеснение только по размеру"),
    ("How much star radii vary", "Насколько различаются радиусы звёзд"),
    ("How often the clipboard is checked; lower is faster but heavier", "Как часто проверять буфер: меньше — быстрее, но больше нагрузки"),
    ("How strongly stars breathe between bright and dim", "Насколько сильно звёзды «дышат» яркостью"),
    ("How strongly the green base mixes in", "Насколько сильно смешивается зелёная основа"),
    ("How strongly the purple base mixes in", "Насколько сильно смешивается фиолетовая основа"),
    ("How strongly the warm paper base mixes in", "Насколько сильно смешивается тёплая бумажная основа"),
    ("How visible the background stars are", "Насколько заметны фоновые звёзды"),
    ("How visible the fibers and grain are", "Насколько заметны волокна и зерно"),
    ("Hue", "Оттенок"),
    ("Icon", "Значок"),
    ("Icon size", "Размер значка"),
    ("Icon, type and size at the card's corner: click to toggle, drag to reorder", "Значок, тип и размер в углу карточки: клик — вкл/выкл, перетаскивание — порядок"),
    ("Identical content is bumped to the top instead of duplicated", "Одинаковое содержание не дублируется, а поднимается наверх"),
    ("Image", "Изображение"),
    ("Image thumbnails", "Миниатюры изображений"),
    ("In-window toast", "Подсказка в окне"),
    ("Indigo", "Индиго"),
    ("Intensity and strength of the current style theme", "Интенсивность и сила текущего стиля"),
    ("Interface language", "Язык интерфейса"),
    ("Invalid shortcut:", "Недопустимая комбинация:"),
    ("Just now", "Только что"),
    ("Keep the main window above other windows", "Держать главное окно поверх остальных"),
    ("Keystrokes", "Симуляция нажатий"),
    ("Large", "Крупный"),
    ("Launch at startup", "Запуск при входе в систему"),
    ("Lay out headings, lists and code blocks; off = monospaced source", "Оформлять заголовки, списки и блоки кода; выкл. — моноширинный исходник"),
    // 上下文：同上。
    ("Left edge", "Слева"),
    ("License", "Лицензия"),
    ("Light", "Светлая"),
    ("Light / dark mode", "Светлая/тёмная тема"),
    ("Lightness", "Светлота"),
    ("Listen address", "Адрес прослушивания"),
    ("Local", "Локально"),
    // 上下文：上锁（不是置顶 Pin）。
    ("Lock", "Заблокировать"),
    // 上下文：卡片锁已上（不是置顶）。
    ("Locked", "Заблокировано"),
    ("MCP service", "Служба MCP"),
    ("MCP service: ", "Служба MCP: "),
    ("MCP tool access", "Доступ к инструментам MCP"),
    ("Masked", "Скрытый вид"),
    ("Max chars per item (controls width)", "Максимум символов на элемент (задаёт ширину)"),
    ("Max entries", "Предел числа записей"),
    ("Max entry size", "Предел размера записи"),
    ("Mini panel", "Мини-панель"),
    ("Minimize", "Свернуть"),
    ("Multi-select", "Мультивыбор"),
    ("No HTTP port configured; stdio mode is client-managed and cannot be probed", "HTTP-порт не настроен; режим stdio управляется клиентом и не подлежит проверке"),
    ("No clipboard history yet", "История буфера обмена пуста"),
    ("No data", "Нет данных"),
    ("No matching results", "Совпадений нет"),
    ("No path", "Нет пути"),
    ("No pinned entries yet", "Закреплённых записей пока нет"),
    ("No recently changed settings", "Недавно изменённых настроек нет"),
    ("No release notes available", "Описание обновления отсутствует"),
    ("No service detected (start it with --mcp-http)", "Служба не обнаружена (запустите её с --mcp-http)"),
    ("None", "Нет"),
    ("None, frosted blur, or translucent", "Нет, матовое размытие или полупрозрачность"),
    ("Not connected", "Не подключено"),
    ("Nothing to undo", "Нечего отменять"),
    ("Notifications", "Уведомления"),
    ("Number of recent entries shown in the panel", "Сколько последних записей показывать в панели"),
    ("Numbers", "Числа"),
    ("Ocean blue", "Океанский синий"),
    ("Off", "Выкл."),
    ("Offset", "Смещение"),
    ("Olive", "Оливковый"),
    ("Opacity", "Прозрачность"),
    ("Opacity of entry cards; lower it to reveal the blurred background", "Плотность карточек; уменьшите, чтобы увидеть размытый фон"),
    ("Open", "Открыть"),
    ("Open in browser", "Открыть в браузере"),
    ("Open repo", "Открыть репозиторий"),
    ("Original size unknown", "Размер оригинала неизвестен"),
    ("Override individual theme tokens: any element, any value", "Переопределять токены темы поштучно: любой элемент, любое значение"),
    ("Own column", "Отдельная колонка"),
    ("Panel height", "Высота панели"),
    ("Paper", "Бумага"),
    ("Paper tint", "Плотность бумаги"),
    ("Paste failed:", "Ошибка вставки:"),
    ("Paste mode", "Способ вставки"),
    // 上下文：热键槽位名（不是动作说明那一句）。
    ("Paste previous", "Вставить предыдущую запись"),
    ("Paste previous entry", "Вставить предыдущую запись"),
    ("Paste the most recent entry into the focused app", "Вставить последнюю запись в приложение в фокусе"),
    ("Pin", "Закрепить"),
    ("Pin entries in the same list, or in a separate draggable column", "Закреплённые записи в том же списке или в отдельной колонке"),
    ("Pin failed:", "Ошибка закрепления:"),
    ("Pin window", "Закрепить окно"),
    ("Pinned", "Закреплено"),
    // 上下文：卡片种类（不是行操作「Pin」）。
    ("Pinned card", "Закреплённая карточка"),
    ("Pinned layout", "Раскладка закреплённых"),
    ("Polling interval", "Интервал опроса"),
    ("Port", "Порт"),
    ("Port for the MCP service; empty = disabled", "Порт службы MCP; пусто — отключено"),
    ("Preset", "Пресет"),
    ("Press key combos and function keys (shortcuts, confirm/cancel)", "Нажимать сочетания и функциональные клавиши (горячие, подтверждение/отмена)"),
    ("Preview length", "Длина превью"),
    ("Preview unavailable", "Предпросмотр недоступен"),
    ("Project", "Проект"),
    ("Project page", "Страница проекта"),
    ("Purple tint", "Плотность фиолетового"),
    ("Quit", "Выход"),
    // 上下文：设置项里的退出行为（不是托盘菜单的「退出」）。
    ("Quit app", "Выход"),
    ("Quit the app and stop recording", "Выйти и остановить запись"),
    ("Randomize layout", "Случайная раскладка"),
    ("Read UI layout", "Читать разметку UI"),
    ("Read geometry and hierarchy of UI elements", "Читать геометрию и иерархию элементов UI"),
    ("Read or write system clipboard text", "Читать или записывать текст буфера обмена"),
    ("Recently changed", "Недавно изменённые"),
    ("Record", "Запись"),
    ("Record clipboard", "Записывать буфер обмена"),
    ("Recording shortcut", "Запись сочетания клавиш"),
    ("Remove temp files left over from the previous run on startup", "Удалять временные файлы, оставшиеся с прошлого запуска"),
    ("Render Markdown", "Отрисовка Markdown"),
    ("Reset", "Сбросить"),
    ("Reset all", "Сбросить всё"),
    // 上下文：重置令牌覆盖表（不是主题默认值）。
    ("Reset all tokens", "Сбросить всё"),
    ("Resize and move any window", "Изменять размер и положение любого окна"),
    ("Resize windows", "Изменять размер окон"),
    ("Restarting…", "Перезапуск…"),
    ("Restore default", "Вернуть по умолчанию"),
    ("Restore this card to defaults", "Вернуть этой карточке значения по умолчанию"),
    // 上下文：同上。
    ("Right edge", "Справа"),
    ("Right-click with a modifier to pop up recent entries at the cursor", "Правый клик с модификатором — всплывают последние записи у курсора"),
    ("Rose", "Розовый"),
    ("Saturation", "Насыщенность"),
    ("Save", "Сохранить"),
    ("Save (Ctrl+Enter)", "Сохранить (Ctrl+Enter)"),
    ("Save failed:", "Ошибка сохранения:"),
    ("Save failed: invalid image data", "Ошибка сохранения: некорректные данные изображения"),
    ("Screen capture", "Снимок экрана"),
    ("Scroll pages and lists to a target position", "Прокручивать страницы и списки до нужной позиции"),
    ("Scroll views", "Прокручивать представления"),
    ("Search", "Поиск"),
    ("Search (expanded)", "Поиск (развёрнут)"),
    ("Search (filter active, click to view)", "Поиск (фильтр активен, нажмите для просмотра)"),
    ("Search history…", "Поиск в истории…"),
    ("Search in browser", "Поиск в браузере"),
    ("Search panel", "Панель поиска"),
    ("Search settings", "Поиск настроек"),
    ("Select a style theme first", "Сначала выберите стиль оформления"),
    ("Select an entry to see details", "Выберите запись для просмотра подробностей"),
    ("Select several types at once; the list shows entries matching any of them", "Включив, можно выбрать несколько типов; список показывает записи любого из них"),
    ("Separate layers with brighter borders on pure black", "При чистом чёрном слои разделяются яркими границами"),
    ("Separate popup", "Отдельное окно"),
    ("Service is running", "Служба работает"),
    ("Set to", "Установлено:"),
    ("Settings", "Настройки"),
    ("Shared column", "Общая колонка"),
    ("Shift + right-click", "Shift + правый клик"),
    ("Shortcuts", "Горячие клавиши"),
    ("Show / hide main window", "Показать / скрыть главное окно"),
    ("Show all types", "Показывать все типы"),
    ("Show an icon and menu in the system tray while running", "Показывать значок и меню в трее во время работы"),
    ("Show image entries as thumbnails; off = size text only", "Показывать изображения миниатюрами; выкл. — только размер"),
    ("Show in file manager", "Показать в проводнике"),
    ("Show main window", "Показать главное окно"),
    ("Show tray icon", "Показывать значок в трее"),
    ("Show type names", "Показывать названия типов"),
    ("Side length of each type button (24-48 pt)", "Сторона каждой кнопки типа (24–48 pt)"),
    ("Size", "Размер"),
    ("Size variation", "Разброс размера"),
    ("Skip clipboard content while passwords are being typed", "Не записывать буфер, когда вводится пароль"),
    ("Skip password fields", "Пропускать поля пароля"),
    ("Small", "Мелкий"),
    ("Source code, issue tracker and releases", "Исходный код, баги и релизы"),
    // 上下文：字号档位名（不是「恢复默认」按钮）。
    ("Standard", "Обычный"),
    ("Star density", "Плотность звёзд"),
    ("Starfield purple", "Звёздное поле (фиолет.)"),
    ("Stars shift opposite to the cursor (parallax)", "Звёзды сдвигаются против курсора (параллакс)"),
    ("Start hidden in the tray instead of opening the window", "Запускать свёрнутым в трей, не открывая окно"),
    ("Start minimized", "Запускать свёрнутым"),
    ("Start recording in the background at sign-in; open the window from the tray", "При входе запускать только фоновую запись; окно — из трея"),
    ("Start the HTTP service: modular-clipboard.exe --mcp-http\n", "Запустите HTTP-службу: modular-clipboard.exe --mcp-http\n"),
    ("Stop adding new entries when off", "При выключении новые записи не добавляются"),
    ("Storage", "Хранилище"),
    // 上下文：图片编辑器的笔触颜色（不是主题色）。
    ("Stroke color", "Цвет"),
    ("Style parameters", "Параметры стиля"),
    ("Style theme", "Стиль оформления"),
    ("Table view", "Табличный вид"),
    ("Teal", "Бирюзовый"),
    ("Temperature", "Температура"),
    ("Text", "Текст"),
    ("Text labels next to the icons: below them when vertical, to the right when horizontal", "Текстовые подписи у значков: при вертикали — снизу, при горизонтали — справа"),
    ("Text size; presets apply instantly", "Размер текста интерфейса; пресеты применяются сразу"),
    ("Texture strength", "Сила текстуры"),
    ("The developer's GitHub profile", "Профиль разработчика на GitHub"),
    ("Theme", "Тема"),
    ("This entry no longer exists", "Эта запись больше не существует"),
    ("Toggle the main window from anywhere", "Переключать главное окно отовсюду"),
    ("Token overrides", "Тонкая настройка токенов"),
    ("Token overrides · {n} overridden", "Тонкая настройка токенов · переопределено {n}"),
    // 上下文：类型过滤栏贴住窗口的哪条边（不是列表顶部）。
    ("Top edge", "Сверху"),
    ("Translate (DeepL web)", "Перевод (DeepL web)"),
    ("Translucent", "Полупрозрачность"),
    ("Transparency of the background layer; text stays opaque", "Прозрачность фона; текст остаётся непрозрачным"),
    ("Trigger modifier; these can stack, keep at least one", "Модификатор-триггер; можно совмещать, оставьте хотя бы один"),
    ("Turn off to disable all MCP calls", "Выключение отключает все вызовы MCP"),
    ("Turned off in settings", "Отключено в настройках"),
    ("Twinkle", "Мерцание"),
    ("Type", "Тип"),
    ("Type content…", "Введите содержимое…"),
    ("Type filter bar", "Панель фильтра типов"),
    ("Type text", "Вводить текст"),
    ("Type text and shortcuts into the focused control", "Вводить текст и горячие клавиши в элемент в фокусе"),
    ("Undo crop", "Отменить обрезку"),
    ("Undo edit", "Отменить изменение"),
    ("Undo stroke", "Отменить штрих"),
    ("Unpin", "Открепить"),
    ("Unpin window", "Снять «поверх всех окон»"),
    ("Unreachable", "Недоступен"),
    ("Up to date", "Обновлений нет"),
    ("Update", "Обновить"),
    ("Update available", "Доступно обновление"),
    ("Version", "Версия"),
    ("View", "Вид"),
    ("Violet", "Фиалковый"),
    ("Warmth", "Температура"),
    ("What's new", "Что нового"),
    ("When off, only types present in history are shown", "При выключении — только типы, присутствующие в истории"),
    ("Whether the panel docks above or below the main window", "Панель поиска сверху или снизу окна"),
    ("Which edge of the main window the bar sticks to", "К какому краю окна прилегает панель типов"),
    ("Width", "Толщина"),
    ("Win + right-click", "Win + правый клик"),
    ("X-Large", "Очень крупный"),
    ("XX-Large", "Самый крупный"),
    ("\n=== Option 1: stdio (local, recommended) ===\nFor local clients such as Claude Desktop / Cursor, add this to your MCP config:\n", "\n=== Вариант 1: stdio (локальный, рекомендуется) ===\nДля локальных клиентов вроде Claude Desktop / Cursor добавьте в конфигурацию MCP:\n"),
    ("\n=== Option 2: HTTP (remote / custom address) ===\nService URL: http://{host}:{port}/mcp\nAuth: {auth}\nConfig:\n", "\n=== Вариант 2: HTTP (удалённый / свой адрес) ===\nАдрес службы: http://{host}:{port}/mcp\nАутентификация: {auth}\nКонфигурация:\n"),
    ("bytes", "байт"),
    // 上下文：同上。
    ("days ago", "дней назад"),
    // 上下文：同上。
    ("hours ago", "часов назад"),
    ("local", "локальный"),
    // 上下文：相对时间，跟在数字后（`5 min ago`）。
    ("min ago", "минут назад"),
    ("{a} + {b} more files", "{a} и ещё {b} файлов"),
    ("{a} tools · {b} callable · all disabled when off", "{a} инструментов · {b} доступных · при выключении все недоступны"),
    ("{a}: {b} registration failed (possibly taken by another app)", "{a}: не удалось зарегистрировать {b} (возможно, занята другой программой)"),
    ("{name} is not a supported shortcut", "{name} нельзя назначить сочетанием клавиш"),
    ("{n} overridden", "Переопределено токенов: {n}"),
    ("↺ Rotate left", "↺ Повернуть влево"),
    ("↻ Rotate right", "↻ Повернуть вправо"),
    ("⇅ Flip vertical", "⇅ Отразить по вертикали"),
    ("⇋ Flip horizontal", "⇋ Отразить по горизонтали"),
    ("└ Opacity", "└ Прозрачность"),
    ("【modular-clipboard · MCP pairing】\nmodular-clipboard exposes a Model Context Protocol (MCP) service that can run UI automation on this machine.\n\nAvailable tools (10):\n", "【modular-clipboard · Сопряжение MCP】\nmodular-clipboard предоставляет службу Model Context Protocol (MCP), которая может выполнять автоматизацию интерфейса на этом компьютере.\n\nДоступные инструменты (10):\n"),
];

/// 计数短语的复数形态表：`英文复数形` → `[one, few, many, other]`。
///
/// # 为什么键是英文**复数**形
///
/// 俄语一/少/多三种形态对应英文同一个复数形（"files"），给每个形态一个
/// 键必然撞键。所以按英文复数形聚合，值是四个 CLDR 形态的数组，下标顺序
/// 与 [`super::Plural::index`] 一一对应。
///
/// 没有收录的计数短语回落 [`TABLE`]（通用形），再回落英文——见
/// [`super::tp`]。上下文注释写在各条目上方，供译者判断场景。
pub(super) static PLURALS: &[(&str, [&str; 4])] = &[
    // 上下文：相对时间，跟在数字后（`5 минут назад`）。
    ("min ago", ["минуту назад", "минуты назад", "минут назад", "минут назад"]),
    ("hours ago", ["час назад", "часа назад", "часов назад", "часов назад"]),
    ("days ago", ["день назад", "дня назад", "дней назад", "дней назад"]),
    // 上下文：多文件条目的预览摘要；`{b}` 是文件总数，单文件走另一分支
    // 所以 one 形态实际到不了，列出来只为四形态齐全。
    (
        "{a} + {b} more files",
        [
            "{a} и ещё {b} файл",
            "{a} и ещё {b} файла",
            "{a} и ещё {b} файлов",
            "{a} и ещё {b} файлов",
        ],
    ),
    // 上下文：类型过滤栏的条目计数后缀（`Текст · 5 записей`）。
    (" entries", [" запись", " записи", " записей", " записей"]),
    // 上下文：同上，多选模式的提示后缀。
    (
        " entries (multi-select)",
        [
            " запись (мультивыбор)",
            " записи (мультивыбор)",
            " записей (мультивыбор)",
            " записей (мультивыбор)",
        ],
    ),
    // 上下文：迷你面板显示条数上限（1~10）的单位后缀。
    (" items", [" запись", " записи", " записей", " записей"]),
    // 上下文：预览字符数上限的单位后缀。
    (" chars", [" символ", " символа", " символов", " символов"]),
];

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

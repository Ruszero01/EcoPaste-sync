use clipboard_rs::{
    common::RustImage, Clipboard, ClipboardContent, ClipboardContext, ClipboardHandler,
    ClipboardWatcher, ClipboardWatcherContext, ContentFormat, RustImageData, WatcherShutdown,
};
use std::{
    fs::create_dir_all,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::spawn,
};
use tauri::{command, AppHandle, Emitter, Manager, Runtime, State};

mod audio;
mod utils;
pub use audio::play_copy_audio;
pub use utils::{save_clipboard_image, schedule_ocr_task};

use tauri_plugin_eco_common::id::generate_id;

// 引入 database 插件
use tauri_plugin_eco_database::{DatabaseState, InsertItem};

// 引入 detector 插件
use tauri_plugin_eco_detector::DetectorState;

pub struct ClipboardManager {
    context: Arc<Mutex<ClipboardContext>>,
    watcher_shutdown: Arc<Mutex<Option<WatcherShutdown>>>,
    /// 记录最后一次写入剪贴板的时间和内容指纹（用于检测自身写入）
    last_write: Arc<Mutex<(u64, String)>>,
}

impl ClipboardManager {
    pub fn new() -> Self {
        ClipboardManager {
            context: Arc::new(Mutex::new(ClipboardContext::new().unwrap())),
            watcher_shutdown: Arc::default(),
            last_write: Arc::default(),
        }
    }

    fn has(&self, format: ContentFormat) -> bool {
        self.context.lock().unwrap().has(format)
    }

    /// 记录剪贴板写入指纹
    pub fn record_write_fingerprint(&self, fingerprint: String) {
        let mut guard = self.last_write.lock().unwrap();
        *guard = (chrono::Utc::now().timestamp_millis() as u64, fingerprint);
    }

    /// 检查是否是自身写入的剪贴板（时间窗口 + 内容指纹检测）
    /// 检测文本、图片、文件等多种格式
    pub fn is_own_write(
        &self,
        current_text: &str,
        has_image: bool,
        has_files: bool,
        current_files: &[String],
    ) -> bool {
        let guard = self.last_write.lock().unwrap();
        let (write_time, ref fingerprint) = *guard;
        let now = chrono::Utc::now().timestamp_millis() as u64;

        // 时间窗口检查：500ms 内认为可能是自身写入
        if now.saturating_sub(write_time) > 500 {
            return false;
        }

        // 图片检测：500ms 时间窗口内检测到图片内容变化，视为自身写入
        if has_image && fingerprint.starts_with("image:") {
            return true;
        }

        // 内容指纹检查（文本和文件）
        if let Some(fingerprint_content) = fingerprint.split(':').nth(1) {
            // 文本匹配
            if !current_text.is_empty()
                && (current_text.contains(fingerprint_content)
                    || fingerprint_content.starts_with(current_text))
            {
                return true;
            }

            // 文件匹配（指纹格式: "files:/path1|/path2"）
            if has_files {
                for file in current_files {
                    if fingerprint_content == file || file.starts_with(fingerprint_content) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

impl ClipboardManager {
    /// 写入纯文本并记录指纹
    /// 使用 set() 并只传入 Text 以清除其他格式（如 HTML/RTF）
    pub fn write_text(&self, value: String) -> Result<(), String> {
        let fingerprint = format!("text:{}", value);
        self.record_write_fingerprint(fingerprint);

        // 使用 set() 只传入 Text 格式，确保清除其他格式
        let contents = vec![clipboard_rs::ClipboardContent::Text(value)];
        self.context
            .lock()
            .map_err(|e| e.to_string())?
            .set(contents)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 写入 HTML 格式并记录指纹
    pub fn write_html(&self, text: String, html: String) -> Result<(), String> {
        let fingerprint = format!("html:{}", text);
        self.record_write_fingerprint(fingerprint);

        // HTML 格式在前，纯文本作为 fallback
        let contents = vec![clipboard_rs::ClipboardContent::Html(html), clipboard_rs::ClipboardContent::Text(text)];

        self.context
            .lock()
            .map_err(|e| e.to_string())?
            .set(contents)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 写入 RTF 格式并记录指纹
    pub fn write_rtf(&self, text: String, rtf: String) -> Result<(), String> {
        let fingerprint = format!("rtf:{}", text);
        self.record_write_fingerprint(fingerprint);

        // RTF 格式在前，纯文本作为 fallback
        let mut contents = vec![clipboard_rs::ClipboardContent::Rtf(rtf)];

        if cfg!(not(target_os = "macos")) {
            contents.push(clipboard_rs::ClipboardContent::Text(text))
        }

        self.context
            .lock()
            .map_err(|e| e.to_string())?
            .set(contents)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 写入图片并记录指纹
    pub fn write_image(&self, path: String) -> Result<(), String> {
        let fingerprint = format!("image:{}", path);
        self.record_write_fingerprint(fingerprint);

        let image = clipboard_rs::RustImageData::from_path(&path).map_err(|e| e.to_string())?;

        self.context
            .lock()
            .map_err(|e| e.to_string())?
            .set_image(image)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct ClipboardListen<R>
where
    R: Runtime,
{
    app_handle: AppHandle<R>,
}

impl<R> ClipboardListen<R>
where
    R: Runtime,
{
    fn new(app_handle: AppHandle<R>) -> Self {
        Self { app_handle }
    }
}

impl<R> ClipboardHandler for ClipboardListen<R>
where
    R: Runtime,
{
    fn on_clipboard_change(&mut self) {
        let app_handle = self.app_handle.clone();

        // 获取 manager 状态
        let manager = app_handle.state::<ClipboardManager>();

        // 读取当前剪贴板内容（用于指纹检测）
        let (current_text, has_image, has_files, current_files) = {
            let context = manager.context.lock().unwrap();
            let text = context.get_text().unwrap_or_default();
            let has_img = context.has(ContentFormat::Image);
            let has_file = context.has(ContentFormat::Files);
            let files = context.get_files().unwrap_or_default();
            (text, has_img, has_file, files)
        };

        // 检查是否是自身写入的剪贴板（时间窗口 + 内容指纹检测）
        if manager.is_own_write(&current_text, has_image, has_files, &current_files) {
            return;
        }

        // 获取数据库状态
        let db_state = match app_handle.try_state::<DatabaseState>() {
            Some(db) => db,
            None => return,
        };

        // 获取 detector 状态（可能不存在，检测是可选的）
        let detector_state = app_handle.try_state::<DetectorState>();

        // 获取 manager 状态
        let manager = app_handle.state::<ClipboardManager>();

        // 读取剪贴板内容
        // count 字段语义：
        // - 文本/代码：字符数
        // - 图片/文件：文件大小（字节数）
        let (item_type, group, value, search, count, subtype, img_width, img_height) = {
            let context = manager.context.lock().unwrap();

            // 检查是否开启"复制为纯文本"模式
            let copy_plain = tauri_plugin_eco_database::config::should_copy_plain(&app_handle);

            // 检查是否是图片类型
            let has_image = context.has(ContentFormat::Image);
            let has_files = context.has(ContentFormat::Files);

            // 类型判断规则：
            // - has_image=true → 优先使用截图数据保存为 image 类型
            // - has_files=true, count=1, !has_image → 1张图片文件 → 保存为 image 类型
            // - has_files=true, count>1 或 has_image=true → 多张图片或混合 → 保存为 files 类型

            // 获取文件列表（需要同时获取用于判断）
            let files = context.get_files().unwrap_or_default();

            if has_image {
                // 有截图数据 → 优先保存为 image 类型
                match context.get_image() {
                    Ok(image) => match save_clipboard_image(&app_handle, Some(&image), None) {
                        Ok((image_path, file_size, width, height)) => {
                            let item_id = generate_id();
                            let image_path_str = image_path.to_string_lossy().to_string();

                            // 安排OCR任务（如果OCR功能开启）
                            schedule_ocr_task(&app_handle, &image_path, &item_id);

                            (
                                "image".to_string(),
                                "image".to_string(),
                                Some(image_path_str),
                                None, // search 留空，由OCR完成后更新
                                Some(file_size),
                                Some("image".to_string()),
                                Some(width as i32),
                                Some(height as i32),
                            )
                        }
                        Err(_) => {
                            // 如果截图保存失败，检查是否有文件兜底
                            if has_files && files.len() == 1 {
                                if let Some(first_file) = files.first() {
                                    match save_clipboard_image(&app_handle, None, Some(first_file)) {
                                        Ok((image_path, file_size, width, height)) => {
                                            let item_id = generate_id();
                                            let image_path_str = image_path.to_string_lossy().to_string();
                                            schedule_ocr_task(&app_handle, &image_path, &item_id);
                                            (
                                                "image".to_string(),
                                                "image".to_string(),
                                                Some(image_path_str),
                                                None,
                                                Some(file_size),
                                                Some("image".to_string()),
                                                Some(width as i32),
                                                Some(height as i32),
                                            )
                                        }
                                        Err(_) => (
                                            "files".to_string(),
                                            "files".to_string(),
                                            serde_json::to_string(&files).ok(),
                                            Some(files.join(" ")),
                                            Some(0),
                                            None,
                                            None,
                                            None,
                                        ),
                                    }
                                } else {
                                    (
                                        "files".to_string(),
                                        "files".to_string(),
                                        None,
                                        Some(String::new()),
                                        Some(0),
                                        None,
                                        None,
                                        None,
                                    )
                                }
                            } else {
                                return;
                            }
                        }
                    },
                    Err(_) => {
                        // 如果读取截图数据失败，检查是否有文件兜底
                        if has_files && files.len() == 1 {
                            if let Some(first_file) = files.first() {
                                match save_clipboard_image(&app_handle, None, Some(first_file)) {
                                    Ok((image_path, file_size, width, height)) => {
                                        let item_id = generate_id();
                                        let image_path_str = image_path.to_string_lossy().to_string();
                                        schedule_ocr_task(&app_handle, &image_path, &item_id);
                                        (
                                            "image".to_string(),
                                            "image".to_string(),
                                            Some(image_path_str),
                                            None,
                                            Some(file_size),
                                            Some("image".to_string()),
                                            Some(width as i32),
                                            Some(height as i32),
                                        )
                                    }
                                    Err(_) => (
                                        "files".to_string(),
                                        "files".to_string(),
                                        serde_json::to_string(&files).ok(),
                                        Some(files.join(" ")),
                                        Some(0),
                                        None,
                                        None,
                                        None,
                                    ),
                                }
                            } else {
                                (
                                    "files".to_string(),
                                    "files".to_string(),
                                    None,
                                    Some(String::new()),
                                    Some(0),
                                    None,
                                    None,
                                    None,
                                )
                            }
                        } else {
                            return;
                        }
                    }
                }
            } else if has_files {
                let files_count = files.len();

                if files_count == 1 {
                    // 只有1个图片文件 → 保存为 image 类型
                    if let Some(first_file) = files.first() {
                        match save_clipboard_image(&app_handle, None, Some(first_file)) {
                            Ok((image_path, file_size, width, height)) => {
                                let item_id = generate_id();
                                let image_path_str = image_path.to_string_lossy().to_string();
                                schedule_ocr_task(&app_handle, &image_path, &item_id);
                                (
                                    "image".to_string(),
                                    "image".to_string(),
                                    Some(image_path_str),
                                    None,
                                    Some(file_size),
                                    Some("image".to_string()),
                                    Some(width as i32),
                                    Some(height as i32),
                                )
                            }
                            Err(_) => (
                                "files".to_string(),
                                "files".to_string(),
                                serde_json::to_string(&files).ok(),
                                Some(files.join(" ")),
                                Some(0),
                                None,
                                None,
                                None,
                            ),
                        }
                    } else {
                        (
                            "files".to_string(),
                            "files".to_string(),
                            None,
                            Some(String::new()),
                            Some(0),
                            None,
                            None,
                            None,
                        )
                    }
                } else {
                    // 多张文件 → 保存为 files 类型
                    let count = files
                        .first()
                        .and_then(|f| std::fs::metadata(f).ok().map(|m| m.len() as i32))
                        .unwrap_or(1);

                    (
                        "files".to_string(),
                        "files".to_string(),
                        serde_json::to_string(&files).ok(),
                        Some(files.join(" ")),
                        Some(count),
                        None,
                        None,
                        None,
                    )
                }
            } else if !copy_plain
                && !context.has(ContentFormat::Html)
                && context.has(ContentFormat::Rtf)
            {
                // 复制为纯文本关闭时，检测 RTF 格式
                match context.get_rich_text() {
                    Ok(rtf) => {
                        let text = context.get_text().ok();
                        (
                            "formatted".to_string(),
                            "text".to_string(), // group = "text" 表示文本分组
                            Some(rtf.clone()),
                            text.clone(),
                            text.as_ref().map(|s| s.len() as i32),
                            Some("rtf".to_string()),
                            None,
                            None,
                        )
                    }
                    Err(_) => return,
                }
            } else if !copy_plain && context.has(ContentFormat::Html) {
                // 复制为纯文本关闭时，检测 HTML 格式
                match context.get_html() {
                    Ok(html) => {
                        let text = context.get_text().ok();
                        (
                            "formatted".to_string(),
                            "text".to_string(), // group = "text" 表示文本分组
                            Some(html),
                            text.clone(),
                            text.as_ref().map(|s| s.len() as i32),
                            Some("html".to_string()),
                            None,
                            None,
                        )
                    }
                    Err(_) => return,
                }
            } else {
                // 其他情况（包括复制为纯文本开启时）读取纯文本
                match context.get_text() {
                    Ok(text) => {
                        // 过滤掉纯换行符和空白字符（批量粘贴时换行操作会产生这些内容）
                        if text.trim().is_empty() {
                            return;
                        }
                        let text_len = text.len() as i32;
                        (
                            "text".to_string(),
                            "text".to_string(),
                            Some(text.clone()),
                            Some(text),
                            Some(text_len),
                            None, // 纯文本的 subtype 为 None
                            None,
                            None,
                        )
                    }
                    Err(_) => return,
                }
            }
        };

        // 如果是文本类型，调用 detector 进行检测
        let (final_type, final_subtype, color_normalized_for_search) = if item_type == "text" {
            if let Some(detector_state) = detector_state {
                let detector = detector_state.inner();
                let result = detector.detect_content(
                    value.clone().unwrap_or_default(),
                    item_type.clone(),
                    tauri_plugin_eco_detector::DetectionOptions {
                        detect_url: true,
                        detect_email: true,
                        detect_path: true,
                        detect_color: true,
                        detect_code: true,
                        detect_markdown: true,
                        code_min_length: 10,
                    },
                );

                match result {
                    Ok(detection) => {
                        log::trace!("[Clipboard] 类型检测结果: value={}, subtype={:?}, is_code={}, is_markdown={}",
                            &value.clone().unwrap_or_default().chars().take(50).collect::<String>(),
                            detection.subtype,
                            detection.is_code,
                            detection.is_markdown);

                        // Markdown 合并到 formatted 类型（与 HTML/RTF 类似）
                        // Markdown 是格式文本，通过 type='formatted', subtype='markdown' 标识
                        if detection.is_markdown {
                            ("formatted".to_string(), Some("markdown".to_string()), None)
                        } else if detection.code_language.as_deref() == Some("markdown") {
                            // 也支持通过代码检测返回 markdown 语言的方式
                            ("formatted".to_string(), Some("markdown".to_string()), None)
                        } else if detection.is_code {
                            // 代码类型：type = "code", subtype = 语言
                            ("code".to_string(), detection.code_language, None)
                        } else if let Some(s) = detection.subtype {
                            // 其他子类型（url/email/path/color）
                            // 如果是颜色类型，使用 color_normalized 作为 search 字段用于去重
                            let color_search = if s == "color" {
                                detection.color_normalized
                            } else {
                                None
                            };
                            (item_type.clone(), Some(s), color_search)
                        } else {
                            // 纯文本
                            (item_type.clone(), None, None)
                        }
                    }
                    Err(e) => {
                        log::warn!("类型检测失败: {}", e);
                        (item_type.clone(), None, None)
                    }
                }
            } else {
                // Detector 插件未初始化，跳过检测
                log::trace!("[Clipboard] Detector 插件未初始化，跳过类型检测");
                (item_type.clone(), None, None)
            }
        } else {
            (item_type.clone(), subtype.clone(), None)
        };

        // 构建 InsertItem
        let time = chrono::Utc::now().timestamp_millis();
        let final_subtype_value = final_subtype.or(subtype);

        // 颜色类型使用标准化的 RGB 向量作为 search 字段用于去重
        let final_search = if final_subtype_value.as_deref() == Some("color") {
            color_normalized_for_search.or(search)
        } else {
            search
        };

        log::debug!(
            "[Clipboard] Insert item: type={}, subtype={}, group={}",
            final_type,
            final_subtype_value.as_deref().unwrap_or("null"),
            group
        );

        let item = InsertItem {
            id: generate_id(),
            item_type: Some(final_type),
            group: Some(group),
            value,
            search: final_search,
            count,
            width: img_width,
            height: img_height,
            favorite: 0,
            time,
            note: None,
            subtype: final_subtype_value,
            deleted: Some(0),
            sync_status: Some("not_synced".to_string()),
            // 注意：code_language 和 is_code 已移除，代码类型通过 type='code' 标识
            source_app_name: None,
            source_app_icon: None,
            position: None,
        };

        // 同步插入数据库
        let db = db_state.blocking_lock();
        match db.insert_with_deduplication(&item, &app_handle) {
            Ok(result) => {
                // 播放复制音效（反馈用户复制操作已完成）
                play_copy_audio(&app_handle);

                // 发送事件通知前端，携带重复数据的 ID（如果是更新操作）
                let payload = if result.is_update {
                    serde_json::json!({ "duplicate_id": result.insert_id })
                } else {
                    serde_json::json!({ "duplicate_id": null })
                };
                let _ = app_handle
                    .emit("plugin:eco-clipboard://database_updated", payload)
                    .map_err(|err| err.to_string());
            }
            Err(e) => {
                log::error!("插入剪贴板数据到数据库失败: {}", e);
            }
        }
    }
}

/// 切换剪贴板监听状态
pub fn toggle_listen<R: Runtime>(app_handle: &AppHandle<R>) {
    let manager = match app_handle.try_state::<ClipboardManager>() {
        Some(m) => m,
        None => return,
    };

    let mut watcher_shutdown = manager.watcher_shutdown.lock().unwrap();

    if (*watcher_shutdown).is_some() {
        // 当前正在监听，停止
        if let Some(shutdown) = (*watcher_shutdown).take() {
            shutdown.stop();
        }
        *watcher_shutdown = None;
    } else {
        // 当前未监听，开始监听
        let listener = ClipboardListen::new(app_handle.clone());

        let mut watcher: ClipboardWatcherContext<ClipboardListen<R>> =
            match ClipboardWatcherContext::new() {
                Ok(w) => w,
                Err(e) => {
                    log::error!("[Clipboard] 创建监听器失败: {}", e);
                    return;
                }
            };

        let watcher_shutdown_chan = watcher.add_handler(listener).get_shutdown_channel();
        *watcher_shutdown = Some(watcher_shutdown_chan);

        spawn(move || {
            watcher.start_watch();
        });
    }
}

/// 内部启动监听函数（供 setup 使用，不依赖 State）
pub fn start_listen_inner<R: Runtime>(app_handle: &AppHandle<R>) -> Result<(), String> {
    let manager = match app_handle.try_state::<ClipboardManager>() {
        Some(m) => m,
        None => return Err("ClipboardManager 未初始化".to_string()),
    };

    let mut watcher_shutdown_state = manager.watcher_shutdown.lock().unwrap();

    if (*watcher_shutdown_state).is_some() {
        return Ok(());
    }

    let listener = ClipboardListen::new(app_handle.clone());

    let mut watcher: ClipboardWatcherContext<ClipboardListen<R>> =
        ClipboardWatcherContext::new().map_err(|e| format!("创建监听器失败: {}", e))?;

    let watcher_shutdown = watcher.add_handler(listener).get_shutdown_channel();

    *watcher_shutdown_state = Some(watcher_shutdown);

    spawn(move || {
        watcher.start_watch();
    });

    Ok(())
}

/// 检查剪贴板监听是否已开启
pub fn is_listen_enabled<R: Runtime>(app_handle: &AppHandle<R>) -> bool {
    let manager = match app_handle.try_state::<ClipboardManager>() {
        Some(m) => m,
        None => return false,
    };

    let watcher_shutdown = manager.watcher_shutdown.lock().unwrap();
    watcher_shutdown.is_some()
}

#[derive(Debug, serde::Serialize)]
pub struct ReadImage {
    width: u32,
    height: u32,
    image: String,
}

#[command]
pub async fn stop_listen(manager: State<'_, ClipboardManager>) -> Result<(), String> {
    let mut watcher_shutdown = manager.watcher_shutdown.lock().unwrap();

    if let Some(watcher_shutdown) = (*watcher_shutdown).take() {
        watcher_shutdown.stop();
    }

    *watcher_shutdown = None;

    Ok(())
}

#[command]
pub async fn has_files(manager: State<'_, ClipboardManager>) -> Result<bool, String> {
    Ok(manager.has(ContentFormat::Files))
}

#[command]
pub async fn has_image(manager: State<'_, ClipboardManager>) -> Result<bool, String> {
    Ok(manager.has(ContentFormat::Image))
}

#[command]
pub async fn has_html(manager: State<'_, ClipboardManager>) -> Result<bool, String> {
    Ok(manager.has(ContentFormat::Html))
}

#[command]
pub async fn has_rtf(manager: State<'_, ClipboardManager>) -> Result<bool, String> {
    Ok(manager.has(ContentFormat::Rtf))
}

#[command]
pub async fn has_text(manager: State<'_, ClipboardManager>) -> Result<bool, String> {
    Ok(manager.has(ContentFormat::Text))
}

#[command]
pub async fn read_files(manager: State<'_, ClipboardManager>) -> Result<Vec<String>, String> {
    let mut files = manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .get_files()
        .map_err(|err| err.to_string())?;

    files.iter_mut().for_each(|path| {
        *path = path.replace("file://", "");
    });

    Ok(files)
}

#[command]
pub async fn read_image(
    manager: State<'_, ClipboardManager>,
    path: PathBuf,
) -> Result<ReadImage, String> {
    create_dir_all(&path).map_err(|op| op.to_string())?;

    let image = manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .get_image()
        .map_err(|err| err.to_string())?;

    let (width, height) = image.get_size();

    let thumbnail_image = image
        .thumbnail(width / 10, height / 10)
        .map_err(|err| err.to_string())?;

    let bytes = thumbnail_image
        .to_png()
        .map_err(|err| err.to_string())?
        .get_bytes()
        .to_vec();

    let mut hasher = DefaultHasher::new();

    bytes.hash(&mut hasher);

    let hash = hasher.finish();

    let image_path = path.join(format!("{hash}.png"));

    if let Some(path) = image_path.to_str() {
        image.save_to_path(path).map_err(|err| err.to_string())?;

        let image = path.to_string();

        return Ok(ReadImage {
            width,
            height,
            image,
        });
    }

    Err("read_image execution error".to_string())
}

#[command]
pub async fn read_html(manager: State<'_, ClipboardManager>) -> Result<String, String> {
    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .get_html()
        .map_err(|err| err.to_string())
}

#[command]
pub async fn read_rtf(manager: State<'_, ClipboardManager>) -> Result<String, String> {
    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .get_rich_text()
        .map_err(|err| err.to_string())
}

#[command]
pub async fn read_text(manager: State<'_, ClipboardManager>) -> Result<String, String> {
    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .get_text()
        .map_err(|err| err.to_string())
}

#[command]
pub async fn write_files(
    manager: State<'_, ClipboardManager>,
    value: Vec<String>,
) -> Result<(), String> {
    let fingerprint = format!("files:{}", value.join("|"));
    manager.record_write_fingerprint(fingerprint);

    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .set_files(value)
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[command]
pub async fn write_image(
    manager: State<'_, ClipboardManager>,
    value: String,
) -> Result<(), String> {
    let fingerprint = format!("image:{}", value);
    manager.record_write_fingerprint(fingerprint);

    let image = RustImageData::from_path(&value).map_err(|err| err.to_string())?;

    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .set_image(image)
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[command]
pub async fn write_html(
    manager: State<'_, ClipboardManager>,
    text: String,
    html: String,
) -> Result<(), String> {
    let fingerprint = format!("html:{}", text);
    manager.record_write_fingerprint(fingerprint);

    // HTML 格式在前，纯文本作为 fallback
    let contents = vec![ClipboardContent::Html(html), ClipboardContent::Text(text)];

    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .set(contents)
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[command]
pub async fn write_rtf(
    manager: State<'_, ClipboardManager>,
    text: String,
    rtf: String,
) -> Result<(), String> {
    let fingerprint = format!("rtf:{}", text);
    manager.record_write_fingerprint(fingerprint);

    // RTF 格式在前，纯文本作为 fallback
    let mut contents = vec![ClipboardContent::Rtf(rtf)];

    if cfg!(not(target_os = "macos")) {
        contents.push(ClipboardContent::Text(text))
    }

    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .set(contents)
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[command]
pub async fn write_text(manager: State<'_, ClipboardManager>, value: String) -> Result<(), String> {
    let fingerprint = format!("text:{}", value);
    manager.record_write_fingerprint(fingerprint);

    manager
        .context
        .lock()
        .map_err(|err| err.to_string())?
        .set_text(value)
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[command]
pub async fn get_image_dimensions(path: String) -> Result<ReadImage, String> {
    let image = RustImageData::from_path(&path).map_err(|err| err.to_string())?;
    let (width, height) = image.get_size();

    Ok(ReadImage {
        width,
        height,
        image: path,
    })
}

/// 预览音效（供前端偏好设置页面使用）
#[command]
pub async fn preview_audio<R: Runtime>(app_handle: AppHandle<R>) {
    play_copy_audio(&app_handle);
}

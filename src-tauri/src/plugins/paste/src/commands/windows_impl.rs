use super::wait;
use enigo::{
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Settings,
};
use std::sync::Arc;
use tauri::{command, AppHandle, Manager, Runtime};
use tauri_plugin_eco_database::{DatabaseState, HistoryItem, QueryOptions};
use tokio::sync::Mutex;

use tauri_plugin_eco_common::active_window::{get_last_valid_window_info, restore_focus_to_window};

use winapi::um::winuser::{GetAsyncKeyState, VK_CONTROL, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RSHIFT, VK_RWIN};

// ==================== 锁机制 ====================

static QUICK_PASTE_LOCK: Mutex<Vec<u32>> = Mutex::const_new(Vec::new());

struct QuickPasteLockGuard(u32);

impl QuickPasteLockGuard {
    fn new(index: u32) -> Self {
        Self(index)
    }
}

impl Drop for QuickPasteLockGuard {
    fn drop(&mut self) {
        let index = self.0;
        tokio::spawn(async move {
            let mut lock = QUICK_PASTE_LOCK.lock().await;
            lock.retain(|&i| i != index);
        });
    }
}

// ==================== 剪贴板写入 ====================

/// 使用 ClipboardManager 写入剪贴板（会自动记录指纹，避免重复检测）
fn write_to_clipboard<R: Runtime>(
    app_handle: &AppHandle<R>,
    item_type: &str,
    subtype: Option<&str>,
    value: &str,
    search: &str,
    plain: bool,
) -> Result<(), String> {
    // 获取 ClipboardManager
    let manager = tauri_plugin_eco_clipboard::get_clipboard_manager(app_handle);
    let manager = manager.inner();

    // 格式文本（formatted/html/rtf）：始终用 search 作为纯文本 fallback
    // 其他类型：plain=true 时用 search，否则用 value
    let text = match item_type {
        "formatted" | "html" | "rtf" => {
            // 格式文本的纯文本 fallback 应该是渲染后的文本（search），而不是原始格式内容（value）
            if search.is_empty() { value } else { search }
        }
        _ => {
            // 其他类型根据 plain 标志决定
            if plain {
                if search.is_empty() { value } else { search }
            } else {
                value
            }
        }
    };

    match item_type {
        "image" => {
            if value.is_empty() {
                return Ok(());
            }
            manager.write_image(value.to_string())
        }
        "formatted" => {
            match subtype {
                Some("rtf") => {
                    // RTF 写入
                    manager.write_rtf(text.to_string(), value.to_string())
                }
                _ => {
                    // HTML 写入（默认）
                    if !value.is_empty() {
                        manager.write_html(text.to_string(), value.to_string())
                    } else {
                        manager.write_text(text.to_string())
                    }
                }
            }
        }
        "html" => {
            // 保持旧兼容，直接写入 HTML
            if !value.is_empty() {
                manager.write_html(text.to_string(), value.to_string())
            } else {
                manager.write_text(text.to_string())
            }
        }
        "rtf" => {
            // 保持旧兼容，直接写入 RTF
            manager.write_rtf(text.to_string(), value.to_string())
        }
        _ => {
            // 纯文本写入
            manager.write_text(text.to_string())
        }
    }
}

// ==================== 内容大小计算 ====================

/// 计算内容大小（字节）
/// - 文本：字符长度（UTF-8 编码）
/// - 图片/文件：文件大小
fn get_content_size(item_type: &str, value: &str) -> usize {
    match item_type {
        "image" | "files" => {
            // 对于文件路径，尝试获取文件大小
            if !value.is_empty() {
                std::fs::metadata(value)
                    .map(|meta| meta.len() as usize)
                    .unwrap_or_else(|_| value.len())
            } else {
                value.len()
            }
        }
        _ => value.len(), // 文本类型使用字符串长度
    }
}

// ==================== 动态延迟计算 ====================

/// 根据内容类型和大小计算写入后等待时间（毫秒）
fn get_write_delay_ms(item_type: &str, size_bytes: usize) -> u64 {
    match item_type {
        "text" | "code" | "color" | "markdown" | "link" | "path" => {
            if size_bytes < 100 {
                15 // 短文本快速就绪
            } else if size_bytes < 1000 {
                25 // 中等文本
            } else {
                40 // 长文本
            }
        }
        "image" => {
            if size_bytes < 1_000_000 {
                40 // 小图片 < 1MB
            } else if size_bytes < 5_000_000 {
                75 // 中等图片 1-5MB
            } else {
                150 // 大图片 > 5MB
            }
        }
        "formatted" | "html" | "rtf" => {
            if size_bytes < 1000 {
                25
            } else if size_bytes < 5000 {
                35
            } else {
                50
            }
        }
        "files" => 40,
        _ => 30,
    }
}

/// 根据内容类型和大小计算粘贴后等待时间（毫秒）
fn get_paste_delay_ms(item_type: &str, size_bytes: usize) -> u64 {
    match item_type {
        "text" | "code" | "color" | "markdown" | "link" | "path" => {
            if size_bytes < 100 {
                20
            } else if size_bytes < 1000 {
                25
            } else {
                30
            }
        }
        "image" => {
            if size_bytes < 1_000_000 {
                40 // 小图片
            } else if size_bytes < 5_000_000 {
                50 // 中等图片
            } else {
                60 // 大图片
            }
        }
        "formatted" | "html" | "rtf" => 40,
        "files" => 40,
        _ => 30,
    }
}

// ==================== 修饰键处理 ====================

fn release_all_modifier_keys(enigo: &mut Enigo) {
    let modifier_keys = [
        (VK_LSHIFT, Key::Shift),
        (VK_RSHIFT, Key::Shift),
        (VK_CONTROL, Key::Control),
        (VK_MENU, Key::Alt),
        (VK_LWIN, Key::LWin),
        (VK_RWIN, Key::RWin),
    ];

    for (vk_code, enigo_key) in modifier_keys.iter() {
        let state = unsafe { GetAsyncKeyState(*vk_code) };
        if (state & 0x8000u16 as i16) != 0 {
            let _ = enigo.key(*enigo_key, Release);
        }
    }
}

// ==================== 粘贴命令 ====================

#[command]
pub async fn paste() {
    let mut enigo = Enigo::new(&Settings::default()).unwrap();

    release_all_modifier_keys(&mut enigo);

    enigo.key(Key::LShift, Press).unwrap();
    enigo.key(Key::Insert, Click).unwrap();
    enigo.key(Key::LShift, Release).unwrap();

    wait(5);
}

#[command]
pub async fn paste_with_focus() {
    // 使用后端缓存的窗口信息（用户最后操作的窗口）
    if let Some(info) = get_last_valid_window_info() {
        let _ = restore_focus_to_window(info.hwnd);
    }

    // 等待焦点切换完成
    wait(50);

    // 创建新的 Enigo 实例，避免旧连接失效
    let mut enigo = Enigo::new(&Settings::default()).unwrap();
    release_all_modifier_keys(&mut enigo);
    wait(20);

    enigo.key(Key::LShift, Press).unwrap();
    wait(20);
    enigo.key(Key::Insert, Click).unwrap();
    wait(20);
    enigo.key(Key::LShift, Release).unwrap();

    wait(20);
}

#[command]
pub async fn quick_paste<R: Runtime>(app_handle: AppHandle<R>, index: u32) -> Result<(), String> {
    let mut lock = QUICK_PASTE_LOCK.lock().await;
    if lock.contains(&index) {
        return Ok(());
    }
    lock.push(index);
    drop(lock);

    let _guard = QuickPasteLockGuard::new(index);

    let db_state = match app_handle.try_state::<DatabaseState>() {
        Some(state) => state,
        None => {
            return Err("数据库插件未初始化".to_string());
        }
    };

    let db_state_arc = Arc::clone(&db_state);
    let query_index = index;
    let items: Vec<HistoryItem> = tokio::task::spawn_blocking(move || {
        let db = db_state_arc.blocking_lock();
        let options = QueryOptions {
            only_favorites: false,
            exclude_deleted: true,
            limit: Some(1),
            offset: Some((query_index - 1) as i32),
            order_by: Some("time DESC".to_string()),
            where_clause: None,
            params: None,
        };
        db.query_history(options)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("查询数据库失败: {}", e))?;

    if items.is_empty() {
        return Ok(());
    }

    let item = &items[0];
    let item_type = item.item_type.as_deref().unwrap_or("text");
    let subtype = item.subtype.as_deref();
    let value = item.value.clone().unwrap_or_default();
    let search = item.search.clone().unwrap_or_default();

    let _id = item.id.clone();
    let write_result = write_to_clipboard(&app_handle, item_type, subtype, &value, &search, false);

    match write_result {
        Ok(_) => {
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            paste().await;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ==================== 批量粘贴命令 ====================

#[command]
pub async fn batch_paste<R: Runtime>(
    app_handle: AppHandle<R>,
    ids: Vec<String>,
    plain: bool,
    skip_first: Option<bool>,
    prepend_newline: Option<bool>,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }

    let db_state = match app_handle.try_state::<DatabaseState>() {
        Some(state) => state,
        None => {
            return Err("数据库插件未初始化".to_string());
        }
    };

    let db_state_arc = Arc::clone(&db_state);

    // 查询所有指定 ID 的项目
    let items: Vec<HistoryItem> = tokio::task::spawn_blocking(move || {
        let db = db_state_arc.blocking_lock();
        let mut all_items = Vec::new();

        for id in &ids {
            let options = QueryOptions {
                only_favorites: false,
                exclude_deleted: true,
                limit: Some(1),
                offset: None,
                order_by: None,
                where_clause: Some("id = ?".to_string()),
                params: Some(vec![id.clone()]),
            };

            if let Ok(mut items) = db.query_history(options) {
                if let Some(item) = items.pop() {
                    all_items.push(item);
                }
            }
        }

        all_items
    })
    .await
    .map_err(|e| e.to_string())?;

    if items.is_empty() {
        return Ok(());
    }

    // 确定实际要粘贴的项目列表
    let skip_first = skip_first.unwrap_or(false);
    let start_index = if skip_first { 1 } else { 0 };
    let items_to_paste: Vec<&HistoryItem> = items.iter().skip(start_index).collect();

    // 如果 prepend_newline 为 true，先写入换行并粘贴
    if prepend_newline.unwrap_or(false) {
        let write_result = write_to_clipboard(&app_handle, "text", None, "\n", "\n", true);
        match write_result {
            Ok(_) => {
                let delay = get_write_delay_ms("text", 1);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                paste_with_focus().await;
                let paste_delay = get_paste_delay_ms("text", 1);
                tokio::time::sleep(std::time::Duration::from_millis(paste_delay)).await;
            }
            Err(e) => {
                log::error!("[Paste] 写入换行失败: {}", e);
            }
        }
    }

    // 逐个粘贴
    for (i, item) in items_to_paste.iter().enumerate() {
        let item_type = item.item_type.as_deref().unwrap_or("text");
        let subtype = item.subtype.as_deref();
        let value = item.value.clone().unwrap_or_default();
        let search = item.search.clone().unwrap_or_default();

        let content_size = if plain { search.len() } else { get_content_size(item_type, &value) };
        let write_delay = get_write_delay_ms(item_type, content_size);
        let paste_delay = get_paste_delay_ms(item_type, content_size);

        // 写入剪贴板
        let write_result = if plain {
            write_to_clipboard(&app_handle, "text", None, &search, &search, true)
        } else {
            write_to_clipboard(&app_handle, item_type, subtype, &value, &search, false)
        };

        match write_result {
            Ok(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(write_delay)).await;
                paste_with_focus().await;
                tokio::time::sleep(std::time::Duration::from_millis(paste_delay)).await;

                // 如果不是最后一项，模拟 Enter 键换行
                if i < items_to_paste.len() - 1 {
                    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
                    enigo.key(Key::Return, Click).map_err(|e| e.to_string())?;

                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    let newline_delay = get_write_delay_ms("text", 1);
                    tokio::time::sleep(std::time::Duration::from_millis(newline_delay)).await;
                }
            }
            Err(e) => {
                log::error!("[Paste] 批量粘贴失败: {}", e);
            }
        }
    }

    Ok(())
}

// ==================== 单个粘贴命令 ====================

#[command]
pub async fn single_paste<R: Runtime>(
    app_handle: AppHandle<R>,
    id: String,
    plain: bool,
) -> Result<(), String> {
    let db_state = match app_handle.try_state::<DatabaseState>() {
        Some(state) => state,
        None => {
            return Err("数据库插件未初始化".to_string());
        }
    };

    let db_state_arc = Arc::clone(&db_state);

    // 查询指定 ID 的项目
    let item: Option<HistoryItem> = tokio::task::spawn_blocking(move || {
        let db = db_state_arc.blocking_lock();
        let options = QueryOptions {
            only_favorites: false,
            exclude_deleted: true,
            limit: Some(1),
            offset: None,
            order_by: None,
            where_clause: Some("id = ?".to_string()),
            params: Some(vec![id]),
        };

        let mut items = db.query_history(options)?;
        Ok::<Option<HistoryItem>, String>(items.pop())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("查询数据库失败: {}", e))?;

    let item = match item {
        Some(item) => item,
        None => return Ok(()),
    };

    let item_type = item.item_type.as_deref().unwrap_or("text");
    let subtype = item.subtype.as_deref();
    let value = item.value.clone().unwrap_or_default();
    let search = item.search.clone().unwrap_or_default();

    // 判断是否应该使用纯文本模式
    let use_plain = plain ||
        (item_type == "formatted" && tauri_plugin_eco_database::config::should_paste_plain(&app_handle));

    let write_result = if use_plain {
        write_to_clipboard(&app_handle, "text", None, &search, &search, true)
    } else {
        write_to_clipboard(&app_handle, item_type, subtype, &value, &search, false)
    };

    match write_result {
        Ok(_) => {
            let content_size = get_content_size(item_type, &value);
            let write_delay = get_write_delay_ms(item_type, content_size);
            tokio::time::sleep(std::time::Duration::from_millis(write_delay)).await;

            // 使用后端缓存的窗口信息
            paste_with_focus().await;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ==================== 颜色粘贴命令 ====================

#[command]
pub async fn paste_color<R: Runtime>(
    app_handle: AppHandle<R>,
    color_value: String,
) -> Result<(), String> {
    // 写入纯文本到剪贴板
    let write_result = write_to_clipboard(&app_handle, "text", None, &color_value, &color_value, true);

    match write_result {
        Ok(_) => {
            let delay = get_write_delay_ms("text", color_value.len());
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            paste_with_focus().await;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

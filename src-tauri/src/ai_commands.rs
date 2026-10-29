//! AI 设置相关的 IPC 命令：环境探测、模型列表、保存设置。
//!
//! `ai_status` 永不报错：pi 没装 / 版本太低都通过 `problem` 字段交给前端展示，
//! 前端据此决定是否禁用「开始会话」而不是弹错误框。

use crate::ai_settings::{self, AiModel, AiSettings};
use serde::Serialize;
use tauri::Manager;

/// 设置页需要的环境快照。
#[derive(Serialize)]
pub struct AiEnv {
    /// 解析出的 pi 绝对路径；解析失败时为 None。
    pub pi_path: Option<String>,
    pub pi_version: Option<String>,
    /// `resolve_pi` 的原始错误文本（前端直接展示）。
    pub problem: Option<String>,
    pub settings: AiSettings,
    /// 正在运行的 AI 会话进程数（来自 `AiManager` 的真实计数）。
    pub live_processes: usize,
}

/// 读当前设置与 pi 环境。任何情况下都返回成功。
#[tauri::command]
pub fn ai_status<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> AiEnv {
    let settings = ai_settings::load();
    let live = app.try_state::<crate::ai_session::AiManager>().map(|m| m.live()).unwrap_or(0);
    match ai_settings::resolve_pi(&settings) {
        Ok(bin) => AiEnv {
            pi_path: Some(bin.program.display().to_string()),
            pi_version: Some(bin.version),
            problem: None,
            settings,
            live_processes: live,
        },
        Err(problem) => AiEnv {
            pi_path: None,
            pi_version: None,
            problem: Some(problem),
            settings,
            live_processes: live,
        },
    }
}

/// 按当前设置解析 pi 并读模型清单。`refresh` 跳过 5 分钟缓存。
#[tauri::command]
pub fn ai_models(refresh: bool) -> Result<Vec<AiModel>, String> {
    let settings = ai_settings::load();
    let bin = ai_settings::resolve_pi(&settings)?;
    ai_settings::list_models(&bin, refresh)
}

/// 校验并保存设置；写盘后 `resolve_pi` 的缓存会失效。
#[tauri::command]
pub fn ai_save_settings(settings: AiSettings) -> Result<(), String> {
    ai_settings::save(&settings)
}

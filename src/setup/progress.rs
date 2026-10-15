//! 启动画面数据与纯输出解析状态机，不执行命令、网络或文件变更。

use chrono::Local;
use rust_i18n::t;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

/// 启动画面的单次进度更新载荷。
///
/// 由 setup 各阶段构造，推送到 `alas-splash://` 页面的
/// `window.__ALAS_SPLASH_UPDATE()`，驱动标题、进度条与提示文案。
#[derive(Clone, Debug, Serialize)]
pub struct SplashUpdate {
    /// 副标题，展示随机小贴士或阶段说明。
    pub subtitle: String,
    /// 阶段主标题（如"更新中""安装依赖"）。
    pub title: String,
    /// 具体进度描述（当前正在执行的动作）。
    pub detail: String,
    /// 总进度百分比（0-100，超界值会被钳制）。
    pub progress: u8,
    /// uv 依赖同步的独立子进度；仅在 uv sync 阶段存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uv_progress: Option<UvProgress>,
    /// 是否为错误状态（启动画面据此切换错误配色与文案）。
    pub is_error: bool,
}

/// uv 依赖同步的子进度（进度条内嵌的独立分段）。
#[derive(Clone, Debug, Serialize)]
pub struct UvProgress {
    /// uv 阶段的进度百分比（0-100）。
    pub progress: u8,
    /// uv 阶段的描述（解析/下载/安装状态与速度估算）。
    pub detail: String,
}

impl SplashUpdate {
    /// 构造一条"进行中"的启动画面进度，副标题固定为 i18n 的连接中文案。
    pub fn loading(title: impl Into<String>, detail: impl Into<String>, progress: u8) -> Self {
        Self {
            subtitle: t!("setup.connecting").to_string(),
            title: title.into(),
            detail: detail.into(),
            progress: progress.min(100),
            uv_progress: None,
            is_error: false,
        }
    }

    /// 构造一条"失败"的启动画面进度，启动画面将切换为错误展示样式。
    pub fn error(title: impl Into<String>, detail: impl Into<String>, progress: u8) -> Self {
        Self {
            subtitle: t!("setup.connection_failed").to_string(),
            title: title.into(),
            detail: detail.into(),
            progress: progress.min(100),
            uv_progress: None,
            is_error: true,
        }
    }

    /// 覆盖副标题文案（如随机小贴士）。
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    /// 附加 uv 同步子进度（独立进度值与阶段描述）。
    pub fn with_uv_progress(mut self, progress: u8, detail: impl Into<String>) -> Self {
        self.uv_progress = Some(UvProgress {
            progress: progress.min(100),
            detail: detail.into(),
        });
        self
    }
}

/// 启动画面小贴士总数，需与 locales 中 `tips.*` 键的数量保持一致。
const TIPS_COUNT: usize = 19;

/// 随机取一条启动画面小贴士（按当前时间戳取模轮换）。
pub fn get_tip() -> String {
    let now = Local::now().timestamp() as usize;
    let idx = now % TIPS_COUNT;
    let key = format!("tips.{idx}");
    t!(&key).to_string()
}

/// 脚本执行的阶段细分，用于在总进度中区分 git 更新与依赖同步两段。
#[derive(Clone, Copy, Debug)]
pub(super) enum ScriptPhase {
    /// git 拉取仓库代码阶段。
    Git,
    /// uv 依赖同步阶段。
    Dependencies,
}

/// git 更新阶段的累计进度状态。
#[derive(Default)]
pub(super) struct GitProgressState {
    /// 当前累计进度百分比。
    progress: u8,
}

/// uv sync 输出解析状态机：跟踪解析/下载/安装各阶段并估算整体进度。
pub(super) struct UvProgressState {
    /// uv 进程启动时刻，用于计算整体耗时与速率。
    started_at: Instant,
    /// 首个包开始下载的时刻（用于估算下载速度）。
    download_started_at: Option<Instant>,
    /// 各包体积（字节），用于按量加权估算总进度。
    package_sizes: HashMap<String, u64>,
    /// 已完成下载的包名集合（去重，避免重复计数）。
    downloaded_packages: HashSet<String>,
    /// 累计已下载字节数。
    downloaded_bytes: u64,
    /// 依赖解析是否已完成。
    resolved: bool,
    /// 包准备（下载）是否已完成。
    prepared: bool,
    /// 安装是否已完成。
    installed: bool,
    /// 上一次上报的进度，用于保证进度单调不回退。
    last_progress: u8,
}

impl UvProgressState {
    /// 创建初始状态；进度从 2% 起步，0-2 预留给 uv 启动前的准备工作。
    pub(super) fn new() -> Self {
        Self {
            started_at: Instant::now(),
            download_started_at: None,
            package_sizes: HashMap::new(),
            downloaded_packages: HashSet::new(),
            downloaded_bytes: 0,
            resolved: false,
            prepared: false,
            installed: false,
            last_progress: 2,
        }
    }
}

pub(super) fn phase_display_name(phase: ScriptPhase) -> String {
    match phase {
        ScriptPhase::Git => t!("setup.code_update").to_string(),
        ScriptPhase::Dependencies => t!("setup.deps_update").to_string(),
    }
}

pub(super) fn splash_retry_update(
    phase: ScriptPhase,
    retry_count: usize,
    max_retries: usize,
    error_text: &str,
) -> SplashUpdate {
    let detail = t!(
        "setup.retry_detail",
        count = retry_count.to_string(),
        max = max_retries.to_string(),
        error = error_text
    );
    match phase {
        ScriptPhase::Git => SplashUpdate::loading(t!("setup.retrying_update"), detail, 18)
            .with_subtitle(t!("setup.syncing", tip = get_tip())),
        ScriptPhase::Dependencies => SplashUpdate::loading(t!("setup.retrying_deps"), detail, 64)
            .with_subtitle(t!("setup.syncing_deps", tip = get_tip()))
            .with_uv_progress(2, t!("setup.uv_resolving", secs = "0")),
    }
}

/// 构造依赖同步阶段的起始进度（64%，uv 子进度从 2% 开始解析）。
pub(super) fn dependency_start_update() -> SplashUpdate {
    SplashUpdate::loading(t!("setup.installing_deps"), t!("setup.uv_parsing"), 64)
        .with_subtitle(t!("setup.syncing_deps", tip = get_tip()))
        .with_uv_progress(2, t!("setup.uv_resolving", secs = "0"))
}

/// 构造依赖同步阶段"等待中"的进度（子命令暂时无输出时保持画面活跃）。
///
/// 按已等待秒数切换文案（10 秒内显示"解析中"，之后显示已耗时），
/// 进度取当前值与全局 uv 进度的较大者，保证单调。
pub(super) fn dependency_wait_update(
    elapsed_secs: u16,
    current_progress: u8,
    uv_progress: &mut UvProgressState,
) -> SplashUpdate {
    let progress = current_progress.max(dependency_global_progress(uv_progress.progress()));
    let detail = if elapsed_secs < 10 {
        t!("setup.uv_parsing").to_string()
    } else {
        t!("setup.uv_syncing", secs = elapsed_secs.to_string()).to_string()
    };

    SplashUpdate::loading(t!("setup.installing_deps"), detail, progress)
        .with_subtitle(t!("setup.syncing_deps", tip = get_tip()))
        .with_uv_progress(uv_progress.progress(), uv_progress.detail())
}

/// 构造运行时工具安装阶段（Python/工具链下载部署）的进度更新。
pub(super) fn runtime_tools_update(
    title: impl Into<String>,
    detail: impl Into<String>,
    progress: u8,
) -> SplashUpdate {
    SplashUpdate::loading(title, detail, progress)
        .with_subtitle(t!("setup.rebuilding_env", tip = get_tip()).to_string())
}

/// 构造运行时工具阶段的等待进度：按耗时在 start/end 区间内线性推进，
/// 并在 8 秒后切换为显示已等待秒数的文案。
pub(super) fn runtime_wait_update(
    title: &str,
    action: &str,
    elapsed_ticks: u16,
    start_progress: u8,
    end_progress: u8,
) -> SplashUpdate {
    let elapsed_secs = elapsed_ticks / 10;
    let progress = scale_progress(elapsed_secs.min(120) as u8, start_progress, end_progress);
    let detail = if elapsed_secs < 8 {
        t!("setup.action_wait", action = action).to_string()
    } else {
        t!(
            "setup.action_elapsed",
            action = action,
            secs = elapsed_secs.to_string()
        )
        .to_string()
    };
    runtime_tools_update(title, detail, progress)
}

/// 构造"切换到下一个 PyPI 镜像重试"的启动画面进度。
pub(super) fn pypi_index_fallback_update(next_index: &str) -> SplashUpdate {
    SplashUpdate::loading(
        t!("setup.retrying_deps"),
        format!("PyPI index: {next_index}"),
        64,
    )
    .with_subtitle(t!("setup.syncing_deps", tip = get_tip()))
    .with_uv_progress(2, t!("setup.uv_resolving", secs = "0"))
}

/// 按 [`ScriptPhase`] 分派给 git 或依赖各自的输出解析器。
pub(super) fn splash_update_for_output(
    line: &str,
    phase: ScriptPhase,
    git_progress: &mut GitProgressState,
    uv_progress: &mut UvProgressState,
) -> Option<SplashUpdate> {
    let sanitized = line.trim();
    if sanitized.is_empty() {
        return None;
    }

    match phase {
        ScriptPhase::Git => splash_update_for_git_output(sanitized, git_progress),
        ScriptPhase::Dependencies => splash_update_for_dependency_output(sanitized, uv_progress),
    }
}

/// 解析 git 更新输出行并映射为进度。
///
/// 上游脚本以 `===== 标题 =====` 分隔各阶段（SHOW DEPLOY CONFIG、
/// GIT INIT 等），按标题映射到预设进度点；其余行尝试解析
/// `git fetch --progress` 的百分比。
fn splash_update_for_git_output(line: &str, state: &mut GitProgressState) -> Option<SplashUpdate> {
    let subtitle = t!("setup.syncing", tip = get_tip()).to_string();

    if line.contains("=====") {
        let detail = line.replace('=', " ");
        let progress = git_section_progress(detail.trim()).unwrap_or(24);
        return Some(git_update_splash(
            line,
            detail.trim(),
            progress,
            state,
            subtitle,
        ));
    }

    if let Some(progress) = git_line_progress(line) {
        return Some(git_update_splash(line, line, progress, state, subtitle));
    }

    None
}

/// 组装 git 阶段的进度更新，进度经状态机钳制为单调递增。
fn git_update_splash(
    raw_line: &str,
    detail: &str,
    progress: u8,
    state: &mut GitProgressState,
    subtitle: String,
) -> SplashUpdate {
    state.progress = state.progress.max(progress);
    let display_detail = if detail.trim().is_empty() {
        raw_line
    } else {
        detail
    };
    SplashUpdate::loading(t!("setup.retrying_update"), display_detail, state.progress)
        .with_subtitle(subtitle)
}

/// 按上游脚本的阶段标题映射预设进度点（18% 起，随阶段推进）。
///
/// 标题与 `deploy/installer.py` 的输出保持一致，上游改动时需同步。
fn git_section_progress(section: &str) -> Option<u8> {
    if section.contains("SHOW DEPLOY CONFIG") {
        Some(18)
    } else if section.contains("UPDATE AZURPILOT") {
        Some(20)
    } else if section.contains("GIT INIT") {
        Some(22)
    } else if section.contains("SET GIT PROXY") {
        Some(23)
    } else if section.contains("SET GIT REPOSITORY") {
        Some(24)
    } else if section.contains("FETCH REPOSITORY BRANCH") {
        Some(25)
    } else if section.contains("PULL REPOSITORY BRANCH") {
        Some(58)
    } else if section.contains("SHOW VERSION") {
        Some(63)
    } else {
        None
    }
}

/// 解析 `git fetch` 进度行的百分比，并按子阶段映射到总进度区间。
///
/// Counting/Compressing（25-29）→ Receiving（29-52）→ Resolving
/// deltas（52-58）→ Updating files（58-62），与阶段标题的进度点衔接。
fn git_line_progress(line: &str) -> Option<u8> {
    let percentage = find_percentage(line)?;
    if line.contains("Counting objects:") || line.contains("Compressing objects:") {
        Some(scale_progress(percentage, 25, 29))
    } else if line.contains("Receiving objects:") {
        Some(scale_progress(percentage, 29, 52))
    } else if line.contains("Resolving deltas:") {
        Some(scale_progress(percentage, 52, 58))
    } else if line.contains("Updating files:") {
        Some(scale_progress(percentage, 58, 62))
    } else {
        None
    }
}

/// 解析 uv sync 的状态行并映射为依赖同步进度。
///
/// 仅识别 uv 的已知状态前缀（Resolved/Downloading/...），其余输出
/// 忽略；识别到的行先喂给状态机再取总进度。
fn splash_update_for_dependency_output(
    line: &str,
    uv_progress: &mut UvProgressState,
) -> Option<SplashUpdate> {
    let is_status_line = line.starts_with("Resolved ")
        || line.starts_with("Downloading ")
        || line.starts_with("Downloaded ")
        || line.starts_with("Prepared ")
        || line.starts_with("Installed ")
        || line.starts_with("Audited ")
        || line.starts_with("+ ");
    if !is_status_line {
        return None;
    }

    uv_progress.observe(line);
    let progress = dependency_global_progress(uv_progress.progress());
    Some(dependency_splash_update(line, progress, uv_progress))
}

/// 组装依赖同步阶段的进度更新（总进度 + uv 子进度两段显示）。
fn dependency_splash_update(
    detail: impl Into<String>,
    progress: u8,
    uv_progress: &mut UvProgressState,
) -> SplashUpdate {
    SplashUpdate::loading(t!("setup.installing_deps"), detail, progress)
        .with_subtitle(t!("setup.syncing_deps", tip = get_tip()))
        .with_uv_progress(uv_progress.progress(), uv_progress.detail())
}

/// 把 uv 子进度（0-100）线性映射到总进度条的 64-90 区间。
fn dependency_global_progress(uv_progress: u8) -> u8 {
    scale_progress(uv_progress, 64, 90)
}

/// 判断一行 stderr 是否为 uv 的正常进度/提示输出。
///
/// uv 把进度写在 stderr 上，这类行不应按错误处理（不记入
/// "最后错误"、日志级别降为 info）。
pub(super) fn is_uv_progress_line(line: &str) -> bool {
    line.starts_with("Resolved ")
        || line.starts_with("Downloading ")
        || line.starts_with("Downloaded ")
        || line.starts_with("Prepared ")
        || line.starts_with("Installed ")
        || line.starts_with("Audited ")
        || line.starts_with("warning: ")
        || line.starts_with("hint: ")
        || line.starts_with("note: ")
}

impl UvProgressState {
    /// 消费一行 uv 状态输出，更新解析/下载/安装各阶段标记与下载统计。
    fn observe(&mut self, line: &str) {
        if line.starts_with("Resolved ") {
            self.resolved = true;
            return;
        }

        if line.starts_with("Downloading ") {
            self.download_started_at.get_or_insert_with(Instant::now);
            if let (Some(package), Some(size)) = (
                extract_uv_package_name(line),
                extract_uv_download_size_bytes(line),
            ) {
                self.package_sizes.entry(package).or_insert(size);
            }
            return;
        }

        if line.starts_with("Downloaded ") {
            if let Some(package) = extract_uv_downloaded_package_name(line) {
                if self.downloaded_packages.insert(package.clone()) {
                    self.downloaded_bytes = self
                        .downloaded_bytes
                        .saturating_add(self.package_sizes.get(&package).copied().unwrap_or(0));
                }
            }
            return;
        }

        if line.starts_with("Prepared ") {
            self.prepared = true;
        } else if line.starts_with("Installed ") || line.starts_with("Audited ") {
            self.installed = true;
        }
    }

    /// 估算 uv 阶段的整体进度（0-100，单调递增）。
    ///
    /// 各阶段锚点：启动后按时间缓慢爬升（0-20）、解析完成 22、
    /// 下载期取"时间估算"与"包数估算"的较大者（28-88）、准备完成
    /// 92、安装完成 98。进度只会前进，避免输出乱序导致回退。
    fn progress(&mut self) -> u8 {
        let progress = if self.installed {
            98
        } else if self.prepared {
            92
        } else if let Some(download_started_at) = self.download_started_at {
            // 双估算取大者：时间估算保证进度不因包信息缺失而停滞，
            // 包数估算让大包下载时进度更贴近真实
            let elapsed_progress = 28 + (download_started_at.elapsed().as_secs() / 4).min(60) as u8;
            let package_progress =
                28 + (self.downloaded_packages.len().min(30) as u8).saturating_mul(2);
            elapsed_progress.max(package_progress).min(88)
        } else if self.resolved {
            22
        } else {
            2 + (self.started_at.elapsed().as_secs() / 10).min(18) as u8
        };

        self.last_progress = self.last_progress.max(progress);
        self.last_progress
    }

    /// 生成 uv 子进度的描述文案：安装中 / 解析中（含耗时）/
    /// 下载中（含已下载量与实时速度估算）。
    fn detail(&self) -> String {
        if self.installed || self.prepared {
            return t!("setup.uv_installing").to_string();
        }

        let Some(download_started_at) = self.download_started_at else {
            return t!(
                "setup.uv_resolving",
                secs = self.started_at.elapsed().as_secs().to_string()
            )
            .to_string();
        };
        if self.downloaded_bytes == 0 {
            return t!("setup.uv_waiting_speed").to_string();
        }

        let elapsed = download_started_at.elapsed().as_secs_f64().max(0.1);
        let speed = self.downloaded_bytes as f64 / elapsed;
        t!(
            "setup.uv_downloading_detail",
            downloaded = format_transfer_size(self.downloaded_bytes),
            speed = format_transfer_speed(speed)
        )
        .to_string()
    }
}

/// 从 `Downloading <包名> (体积)` 行提取包名。
///
/// 兼容 `numpy==2.4.3`、`numpy @ https://...` 与裸包名三种格式；
/// 包名统一小写以便与 Downloaded 行配对。
fn extract_uv_package_name(line: &str) -> Option<String> {
    // "Downloading numpy==2.4.3 (8.2 MiB)" or "Downloading numpy (8.2 MiB)" or "Downloading numpy @ https://..."
    let rest = line.strip_prefix("Downloading ")?;
    let name = rest
        .split_once("==")
        .map(|(n, _)| n)
        .or_else(|| rest.split_once(" @ ").map(|(n, _)| n))
        .or_else(|| rest.split_once(" (").map(|(n, _)| n))
        .unwrap_or(rest);
    normalize_uv_package_name(name)
}

/// 从 `Downloaded <包名>` 行提取包名（取首个空白分隔段并去掉版本号）。
fn extract_uv_downloaded_package_name(line: &str) -> Option<String> {
    let name = line
        .strip_prefix("Downloaded ")?
        .split_whitespace()
        .next()?;
    let name = name.split_once("==").map(|(name, _)| name).unwrap_or(name);
    normalize_uv_package_name(name)
}

/// 包名归一化：去空白并转小写；空名返回 `None`。
fn normalize_uv_package_name(name: &str) -> Option<String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 从 `Downloading ... (8.2 MiB)` 行提取括号内的人类可读体积并转为字节数。
fn extract_uv_download_size_bytes(line: &str) -> Option<u64> {
    let (_, size) = line.rsplit_once('(')?;
    let size = size.strip_suffix(')')?;
    parse_transfer_size_bytes(size)
}

/// 解析人类可读体积（`1.5MiB`、`42 KiB` 等）为字节数。
///
/// 支持二进制单位（KiB/MiB/GiB）与十进制写法（KB/MB/GB）；
/// 非法输入、非有限数或超出 u64 范围时返回 `None`。
fn parse_transfer_size_bytes(size: &str) -> Option<u64> {
    let size = size.trim();
    let unit_start =
        size.find(|character: char| !character.is_ascii_digit() && character != '.')?;
    let (number, unit) = size.split_at(unit_start);
    let number: f64 = number.parse().ok()?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = number * multiplier;
    (bytes.is_finite() && bytes >= 0.0 && bytes <= u64::MAX as f64).then_some(bytes.round() as u64)
}

/// 把字节数格式化为人类可读体积（自动选择 B/KiB/MiB/GiB 单位，一位小数）。
fn format_transfer_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// 把字节每秒的速率格式化为人类可读体积速率（负数按 0 处理）。
fn format_transfer_speed(bytes_per_second: f64) -> String {
    format_transfer_size(bytes_per_second.max(0.0).round() as u64)
}

/// 把 0-100 的百分比线性缩放到 `[start, end]` 区间（进度条分段衔接用）。
fn scale_progress(percentage: u8, start: u8, end: u8) -> u8 {
    let percentage = percentage.min(100) as u16;
    let start = start as u16;
    let end = end as u16;
    (start + ((percentage * (end - start)) / 100)) as u8
}

/// 从一行文本中提取最后一个 `N%` 形式的百分比数字；无百分比返回 `None`。
fn find_percentage(s: &str) -> Option<u8> {
    s.split('%')
        .next()
        .and_then(|before| {
            before
                .rsplit(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
        })
        .and_then(|num| {
            num.parse::<f32>()
                .ok()
                .map(|v| v.round().clamp(0.0, u8::MAX as f32) as u8)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_find_percentage() {
        assert_eq!(Some(8), find_percentage("8%"));
        assert_eq!(Some(25), find_percentage("loading 25%..."));
        assert_eq!(Some(100), find_percentage("100%..."));
        assert_eq!(None, find_percentage("%1"));
    }

    #[test]
    fn test_git_line_progress_ranges() {
        assert_eq!(
            Some(25),
            git_line_progress("remote: Counting objects:   1% (1/66)")
        );
        assert_eq!(
            Some(40),
            git_line_progress("Receiving objects:  50% (43546/87092), 179.10 MiB | 5.03 MiB/s")
        );
        assert_eq!(
            Some(58),
            git_line_progress("Resolving deltas: 100% (66157/66157), done.")
        );
        assert_eq!(
            Some(62),
            git_line_progress("Updating files: 100% (9881/9881), done.")
        );
    }

    #[test]
    fn test_git_section_progress_ranges() {
        assert_eq!(Some(25), git_section_progress("FETCH REPOSITORY BRANCH"));
        assert_eq!(Some(58), git_section_progress("PULL REPOSITORY BRANCH"));
        assert_eq!(Some(63), git_section_progress("SHOW VERSION"));
    }

    #[test]
    fn test_parse_uv_transfer_size() {
        assert_eq!(parse_transfer_size_bytes("1.5MiB"), Some(1_572_864));
        assert_eq!(parse_transfer_size_bytes("42 KiB"), Some(43_008));
        assert_eq!(parse_transfer_size_bytes("1.0 GiB"), Some(1_073_741_824));
        assert_eq!(parse_transfer_size_bytes("unknown"), None);
    }

    #[test]
    fn test_uv_progress_tracks_completed_downloads_once() {
        let mut progress = UvProgressState::new();
        let downloading =
            splash_update_for_dependency_output("Downloading demo-package (1.5MiB)", &mut progress)
                .expect("downloading output updates splash");
        assert_eq!(downloading.uv_progress.expect("uv progress").progress, 28);

        progress.download_started_at = Some(Instant::now() - Duration::from_secs(1));
        let downloaded =
            splash_update_for_dependency_output("Downloaded demo-package", &mut progress)
                .expect("downloaded output updates splash");
        assert_eq!(progress.downloaded_bytes, 1_572_864);
        assert!(downloaded.uv_progress.expect("uv progress").progress < 100);

        progress.observe("Downloaded demo-package");
        assert_eq!(progress.downloaded_bytes, 1_572_864);
    }
}

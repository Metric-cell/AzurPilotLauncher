//! 依赖下载源的规范化、测速与纯排序策略；不读取或改写工作区配置。

use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT},
    redirect::Policy,
};
use std::{
    thread,
    time::{Duration, Instant},
};
use tracing::{info, warn};

/// uv 安装 Python standalone 的默认镜像列表：前两条为国内镜像用于加速，
/// 后两条为官方源兜底；按顺序尝试直到成功。
pub(super) const DEFAULT_UV_PYTHON_INSTALL_MIRRORS: &[&str] = &[
    "https://registry.npmmirror.com/-/binary/python-build-standalone/",
    "https://mirror.nju.edu.cn/github-release/astral-sh/python-build-standalone/",
    "https://python-standalone.org/mirror/astral-sh/python-build-standalone/",
    "https://downloads.astral.sh/python/",
    "https://github.com/astral-sh/python-build-standalone/releases/download/",
];
/// 默认 PyPI 索引（国际源）。
pub(super) const DEFAULT_PYPI_INDEX: &str = "https://pypi.org/simple/";
/// 内置的国内 PyPI 镜像列表：国际源不可用时按顺序回退。
const BUILTIN_PYPI_INDEXES: &[&str] = &[
    "https://mirrors.aliyun.com/pypi/simple/",
    "https://mirrors.cloud.tencent.com/pypi/simple/",
    "https://repo.huaweicloud.com/repository/pypi/simple/",
    "https://mirrors.cernet.edu.cn/pypi/web/simple/",
];

/// 规范化 PyPI 索引 URL：去空白、忽略 "null"、统一补齐尾部斜杠，
/// 便于后续做等价比较；无效输入返回 `None`。
fn normalize_pypi_index(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    if trimmed.ends_with('/') {
        Some(trimmed.to_owned())
    } else {
        Some(format!("{trimmed}/"))
    }
}

/// 判断两个索引 URL 是否等价（忽略尾部斜杠与大小写差异）。
fn pypi_indexes_match(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

/// 向候选列表追加索引 URL，等价项去重（保持首次出现的顺序）。
fn push_unique_pypi_index(indexes: &mut Vec<String>, url: &str) {
    let Some(index) = normalize_pypi_index(url) else {
        return;
    };
    if !indexes
        .iter()
        .any(|existing| pypi_indexes_match(existing, &index))
    {
        indexes.push(index);
    }
}

/// 按偏好顺序返回候选 PyPI 索引。
///
/// deploy.yaml 中用户配置的镜像最优先——用户的选择决定 uv 首先对
/// 哪个源解析；内置国内镜像作为回退链，最后是官方 PyPI。
fn pypi_index_candidates(configured: Option<&str>) -> Vec<String> {
    let mut indexes = Vec::new();
    if let Some(index) = configured {
        push_unique_pypi_index(&mut indexes, index);
    }
    for index in BUILTIN_PYPI_INDEXES {
        push_unique_pypi_index(&mut indexes, index);
    }
    push_unique_pypi_index(&mut indexes, DEFAULT_PYPI_INDEX);
    indexes
}

/// 对候选 PyPI 索引并发探测延迟，按"延迟最快优先"排序返回。
///
/// 每个候选源在独立线程中测延迟（HEAD 请求），全部探测完成后按
/// 延迟升序排序；用户在 deploy.yaml 配置的镜像无论测速结果如何
/// 都会被提到最前（尊重用户选择）。所有源都探测失败时退回配置顺序。
pub(super) fn ranked_pypi_indexes(configured: Option<&str>) -> Vec<String> {
    let indexes = pypi_index_candidates(configured);
    let client = pypi_probe_http_client();
    let handles = indexes
        .iter()
        .cloned()
        .enumerate()
        .map(|(order, index)| {
            let client = client.clone();
            thread::spawn(move || {
                let latency = client
                    .as_ref()
                    .and_then(|client| measure_pypi_index_latency(client, &index));
                (order, index, latency)
            })
        })
        .collect::<Vec<_>>();

    let mut probes = Vec::with_capacity(indexes.len());
    for handle in handles {
        if let Ok(probe) = handle.join() {
            probes.push(probe);
        }
    }

    for (_, index, latency) in &probes {
        if let Some(latency) = latency {
            info!("PyPI index probe {index}: {} ms", latency.as_millis());
        } else {
            warn!("PyPI index probe {index}: unavailable");
        }
    }

    rank_probe_results(indexes, configured, &probes)
}

/// 排序策略与网络测速分离，保留配置优先、稳定排序和全失败回退。
fn rank_probe_results(
    indexes: Vec<String>,
    configured: Option<&str>,
    probes: &[(usize, String, Option<Duration>)],
) -> Vec<String> {
    let mut ranked_probe_orders = probes
        .iter()
        .filter_map(|(order, _, latency)| latency.map(|latency| (*order, latency)))
        .collect::<Vec<_>>();
    ranked_probe_orders.sort_by_key(|(order, latency)| (*latency, *order));

    let Some((fastest, _)) = ranked_probe_orders.first().copied() else {
        warn!("No PyPI index responded to probing; using configured order");
        return indexes;
    };

    let fastest_index = indexes[fastest].clone();
    let configured = configured.and_then(normalize_pypi_index);
    let mut ranked: Vec<String> = Vec::with_capacity(indexes.len());
    for (order, _) in ranked_probe_orders {
        let index = &indexes[order];
        if !ranked
            .iter()
            .any(|existing| pypi_indexes_match(existing, index))
        {
            ranked.push(index.clone());
        }
    }
    for index in indexes {
        if !ranked
            .iter()
            .any(|existing| pypi_indexes_match(existing, &index))
        {
            ranked.push(index);
        }
    }
    if let Some(configured) = configured {
        if let Some(position) = ranked
            .iter()
            .position(|existing| pypi_indexes_match(existing, &configured))
        {
            if position > 0 {
                let configured = ranked.remove(position);
                info!("Using the PyPI index configured in deploy.yaml: {configured}");
                ranked.insert(0, configured);
            }
        }
    }
    info!("Fastest PyPI index selected first: {fastest_index}");
    ranked
}

/// 构造索引探测专用的 HTTP 客户端：短超时（3 秒连接 / 5 秒总超时）、
/// 禁用系统代理（探测的是直连可达性）、带 PyPI simple API 的 Accept 头。
/// 构建失败时返回 `None`（调用方回退为不排序）。
fn pypi_probe_http_client() -> Option<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("AzurPilot Launcher PyPI probe"),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/vnd.pypi.simple.v1+html,*/*;q=0.8"),
    );

    Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .redirect(Policy::limited(5))
        .no_proxy()
        .default_headers(headers)
        .build()
        .ok()
}

/// 测量索引的响应延迟（HEAD 请求往返时间）；不可达或非 2xx 返回 `None`。
fn measure_pypi_index_latency(client: &Client, index: &str) -> Option<Duration> {
    let start = Instant::now();
    let response = client.head(index).send().ok()?;
    response.status().is_success().then(|| start.elapsed())
}

/// 取 Python standalone 下载镜像列表：用户显式指定的镜像优先（唯一），否则用内置列表。
pub(super) fn uv_python_install_mirrors(explicit: Option<std::ffi::OsString>) -> Vec<String> {
    if let Some(mirror) = explicit {
        return vec![mirror.to_string_lossy().into_owned()];
    }

    DEFAULT_UV_PYTHON_INSTALL_MIRRORS
        .iter()
        .map(|mirror| (*mirror).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_index_keeps_priority_and_candidates_are_deduplicated() {
        let configured = "https://mirrors.aliyun.com/pypi/simple";
        let candidates = pypi_index_candidates(Some(configured));
        assert_eq!(candidates[0], format!("{configured}/"));
        assert_eq!(
            candidates
                .iter()
                .filter(|index| index.contains("aliyun"))
                .count(),
            1
        );
        assert!(normalize_pypi_index(" null ").is_none());
        assert!(normalize_pypi_index("  ").is_none());
    }

    #[test]
    fn ranking_uses_latency_stably_but_preserves_explicit_preference() {
        let indexes = vec!["slow/".to_owned(), "fast/".to_owned(), "down/".to_owned()];
        let probes = vec![
            (0, indexes[0].clone(), Some(Duration::from_secs(1))),
            (1, indexes[1].clone(), Some(Duration::from_millis(10))),
            (2, indexes[2].clone(), None),
        ];
        assert_eq!(
            rank_probe_results(indexes.clone(), None, &probes),
            vec!["fast/", "slow/", "down/"]
        );
        assert_eq!(
            rank_probe_results(indexes.clone(), Some("slow"), &probes),
            indexes
        );
        let failed: Vec<_> = probes
            .into_iter()
            .map(|(order, index, _)| (order, index, None))
            .collect();
        assert_eq!(rank_probe_results(indexes.clone(), None, &failed), indexes);
    }

    #[test]
    fn explicit_python_mirror_is_used_without_implicit_fallbacks() {
        assert_eq!(
            uv_python_install_mirrors(Some("https://custom.example/".into())),
            vec!["https://custom.example/"]
        );
        assert_eq!(
            uv_python_install_mirrors(None).len(),
            DEFAULT_UV_PYTHON_INSTALL_MIRRORS.len()
        );
    }
}

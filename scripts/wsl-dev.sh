#!/usr/bin/env bash
# WSL 开发入口：检查、测试、构建及独立的启动画面预览。
set -euo pipefail

launcher_repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$launcher_repo"

if [[ -f "$HOME/.cargo/env" ]]; then
    source "$HOME/.cargo/env"
fi

command_name=${1:-preview}
if (($#)); then
    shift
fi

case "$command_name" in
    check)
        exec cargo check --locked --all-targets "$@"
        ;;
    test)
        exec cargo test --locked --all-targets "$@"
        ;;
    build)
        exec cargo build --locked "$@"
        ;;
    preview|dev)
        if [[ -z ${DISPLAY:-} && -z ${WAYLAND_DISPLAY:-} ]]; then
            printf '%s\n' '缺少显示环境。请在支持 WSLg 的 WSL 终端中运行。' >&2
            exit 1
        fi
        # 当前 WSLg 的 GPU 路径会触发 DRI/DMABUF 警告及高 CPU；仅预览使用兼容路径。
        if [[ -n ${WSL_DISTRO_NAME:-} && -n ${DISPLAY:-} ]]; then
            export GDK_BACKEND=${GDK_BACKEND:-x11}
            export WEBKIT_DISABLE_DMABUF_RENDERER=${WEBKIT_DISABLE_DMABUF_RENDERER:-1}
        fi
        launcher_target=$(cargo metadata --locked --no-deps --format-version 1 |
            python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
        launcher_preview_dir="$launcher_target/debug"
        mkdir -p -- "$launcher_preview_dir/deploy" "$launcher_preview_dir/config"
        # 仅满足源码中的同目录布局探测；不连接真实后端配置。
        if [[ ! -e "$launcher_preview_dir/deploy/installer.py" ]]; then
            printf '%s\n' '# 仅供启动画面预览使用。' \
                'raise SystemExit("此目录仅用于启动器预览，不能安装或运行后端")' \
                > "$launcher_preview_dir/deploy/installer.py"
        fi
        if [[ ! -e "$launcher_preview_dir/config/deploy.yaml" ]]; then
            printf '%s\n' 'Deploy:' '  Webui:' '    WebuiHost: 127.0.0.1' \
                '    WebuiPort: 22267' '    Password: null' \
                > "$launcher_preview_dir/config/deploy.yaml"
        fi
        if [[ $command_name == dev ]]; then
            # 预览标记在初始化真实运行环境前停止；Tauri 监听源文件并重启。
            exec cargo tauri dev -- --locked -- --preview-crash --lang zh-CN "$@"
        fi
        cargo build --locked
        exec "$launcher_preview_dir/alas-launcher" --preview-crash --lang zh-CN "$@"
        ;;
    *)
        printf '%s\n' '用法：scripts/wsl-dev.sh {check|test|build|preview|dev} [附加参数]' >&2
        exit 2
        ;;
esac

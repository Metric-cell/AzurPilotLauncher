Windows 集中部署

推荐布局：

```text
C:\Azurpilot\
  AzurPilot.exe
  WebView2Loader.dll
  backend\                 后端源码、config、log、frontend
    .venv\                 Python、uv、Git、ADB 与 Python 依赖
  .launcher\
    preferences.json       启动器偏好
    nodejs\                便携 Node.js
    webview2\              浏览器数据与页面偏好
    cache\{uv,npm,pip}\    依赖缓存
    temp\                  本次程序与子进程临时文件
    account-keys\          本机解锁密钥，仍由 DPAPI 和用户 ACL 保护
```

Windows 根据 EXE 所在位置确定 `.launcher/`，在创建任何窗口前配置主窗口与 splash 的 WebView2 数据目录。缓存、临时目录和 `AZURPILOT_LOCAL_DATA_DIR` 仅通过本次进程环境传给子进程，不修改系统环境变量。后端优先识别既有同目录发行布局，也支持 `backend/deploy/installer.py`。本机解锁密钥不得放入后端目录或加入源码备份；集中部署应使用上述 backend 子目录布局。

迁移时先完整停止当前程序及其持有的后端，保留配置与 WebView 数据，再整体移动源码及 `.venv`。检查 `pyvenv.cfg` 的 `home`、Python 版本 junction、激活脚本和旧启动入口中的绝对路径，验证 `sys.prefix`、`sys.base_prefix`、依赖导入、前端与后端健康状态后再清理旧目录。已有账号的本机解锁绑定与项目绝对路径相关，迁移后须用实例密码重新绑定。

Windows 的 uv Python trampoline 即使处于 relocatable 环境，也可能保留 managed Python 的绝对路径。迁移后若提示 `uv trampoline failed to spawn Python child process`，先确认 `.venv/python` 内的真实解释器可运行，修正其版本 junction，再暂存 `Scripts/python*.exe`，使用现有 `Scripts/uv.exe venv --allow-existing --relocatable --python <新位置的真实解释器> <新位置的.venv>` 重新生成启动文件。保留 `Lib/site-packages`，不删除整个虚拟环境；验证依赖可导入后再移除旧启动文件。

Windows 自带的 WebView2 运行时、系统默认浏览器以及系统管理的注册表和快捷方式属于系统集成，不复制到安装目录；快捷方式应指向新的 EXE。自更新的事务文件仍与 EXE 同盘保存。

# 启动器更新与原发行兼容

`src/update/` 默认沿用维护者原有的 HTTPS `stable.json`、mTLS 载荷下载和 tag 发布流程，不要求迁移更新服务器、R2/CDN、SSH 部署或安装包下载渠道。主进程显示进度，在已拉起新版后退出。

## 默认发行协议

默认清单为 `https://ap-launcher-update.nanoda.work/updata/stable.json`，失败后尝试原备用地址 `https://ap.launcher-update.nanoda.work/updata/stable.json`。`LAUNCHER_UPDATE_URL` 可在编译时覆盖主地址。清单继续使用原有 `version`、`platforms[平台].url` 和 `sha256` 字段，兼容 `pub_date`、`notes` 等附加字段；不要求新增签名、长度或有效期。

正式打包继续从 `ALAS_LAUNCHER_MTLS_IDENTITY_PEM_B64` 读取 base64 PEM 客户端身份，`REQUIRE_LAUNCHER_MTLS_IDENTITY` 要求构建时提供；开发构建可回退到不入库的 `证书.txt`。公开清单使用不带身份的独立 HTTP 客户端，只有载荷客户端装载 mTLS 身份。沿用原服务的 User-Agent 与直连策略，不自动启用系统代理。

默认允许原主/备用服务器及载荷域名 `ap-launcher-update-cdn-cn.nanoda.work`、`alas.nanoda.work`，每一跳重定向都在请求前检查。自定义主清单不会自动获得官方 CDN 的权限。所有地址必须使用标准 443 端口的 HTTPS 域名，不允许用户凭据、fragment 或 IP 地址。

清单最多 64 KiB，载荷最多 512 MiB。旧清单没有长度时流式计数并验证 SHA-256，下载完成后将实际长度与来源、平台、版本、摘要一起保存到私有暂存目录；替换时再次验证。该快照不是发布签名。默认协议依赖 HTTPS 服务提供可信元数据，无法独立抵御清单服务器被入侵；mTLS 客户端身份也不能证明载荷由维护者签发。这一限制不要求更换发行基础设施。

`.github/workflows/package.yml` 保留原构建、R2/CDN 上传、SSH 发布 `stable.json` 和旧版兼容部署，不引入新的签名 secrets，也不自动发布下面的可选签名协议。

## 可选签名模式的编译配置

维护者可显式配置以下信任根，在现有服务器上额外提供签名清单；无需改用 GitHub Releases。设置非空的签名 URL、公钥或任一额外 origin 配置即要求完整、有效的签名配置。缺少 URL/公钥或配置无效会报错，网络错误、无签名或签名失败均不会退回默认协议。Windows 助手也使用同一编译期模式，命令行不能切换协议。

| 环境变量 | 含义 |
| --- | --- |
| `AZURPILOT_UPDATE_MANIFEST_URL` | 清单的完整 HTTPS URL；无用户凭据、query 或 fragment，必须指向文件。 |
| `AZURPILOT_UPDATE_PUBLIC_KEY` | 可信 Ed25519 原始公钥，64 个十六进制字符。不能使用测试 fixture 公钥。 |
| `AZURPILOT_UPDATE_METADATA_ORIGINS` | 允许清单及 `.sig` 重定向经过的额外 HTTPS origin，逗号分隔；默认仅清单原始 origin。 |
| `AZURPILOT_UPDATE_PAYLOAD_ORIGINS` | 允许载荷及其重定向经过的额外 HTTPS origin，逗号分隔；默认仅清单原始 origin。 |
| `AZURPILOT_UPDATE_CHANNEL` | 签名渠道，默认 `stable`，1–32 个 ASCII 字母、数字或连字符。 |
| `AZURPILOT_UPDATE_MIN_SEQUENCE` | 编译期最小发布序号，默认 `0`。 |

origin 示例为 `https://cdn.example.org`；不接受路径、非 443 端口或 IP 地址。来源来自二进制的编译配置，运行时环境变量、响应头和清单内容均不能扩展来源白名单。每个重定向目标在发出请求之前验证，最多跟随五跳；禁用自动重定向、自动解压和 Referer，保留 TLS 证书验证。签名模式保留系统代理设置，不读取或发送旧服务的 mTLS 身份。

GitHub Releases 下载通常会从 `https://github.com` 重定向到 `https://release-assets.githubusercontent.com`。使用此方案时，需要把后者同时加入 metadata 和 payload 的编译期白名单。签名中的 `source` 仍为原始的完整清单 URL，不能替换成某次请求的临时 CDN URL。

`build.rs` 追踪上述六个变量，签名 URL 与公钥默认为空，未选择签名模式时使用默认发行协议。签名依赖为 `ed25519-dalek = "2.2.0"`、`semver = "1"`；文件锁采用 Rust 1.89 起稳定的 `File::try_lock`。Windows 复用 `winapi`，需要 `handleapi`、`processthreadsapi`、`securitybaseapi`、`sddl`、`fileapi`、`minwinbase`、`winbase` features；Unix 复用 `nix::libc::O_NOFOLLOW`。

## 清单和签名字节

清单最多 64 KiB。清单 URL 后追加 `.sig` 得到签名 URL。签名响应必须是**恰好 64 字节的原始 Ed25519 签名**，不是 Base64 或十六进制。使用 `ed25519_dalek::VerifyingKey::verify_strict` 验证，并拒绝弱公钥。

签名消息是以下字节直接拼接：

```text
ASCII("AzurPilot launcher update manifest v1") || 0x00 || 清单文件原始字节
```

空格、换行和字段顺序都是被签名内容；验证前不解析或重新序列化 JSON。该方案使用普通 Ed25519，不是 Ed25519ph。JSON 字段如下：

```json
{
  "schema": 1,
  "product": "moe.taiho.alas-launcher",
  "channel": "stable",
  "source": "https://updates.example.org/stable.json",
  "sequence": 42,
  "version": "3.0.0",
  "issued_at": 1800000000,
  "expires_at": 1800003600,
  "artifacts": [
    {
      "platform": "windows-x86_64",
      "url": "https://updates.example.org/alas-launcher-windows-x86_64.exe",
      "sha256": "64个十六进制字符",
      "size": 123456
    }
  ]
}
```

有效平台为 `windows`、`linux`、`darwin` 与 `x86_64`、`aarch64` 的六种组合。版本必须为 SemVer；预发布标识遵循 SemVer 比较，构建元数据不参与新旧判定。拒绝重复平台、未知字段、空载荷或大于 512 MiB 的载荷。载荷下载必须同时满足已签名的精确长度和 SHA-256 摘要；流式读取最多消耗签名长度加一字节，不能靠伪造或省略 Content-Length 绕过上限。

`issued_at`、`expires_at` 是 Unix 秒数，有效期最长七天，允许签发时间最多比本地时钟提前五分钟。过期清单不能授权下载或安装。只有签名、来源、渠道和全部字段均验证通过、仅有效期已过的清单，才允许完整启动器延期更新并继续启动（包括首次准备后端）；mini 仍须获取新版。错误签名或来源不能通过伪造过期字段取得该待遇。若选择此模式，发布端需要续签并提高 `sequence` 才能继续提供更新，可以保持同一个版本。默认发行协议没有这个有效期要求。

## 签名回退保护

清单版本不能低于当前启动器。首次验证后，在可执行文件旁保存已签名清单和签名作为持久化高水位，后续要求版本和 `sequence` 均不回退；相同 `sequence` 必须逐字节复用同一份清单。旧高水位即使已过期仍验证其签名并保留版本下限；它不能授权安装过期版本。状态文件损坏、换用不兼容的来源/渠道/公钥或无法落盘均返回错误，不静默重置信任状态。

状态文件名是 `.azurpilot-update-<目标文件名摘要>.state`，配套 `.lock` 用内核文件锁串行化整个更新过程。锁文件保留，避免删除后产生两个可同时加锁的 inode。状态写入经私有同目录暂存、sync 和原子替换。发布密钥或来源迁移需要通过预先审查的新版二进制完成，并明确迁移已有高水位；自动从网络接受新根密钥不在本协议中。

## 两种协议共用的安装边界

两种模式均要求替换版本严格高于当前启动器，并在整个更新替换期间持锁；默认协议不把未签名清单保存为可信高水位。

下载与替换均使用可执行文件所在目录下随机的私有暂存目录，避免跨文件系统 rename 和共享 `/tmp` 的路径竞争。Unix 暂存目录权限 0700、文件 0600，拒绝 symlink、额外 hardlink 与组/其他用户可写的目标目录或文件；祖先目录必须由当前用户/root 拥有，共享祖先须有 sticky bit，防止整体移走私有安装目录后重建同名路径。在最终校验后保留文件句柄，检查路径仍对应同一 inode，再原子替换目标，刷盘并重启。载荷仅在验证通过后设置 0755。

Windows 使用带当前 token 用户 SID、SYSTEM 和 Administrators 的保护性 DACL 创建暂存目录，不继承其他用户写权限，普通用户的 `asInvoker` 启动器可以更新自己的安装目录。助手参数只有 `--apply-launcher-update <目标>`，载荷和协议快照位于助手同目录的固定文件名。助手必须位于目标旁的更新暂存目录，目标内容必须与正在运行的旧启动器助手完全相同；普通命令行不能授权覆盖任意文件。助手按编译期模式复验来源、版本、长度与摘要；签名模式另验签名、有效期和高水位。助手持锁后将载荷复制到自己新建的受保护目录，复制后再次校验，随后重试原子替换并启动新版。源文件打开时禁止并发写入。

启动取消通过 `&AtomicBool` 传入，逐跳请求前后、每次流读取前后、签名验证后、持锁后和最终替换/启动前均检查。取消返回独立的 `Outcome::Cancelled`，不能进入离线继续路径。元数据单次请求总期限为 15 秒；载荷采用每次阻塞操作 30 秒的超时，持续传输可超过 30 秒，每次读取后重新检查取消。不做未经约束的续传或协议降级。Unix 原子 rename、Windows 启动已验证助手分别是提交点；此前的取消阻止提交，此后不能撤销已经完成的替换或助手移交。若取消恰在 Unix 替换之后到达，后续重启检查仍会阻止拉起新进程。

本地安装边界防止其他本地用户通过暂存路径替换文件。只有签名模式能独立约束不可信的远程分发服务，默认协议仍信任其 HTTPS 元数据。已经能以同一用户身份修改启动器目录、操作进程或改变系统时钟的攻击者，以及管理员/内核，不在隔离范围内。同一用户删除全部本地高水位后的首次启动，仍由当前二进制版本和编译期最小序号约束；没有受保护硬件计数器，无法抵御拥有本地状态完整控制权的回退攻击。本地时钟也必须可信，签名和到期字段不能自行提供可信授时。

## 发布工具与验收

`scripts/sign-launcher-update.py` 使用 Python 3.10+ 和系统 OpenSSL 3，读取已有的外部 Ed25519 PEM 私钥，绝不生成密钥或访问网络。输入目录放置 `alas-launcher-{platform}`，Windows 加 `.exe`；这些必须是用于直接替换启动器的原始二进制。

```bash
python3 scripts/sign-launcher-update.py \
  --payload-dir launcher-updater --output signed/stable.json \
  --source https://updates.example.org/stable.json \
  --payload-base-url https://updates.example.org/releases/v3.0.0/ \
  --channel stable --version 3.0.0 --sequence 42 \
  --issued-at 1800000000 --expires-at 1800003600 \
  --key /private/existing-ed25519.pem --public-key "$AZURPILOT_UPDATE_PUBLIC_KEY"
```

不同载荷 origin 另加 `--allowed-origin https://cdn.example.org`，它必须与编译配置一致。工具拒绝覆写输出。选择签名模式后，先上传不可变载荷，再发布彼此匹配的 `.json` 和 `.json.sig`；两文件切换期间不匹配会安全拒绝，不允许退回旧的未签名协议。工具未接入默认发布工作流；未选择签名时原工作流照常发布旧清单及载荷。签名私钥不得嵌入安装包，CI 中只能用于受信任的正式发布步骤，PR 测试不需要发布秘密。

离线验证命令：

```bash
cargo test --locked update::
python3 scripts/test-sign-launcher-update.py
```

Rust 测试使用本地内存 HTTP fixture 和临时安装目录，覆盖旧协议无长度清单、备用源、协议配置与快照隔离，以及原始字节篡改、缺签/错误公钥、错误来源/渠道、过期延期与拒绝安装、未来/过长有效期、元数据大小、恶意逐跳重定向、载荷长度/摘要、并发锁、持久化回退、安装前换包、错误助手目标，以及请求/流读取/最终安装前的取消。Python/OpenSSL 测试与 Rust 共用 `src/update/fixtures/valid.json(.sig)`，验证签名输出逐字节一致；测试密钥来自公开 RFC 8032 测试向量，绝不能用于发布。没有测试会访问真实更新服务或执行 fixture 载荷。

主进程集成入口仅为 `check_and_restart(&AtomicBool, FnMut(Progress)) -> Result<Outcome>`、`can_continue_offline(&Error, version, root)` 和 `try_apply_from_args() -> Result<bool>`。在 Windows 控制台初始化之前调用助手入口；`Outcome::Restarted` 后退出旧进程，`Outcome::Cancelled` 后结束启动线程并让既有清理路径退出。元数据连接失败、超时或 5xx 且已有完整后端环境时可离线继续；完整启动器还可延期已认证但过期的签名更新。其他签名、来源、格式、版本、载荷或取消错误不能转为离线放行。

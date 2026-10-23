# RDP Remote Desktop（DBX 插件）

在 DBX 中连接 Windows 远程桌面的本地插件：新建 RDP 连接 → 打开工作台 →
Canvas 实时渲染远端画面（分辨率可动态调整）→ 键鼠操作 → 文本复制/粘贴 →
断开自动重连。

- 插件 ID：`io.github.caizong10.rdp`
- 协议栈：[IronRDP](https://github.com/Devolutions/IronRDP)（纯 Rust，
  TCP → TLS → CredSSP/NLA → 会话），Sidecar 以 `stdio-framed` 二进制通道
  与 DBX 通信
- **分发方式**：源码托管在
  [github.com/caizong10/dbx-plugin-rdp](https://github.com/caizong10/dbx-plugin-rdp)，
  安装包（未签名候选包 `.dbxp`）发布在
  [Releases](https://github.com/caizong10/dbx-plugin-rdp/releases)。仓库不提交
  构建产物。版本号按“满 9 进 1”推进：末位 0→9 之后进位到中间位
  （0.2.9 之后是 0.3.0）。

## 功能

| 能力 | 说明 |
|---|---|
| 连接管理 | DBX 原生连接表单（主机/端口/用户名/密码/域/色深/分辨率模式），密码进 Secret Store |
| 画面渲染 | 脏矩形瓦片化（RDP1 帧协议）经二进制通道推流；前端只重绘脏区域并按动画帧合并，Canvas 2D 双缓冲，随窗口缩放 |
| 键鼠 | 仅本地单光标（服务器端指针渲染已关闭）；指针移动 16ms 节流 + 尾点补发，会话循环批量合并陈旧移动事件；滚轮按帧累加合并（惯性滚动不再逐条成包）；按键按 **PC/AT 物理扫描码**（set 1）发送，左右修饰键分别映射并正确区分扩展键；失焦自动释放已按下的按键 |
| 快捷键 | Ctrl+C / Ctrl+V / Ctrl+X / Ctrl+Z / Ctrl+A 等组合键按物理按键下发，由远端自行解释，因此与远端剪贴板、编辑器的快捷键行为一致；文本复制/粘贴不再只能用鼠标右键。无修饰键的可打印字符仍走 Unicode 事件（输入法提交路径），组合键走物理扫描码，两条路径互不重复；浏览器本地 copy/cut/paste 被拦截，避免粘贴内容被隐藏输入框二次发送 |
| 分辨率与缩放 | 工作台顶栏可选目标分辨率（或适应窗口）与本地视图缩放（100%–200%），点“应用”以该分辨率立即重连适配远端；缩放只改变本地显示，不触发重连 |
| 中文输入法 | 隐藏输入框捕获 IME 组合，提交文本按 Unicode 键盘事件发送 |
| 剪贴板 | CF_UNICODETEXT 文本双向自动同步（远端复制→本地自动写入；本地复制→自动通告远端），支持鼠标右键菜单与 Ctrl+C/Ctrl+V 快捷键两种触发方式；会话激活后客户端主动发送能力集+临时目录+格式列表，打破与服务器互等 MonitorReady 导致的通道卡死 |
| 断线重连 | 指数退避（0.5s→30s，上限 10 次），UI 重连条 + 手动重试 |
| 证书信任 | 自签证书按 SHA-256 指纹记住（存插件私有数据目录，不含任何凭据）；宿主支持弹窗时先弹窗确认，不支持时首次信任（TOFU），之后每次严格比对指纹 |
| 异步握手 | 连接 RPC 2.5s 内返回，慢握手转后台线程完成并经状态事件回报结果，不受宿主 10s RPC 超时限制；CredSSP/NLA 登录失败时明确提示“用户名或密码不正确”，不再显示笼统握手错误 |
| 主题与语言 | 使用 DBX 注入的主题 CSS 变量并响应切换事件；界面支持 zh-CN / en |

## 构建

前置：Node.js ≥ 22、Rust（任意稳定版工具链）。

```bash
npm install
npm run build            # 前端 → ui/
cd backend && cargo build --release
# 把产物放到 manifest 声明的位置：
#   copy target\release\dbx-plugin-dbx-plugin-rdp.exe ..\bin\windows-x64\
```

> 重新构建后用产物覆盖 `bin/windows-x64/` 下的可执行文件，再执行
> `dbx-plugin package .` 生成新的 `.dbxp`。仓库不跟踪该二进制，也不跟踪
> `dist/`；安装包通过 GitHub Releases 分发。

> **本机构建配置**：`.cargo/config.toml` 属于按机器定制、已被 `.gitignore`
> 排除。需要时从 `.cargo/config.toml.example` 复制，其中说明了镜像源与
> GNU 工具链 shim 的用法。cargo 会从仓库根目录向上查找该文件，因此后端
> 构建也复用它，无需再建 `backend/.cargo/config.toml`。使用 MSVC 工具链则
> 完全无需该文件。

## 本地安装（未签名包）

1. 打包（**两步**，顺序敏感）：

   ```bash
   node scripts/relock-for-package.mjs   # 把 Cargo.lock 刷新到打包器期望状态
   dbx-plugin package .
   ```

   `dbx-plugin package` 会向 cargo 注入
   `patch.crates-io.dbx-plugin-sdk.path=…` 并以 `--locked` 构建。本项目的
   SDK 是 path 依赖，该 patch 永远“未被使用”，cargo 会把这一点记录进
   `Cargo.lock`（`[[patch.unused]]`）。锁里缺该条目时 `--locked` 直接报
   "cannot update the lock file"；而日常 `cargo build` / `dbx-plugin dev`
   又会把它抹掉。因此：日常开发保持原样，打包前运行一次 relock 脚本即可，
   打完包后的任意普通构建会自动回到开发状态。

   产物：`dist/io.github.caizong10.rdp-<版本>-windows-x64.dbxp` 及同名
   `.artifact.json`。官方发布包可从 Releases 直接下载，无需自行构建。

2. 打开 **DBX → 插件中心（Plugin Center）**。
3. 开启 **「允许安装未签名开发包」**（未开启时安装会被拒绝，这是预期行为）。
4. 安装下载的（或自建 `dist/` 下的）`.dbxp` 文件。
5. 新建连接：连接类型中选择 **RDP Remote Desktop**，填主机/端口（默认
   3389）/用户名/密码 → 测试 → 连接，工作台会自动打开远程桌面画面。

## 开发联调（不启动 DBX）

```bash
npx @dbx-app/plugin-cli dev --path . --port 5190
```

浏览器打开 `http://127.0.0.1:5190` 即可走完整的
新建连接 → 测试 → 连接 → 工作台 → 断开闭环。

开发辅助开关与脚本：

- `DBX_RDP_FAKE=1`：Sidecar 换用无网络的假引擎，仅用于无真实 Windows
  机器时联调 UI 流程（release 二进制默认永远使用真实 IronRDP 引擎）。
- `scripts/smoke-sidecar.mjs`：framed 协议握手自检。
- `scripts/closure-devhost.mjs`：开发宿主闭环自动化（16 项）。
- `scripts/real-handshake.mjs`：真实凭据握手验收
  （`node scripts/real-handshake.mjs <host> <port> <user> <pass> [ignoreCert]`）。
- `scripts/frame-smoke-real.mjs`：真机帧管线验收（帧布局/像素/输入注入/动态缩放）。
- `scripts/acceptance-check.mjs`：打包前本地验收清单（§6）。

测试：`npm test`（前端契约 30 项）、`cd backend && cargo test --release`
（Rust 37 项）。真机验收脚本见上；`scripts/acceptance-check.mjs` 覆盖事实卡
§6 本地验收清单（8 项），`scripts/reconnect-drill.mjs` 演练断线自动重连。

真机验收脚本需要目标 RDP 机器的地址与凭据，请通过参数或环境变量传入，
仓库内不保存任何真实主机、账号或口令。

### Windows + GNU 工具链特化说明

DBX CLI 以 `\\?\` verbatim 路径调用 cargo，GNU 工具链（ld/as/dlltool/gcc）
无法解析。仓库内 `tools/` 提供了剥前缀的链接器与工具包装源码
（`linker-shim.rs`、`tool-shim.rs`），需先用 rustc 编译出 `.exe`，再把
shim 目录加入 `PATH`，并在本机 `.cargo/config.toml` 里把 `linker` 指向
`tools/linker-shim.exe` **的绝对路径**（cargo 按调用目录解析相对路径，
打包时工作目录不在仓库根，用相对路径会失败）。

`.cargo/config.toml` 已被 `.gitignore` 排除，请从
`.cargo/config.toml.example` 复制，文件注释里有完整的 shim 构建命令。
若你的机器用 MSVC 工具链，以上均不需要。

## 发布

打 tag 即触发 [`.github/workflows/plugin-release.yml`](.github/workflows/plugin-release.yml)：
跑完前后端测试 → 构建 UI 与 sidecar → `dbx-plugin package` 生成未签名候选包
→ 附加到该 tag 的 GitHub Release。

```bash
git tag v0.2.3 && git push origin v0.2.3
```

工作流只在 `windows-latest` 上构建；要覆盖 Linux/macOS 需先在对应平台验证
sidecar。安装包属于构建产物，不提交到 Git。

## 已知限制

- **剪贴板仅同步文本**（CF_UNICODETEXT）。位图、文件列表等格式按 MS-RDPECLIP
  的延迟渲染模式未实现：远端复制图片/文件时本插件不会响应格式通告。
- 中文经**输入事件**（Unicode 键盘事件）发送到远端；若需要输入法候选窗
  在远端弹出，仍需在远端系统内切换输入法。
- **Alt+Tab、Win 键、Ctrl+Alt+Del** 由本地操作系统/浏览器截获，通常不会送到
  远端（这是 Web 宿主的固有限制，非本插件遗漏）。插件已为 `MetaLeft`/
  `MetaRight` 准备了扫描码，但 Win 键在本地就被系统吃掉，实际收不到。
- 图形管线为未压缩位图 + RDP6.1 块压缩传输；未启用 RemoteFX / GFX /
  ClearCode，低带宽下画面流量偏大。
- 音频、驱动器/端口/SmartCard 重定向、多显示器、RemoteApp 未实现。
- Kerberos 显式禁用（NLA 使用 NTLM）：KDC 网络访问被离线替身拒绝，
  不会代表用户向域控发起请求。
- 自动重连采用重新握手（非会话快照恢复）；需要重新输入凭据的场景由
  Secret Store 承担。
- 开发宿主不模拟 `host/requestUserInput` 与真实 Secret Store；证书指纹
  弹窗、密码持久化与权限行为需在真实 DBX 内复测（未签名包 +
  「允许安装未签名开发包」即可复现）。
- Sidecar 以当前用户权限运行（非 OS 沙箱）；`DBX_PLUGIN_DATA_DIR` 下仅
  存放证书指纹白名单。

## 安全边界

- 密码只从宿主下发的生命周期载荷（`connection_secrets`）读取，仅存在于
  Sidecar 进程内存；不写入 config、事件、context、日志或错误消息（测试断言）。
- 证书指纹白名单不含凭据，仅 `host + SHA-256`。
- `.dbx-dev/`（开发明文凭据）已被 `.gitignore` 排除，且 `dbx-plugin
  package` 会拒绝包含它的输入。

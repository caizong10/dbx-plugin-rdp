# 开发与构建

## 前置

- Node.js ≥ 22
- Rust 稳定版工具链（MSVC 或 GNU 均可）

## 构建

```bash
npm install
npm run build                      # 前端 → ui/
cd backend && cargo build --release
```

把 sidecar 产物放到 `manifest.json` 声明的位置：

```bat
copy backend\target\release\dbx-plugin-dbx-plugin-rdp.exe bin\windows-x64\
```

`bin/windows-x64/*.exe`、`dist/`、`target/` 都不纳入版本控制。

## 本地打包成 .dbxp

打包分两步，顺序敏感：

```bash
node scripts/relock-for-package.mjs   # 把 Cargo.lock 刷到打包器期望的状态
npx dbx-plugin package .
```

`dbx-plugin package` 会向 cargo 注入
`patch.crates-io.dbx-plugin-sdk.path=…` 并以 `--locked` 构建。本项目的 SDK 是
path 依赖，该 patch 永远「未被使用」，cargo 会把这一点记录进 `Cargo.lock`
（`[[patch.unused]]`）。锁里缺该条目时 `--locked` 直接报 "cannot update the
lock file"；而日常 `cargo build` / `dbx-plugin dev` 又会把它抹掉。所以：日常
开发保持原样，打包前跑一次 relock 脚本即可。

产物：`dist/io.github.caizong10.rdp-<版本>-windows-x64.dbxp` 及同名
`.artifact.json`。

## 在本地 DBX 里试用

1. 打开 **DBX → 插件中心**。
2. 开启 **「允许安装未签名开发包」**（未开启时安装会被拒绝，这是预期行为）。
3. 安装构建出的 `.dbxp`。
4. 新建连接：连接类型选 **RDP Remote Desktop**，填主机/端口（默认 3389）/
   用户名/密码 → 测试 → 连接，工作台会自动打开远程桌面画面。

## 开发联调（不启动 DBX）

```bash
npx @dbx-app/plugin-cli dev --path . --port 5190
```

浏览器打开 `http://127.0.0.1:5190`，可走完整的
新建连接 → 测试 → 连接 → 工作台 → 断开 闭环。

辅助开关与脚本：

- `DBX_RDP_FAKE=1`：Sidecar 换用无网络的假引擎，便于没有真实 Windows 机器时
  联调 UI 流程（release 二进制默认永远使用真实 IronRDP 引擎）。
- `scripts/smoke-sidecar.mjs`：framed 协议握手自检。
- `scripts/closure-devhost.mjs`：开发宿主闭环自动化（16 项）。
- `scripts/real-handshake.mjs`：真实凭据握手验收
  （`node scripts/real-handshake.mjs <host> <port> <user> <pass> [ignoreCert]`）。
- `scripts/frame-smoke-real.mjs`：真机帧管线验收。
- `scripts/acceptance-check.mjs`：打包前本地验收清单。
- `scripts/reconnect-drill.mjs`：断线自动重连演练。

真机验收脚本需要目标 RDP 机器的地址与凭据，请通过参数或环境变量传入；
仓库内不保存任何真实主机、账号或口令。

## 测试

```bash
npm test                      # 前端 30 项
cd backend && cargo test --release   # Rust 25 项
```

## 发布

打 tag 即触发 [`.github/workflows/plugin-release.yml`](../.github/workflows/plugin-release.yml)：
跑测试 → 构建 UI 与 sidecar → `dbx-plugin package` 生成**未签名候选包** →
附加到该 tag 的 GitHub Release。

```bash
git tag v0.2.3 && git push origin v0.2.3
```

工作流目前只在 `windows-latest` 上构建；要覆盖 Linux/macOS 需先在对应平台
验证 sidecar。

## Windows + GNU 工具链

DBX CLI 以 `\\?\` verbatim 路径调用 cargo，GNU 工具链（ld/as/dlltool/gcc）无法
解析。仓库内 `tools/` 提供了剥前缀的链接器与工具包装源码
（`linker-shim.rs`、`tool-shim.rs`），需先用 rustc 编译出 `.exe`，把 shim 目录
加入 `PATH`，并在本机 `.cargo/config.toml` 里把 `linker` 指向
`tools/linker-shim.exe` **的绝对路径**（cargo 按调用目录解析相对路径，打包时
工作目录不在仓库根，用相对路径会失败）。

`.cargo/config.toml` 已被 `.gitignore` 排除，请从 `.cargo/config.toml.example`
复制，文件注释里有完整的 shim 构建命令。使用 MSVC 工具链则以上都不需要。

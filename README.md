# RDP Remote Desktop（DBX 插件）

在 DBX 中连接 Windows 远程桌面的插件：新建 RDP 连接后打开工作台，直接在
Canvas 上操作远端桌面，支持键鼠、中文输入与文本剪贴板双向同步。

- 插件 ID：`io.github.caizong10.rdp`
- 许可证：Apache-2.0

## 技术栈

- **前端**：TypeScript + Vite，Canvas 2D 渲染远端画面
- **后端**：Rust sidecar，[IronRDP](https://github.com/Devolutions/IronRDP)
  实现 RDP 协议栈（TCP → TLS → CredSSP/NLA → 会话）
- **通信**：DBX `stdio-framed` 协议，画面与输入走二进制通道
- **RDP 通道**：CLIPRDR（剪贴板）、DisplayControl（动态分辨率）

## 功能

| 能力 | 说明 |
|---|---|
| 连接管理 | DBX 原生连接表单，密码存入 Secret Store |
| 画面渲染 | 脏矩形增量推流，按动画帧合并重绘，随窗口缩放 |
| 键鼠与快捷键 | 按物理扫描码下发，Ctrl+C / Ctrl+V 等组合键与远端行为一致 |
| 文本剪贴板 | Windows、Linux、macOS 双向自动同步（CF_UNICODETEXT） |
| 文件剪贴板 | 仅 Windows：双向文件/文件夹复制；远端到本地按需读取，支持资源管理器复制进度与兼容回退 |
| 中文输入法 | 通过隐藏输入框捕获 IME，以 Unicode 事件提交到远端 |
| 分辨率与缩放 | 目标分辨率切换，或本地视图缩放 100%–200% |
| 断线重连 | 指数退避自动重连，并提供手动重试 |
| 证书信任 | 按 SHA-256 指纹记忆，未信任时由宿主弹窗确认 |
| 主题与语言 | 跟随 DBX 主题变量，界面支持 zh-CN / en |

## 文档

- [开发与构建](docs/DEVELOPMENT.md)
- [已知限制](docs/LIMITATIONS.md)
- [安全边界](docs/SECURITY.md)
- [提交到官方插件商店](docs/STORE_SUBMISSION.md)

## 许可证

[Apache-2.0](LICENSE)

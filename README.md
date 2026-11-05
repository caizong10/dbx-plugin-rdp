# RDP Remote Desktop（DBX 插件）

在 DBX 中连接 Windows 远程桌面的插件：新建 RDP 连接后打开工作台，直接在
Canvas 上操作远端桌面，支持键鼠、远端输入法，以及剪贴板双向同步。

当前版本：**0.2.8**。

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
| 远端输入法 | 画布获得焦点后转发物理键，由远端键盘布局及输入法处理 |
| 分辨率与缩放 | 支持手动调整分辨率；动态模式首次连接后尝试按窗口和设备像素比适配，画面保持完整显示 |
| 断线重连 | 指数退避自动重连，并提供手动重试 |
| 证书信任 | 按 SHA-256 指纹记忆，未信任时由宿主弹窗确认 |
| 主题与语言 | 跟随 DBX 主题变量，界面支持 zh-CN / en |

## 安装与使用

1. 在 [Releases](https://github.com/caizong10/dbx-plugin-rdp/releases) 下载适用于当前平台的 `.dbxp`。
2. 在 DBX 插件中心安装；未签名的开发候选包需先启用「允许安装未签名开发包」。
3. 新建 RDP 连接并填写远端地址、账号等信息，连接后点击远程画面即可输入。

默认请求 32 位色深；服务端可能使用不同的实际色深或拒绝动态分辨率。缩放受窗口大小限制，以保证完整显示、不出现滚动条。旧连接若保存了 16 位色深，需要在连接配置中手动调整。

## 0.2.8 更新

- 改进键鼠输入、焦点和远端输入法处理。
- 改进连接提示、重连、分辨率调整与画面适配。
- 默认请求 32 位色深，剪贴板诊断日志默认关闭。

## 文档

- [开发与构建](docs/DEVELOPMENT.md)
- [已知限制](docs/LIMITATIONS.md)
- [安全边界](docs/SECURITY.md)
- [提交到官方插件商店](docs/STORE_SUBMISSION.md)

## 许可证

[Apache-2.0](LICENSE)

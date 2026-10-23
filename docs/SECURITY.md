# 安全边界

## 凭据

- 密码只从宿主下发的生命周期载荷（`connection_secrets`）读取，仅存在于
  Sidecar 进程内存；不写入 config、事件、context、日志或错误消息（有测试断言
  覆盖）。
- 证书指纹白名单不含凭据，仅 `host + SHA-256`，存放在插件私有数据目录。

## 网络

- Sidecar 直接建立 TCP → TLS → CredSSP/NLA 连接，不经过 DBX 宿主转发。
- Kerberos 被显式禁用，不会代表用户向域控发起请求。

## 权限

插件只申请运行所必需的最小权限集：

| 权限 | 用途 |
|---|---|
| `host.workbench` | 打开/管理工作台 |
| `host.events` | 接收状态事件、回报握手结果 |
| `host.binary` | 二进制帧通道（画面推流与输入注入） |

不申请 `host.clipboard:read`：剪贴板由 Sidecar 通过系统剪贴板 API 直接处理，
UI 不读取剪贴板内容。

## 仓库卫生

- `.dbx-dev/`（开发期明文凭据）已被 `.gitignore` 排除，且 `dbx-plugin package`
  会拒绝包含它的输入。
- 仓库不提交 `.dbxp` 安装包、签名私钥或访问令牌。
- 构建产物（`dist/`、`bin/`、`target/`）均不纳入版本控制。

## 进程模型

- Sidecar 以当前用户权限运行，不是 OS 级沙箱。
- 每次连接一个会话线程；断开时通过 channel 通知其自行退出。

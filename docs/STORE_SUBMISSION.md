# 提交到 DBX 官方插件商店

依据 [DBX 官方文档：开发和提交 DBX 插件](https://dbxio.com/cn/docs/plugin-development)
与 [`t8y2/dbx-store`](https://github.com/t8y2/dbx-store) 的 `CONTRIBUTING.md` 整理。

## 一、先弄清三个仓库的分工

| 内容 | 去哪里 |
|---|---|
| 插件源码、Rust sidecar、测试、发布脚本 | **你自己的仓库**（本仓库） |
| 上架登记（名称、图标、介绍、版本、产物 URL） | [`t8y2/dbx-store`](https://github.com/t8y2/dbx-store)，向 `main` 提 PR |
| DBX 宿主 / SDK / CLI / manifest schema | [`t8y2/dbx`](https://github.com/t8y2/dbx)，仅改平台时才提 PR |

**不要**把插件源码提交给 `dbx-store`，也不要向 `t8y2/dbx` 提插件本身的 PR。

## 二、完整流程（7 步）

1. **插件源码公开可审阅。** 本仓库已满足。
2. **为每个支持的平台构建一个未签名 `.dbxp` 候选包**，并发布到**不可变的
   HTTPS URL**。本仓库用 GitHub Release 承担这一步：打 tag 后工作流会自动
   构建并附加产物。
3. **Fork `t8y2/dbx-store`**，基于 `main` 建分支，提交两个文件：
   - `publishers/caizong10.json` —— **仅首次提交需要**
   - `candidates/io.github.caizong10.rdp.json` —— 候选元数据
4. **填好 PR 模板**：能力（capabilities）、权限（permissions）、数据访问、
   网络访问、原生 sidecar 的行为说明。审核重点就是这些——RDP 插件会建立
   任意 TCP/TLS 出站连接并在本地运行原生进程，必须写清楚。
5. **CI 校验**。此时 PR 会**保持红色**，提示
   `open candidate(s) awaiting DBX Store signing`。这是**预期状态**，不是失败：
   它专门用来阻止未签名的内容被合并。
6. **维护者审核通过后**，运行受保护的 **Sign plugin PR candidates** 工作流：
   它会同步分支、校验候选字节、用仓库密钥签名、把签名产物发布到 R2、并把
   最终化的 `plugins/<plugin-id>.json` 与 `catalog/index.json` 回写到你的 PR。
7. **CI 转绿后由维护者合并**，插件进入官方目录。

> 已上架插件发新版时：只提交 `candidates/<plugin-id>.json`（填新版本号即可），
> `publishers/*.json` 不必再提。省略的字段保留原值，填写的字段覆盖原值。

## 三、需要提交的两个文件

### `publishers/caizong10.json`（仅首次）

```json
{
  "id": "caizong10",
  "name": "caizong10",
  "status": "unverified"
}
```

`status` 由维护者审核后调整，提交时固定写 `unverified`。

### `candidates/io.github.caizong10.rdp.json`

```json
{
  "schemaVersion": 1,
  "id": "io.github.caizong10.rdp",
  "publisher": "caizong10",
  "version": "0.2.4",
  "name": "RDP Remote Desktop",
  "description": "Open Windows remote desktops inside DBX with a native Rust (IronRDP) sidecar.",
  "icon": "https://raw.githubusercontent.com/caizong10/dbx-plugin-rdp/v0.2.4/assets/plugin.svg",
  "tags": ["rdp", "remote-desktop", "windows"],
  "permissions": ["host.workbench", "host.events", "host.binary"],
  "source": "https://github.com/caizong10/dbx-plugin-rdp/tree/v0.2.4",
  "homepage": "https://github.com/caizong10/dbx-plugin-rdp",
  "license": "Apache-2.0",
  "releaseNotes": "Adds Linux and macOS (Intel + Apple Silicon) builds alongside Windows. Keyboard shortcuts (Ctrl+C/V/X) reach the remote desktop via a corrected PC/AT scan-code table.",
  "localizations": {
    "zh-CN": {
      "name": "RDP 远程桌面",
      "description": "在 DBX 工作台内打开 Windows 远程桌面，由原生 Rust（IronRDP）sidecar 驱动。"
    }
  },
  "targets": [
    {
      "target": "windows-x64",
      "url": "https://github.com/caizong10/dbx-plugin-rdp/releases/download/v0.2.4/io.github.caizong10.rdp-0.2.4-windows-x64.dbxp",
      "sha256": "0c179c856faf76649e40eeae43e4b37ba6bae676c66f0a770a01b4e88a5c132e",
      "size": 3236353
    },
    {
      "target": "linux-x64",
      "url": "https://github.com/caizong10/dbx-plugin-rdp/releases/download/v0.2.4/io.github.caizong10.rdp-0.2.4-linux-x64.dbxp",
      "sha256": "38d553de93f1aa2801080e6eab67ef08b7fb580fbc93b6f2703cd5e6e2768853",
      "size": 3313333
    },
    {
      "target": "darwin-x64",
      "url": "https://github.com/caizong10/dbx-plugin-rdp/releases/download/v0.2.4/io.github.caizong10.rdp-0.2.4-darwin-x64.dbxp",
      "sha256": "c5679e20181bb6dee8a0e378fe42067a8fa6fba1475e6d0d463a7ec40254baf2",
      "size": 3189100
    },
    {
      "target": "darwin-arm64",
      "url": "https://github.com/caizong10/dbx-plugin-rdp/releases/download/v0.2.4/io.github.caizong10.rdp-0.2.4-darwin-arm64.dbxp",
      "sha256": "0580e280a56d6be6afdc0fe1e9442178a0a6526f842f25793d07b3117851ae95",
      "size": 2971405
    }
  ]
}
```

`sha256` 与 `size` 必须逐个从**同一个**未签名 `.dbxp` 取，不能凭记忆或复用旧
版本号：

```bash
sha256sum io.github.caizong10.rdp-0.2.4-<target>.dbxp
stat -c%s io.github.caizong10.rdp-0.2.4-<target>.dbxp
```

## 四、会被直接拒签的硬性约束

- `sha256` / `size` 必须与未签名候选包的**实际字节**完全一致。
- 候选包内 `manifest.json` 的 `id`、`version`、`publisher` 必须与候选元数据
  **逐字一致**（本仓库三者都已对齐）。
- URL 必须是 HTTPS，且**不能**指向 `t8y2/dbx-store` 的 release。
- 已上架或已撤销的版本不能重复提交。
- `verified` 字段由维护者设置，提交内容里不要写 `true`。
- **绝不手工编辑** `plugins/*.json` 与 `catalog/index.json`，它们由签名工作流产
  生。
- PR 里不要带插件源码目录、`.dbxp` 二进制、签名私钥或任何 token。

## 五、提交前自检清单

- [ ] 仓库公开，且 tag 对应的提交与 Release 产物一致
- [ ] Release 里的 `.dbxp` 是**未签名**候选包
- [ ] 包内 `manifest.json` 的 id / version / publisher 与候选元数据一致
- [ ] `source` 指向**该版本**的 tag（不是分支）
- [ ] `icon` 用 HTTPS 且指向该 tag 的固定资源
- [ ] `permissions` 与 `manifest.json` 完全一致（最小权限）
- [ ] PR 模板里写清原生 sidecar 与网络访问行为

## 六、当前状态

| 项 | 状态 |
|---|---|
| 源码公开 | 已完成 |
| 四平台构建与测试 | 已完成（CI：windows-x64 / linux-x64 / darwin-x64 / darwin-arm64 全绿） |
| 未签名 `.dbxp` + Release | 已完成（`v0.2.4`，四个平台） |
| 向 `dbx-store` 提 PR | **待办** |
| 维护者签名合并 | 等待审核 |

> `v0.2.3` 只有 `windows-x64` 一个产物，已被 `v0.2.4` 取代；提交商店请用
> `v0.2.4`，四个 target 一次提交完。

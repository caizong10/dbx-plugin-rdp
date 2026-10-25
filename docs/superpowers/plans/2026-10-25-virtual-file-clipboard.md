# 虚拟文件剪贴板实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans (inline execution in this session). Steps use checkbox syntax for tracking.

**Goal:** 让远端文件夹第一次粘贴就启动按需传输，并尽可能显示 Windows 原生复制进度，同时保留 0.2.6 回退路径。

**Architecture:** 在 Windows 上新增 COM `IDataObject`/`IStream` 虚拟文件提供者；流读取通过 session-thread 的 CLIPRDR FileContents 请求取得远端数据。当前临时下载 + CF_HDROP 流程作为兼容回退，只有虚拟对象不可用或流失败时使用。

**Tech Stack:** Rust 2021、Windows COM ABI（`windows-sys`）、IronRDP cliprdr 0.7、现有 Win32 clipboard owner、Cargo/Vitest。

**Spec:** `docs/superpowers/specs/2026-10-25-virtual-file-clipboard-design.md`

## Global Constraints

- 不覆盖或删除 0.2.6 备份与已生成插件包。
- 所有新增行为先写真实回归测试并观察 RED，再写生产代码。
- 非 Windows 构建继续保持文本剪贴板行为。
- 不把文件正文、凭据写入诊断日志。
- 虚拟对象失败时必须有可测试的回退路径。
- 未完成远端实测前，不声称与 mstsc 完全一致。

---

### Task 1: 建立 Windows 虚拟文件原型接口

**Files:**
- Create: `backend/src/clip_virtual.rs`
- Modify: `backend/src/lib.rs`
- Test: `backend/tests/clipboard_virtual_windows.rs`

**Interfaces:**
- Produces `VirtualFileEntry { name, relative_path, size, is_directory }`。
- Produces `VirtualClipboard::new(entries, reader)`、`VirtualClipboard::formats()`、`VirtualClipboard::read_file(index, offset, length)`。
- `reader` 是测试可控的分块读取回调；原型阶段不接 RDP。

- [ ] Write a failing Windows test that creates a private WindowStation, registers a virtual clipboard object, enumerates `FileGroupDescriptorW`, requests `FileContents`, and asserts two reads for a 3 MiB fixture.
- [ ] Run the focused test and confirm it fails because the virtual provider does not yet exist.
- [ ] Implement the smallest COM data object/stream adapter that exposes descriptor metadata and a bounded reader.
- [ ] Run the focused test and confirm it passes.
- [ ] Run existing `clipboard_windows` test to prove CF_HDROP fallback remains intact.

### Task 2: Add generation-safe on-demand transfer bridge

**Files:**
- Modify: `backend/src/clip_transfer.rs`
- Modify: `backend/src/rdp.rs`
- Test: `backend/tests/clipboard_regression.rs`

**Interfaces:**
- Adds `VirtualReadRequest { generation, file_index, position, length }`.
- Adds `VirtualReadResponse { generation, file_index, position, data, error }`.
- Session loop owns CLIPRDR operations; the COM stream never writes to the socket directly.

- [ ] Write tests for two-block reads, out-of-order stale generation responses, short reads, and remote errors.
- [ ] Run focused tests and verify RED.
- [ ] Add bounded request/response rendezvous with one outstanding read per virtual stream.
- [ ] Route requests through existing `clip_op!` and CLIPRDR lock/data-id handling.
- [ ] Run focused and full Rust tests.

### Task 3: Register virtual object and remove empty first-paste behavior

**Files:**
- Modify: `backend/src/clip_owner.rs`
- Modify: `backend/src/clipboard.rs`
- Modify: `backend/src/rdp.rs`
- Test: `backend/tests/clipboard_regression.rs`, `backend/tests/clipboard_windows.rs`

**Interfaces:**
- Adds `clip_owner::offer_virtual(entries, reader)` and `clip_owner::cancel_virtual(generation)`.
- `OfferRemoteFiles` publishes virtual metadata immediately when supported.
- Fallback waits for real staging paths and never publishes an empty `CF_HDROP` as success.

- [ ] Write regression test that first paste request blocks until data is available instead of receiving an empty list.
- [ ] Run RED.
- [ ] Wire the virtual provider to remote descriptors and transfer bridge.
- [ ] Preserve folder roots, empty directories, and DropEffect behavior in fallback.
- [ ] Run full tests and Windows private-clipboard tests.

### Task 4: Cancellation, cleanup, and diagnostics

**Files:**
- Modify: `backend/src/clip_virtual.rs`, `clip_owner.rs`, `clip_transfer.rs`, `rdp.rs`, `diag.rs`
- Test: `backend/tests/clipboard_regression.rs`, `backend/tests/clipboard_windows.rs`

- [ ] Write tests for session shutdown, paste cancellation, new-generation supersession, and no stale files.
- [ ] Run RED.
- [ ] Implement cancellation and bounded cleanup.
- [ ] Log virtual read metadata without content.
- [ ] Run full Rust tests and check no diagnostic secrets.

### Task 5: Build, package, and publish candidate

**Files:**
- Modify: `manifest.json`, `package.json`, `package-lock.json`, `backend/Cargo.toml`, `backend/Cargo.lock`
- Create: `docs/CLIPBOARD-<version>.md`

- [ ] Run backend tests, frontend tests, typecheck, UI build, release build, and package build.
- [ ] Extract the package and run sidecar handshake self-test.
- [ ] Verify package checksum and staged binary checksum.
- [ ] Keep 0.2.6 package and backup unchanged.
- [ ] Report real remote validation as pending until user tests on target computer.

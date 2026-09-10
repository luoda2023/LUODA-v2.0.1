# 端到端验收清单：PC 连接并控制手机

> 适用版本：**LDesk 2.2.32**（含手机端「投屏授权 token 持久化」）
> 核心目标：手机端**授权一次**后，PC 可反复连接并控制手机，**不再每次弹系统授权窗**。

---

## 0. 前置条件

| 项 | 要求 |
|----|------|
| 手机 | Android 10 及以上（重点机型：OPPO、华为；Android 14 已验证免选窗口） |
| 手机端 APK | 按 CPU 架构选包：64 位真机用 `LDesk-arm64-v8a-v2.2.32-direct-only.apk`；32 位用 `armeabi-v7a`；模拟器/x86 用 `x86_64`（三包均含 libc++_shared.so，安装后可正常启动） |
| PC 端 | LDesk 便携版（portable-x64），与手机端同一 ID 服务可互通 |
| 网络 | 手机与 PC 可互通（同局域网或均在线，经 ID 服务器/直连） |
| 手机 ID | 手机端「共享屏幕」页显示的 ID（数字串） |
| 连接确认模式 | 默认「点击确认」；如需免点，可改为「密码确认」（见 3.3） |

---

## 1. 手机端：安装与首次授权（一次性）

- [ ] 1.1 安装 2.2.32 APK（可覆盖 2.2.31 安装，无需卸载；versionCode 已递增），安装后**能正常启动**（armv7/x86_64 包已含 libc++_shared.so；旧 2.2.31 的 armv7/x86_64 包缺此文件会启动崩溃，勿用）
- [ ] 1.2 打开 App，冷启动**不弹任何授权窗**（已授权设备静默启动；首次安装会引导一次标准权限）
- [ ] 1.3 进入「共享屏幕」页，确认顶部显示本机 **ID** 与连接状态「Ready」
- [ ] 1.4 开启**输入权限**（无障碍）：
      「共享屏幕」页 → 输入开关 → 跳转系统设置 → 打开 **LUODA 输入服务**（AccessibilityService）→ 返回
      （系统级授权，开启一次即记住；`InputService.isOpen` 变为 true）
- [ ] 1.5 开启**投屏权限**（MediaProjection）：
      「共享屏幕」页 → 点击「开始共享/服务」→ 系统弹出「将开始投屏/录屏」→ **允许**
      （授权后 token 被持久化保存，logcat 出现 `saveProjectionToken: persisted screen-capture grant`）

---

## 2. PC 端：准备

- [ ] 2.1 运行 LDesk 便携版，确认 PC 端在线（左侧设备/连接区正常）
- [ ] 2.2 记下手机端 ID 与密码（一次性密码会刷新；或手机端改为「密码确认」模式并设置固定密码）

---

## 3. 连接确认模式（手机端设置，可选）

| 模式 | 设置路径 | PC 连接体验 |
|------|---------|------------|
| 点击确认（默认） | 共享屏幕页右上角菜单 → Accept sessions via click | 每次连接手机端弹「接受？」通知，需手动点接受 |
| 密码确认 | 右上角菜单 → Accept sessions via password | PC 输对密码即连，手机端免点（陌生连接仍弹一次确认） |
| 两者 | Accept sessions via both | 密码或点击均可 |

- [ ] 3.1 验收建议：临时用「密码确认」+ 固定密码，专注验证投屏/控制链路；确认通过后再切回实际生产模式

---

## 4. 核心场景：授权一次 → 杀进程 → 重启 → PC 直连（核心验证）

> 本场景验证 2.2.32 的投屏 token 持久化：**进程被杀后不丢失授权，PC 直连不弹窗**。

- [ ] 4.1 完成第 1 节授权后，从最近任务列表**划掉 LDesk 进程**（或直接重启手机）
- [ ] 4.2 重新打开 LDesk，冷启动**不弹授权窗**（logcat 出现 `restoreProjectionToken: restored previously granted projection`）
- [ ] 4.3 PC 端输入手机 ID → 连接 → （按 3.1 模式）确认/输密码
- [ ] 4.4 **预期：直接进入手机画面，系统「投屏/录屏」授权窗不出现**
      （logcat 出现 `ensureCaptureStarted: reused persisted projection token`）
- [ ] 4.5 手机画面稳定显示（无黑屏、无「等待画面」卡死），画面流畅
- [ ] 4.6 **投屏验证**：手机回到桌面/打开任意 App，PC 端画面实时同步（含状态栏、动画）

---

## 5. 输入控制验证（PC 操控手机）

- [ ] 5.1 **鼠标左键**：PC 端点击手机画面上的 App 图标 → 手机端对应 App 打开
- [ ] 5.2 **拖拽/滑动**：PC 端按住拖动（如桌面翻页、列表滚动）→ 手机端同步滑动
- [ ] 5.3 **键盘**：PC 端在手机输入框打字（中文/英文）→ 手机端文字逐字上屏
- [ ] 5.4 **系统键**：PC 端按 Esc/Home/Back → 手机端返回桌面/返回上一级
- [ ] 5.5 **锁屏亮屏**：手机熄屏时 PC 端点击 → 手机自动亮屏并响应
- [ ] 5.6 前置要求：输入开关（无障碍）已开启（第 1.4 步）；若未开启，输入无效但投屏仍正常

---

## 5b. 无障碍加固验收（2.2.32 新增）

> 验证 `stopWithTask=false` + `onUnbind` 加固：划掉任务不再误杀无障碍服务，系统回收后能自动重启。

- [ ] 5b.1 确认第 1.4 步无障碍已开启后，从最近任务**划掉 LDesk**，再重新打开
- [ ] 5b.2 打开「共享屏幕」页 → 输入开关状态仍为**开启**（无需重新跳设置开启；logcat 有 `onServiceConnected!` 表示服务已随系统自动重建）
- [ ] 5b.3 PC 连接后键盘/鼠标立即生效（服务未被划掉任务误杀）
- [ ] 5b.4 **状态同步**：在共享页停留时用 `adb shell am force-stop com.luoda.remote` 强杀进程 → 打开 App → 输入开关状态与服务实际状态一致（服务被系统回收时开关立即变关，自动重启后立即变开；由 `on_state_changed` input 通知驱动，无需手动刷新）

---

## 6. 断开与重连（无需重新授权）

- [ ] 6.1 PC 端断开连接 → 手机端回到共享页（服务保持在线，token 保留）
- [ ] 6.2 PC 再次连接手机 → **仍不弹投屏授权窗**，直接出画面
- [ ] 6.3 手机端主动停止共享（「停止服务」）→ 重新开启 → PC 再连 → 依然免授权
      （2.2.32 语义：用户主动停服务不撤销已记住的授权）

---

## 7. 异常与边界场景

| # | 场景 | 预期行为 |
|---|------|---------|
| 7.1 | 系统撤销投屏授权（设置 → 应用 → 特殊访问 → 屏幕录制 → 关闭） | 下次 PC 连接**重新弹一次授权窗**；授权后恢复正常（token 已自动清除，不残留） |
| 7.2 | 用户拒绝授权窗 | 不弹窗死循环；手机端保持在线，下次连接再弹 |
| 7.3 | 恢复的 token 失效（重启后系统已回收） | 自动回退弹授权窗并补投屏，不出现黑屏卡死（logcat：`restored token unusable, requesting fresh grant`） |
| 7.4 | 手机端进程被杀后立即连接 | 服务随 App 冷启动重建，恢复 token 后正常投屏 |
| 7.5 | 只开投屏、没开输入权限 | 画面正常，键盘/鼠标无响应（属预期，非缺陷） |
| 7.6 | 陌生 PC 连接（未授权 ID） | 手机端弹一次连接确认，用户接受后进入正常链路 |

---

## 8. 判定通过标准

- [ ] 4.2/4.4/6.2：杀进程、重启、断开重连后**全程无系统授权弹窗**
- [ ] 第 5 节输入控制 6 项全部生效
- [ ] 7.1 撤销授权后能重新弹窗并恢复投屏（兜底路径可用）
- [ ] 手机端 logcat 关键日志齐全：
  - `saveProjectionToken: persisted screen-capture grant`（首次授权后）
  - `restoreProjectionToken: restored previously granted projection`（重启后）
  - `ensureCaptureStarted: reused persisted projection token`（PC 连接时）
  - 无 `createVirtualDisplay returned null` / `SecurityException` 异常刷屏

---

## 9. 日志获取

```bash
# adb 抓取关键 tag（LOG_SERVICE / permissionRequest / input service）
adb logcat -s LOG_SERVICE permissionRequest "input service" mMainActivity | tee phone-2.2.32.log

# 或手机端：共享屏幕页右上角 → 分享运行日志（导出 runtime log）
```

---

## 10. 常见问题排查

| 现象 | 排查方向 |
|------|---------|
| 首次授权后仍每次弹窗 | 确认 logcat 有 `saveProjectionToken: persisted`；若无，见第 9 节查 `result intent has no data Uri` 日志（个别 OEM 无法持久化，属系统限制） |
| 连接后黑屏/卡等待画面 | logcat 查 `startCapture failed` / `createVirtualDisplay returned null`；先断开重连一次 |
| 画面有但无法控制 | 检查无障碍「LUODA 输入服务」是否仍开启（2.2.32 已加固：`stopWithTask=false` 划掉任务不杀服务、`onUnbind` 配合系统自动重启；若仍失效，多为 ROM 后台清理/「停止应用」，重新打开一次并在系统设置中允许后台运行） |
| PC 找不到手机/灰点 | 确认手机端服务在线（共享页状态 Ready）；重启便携版后重试 |
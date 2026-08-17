# 打包与分发

打包用 [cargo-packager]，配置在 `crates/roam/Cargo.toml` 的
`[package.metadata.packager]`。它一套配置覆盖 macOS / Linux / Windows 的所有格式，
所以这里不再维护手写的 bundle 脚本。

```sh
cargo install cargo-packager --locked

cargo packager -p roam --release --formats app,dmg   # macOS
cargo packager -p roam --release --formats deb,appimage
cargo packager -p roam --release --formats nsis      # 或 wix
```

产物落在 `target/release/`：`Roam.app`、`Roam_0.1.0_aarch64.dmg`。

## 不能上 App Store，因为 sftp

sftp 后端会运行系统 `ssh`（OpenDAL 的 sftp service 是 fork `ssh` 的）。**App Sandbox
不允许沙箱进程启动 bundle 之外的可执行文件**，也没有任何 entitlement 能开这个口子。

这一条是**实测**的。同一个 bundle、同一台 sftp 服务器、同一份代码，只改 entitlements：

| 签名 | sftp 密码认证测试 |
| --- | --- |
| hardened runtime **+ app-sandbox** | **失败**：`failed to connect to the remote host: Operation not permitted (os error 1)` |
| 仅 hardened runtime | 通过 |

顺带一个坑：给**非 bundle** 的二进制加 sandbox entitlement 再运行，进程会以 SIGTRAP
立刻死掉、什么都不输出 —— 那不是"沙箱下 sftp 失败"，那是根本没启动。必须放进真
bundle 才测得出上面那一行。

结论：**签 hardened runtime、不签 sandbox**，走 App Store 之外的分发。想上 App Store
就得砍掉 sftp 后端。

cargo-packager 恰好天然符合这个要求，不需要额外设置：

- 对原生二进制**总是**传 `--options runtime`；
- 只有在配置了 `macos.entitlements` 时才传 `--entitlements`。

所以**把 `entitlements` 留空**就是"不开沙箱"。已验证签名结果：
`flags=0x10000(runtime)`，`app-sandbox` 权限数 0，内层二进制与 bundle 都签。

## 签名与公证

**证书。** 目前这台机器只有一张 **Apple Development** 证书 —— 那是本机开发用的：签出来
的 app 在别人的 Mac 上过不了 Gatekeeper，也**不能公证**。分发需要 **Developer ID
Application**（Apple Developer Program，99 美元/年）。

拿到之后把配置里那行注释打开：

```toml
[package.metadata.packager.macos]
signing-identity = "Developer ID Application: NAME (TEAMID)"
```

身份**字符串本身不是秘密** —— 任何已签名 app 里都能读到它，所以它属于仓库；私钥
（`.p12`）永远不属于。

**公证是凭据驱动的，不写在配置里。** cargo-packager 在找到下面任一组时自动公证，找不到
就只打一行警告继续走：

- `APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID`（应用专用密码）
- `APPLE_API_KEY` + `APPLE_API_ISSUER` + `APPLE_API_KEY_PATH`（App Store Connect API key）
- `APPLE_KEYCHAIN_PROFILE`（`notarytool store-credentials` 存好的 profile）

CI 上导入证书用 `APPLE_CERTIFICATE`（base64 的 .p12）+ `APPLE_CERTIFICATE_PASSWORD`，
不必自己折腾临时钥匙串。

## 构建由 cargo-packager 触发

cargo-packager 只打包、**不构建**，所以 `before-packaging-command` 里写的是
`cargo build --release -p roam`。产出跟着**当前机器的架构**走 —— 现在是 arm64，DMG 名字
里的 `aarch64` 就是它。

不做 universal：那意味着把整棵依赖树（含 gpui）编两遍再 `lipo`，对一个"在哪台机器上构建
就在哪台机器上跑"的工具不值得。真要做的话，是加一个先构建两个 target 再 lipo 到
`target/release/roam` 的脚本，把 `before-packaging-command` 指向它 —— cargo-packager 本身
没有 universal 的概念。

注意 `before-packaging-command` 的工作目录是**包目录**（`crates/roam`）而不是仓库根；这里
用 `cargo build` 不受影响，但换成脚本路径时要算进去。

## 图标

`assets/app-icon/` 下是完整一套：`roam.icns`（macOS）、`roam.ico`（Windows）、
`icon-{16..1024}.png`（Linux hicolor 主题）、以及 `roam-icon.svg` 源文件。配置把它们
全列出来，cargo-packager 按平台各取所需 —— 已验证 bundle 里的 `roam.icns` 与源文件
**字节一致**。

这套图标与 `crates/roam-ui/assets/icons/` 是两回事，别混：后者是 UI 里画的 86 个界面
图标，由 rust-embed 编进二进制。

## 两个会咬人的环境问题

**DMG 需要联网。** cargo-packager 打 DMG 时会去 GitHub 下 `create-dmg`（pin 到某个
commit）。这台机器上 `all_proxy` 指向一个 SOCKS 代理，而它调的 `curl` 不支持，于是报
`SOCKS feature disabled`。绕法：

```sh
env -u all_proxy -u ALL_PROXY cargo packager -p roam --release --formats dmg
```

**`license-file` 别指向不存在的文件。** `Cargo.toml` 里声明了 `license = "MIT"`，但仓库
里**没有 LICENSE 文件**。配置里因此没有 `license-file`；要发布的话这个得补上（涉及版权
署名，留给你定）。

## 其他平台

`roam-core` 本身没有平台障碍（CI 的 Linux job 已经在跑它的测试）。挡在前面的是这些，
都是查过代码而不是推测的：

**Linux** — `deb`/`appimage` 的配置已经就绪，但：

- `gpui_platform` 需要 `x11`/`wayland` feature（当前只开了 `font-kit` 和
  `runtime_shaders`，因为只面向 macOS），构建还需要对应的系统开发包；
- **`roam-ui` 在 Linux 上从未构建过** —— 未验证项，不是"应该没问题"。

**Windows** — 有真实的功能损失：

- sftp 的**密码认证不成立**。那套机制是 POSIX `sh` 写的 askpass + shim
  （`#!/bin/sh`），Windows 的 OpenSSH 没有 askpass 这套东西。密钥认证或许可行
  （Win10+ 自带 `ssh.exe`），要另外验。
- 文件权限那几处是 `#[cfg(unix)]`：`profiles.toml` 的 `0600` 和助手脚本的 `0700` 在
  Windows 上都不生效。而这个文件**装着明文凭据** —— Windows 上得换成 ACL，否则那句
  "仅本人可读"就是假的。

## 已验证 / 未验证

**已验证**（都在本机真做过）：

- `cargo packager -p roam --release --formats app,dmg` 一条命令产出 `Roam.app`（arm64）
  与 13 MB 的 `Roam_0.1.0_aarch64.dmg`；
- Info.plist：`CFBundleIdentifier=dev.roam.Roam`（与 roam-core 里
  `ProjectDirs::from("dev","roam","Roam")` 一致，改它会让已有配置失联）、
  `CFBundleName=Roam`、`0.1.0`、`LSMinimumSystemVersion=11.0`；
- bundle 里的图标与 `assets/app-icon/roam.icns` 字节一致；
- 用 Apple Development 身份签名后：`flags=0x10000(runtime)`、无 sandbox、
  `codesign --verify --strict --deep` 通过、**签名后 app 仍能启动**（窗口 1180×792）；
- DMG 能挂载、含 `/Applications` 链接、**从挂载点直接启动 app 也能开窗口**；
- hardened runtime 下 sftp 密码认证通过；加上 app-sandbox 则失败。

**未验证**：

- **公证与 Gatekeeper**。没有 Developer ID 证书，所以公证路径一次都没跑过；"在别人的
  Mac 上双击能打开"同理未验证。
- Linux 与 Windows 的构建与打包格式（`deb`/`appimage`/`nsis`/`wix` 一个都没跑过）。

[cargo-packager]: https://github.com/crabnebula-dev/cargo-packager

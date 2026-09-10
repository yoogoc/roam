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

## 分发配置

当前配置沿用 hardened runtime，未开启 App Sandbox。SFTP 已移除，应用不再为连接
启动外部 SSH 程序，Linux 安装包也不再依赖 `openssh-client`。此次迁移没有改变签名、
文件访问权限或分发渠道；App Sandbox 和 App Store 分发仍需单独验证。

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

界面图标由 `gpui_kit::assets::Assets` 提供，应用启动时显式注册。
`crates/roam-ui/assets/icons/` 中保留的是旧版资源；当前构建不使用它们。

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

## CI：push 时自动打六个包

`.github/workflows/package.yml`，矩阵是 mac / windows / linux × amd64 / arm64。

触发限定在 **push 到 main、打 tag、以及手动 dispatch** —— 不是所有分支。每个矩阵项都是
一次完整的依赖树构建（含 gpui），六份并行；特性分支不需要安装包。打 tag 时额外有一个
`release` job 把产物挂到 GitHub Release 上。

下表的"状态"一律指**在本机验证到哪一步**。**这份 workflow 本身从未在 runner 上跑过**
（仓库还没有 remote），所以任何一行都不代表"CI 上验证过"。

| 矩阵项 | runner | 格式 | 本机验证到哪 |
| --- | --- | --- | --- |
| macos-arm64 | `macos-15` | app, dmg | 打包+签名+启动，反复跑过 |
| macos-amd64 | `macos-15` 上交叉编译 | app, dmg | 打包跑通，产出 x86_64 的 `Roam_0.1.0_x64.dmg` |
| linux-amd64 | `ubuntu-24.04` | deb, appimage | 见下（容器验的是 arm64，amd64 属推断） |
| linux-arm64 | `ubuntu-24.04-arm` | deb, appimage | **`.deb` 已打成**；AppImage 见下 |
| windows-amd64 | `windows-2022` | nsis | **完全没验过**（没有 Windows 机器） |
| windows-arm64 | `windows-11-arm` | nsis | **完全没验过** |

Intel mac 用**交叉编译**而不是申请 Intel runner：macOS SDK 两个架构都能出，而 Intel
runner 正在退役。本机实测过这条路 —— 产物落在 `target/x86_64-apple-darwin/release/`，
所以工作流里的上传路径统一用 triple 目录。

`before-packaging-command` 会读 `ROAM_BUILD_TARGET`，有值就加 `--target`。因此**同一条
命令**在本地（不设变量，构建 host）和 CI（设了变量，交叉编译）都是对的，不需要维护两套。

**没有验证过的平台标了 `continue-on-error: true`。** 那不是为了让徽章好看：已验证的平台
一旦坏掉照样让整个 run 变红，而没建过的平台不会把它掩盖掉。每个 `true` 都是一句关于
"到底验证到哪"的声明 —— 某个平台第一次成功出包之后，就该把它删掉。

**runner label 我没法在这台机器上验证。** `ubuntu-24.04-arm` 和 `windows-11-arm` 是
GitHub 较新提供的 arm64 runner；如果你的仓库还拿不到它们，那两项会以"找不到 runner"失败，
而不是构建失败 —— 这两种失败看起来很像，别混。

## 其他平台

`roam-core` 本身没有平台障碍（CI 的 Linux job 已经在跑它的测试）。挡在前面的是这些，
都是查过代码而不是推测的：

**Linux** — **编译已经在容器里验证过了**（arm64 的 `rust:1-slim`，`cargo check -p roam
--release` 通过，`roam-core` / `roam-ui` / `roam` 三个 crate 全过，0 错误）。为此做了一件事：
旧版本将 `gpui_platform` 的四个 feature 全部开启；现在由 `gpui-kit` 统一启用。

这四个能同时开是因为它们映射到**按 target cfg 引入**的子 crate ——
`font-kit`/`runtime_shaders` → `gpui_macos`，`x11`/`wayland` → `gpui_linux` —— 所以在
macOS 上开 Linux 那两个不会拉进任何东西（macOS 侧重新验证过，0 错误）。

需要的系统包（这份清单是在容器里试出来的，不是抄的）：

```
pkg-config libssl-dev
libx11-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
libasound2-dev libfontconfig1-dev libfreetype6-dev
libgbm-dev libvulkan-dev
```

**`.deb` 已经在容器里真打出来了**（`roam_0.1.0_arm64.deb`，从 72 MB 的 aarch64 ELF
二进制）。**AppImage 则失败**：linuxdeploy 要挂载自己的 AppImage 来运行，容器里没有 FUSE，
于是 `terminate called after throwing an instance of 'std::logic_error'`。修法是
`APPIMAGE_EXTRACT_AND_RUN=1`（工作流里已设），让它解压而不是挂载 —— runner 上同样会撞这堵墙。
这条修法本身**未验证**：容器里没再跑一次。

另外 cargo-packager **不自动探测 `.deb` 依赖**，只写配置里的 `depends`，不配就产出一个
"能装、跑不起来"的包。配置里按构建依赖列出对应运行时包。

AppImage 的工具链在两个架构下都齐：`AppRun-{x86_64,aarch64}`、
`linuxdeploy-{arch}.AppImage`、`linuxdeploy-plugin-appimage-{arch}.AppImage` 六个资产
都实测返回 200。

**Windows** — 提供与其他平台相同的五种连接类型。`profiles.toml` 的 `0600`
权限设置仍只在 Unix 生效，Windows 的配置文件访问控制需要通过 ACL 单独验证。

## 已验证 / 未验证

**历史验证记录**（迁移前在本机完成，不代表新版依赖的打包结果）：

- `cargo packager -p roam --release --formats app,dmg` 一条命令产出 `Roam.app`（arm64）
  与 13 MB 的 `Roam_0.1.0_aarch64.dmg`；
- Info.plist：`CFBundleIdentifier=dev.roam.Roam`（与 roam-core 里
  `ProjectDirs::from("dev","roam","Roam")` 一致，改它会让已有配置失联）、
  `CFBundleName=Roam`、`0.1.0`、`LSMinimumSystemVersion=11.0`；
- bundle 里的图标与 `assets/app-icon/roam.icns` 字节一致；
- 用 Apple Development 身份签名后：`flags=0x10000(runtime)`、无 sandbox、
  `codesign --verify --strict --deep` 通过、**签名后 app 仍能启动**（窗口 1180×792）；
- DMG 能挂载、含 `/Applications` 链接、**从挂载点直接启动 app 也能开窗口**；

**未验证**：

- **公证与 Gatekeeper**。没有 Developer ID 证书，所以公证路径一次都没跑过；"在别人的
  Mac 上双击能打开"同理未验证。
- Linux 与 Windows 的构建与打包格式（`deb`/`appimage`/`nsis`/`wix` 一个都没跑过）。

[cargo-packager]: https://github.com/crabnebula-dev/cargo-packager

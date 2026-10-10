# 打包与分发

打包用 [cargo-packager]，配置在 `crates/roam/Cargo.toml` 的
`[package.metadata.packager]`。它一套配置覆盖 macOS / Linux / Windows 的所有格式，
所以这里不再维护手写的 bundle 脚本。

```sh
cargo install cargo-packager --locked

cargo build --release -p roam
cargo packager -p roam --release --formats app,dmg   # macOS
cargo packager -p roam --release --formats deb,appimage
cargo packager -p roam --release --formats nsis      # 或 wix
```

产物落在 `target/release/`：`Roam.app`、`Roam_0.1.0_aarch64.dmg`。

## 分发配置

当前配置沿用 hardened runtime，未开启 App Sandbox。SFTP 使用 Rust 的 `russh` /
`russh-sftp`，支持用户名密码认证，不启动外部 SSH 程序，Linux 安装包无需
`openssh-client`。这次接入没有改变签名、
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

## 构建与打包分开执行

cargo-packager 只打包、**不构建**，因此先显式执行 `cargo build --release -p roam`，
再运行 cargo-packager。产出跟着**当前机器的架构**走 —— 现在是 arm64，DMG 名字里的
`aarch64` 就是它。

不做 universal：那意味着把整棵依赖树（含 gpui）编两遍再 `lipo`，对一个"在哪台机器上构建
就在哪台机器上跑"的工具不值得。真要做的话，是加一个先构建两个 target 再 lipo 到
`target/release/roam` 的脚本，再让 cargo-packager 使用合并后的二进制 —— cargo-packager
本身没有 universal 的概念。

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

## CI：按版本标签或手动打包

`.github/workflows/package.yml`，矩阵是 mac / windows / linux × amd64 / arm64。

触发限定在 **push `vX.Y.Z` tag、以及手动 dispatch**。`main` 推送只运行普通 CI
检查，不自动打包或发布。共享 CI
通过后才构建安装包，包和更新清单全部验签成功后上传到草稿 Release，再统一发布。

- `vX.Y.Z`：版本必须与 Cargo 一致，发布正式版并标记为 latest；六个平台都必须
  构建成功。先运行 `python3 scripts/release.py set X.Y.Z`，提交版本变更后创建标签。
- 手动 dispatch：生成 `X.Y.Z-dev.<GITHUB_RUN_NUMBER>` 开发版本的 Actions artifact，
  不发布 Release 或更新清单。Cargo 当前是正式版时，自动使用下一个 patch 作为开发
  目标，例如 `0.1.0` 的手动构建会生成 `0.1.1-dev.123`。
- 已发布的版本不可覆盖，重跑失败的草稿可以继续上传。

`scripts/release.py` 同时修改 workspace 版本与 Cargo.lock 中本地包版本，不更新
依赖。macOS 的 Info.plist 保持系统要求的数字版本，`RoamVersion` 保留完整 SemVer；
Debian 开发版使用 `~dev.N`，保证它排在对应正式版之前。

下表的"状态"一律指**在本机验证到哪一步**；各平台当前的 CI 结果以 GitHub Actions 为准。

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

CI 先用矩阵里的 target 显式执行
`cargo build --release -p roam --target <triple>`，再把同一个 target 传给 cargo-packager。
构建步骤不放在 cargo-packager hook 中：hook 在 Unix 上通过 `sh` 执行，在 Windows 上通过
`cmd.exe` 执行，依赖 shell 变量展开会让其中一侧直接失败。

**开发版允许未验证的平台失败，正式版要求全部平台成功。** 那不是为了让徽章好看：已验证的平台
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

**Windows** — 提供与其他平台相同的连接类型。`profiles.toml` 的 `0600`
权限设置仍只在 Unix 生效，Windows 的配置文件访问控制需要通过 ACL 单独验证。

## 已验证 / 未验证

**历史验证记录**（迁移前在本机完成，不代表新版依赖的打包结果）：

- `cargo build --release -p roam` 后运行
  `cargo packager -p roam --release --formats app,dmg`，产出 `Roam.app`（arm64）
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

## 自动更新

更新实现参考 Beacon，放在独立的 `roam-updater` crate；网络操作运行在 Tokio，
设置页与主窗口共享更新状态。默认在启动后立即检查一次，此后每 24 小时检查；默认不
自动下载。开发版默认选择开发渠道，正式版默认选择稳定渠道，用户选择保存在
与 profiles.toml 同目录的 `updates.toml`。下载缓存位于同目录的 `updates/`。
更新请求默认读取 macOS/Windows 的系统代理设置，也支持 `HTTP_PROXY`、
`HTTPS_PROXY`、`ALL_PROXY` 等环境变量；Linux 使用环境代理配置。更新只选择
高于当前版本的发布，跳过草稿，稳定渠道
还会过滤预发布版本。

设置 → 应用更新提供渠道切换、自动检查/下载、手动检查、下载取消、进度、失败重试、
发布说明、错误复制与安装结果。侧栏更新按钮在有新版本或下载完成时高亮。
安装必须确认重启；正在排队或执行的文件传输会阻止安装，在辅助进程准备前后
都会重新检查。关闭设置页不会取消后台检查或下载。

| 安装形式 | 更新方式 |
| --- | --- |
| macOS `.app` | 下载签名 `.app.tar.gz`，在可写的应用目录中替换并重启，替换或重新打开失败时恢复旧应用 |
| Windows NSIS | 下载签名安装器，退出后显示安装进度，安装到原目录并重新启动 |
| Linux AppImage | 下载签名 AppImage，在原文件系统中替换并重新启动，失败时恢复旧文件 |
| `.deb`、源码与便携二进制 | 检查版本并提供发布页入口，通过包管理器或手动安装 |

辅助进程由当前程序复制而来，在 GPUI/Tokio 初始化前处理 `--roam-update`，等待
主程序释放退出锁后才开始安装。更新清单和包使用 Minisign 验签，下载大小限制为
2 GiB，并与签名清单的长度精确匹配；取消、下载失败和验签失败清除临时下载。
安装前再次验签，重启后显示安装结果。包管理器安装不会被直接覆盖。

### 签名密钥

Roam 使用独立于 Beacon 的长期签名密钥。公钥提交在
`assets/packaging/update-public-key`，由客户端和发布校验器共同内置，CI 不允许
通过环境变量替换信任的公钥。私钥位于仓库之外：
`~/.local/share/roam/update-signing/roam-update.key`，目录权限 0700，私钥权限 0600。
请另外安全备份私钥；GitHub Secrets 不能读回。不要为每次发布重新生成密钥，
已有客户端只信任原来的公钥。

仓库 Secrets：

- `ROAM_UPDATE_PRIVATE_KEY`：cargo-packager 私钥文件的原文。
- `ROAM_UPDATE_PRIVATE_KEY_PASSWORD`：私钥有密码时配置，没有密码则留空。

维护命令（初次配置时才生成密钥）：

```sh
cargo install cargo-packager --version 0.11.8 --locked
cargo packager signer generate --path /secure/location/roam-update.key
# 将 .pub 公钥文件复制到 assets/packaging/update-public-key。
gh secret set ROAM_UPDATE_PRIVATE_KEY --repo yoogoc/roam < /secure/location/roam-update.key
```

发布缺少私钥时会在构建前报错，使用不匹配私钥则在验签阶段失败。手动开发构建允许
未验证平台失败，但不会发布更新清单；正式版要求全部平台完整。这里的更新签名与 Apple Developer ID / 公证及 Windows 代码签名是不同
机制，仍需按前文配置操作系统的分发签名。

参考：[cargo-packager 更新签名](https://docs.crabnebula.dev/packager/updater/)、
[GitHub Release API](https://docs.github.com/en/rest/releases/releases)。

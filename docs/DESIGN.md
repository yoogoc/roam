# Roam 设计方案

跨后端文件浏览器 · OpenDAL + GPUI

**技术栈**（已核对 crates.io，2026-08-14）

| crate | 版本 | 说明 |
| --- | --- | --- |
| `opendal` | 0.58.1 | 0.58.0 已被 yank，直接锁 0.58.1 |
| `gpui` | 0.2.2 | 已正式发布到 crates.io，不必再走 git 依赖 |
| `gpui-component` | 0.5.1 | 60+ 组件，含虚拟化 table/list、dock、tree |
| `tokio` | 1.x | OpenDAL 的 HTTP 类服务必需 |

---

## 1. 总体架构

三层，核心约束是 **Core 层不依赖 gpui**，可以脱离 UI 单独跑集成测试（用 opendal 的 `memory` / `fs` service 做 fixture）。

```
┌──────────────────────────────────────────────┐
│  UI 层  (roam-ui, 依赖 gpui)                  │
│  Workspace · PaneView · TreeView             │
│  TransferPanel · ConnectionDialog            │
├──────────────────────────────────────────────┤
│  Core 层  (roam-core, 纯 Rust + tokio)        │
│  Vfs · ListingCache · TransferEngine         │
│  ProfileStore · Capabilities                 │
├──────────────────────────────────────────────┤
│  Backend 层  (OpenDAL Operator + Layers)      │
│  fs · s3 · gcs · azblob · webdav · sftp ...  │
└──────────────────────────────────────────────┘
```

### Crate 布局

```
roam/
├── Cargo.toml              # workspace
├── crates/
│   ├── roam-core/          # 无 gpui 依赖
│   ├── roam-ui/            # gpui 视图层
│   └── roam/               # bin，负责组装 + 平台入口
└── docs/DESIGN.md
```

---

## 2. 异步桥接（最关键的一处）

GPUI 有自己的 executor（`cx.background_spawn` / `cx.spawn`）。OpenDAL 的网络类 service（s3 / gcs / azblob / webdav）底层是 reqwest，**需要 tokio reactor**。直接把 opendal 的 future 丢到 GPUI executor 上 poll 会 panic：`there is no reactor running`。

方案：进程启动时建一个多线程 tokio Runtime，全局持有 `Handle`。所有 opendal 调用在 tokio 上执行，在 GPUI 上 await —— `tokio::task::JoinHandle` 本身就是 `Future`，可以被任意 executor await，这就是桥。

```rust
// roam-core/src/rt.rs
#[derive(Clone)]
pub struct Rt(tokio::runtime::Handle);

impl Rt {
    pub fn spawn<F, T>(&self, fut: F) -> impl Future<Output = Result<T>> + Send
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.0.spawn(fut);      // 在 tokio 上跑
        async move { handle.await.map_err(Error::from)? }   // 在 gpui 上 await
    }
}
```

UI 侧使用：

```rust
cx.spawn(async move |this, cx| {
    let entries = rt.spawn(async move { vfs.list_all(&path).await }).await?;
    this.update(cx, |this, cx| {
        this.entries = entries;
        cx.notify();
    })
})
.detach_and_log_err(cx);
```

**三条铁律**（写进 CI lint / code review checklist）：

1. `Operator` 永远不在 GPUI 线程上被 await。
2. 任何地方不得在 GPUI 线程上 `Handle::block_on` —— 必死锁。
3. UI 层只持有 `Vfs`（`Clone + Send + Sync`），不持有 `Operator`。

---

## 3. 数据模型

```rust
// roam-core/src/model.rs
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RemotePath {
    pub session: SessionId,
    pub path: Arc<str>,        // opendal 语义：目录必须以 '/' 结尾
}

#[derive(Clone)]
pub struct DirEntry {
    pub name: Arc<str>,        // basename，展示用
    pub path: Arc<str>,        // opendal 完整 path
    pub kind: EntryKind,       // Dir | File | Unknown
    pub size: Option<u64>,
    pub modified: Option<jiff::Timestamp>,
    pub etag: Option<Arc<str>>,
    pub meta_complete: bool,   // false → 需要 lazy stat 补全
}
```

> **时间类型**：opendal 0.58 的 `Metadata::last_modified()` 返回 `Option<opendal::raw::Timestamp>` —— 那是 `jiff::Timestamp` 的 newtype，不是 chrono。依赖 `jiff = "0.2"`，在 `DirEntry::from_parts` 这一个边界上 `.map(Into::into)` 转换。

### 元数据完整性这个坑

opendal 0.58 的 `ListOptions` 只有 `limit / recursive / start_after / versions / deleted` —— 老版本的 `Metakey` 已经没了。**list 返回的 `Metadata` 完整度完全取决于后端**：S3 的 `ListObjectsV2` 会带 size 和 last_modified；有些后端只给 `mode`。

对应设计：

- 表格里 size / mtime 缺失就渲染 `—`，**绝不为了补全字段阻塞首屏**。
- 只对**当前视口内** `meta_complete == false` 的条目发 `stat`，挂在虚拟化列表的可见区间回调上（gpui-component 的 `TableDelegate::visible_rows_changed` 就是这个钩子）。
- 补全请求带并发上限（默认 16）+ 按 path 去重 + 滚出视口即取消。

已实测的后端差异（M1）：

- 本地 `fs` 的 lister **会**逐条 stat，元数据首屏就是完整的，走不到补全路径。所以补全逻辑必须有自己的单元测试（手工构造只带 mode 的 `Metadata`），否则它会是一段没人走过的代码。
- `has_content_length()` 是 crate-private，拿不到「这个 0 是真 0 还是未知」的答案。因此 `meta_complete` 用推断：目录只要 mode 已知就算完整；文件以 `last_modified().is_some()` 作为后端确实返回了元数据的判据。

---

## 4. VFS 抽象

```rust
#[derive(Clone)]
pub struct Vfs { inner: Arc<VfsInner> }   // 内含 Operator + Capability + Rt

impl Vfs {
    pub fn list(&self, path: &str) -> impl Stream<Item = Result<Vec<DirEntry>>>;
    pub async fn stat(&self, path: &str) -> Result<DirEntry>;
    pub async fn read_range(&self, path: &str, r: Range<u64>) -> Result<Bytes>;  // 预览用
    pub async fn reader(&self, path: &str) -> Result<Reader>;
    pub async fn writer(&self, path: &str) -> Result<Writer>;
    pub async fn mkdir(&self, path: &str) -> Result<()>;
    pub async fn rename(&self, from: &str, to: &str) -> Result<()>;
    pub async fn copy(&self, from: &str, to: &str) -> Result<()>;
    pub async fn delete(&self, path: &str) -> Result<()>;
    pub async fn remove_all(&self, path: &str) -> Result<()>;
    pub async fn presign_read(&self, path: &str, dur: Duration) -> Result<Option<Url>>;
}
```

### Capability 驱动 UI

```rust
let cap = operator.info().capability();
// cap.rename / cap.copy / cap.presign / cap.write_can_append / cap.list_with_versions ...
```

> 0.58 的方法名是 `capability()`；旧版本的 `full_capability()` 已经没有了。

这一步不能省。S3 没有原生 rename、fs 没有 presign —— 不查 capability 就会做出一堆点了必然报错的右键菜单项。**菜单项的 enable 状态直接由 capability 推导**，不可用的项显示 disabled 并说明原因，而不是隐藏（隐藏会让用户以为是 bug）。

> **原因写进 label，不用 tooltip**：`gpui-component` 的 `PopupMenuItem` 没有 tooltip；而且 disabled 的行本来就不稳定地触发 hover 事件，tooltip 经常根本看不到。所以渲染成「复制分享链接（该后端不支持分享链接）」。这个拼接由 `MenuItem::display_label()` 负责，可单测。

M2 实测的差异（用 fake 凭据构建 Operator，不发网络请求即可读到 capability）：

| | 本地 `fs` | S3 |
| --- | --- | --- |
| `rename` | ✅ 原生 | ❌ 无 |
| `presign` | ❌ | ✅ |

---

## 5. Operator 构建与分层

```rust
// 注意 from_uri 只接受一个参数：options 以元组形式随 uri 一起传入。
let op = Operator::from_uri((profile.uri.as_str(), options))?
    .layer(RetryLayer::new().with_max_times(3).with_jitter())
    .layer(TimeoutLayer::new().with_timeout(Duration::from_secs(30)))
    .layer(ConcurrentLimitLayer::new(32));
```

> **没有 `.finish()`**：0.58 的 `Operator::new` / `from_uri` 直接返回 `Operator`，`.layer()` 也返回 `Operator`。旧版那套 `OperatorBuilder` + `.finish()` 的写法不再适用。
>
> **`from_uri` 是单参数的**：签名是 `from_uri(uri: impl IntoOperatorUri)`。要带 options 就传元组 —— `IntoOperatorUri` 对 `(&str, O)` / `(String, O)` 都有 impl，其中 `O: IntoIterator<Item = (K, V)>`。

`from_uri` 让「连接配置」可以直接存成一个 URI 字符串 + 一组 options，profile 的序列化非常简单。

### 0.58 的拆包结构

`opendal` 现在只是一个 facade：核心在 `opendal-core`，每个 service 和 layer 都是独立 crate，通过 feature 挂进 `opendal::services::*` / `opendal::layers::*`。实际影响是 feature 必须显式列全 —— `default-features = false` 会同时关掉 `auto-register-services` 和 `executors-tokio`，而 `from_uri` 依赖前者。M1 只开 `services-fs`，**不开** `http-transport-reqwest`（fs 用不到 reqwest，省掉大量编译时间），M2 接云端时再加。

### 凭据（已按用户要求改为不进钥匙串）

> **这一节被推翻过一次。** 原设计是：配置文件只存键名，secret 走系统钥匙串,`Profile::validate()` 按键名片段拦截任何看起来像凭据的 option。用户明确要求**不写入系统钥匙串**,所以现在凭据和其他选项一起存在配置文件里。原方案的「明文永不落盘」不再成立,这里如实记录取舍,而不是留着一段已经失效的安全声明。

配置文件存**全部**连接信息:名称、URI、region、endpoint,以及凭据本身。实际路径由 `directories` 决定（macOS 是 `~/Library/Application Support/dev.roam.Roam/profiles.toml`）；`ROAM_CONFIG` 可覆盖。

- **文件权限 `0600`** —— 现在这是唯一的保护措施,而不再是「文件本来也不含 secret,顺手加固」。
- **哪些字段是凭据由 schema 说了算**,不再靠键名猜。`Profile::secret_keys()` 和表单的掩码用的是同一个来源（`service::Field::is_secret()`）。原来那套片段匹配会把 `access_key_id` 也判成凭据 —— 它其实不是,而 schema 知道这一点。
- **编辑时凭据会回显**。原设计故意留空表示「沿用」,但那是钥匙串时代的产物:值现在就在 profile 里,藏起来只会让「改一下 region」变成「顺手清空密码」。
- **原来的拦截被反转**:`Profile::validate()` 不再拒绝带凭据的 option —— 它当初会拒绝的,恰好就是现在表单产出的每一个 profile。测试也跟着反转了,并且是**显式断言凭据确实写在文件里**（`the_saved_file_does_contain_the_credential`）,而不是留一个模糊的空缺让人推断。

**能拿到这个文件的人就能拿到这些凭据。** 它不适合同步、提交或粘进 issue —— 原来的设计可以,现在不行。UI 上也写了一行说明,不靠用户自己推断。

`Profile::id` 仍是稳定标识,改名不改 id。

### 连接表单:由 schema 生成

原来的新建/编辑连接是两个自由文本框,要手写 URI 和 `key = value` 行。option 名拼错要等到连接时才知道,而且错误信息来自服务端,不指向那一行。

现在 `roam-core/src/service.rs` 一张表驱动三件此前可以各说各话的事:**表单渲染哪些字段**、**保存时校验什么**、**交给 OpenDAL 的 options**。表里的 option 名不是我编的 —— 每一个都是集成测试已经真连上去用过的键。

- `Role` 决定值的去向:`Option` 进 `options`,`UriHost`（bucket / container）和 `UriPrefix`（前缀 / 路径）**组合成 URI**,所以 URI 不用手写。一条规则覆盖全部六个后端:`scheme://{host}/{prefix}`。
- `FieldKind` 决定渲染方式:`Secret` 掩码显示,`Toggle { on, off }` 渲染成开关 —— 因为 `enable_virtual_host_style` 要的是字符串 `"true"`/`"false"`,`known_hosts_strategy` 要的是 `"accept"`/`"strict"`,都不是布尔。
- **切换服务类型会保留两边共有的字段**。s3 → gcs 时 bucket 和 endpoint 留下,S3 独有的 key 不跟过去。
- 必填项缺失在**保存时**就报错并指名字段（"请填写 Access Key ID"),对话框保持打开不丢输入。

写这个表单时找出两个真问题:

- **开关处于关闭态不能写盘。** 原本把 off 值也写进 options,于是「打开编辑、只改 region、保存」会顺手给 profile 盖上一个用户从未表达过的 `enable_virtual_host_style = false`。往返一次必须是恒等变换 —— 这条由 `editing_without_changing_anything_leaves_the_profile_identical` 守着,它就是被这个 bug 逼出来的。
- **fs 的目录该进 URI,不该当 option。** 原来 uri 固定 `fs:///` 加一个 `root` 选项。实测 OpenDAL 的 fs service 直接认 `fs:///path`（真列举验证过),所以目录改用 `UriPrefix`,顺带让手写的 `fs:///tmp` 这种 profile 继续有效。绝对路径需要保留前导斜杠,这由 `Field::absolute` 标记,否则表单会把 `/Users/x` 显示成 `Users/x`。

**对话框不会滚动,也没有按钮。** 表单一变长就露出来了:S3 有 7 个字段,内容直接跑到窗口下面去。这是两个独立的坑,只是同一张表单同时踩中:

- **限高不能加在滚动元素自己身上。** 这条让 bug 活过了两个版本:`.max_h(…).overflow_y_scrollbar()` 写在一起看着天经地义,实际永远不可能滚。`Scrollable` 会把元素的 `max_size` 同时复制到外层包装**和**被滚动的内容上,内容随后按 `h_auto` 排版 —— 一个被限高的内容盒子恰好只有上限那么高,于是永远不会溢出它自己的滚动区:滚动条不出现,滚轮无效,超出上限的行被静默裁掉。所以初版那句「字段列表限高并滚动」其实是「限高并**裁剪**」:先被吃掉的是排在最后的 virtual-host 开关(于是有人报「没有这个开关」),把开关挪到 Endpoint 下面之后,轮到 S3 的两个凭据(于是有人报「凭据不见了」)。同一个 bug,两次不同的症状。
- **该被限高的是对话框。** gpui-component 其实早就把 dialog 的 children 包在滚动区里了 —— `flex_1` + `overflow_hidden` 外面,套一个 `size_full` 的滚动区,注意它**没有**给那个滚动元素加 `max_size`(`overflow_hidden` 让 flex 的自动最小高度变成 0,所以它真的能被压缩)。缺的只是 popup 的一个上限。`Dialog` 实现了 `Styled`,`refine_style` 就落在 popup 上,所以 `.max_h()` 是有效的(`w` / `max_w` 是专门的方法,容易让人以为纵向没接口,其实只是没有同名的那个)。取视口的 0.8:popup 锚在 1/10 高处,正好上下留一样的边。于是标题和按钮固定,整张表单作为一个区域一起滚 —— 表单自己不再限高,也不再挂滚动条。
- **按钮压根没被渲染。** `DialogButtonProps` 的 `ok_text` / `cancel_text` / `show_cancel` 只有 `AlertDialog` 会画成按钮;普通 `Dialog` 只拿它当 Enter / Esc 的回调,`footer` 是 `None` 就什么都不画。所以这个框此前只能 Esc 退出、Enter 保存,鼠标无路可走 —— 而这两个入口都不写在界面上。**全 app 五个对话框都中招**,最糟的是删除确认:它还特意关掉了背景点击和右上角的叉(见下),于是一个专门用来问「确定吗」的框,唯一的出路是按 Esc。现在 footer 由 `ui/src/dialog.rs` 的 `DialogButtons` 扩展 trait 统一给出,点击和 Enter 走同一个闭包 —— 返回 false(校验没过)就不关,输入不丢。有一条测试扫源码,禁止任何 view 再自己写 `DialogButtonProps`。

存储说明从表单底部挪到字段**上面**:它讲的是凭据会被怎么存,该在人输入凭据之前读到,而不是在滚动的另一头。

验证:表单在真实平台文本栈下打开过（`examples/connection_dialog`,现在直接打开 **S3** 这个最高的表单 —— 窗口 672px 高时对话框上限约 538px,而 7 个带标签的字段连同名称 / 类型 / 说明约 690px,所以这个例子真的触发了滚动而非空跑;窗口 900×672,无 abort —— 掩码输入和开关都是新东西,而 placeholder 那次崩溃的教训就是这类排版问题只在真实文本栈下暴露);另有一条**闭环测试**（`a_profile_built_the_way_the_form_builds_it_connects`）把「人会输入的值 → `build_profile` → 真实 MinIO 上传并列举」整条走通 —— 其余测试都是手搓 profile,只验证了 schema 自己,没验证过它对服务器是否成立。

---

## 6. 列举与缓存

- `ListingCache`：`DashMap<RemotePath, CacheEntry>`，`CacheEntry { entries: Arc<Vec<DirEntry>>, loaded_at, complete }`。
- **切目录先出缓存**：命中就瞬时渲染，同时后台重新 list，完成后 diff 更新。用户感受是「秒开」。
- **流式列举**：`lister_with(path)` 逐条消费，每 500 条或每 100ms 通过 channel 推一批给 UI。S3 上一个万级对象的前缀不会白屏。
- **取消**：每次导航 bump 一个 `generation: u64`；旧 generation 的批次到达时直接丢弃，同时 drop 掉 lister 让请求中断。
- **历史**：`Vec<RemotePath>` 的 back/forward 栈，配面包屑。

两个实现细节，都是踩出来的：

- **`list` 会把被列举的目录本身也返回一条**，而且根目录下它的 path 是 `/` 而不是 `""`。过滤必须在归一化后比较（`as_dir(entry.path()) == dir`），直接比原始字符串会漏掉根目录那一条。
- **`list` 一个不存在的目录返回空列表，不是 `NotFound`** —— 对象存储语义，本地 `fs` 也照此实现。所以 `list` 不能用来判断路径是否存在，UI 也不能把「空面板」当成导航成功的证据；真要检查存在性得用 `stat`。

---

## 7. 传输引擎

这是最容易做烂的部分，单独成模块。

```rust
pub enum TaskKind {
    Download  { src: RemotePath, dst: PathBuf },
    Upload    { src: PathBuf,    dst: RemotePath },
    CrossCopy { src: RemotePath, dst: RemotePath },   // 跨后端
    Delete    { target: RemotePath, recursive: bool },
}

pub enum TaskState {
    Queued,
    Running { done: u64, total: Option<u64>, bytes_per_sec: u64 },
    Paused,
    Failed(Arc<Error>),
    Done,
}
```

设计要点：

- **两级并发**：全局 `Semaphore` 限制同时运行的任务数（默认 4）；单个大文件内部用 opendal 的并发 writer 做分片并行 —— `writer_with(path).concurrent(8).chunk(8 * 1024 * 1024)`。
- **跨后端复制**：`src.reader()` → `dst.writer()` 流式 pump，**绝不整读进内存**。
- **同后端复制优先走 `Operator::copy`**（服务端复制，零下行流量）；capability 不支持时才降级到 pump。
- **进度**：每个任务持有 `watch::Sender<Progress>`，UI 端订阅并**节流到 ~10Hz**。每个 chunk 都 `cx.notify()` 会把渲染线程打爆。
- **目录递归**：先 `list_with(path).recursive(true)` 展平成文件级任务列表再入队。这样总量已知，进度条才是准的，而不是一个假的转圈。
- **断点续传**：v1 不做，只靠 RetryLayer 覆盖网络抖动；v2 再基于 multipart upload id 做持久化。

---

## 8. UI 结构

```
Root (TitleBar)
└── Dock
    ├── Left    Sidebar: 连接列表(profiles) + 收藏 + 懒加载目录 Tree
    ├── Center  Workspace
    │             ├── Toolbar: 后退/前进/上级/刷新 + Breadcrumb + 过滤框 + 视图切换
    │             └── Tabs → PaneView (虚拟化 Table: 名称 / 大小 / 修改时间 / 类型)
    ├── Right   详情 + 预览（文本 / 图片 / Markdown）
    └── Bottom  TransferPanel (任务列表 + 进度) · StatusBar
```

组件直接映射到 gpui-component：`dock` `sidebar` `tree` `table`(虚拟化) `breadcrumb` `tab` `input` `menu`(右键) `dialog` `notification` `progress` `spinner` `skeleton` `theme` `highlighter`(预览高亮)。

### 状态与性能

- `Entity<AppState>` 全局：sessions、transfers、config。
- `Entity<PaneView>` 每标签页一个，用 `cx.subscribe` 订阅 AppState 的事件。
- **表格数据不做深拷贝**：条目存 `Arc<Vec<DirEntry>>`，排序 / 过滤只生成 `Vec<u32>` 索引视图。10 万条目录下每帧克隆 Vec 会直接卡死。

---

## 9. 交互

| 动作 | 行为 |
| --- | --- |
| 双击目录 | 进入 |
| Shift+Click / Cmd+Click | 范围选 / 多选 |
| ↑↓ / Enter / Backspace | 移动 / 进入 / 上级 |
| Cmd+R / Cmd+F | 刷新 / 过滤 |
| Space | 快速预览 |
| Delete | 删除（带确认，递归删除需二次确认） |
| 从 Finder 拖入 | 上传 |

- **排序**在本地已加载条目上做，目录优先。
- **过滤**是本地子串匹配，即时。
- **搜索**走 `list recursive` + 客户端匹配，UI 上要明确标出这是一次**全量遍历**、代价高、可随时取消 —— 不能让用户在一个 TB 级 bucket 上无意中点出一次天价遍历。
- **拖出到 Finder** 需要先下载到临时目录再提供 file promise，GPUI 对外部拖出的支持有限，v1 只做拖入。

---

## 10. 错误处理

把 `opendal::ErrorKind` 映射成人话 + 可操作项：

| ErrorKind | 提示 | 操作按钮 |
| --- | --- | --- |
| `NotFound` | 路径已不存在 | 刷新 |
| `PermissionDenied` | 没有访问权限 | 编辑凭据 |
| `RateLimited` | 请求过于频繁 | 稍后重试（自动退避） |
| `ConfigInvalid` | 连接配置有误 | 打开连接设置 |

- 前台操作的错误 → `notification` toast，可展开看详情。
- 后台任务的错误 → 写进传输面板对应行，**不弹窗**。批量失败弹 200 个 toast 是灾难。

---

## 11. 里程碑

| 阶段 | 内容 |
| --- | --- |
| M1 骨架 ✅ | tokio 桥接 + `fs` service + 虚拟化 Table 列举 + 导航 / 面包屑 |
| M2 多后端 ✅ | profile 配置 + keyring + s3 / webdav / gcs + capability 驱动菜单 |
| M3 操作 ✅ | mkdir / rename / delete / copy + 右键菜单 + 确认对话框 |
| M4 传输 ✅ | 传输引擎 + 上传下载 + 进度面板 + 拖入 |
| M5 体验 ✅ | 多标签 + 目录树 + 预览面板 + 主题 + 键盘操作 |
| M6 进阶 ✅ | 跨后端复制 + presign 分享链接 + 版本(versions)浏览 |

M1 建议**只接 `fs` service**：本地目录同样跑通 Vfs 抽象，能把异步桥接和虚拟化列表这两个真正的技术风险先压掉，不被云凭据配置分散注意力。

---

## 12. 风险

| 风险 | 应对 |
| --- | --- |
| GPUI 0.2.x API 仍在演进、文档稀薄 | 锁死版本；用法对照 gpui-component 的 gallery 示例 |
| tokio ↔ GPUI 双 executor 误用 | §2 三条铁律进 review checklist；封装成只暴露 `Rt::spawn` 的单一入口 |
| 10 万级目录卡顿 | 虚拟化 + 流式分批 + 索引视图 —— **已实测，见 §14** |
| list 元数据缺失（§3） | 视口内 lazy stat，UI 容忍 `—` |
| 各后端行为差异（rename / 空目录 / 大小写） | capability 驱动 + 对每个后端跑同一套 Vfs 契约测试 |
| 跨平台 | GPUI 的 Linux 支持仍在完善，v1 先 macOS，Core 层保持平台无关 |

---

## 13. 依赖清单

M1 实际使用的（见根 `Cargo.toml`）：

```toml
[workspace.dependencies]
opendal = { version = "0.58.1", default-features = false, features = [
    "auto-register-services", "executors-tokio", "services-fs",
    "layers-retry", "layers-timeout", "layers-concurrent-limit",
] }
gpui = "0.2.2"
gpui-component = "0.5.1"
# "time" + "net" 是 Runtime::enable_all() 真正需要的
tokio = { version = "1", features = ["rt-multi-thread", "sync", "macros", "time", "net"] }
futures = "0.3"
anyhow = "1"
thiserror = "2"
jiff = "0.2.28"      # 不是 chrono，见 §3
dashmap = "6"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

M2 追加的（已落地）：

```toml
opendal = { ..., features = [..., "http-transport-reqwest",
    "services-s3", "services-gcs", "services-azblob", "services-webdav"] }
serde = { version = "1", features = ["derive"] }
toml = "1.1"          # 不是 0.8
keyring = "4.1"       # 不是 3.x；默认的 v1 feature 保留 v1 API 并自动挂平台 store
directories = "6"
```

`services-sftp` 在 macOS / Linux 上开启，Windows 上不开 —— 它和其他后端不是一类东西，见下。

---

## 14. 落地现状

`cargo test --workspace` **252 项离线通过 + 47 项后端集成**（roam-core 163 + 规模 7 · roam-ui 82 · S3 23 · azblob/gcs/webdav/sftp 22 · UI-S3 2），我们自己的代码无编译警告。

```
crates/roam-core/   无 gpui 依赖 · 67 项测试
  rt.rs        tokio↔gpui 桥（Rt::spawn 单一入口）
  vfs.rs       Vfs::local / from_profile · 流式 list · list_recursive
               · fill_metadata · presign_read · create_dir / rename / copy
               / delete · download_to / upload_from / copy_to · capability
  transfer.rs  TransferEngine · TaskProgress（协作式取消）· 两级并发
               · retry / retry_all_failed · plan_download / plan_upload
               / plan_duplicate_dir / plan_move_dir（目录重命名）
  model.rs     DirEntry / from_parts / sort_indices（索引视图）
  path.rs      OpenDAL 路径归一化 · 面包屑切分 · validate_name · duplicate_name
  preview.rs   预览分类（扩展名 + NUL 字节兜底）· 读取上限
  tree.rs      DirTree：懒加载目录树模型（loaded / expanded 分离）
  model.rs     …… · view_indices（排序 + 过滤的索引视图）· matches_filter
               · ObjectVersion（含删除标记）
  cache.rs     ListingCache（按 session 隔离）
  fmt.rs       字节数 / 时间显示，缺失即 `—`
  profile.rs   Profile / ProfileStore（TOML）· 敏感键拦截 · slugify / unique_id
               · parse_kv_lines
  service.rs   每个后端的字段 schema（表单 · 校验 · options 三者共用）
  menu.rs      capability → 菜单项映射 · display_label · mutates / needs_confirmation
crates/roam-ui/     17 项 gpui 集成测试
  workspace.rs      连接侧边栏 · 会话切换 · 连接对话框 · capability 徽章条
  connection_form.rs schema 驱动的表单视图 → Profile
  browser.rs        工具栏 · 面包屑 · 导航 · generation 守卫 · 右键菜单动作
  delegate.rs       虚拟化 Table 的 TableDelegate · capability 驱动的右键菜单
crates/roam/        main.rs，`roam [目录]`，ROAM_CONFIG 可覆盖配置路径
```

### 已完成的里程碑

**M1** —— tokio 桥接、`fs` service、虚拟化 Table、导航与面包屑。UI 测试用 `gpui::TestAppContext` 真正驱动视图：列举顺序、双击进目录、双击文件不动、两级下钻、上一级到根停住、后退 / 前进回溯、面包屑跟随、陈旧批次被 generation 挡掉、重访目录先出缓存。

**M2** —— profile 配置、凭据存储（当时走 keyring，后按用户要求改为随 profile 落盘，见 §5）、s3 / gcs / azblob / webdav、capability 驱动菜单。测试覆盖：首启无配置文件不报错、连接 profile 切换后端、切回本机不残留上一个后端的列举、缺凭据时**拒绝切换**并报错、切换会话清空历史、保存表单写出的 TOML（这两条后来随 §5 的改动反转了：现在断言凭据确实写在文件里）。

会话切换测试用**两个不同的 fs 后端**（两个临时目录），所以切换是真的换了 Operator，而不是同一后端换路径。

### gpui-component 0.5.1 的 placeholder 崩溃（已规避）

**placeholder 里绝对不能有 `\n`**，否则打开对话框的瞬间进程 abort：

```
panicked at gpui/src/platform/mac/text_system.rs:448:
end byte index 46 is out of bounds for string of length 23
fatal runtime error: failed to initiate panic, error 3, aborting
```

成因在 `gpui-component/src/input/element.rs` 的 `state.text.len() == 0` 分支（输入框为空、显示 placeholder 时）：

```rust
return display_text.to_string().split("\n")
    .map(|line| window.text_system().shape_line(line.into(), font_size, &runs, None))
```

它把 placeholder 按 `\n` 切成多行，却把为**整个字符串**构建的 `runs` 传给每一行。于是 `run.len`（46 字节，整串）超出单行的字节长度（23 字节，第一行），gpui 在 `&text[..][..run.len]` 处越界。这是库的 bug，不是误用，只能规避。

- **非空的值不受影响** —— 那条路径走 `text_wrapper`，按行计算 runs 是正确的。所以多行的 options 值（编辑连接时回填的 `k = v` 多行文本）没问题，只有 placeholder 会崩。
- 多行的填写说明因此放在每个字段下方的 hint 文字里，placeholder 只留一行示例。
- placeholder 提为常量并有单测断言不含换行。单测**测不出这个崩溃**（`TestPlatform` 的 text system 是 stub），所以另加了 `crates/roam-ui/examples/connection_dialog.rs`：它用真实平台文本栈直接打开对话框，排版失败就 abort，跑得起来就是通过。已用「把换行加回去 → 复现同一个 abort → 去掉 → 正常」确认了因果。

顺带两条 `open_dialog` 的调用约束（写这个 example 时踩到的）：

- 不能在窗口构造期间调 —— 那时 `Root` 还没装成窗口根，会 panic `window first layer should be a gpui_component::Root`。
- 不能在 `Root` 的 update 里调（例如 `AnyWindowHandle::update` 的回调里）—— 会 panic `cannot update Root while it is already being updated`。
- 需要在构造后自动打开时，用 `cx.defer_in(window, …)`。

### 所有图标都是不可见的（两层静默失败叠加）

用户报的现象:**新建连接、刷新这些按钮显示不出来,只有鼠标划过才看到一个轮廓**。

根因不在按钮上,在资源加载。`gpui_component::IconName` 用**相对路径**指代图标(`IconName::ArrowLeft` → `"icons/arrow-left.svg"`),但这个 crate **不带 SVG 文件** —— 提供图标是 app 的责任。而我们从来没注册过 `AssetSource`。两层都静默:

1. `Application::new()` 装的是 `()` 作为 asset source,它的 `load()` 对**任何**路径都返回 `Ok(None)`。
2. gpui 的 `svg_renderer.rs` 拿到 `Ok(None)` 就 `return Ok(None)` —— 不画、不报错、不打日志。

于是 app 里**每一个图标**都是空白。工具栏那些按钮是 `.ghost()` 的**纯图标**按钮:静止态背景透明、内容又是空的,所以整个按钮就是一片什么都没有的空隙,只有 hover 画出背景时才「出现」。用户描述的正是这个。

修法:`crates/roam-ui/src/assets.rs` 用 `rust-embed` 把 86 个图标编进二进制(gpui-component 0.5.1 引用的全集)。来源见 `assets/icons/ATTRIBUTION.md`:75 个取自 lucide(ISC),1 个 `github.svg` 取自 Simple Icons(CC0),剩下 10 个是 gpui-component 自己定义、lucide 没有对应物的(`close`、`dash`、`sort-ascending`、`window-*` 等),按 lucide 的几何(24×24、`stroke-width="2"`、圆头圆角)手绘。

**光栅化之后只剩 alpha** —— gpui 把 pixmap 转成 alpha mask、颜色由主题给,所以这些文件里的 `stroke="currentColor"` 是惰性的,不是活的颜色引用。

守卫测试三条,因为「路径能解析」不等于「画得出东西」:

- 图标清单**从源码推导**(扫 `IconName::` 再按 kebab 规则转文件名),而不是手写 —— 手写的清单一定会过期。附一条断言把这套推导和 gpui-component 自己的拼写钉在一起(`Settings2` → `settings-2` 是digit 规则的样本)。
- 断言 vendored 集合是 86 个,因为 gpui-component 自己也会画我们从没点名的图标(表头排序箭头、对话框关闭、滚动条的 resize corner)。
- **用 gpui 内部同一个 `resvg` 0.45 真的光栅化全部 86 个**,断言 16px 下 alpha 覆盖率在 2%–95% 之间。这一条才是手绘那 10 个可信的依据:文件能解析但画不出像素,按钮照样是隐形的。已因果确认:换成合法但空白的 svg → 报 `0.0% coverage`;换成填满的方块 → 报 `100% — that is a filled square`;删掉一个正在用的图标 → 三条测试同时失败并指名是哪个文件。

真实运行验证:给 `load()` 临时加一行打印,跑真 app —— **24 次命中、0 次 MISS**,其中正是 `plus.svg`(新建连接)、`redo.svg`(刷新)、`arrow-*`(后退/上级);窗口 1180×792 在屏。之后撤掉打印。

顺便按要求审了全部 22 个 Button:6 个工具栏纯图标按钮是这个 bug 最明显的受害者;`toggle-theme` 与 `toggle-transfers` 的图标是**条件式**的(`Sun`/`Moon`、`ChevronDown`/`ChevronUp`),用正则扫按钮定义会漏掉它们 —— 这也是清单必须从源码推导的另一个理由。**没有**哪个按钮是既无图标又无文字的,也就是说不存在第二个独立成因。

### M3 的后端约束（都做成了 disabled + 原因）

写这一阶段时撞到三条 OpenDAL 的硬约束，全部按 capability 驱动菜单的原则处理 —— 显示为 disabled 并说明原因，而不是让用户点了收到一个三层之下的报错：

| 动作 | 约束 | 菜单里的显示 |
| --- | --- | --- |
| 重命名目录 | `Operator::rename` 对任何以 `/` 结尾的路径直接返回 `ErrorKind::IsADirectory`，**与后端无关** | 重命名（暂不支持重命名目录） |
| 复制目录 | 单次服务端 copy 做不到，需要遍历 | **M4 已解除**：走传输引擎展平成每文件一次 copy |
| S3 重命名 | S3 没有原生 rename（`capability.rename == false`） | 重命名（该后端不支持重命名） |

S3 的 rename 没有做 copy+delete 的降级：那不是原子操作，中途失败会留下两份。要做的话应该在 M4 的传输引擎里做成一个可见的、有进度和失败处理的任务，而不是伪装成一次重命名。

变更操作在 `Vfs` 层**各自重新检查一遍 capability**，不信任调用方。菜单会禁用不支持的项，但快捷键或以后新增的调用点可能直接进来，`Error::Unsupported` 明确表示「什么都没做」，区别于「做了但失败」。有测试断言拒绝发生在任何请求之前（用 `CapabilityOverrideLayer` 造一个能力全空的 operator，然后确认磁盘上什么都没变）。

### 其他 M3 决定

- **工具栏也有「新建文件夹」**：空目录没有行可以右键，只靠右键菜单的话第一个文件夹永远建不出来。
- **删除是唯一需要确认的动作**，目录用更强的措辞（「及其全部内容将被删除，此操作无法撤销」）。`EntryAction::needs_confirmation()` / `mutates()` 把这些规则放在 core 里，可单测。
- **变更后强制刷新**：`run_mutation` 成功后 invalidate 缓存并重新列举。有测试专门覆盖「先缓存过再删除」的情况 —— 陈旧缓存会继续显示已删除的行，比转一下圈更糟。
- **`pending_mutations` 是真实状态**，不是 fire-and-forget：`Browser::is_busy()` 同时反映列举和变更。这既让 UI 能显示忙碌，也让测试可以等待真实完成，而不是猜一个延时。

### M4：传输引擎的几个关键决定

**取消是协作式的，不是 abort。** 每个 chunk 检查一次 `TaskProgress::check()`，返回 `Error::Cancelled` 让 pump 正常退栈 —— 于是它能删掉自己写了一半的文件、能调 `writer.abort()` 让后端丢弃 multipart upload。如果改成 abort 任务，future 会在 await 点被直接丢弃，没有任何清理机会，留下一堆看起来完整的半截文件和还在计费的孤儿分片。有测试断言取消后本地不留残file。

**进度是轮询的，不是每 chunk 通知。** 面板每 100ms（约 10Hz）拉一次 `engine.snapshot()`，只在 `is_active()` 为真时轮询，空闲时定时器自行退出。一个任务每几毫秒就能完成一个 8MB 分片，逐 chunk `cx.notify()` 会把时间花在重绘上而不是搬字节上。

**速度取自启动至今的平均值**，不是瞬时值 —— 瞬时值随每个 chunk 剧烈抖动，读起来没法用。

**总量未知时不画进度条。** 后端没给大小就只显示已传字节数加「大小未知」，画一个满格或乱跳的条是撒谎。`fraction()` 同时把超过 100% 的情况夹住（后端少报大小时会发生）。

**目录先展平再入队。** `plan_download` / `plan_duplicate_dir` 先 `list recursive` 展成文件级任务列表再入队，所以任务数和总字节数从一开始就是准的。一个自己会不断长大的进度条比开始前停顿一下更糟。

**同后端复制走服务端 copy**，字节完全不经过本进程 —— 零下行流量、一次请求。跨后端才流式 pump，而且是逐 chunk，绝不整读进内存（10GB 的复制不该需要 10GB 内存）。`same_backend` 用 `Arc::ptr_eq` 判断。

**M3 推迟的目录复制现在能做了**：`创建副本` 对目录展平成每文件一次服务端 copy，走传输引擎，有进度也能取消。菜单里对应的 disabled 已经去掉。

### 又踩了一次自己定的铁律

`list_recursive` 我一开始写成直接 `await` operator 的 `async fn`，而它是从 GPUI executor 上的规划逻辑调用的 —— 于是 §2 那个 panic 如实出现：

```
there is no reactor running, must be called from the context of a Tokio 1.x runtime
   at opendal-layer-timeout/src/lib.rs:331
```

**铁律一（Operator 永远不在 GPUI 线程上被 await）不是写着好看的。** 修法是让 `list_recursive` 像其他公开方法一样走 `rt.spawn`。另外给引擎专用的三个 pump（`download_to` / `upload_from` / `copy_to`）加了 `debug_assert!(Handle::try_current().is_ok())`，下次误用时报错会直指这条规则，而不是甩出 opendal 那句不知所云的 reactor panic。

顺带修了另一处：`into_bytes_stream` 的错误是 `io::Error`，里面裹着真正的 `opendal::Error`。我原来一律包成「本地文件操作失败」，于是远端 404 会显示成本地文件问题、还丢掉了 `ErrorKind`（`recovery()` 就不会提示刷新）。现在用 `io::Error::downcast` 把它取回来。

### M5 的决定与踩到的坑

**`read_with().range(0..limit)` 在 limit 超过文件长度时报 `RangeNotSatisfied`。** 预览用的「最多读 N 字节」原本就是这么写的 —— 结果所有小于上限的文本文件（也就是几乎全部）都预览失败。改成流式读到上限就停：丢弃 stream 会取消剩余请求，上界依然是真的。这个坑被单测抓到（一个 2 字节的文件）。

**分类靠扩展名，但字节有最终否决权。** 未知扩展名一律先当文本尝试，读回来后用 NUL 字节检测拦下二进制 —— 比「不认识就不预览」有用得多。有测试专门覆盖「未标注扩展名的二进制」。

**截断要说出来。** 预览只读前 128KB，超出时底部明确写「只显示前 128 KB」。悄悄停在中间会让人以为文件本身就是断的。

**图片有独立且更高的上限（8MB）**，因为解码必须拿到完整文件；超过就拒绝并说明原因 —— 因为按了下空格就拉 40MB 过来不叫预览，叫下载。

**键盘绑定与动作分离，且必须显式安装。** `roam_ui::init(cx)` 负责 `bind_keys`；没调用的话每个快捷键都静默失效 —— 这种 bug 能活很久。所以 bin 和每个 example 都调它，测试的 harness 也调，UI 测试是**真的派发按键**走绑定，而不是直接调 handler。

**快捷键只作用于 `Browser` key context**，所以在过滤框或对话框里打字不会触发导航。Table 自己在更内层的 context 绑了 ↑↓←→，保持不动。

**没有给重命名绑快捷键。** 这个面板里 Enter 是「进入」；让 Enter 依焦点有第二种含义，正是人们误改文件名的方式。重命名留在右键菜单。

**过滤走索引视图**（`view_indices`），条目本身从不改动，所以清空过滤是零成本的。流式到达的新行也要过一遍当前过滤，否则边加载边输入会漏进不匹配的行。状态栏在过滤时显示「3 / 42 项」而不只是「3 项」—— 后者看着像目录里只有三个东西。

**目录树只列展开过的目录**，在大 bucket 上打开侧边栏只花一次请求。「已加载」和「已展开」是两个独立的事实：没列过的节点不会声称自己是叶子（否则所有折叠节点都会失去展开三角），列举失败也不记录 children，于是再展开一次是重试而不是冻结成空节点。

### 多标签：每个标签是独立会话

这是这一步最重要的决定 —— **标签之间会话独立**，一个标签在本机、另一个在 S3 是正常状态。这正是让 M4 的跨后端复制在界面上真正可达的前提：两个后端同时开着，才谈得上在它们之间搬东西。

由此带来的一串连带改动：

- `连接` 只切换**当前标签**的会话，其他标签保持原样。
- 侧边栏的高亮、capability 徽章条、标题都读活动标签。
- 目录树跟随活动标签：切标签时重置到那个标签的会话与目录。**后台标签列举完成不能移动侧边栏** —— 所以 `on_directory_changed` 带上标签 id 经 Workspace 路由，由它判断是不是活动标签。
- 新标签**开在当前目录**，而不是根目录：后者会把你刚做的导航丢掉。
- **最后一个标签关不掉**。一个空窗口且没有回去的路，不是值得能到达的状态。
- 删除 profile 时，所有指向它的标签一起回落到本机 —— 不只是活动的那个。

标签栏用了 `gpui-component` 的 `TabBar`，关闭按钮走 `Tab::suffix`。

> **一处偏离设计**：目录树没用 `gpui-component` 的 `TreeView`。它的 `set_items` 要求每次传入完整的递归 `TreeItem` 树，而懒加载的数据天然是「扁平 + 深度」并且每个节点还要独立的 loading 状态。自己渲染缩进行代码更少、状态更清楚。视觉结果一致。

### 对真实后端协议的验证（不需要你的云账号）

前五个里程碑的代码**从未面对真实网络** —— 所有测试都跑在 OpenDAL 的本地 `fs` service 上，签名、XML 解析、列举分页、multipart、presign 一行都没真正执行过。这是当时最大的未知。

它不需要真实云凭据：本地跑对应的兼容服务就够了，HTTP 路径是真的。`scripts/test-backends.sh` 负责起停四个后端（MinIO / Azurite / fake-gcs-server / WebDAV）：

```
scripts/test-backends.sh test all       # 起全部服务并跑集成测试
scripts/test-backends.sh test webdav    # 只跑一个
scripts/test-backends.sh up             # 只起服务，打印要 export 的环境变量
scripts/test-backends.sh down           # 停掉并清理
```

> Azurite 不会按需创建容器，OpenDAL 也不会，所以脚本里用一段 SharedKey 签名的 PUT 把容器建出来。这是接 azblob 时唯一需要额外处理的一步。

测试用 `ROAM_S3_ENDPOINT` 门控，没设就打印一行 skip 后返回 —— 所以 `cargo test` 在没有 Docker 的机器上照样全绿，而不是失败或静默通过。

13 项 core 集成测试 + 2 项 UI 测试，覆盖：

| 验证的东西 | 为什么之前是未知 |
| --- | --- |
| 写入 / 读取往返 | 请求签名从未执行 |
| `presign` / `presign_read` 为真、`rename` 为假 | capability 之前只从本地构造的 Operator 读过 |
| **list 确实带 size 与 last_modified** | §3 的核心假设，一直只是假设 |
| prefix 表现为目录（无自身对象也算） | 对象存储没有真目录 |
| **超过 1000 键的分页**（上传 1005 个对象） | continuation token 从未走过 |
| multipart 上传（CHUNK+4096）字节精确往返 | 分片边界是最容易错的地方 |
| 服务端复制 | 之前只在 fs 上验证 |
| 递归删除整个 prefix | S3 的批量删除语义 |
| **presign URL 用无凭据的客户端能取到** | 分享链接的全部意义所在 |
| `NotFound` / `PermissionDenied` 映射成的**那句用户看到的话** | 错误映射之前只对 fs 验证过 |
| S3 → 本地的流式跨后端复制 | 两个真后端之间的 pump |
| UI 层连接真实 S3 并列举、错误凭据在面板上报错 | Workspace::connect 从未面对真服务器 |

**S3 全部一次通过，代码没有改动。** 但接上 azblob / gcs / webdav 之后就不是了 —— 见下面两条。

配置上唯一的坑：MinIO 用 path-style 寻址，profile 里要 `enable_virtual_host_style = false`；真实 AWS S3 不需要。

### 各后端真实上报的 capability

之前这张表是靠猜的，现在是从真服务器读回来的：

| | `write_can_multi` | `copy` | `rename` | `presign` | `create_dir` |
| --- | --- | --- | --- | --- | --- |
| 本地 `fs` | — | ✅ | ✅ | ❌ | ✅ |
| s3 | ✅ | ✅ | ❌ | ✅ | ✅ |
| azblob | ✅ | ✅ | ❌ | **❌** | ✅ |
| gcs | ✅ | ✅ | ❌ | ✅ | ✅ |
| webdav | **❌** | ✅ | **✅** | ❌ | ✅ |

两个和「云 = 都一样」直觉相反的事实：**azblob 不支持 presign**（所以分享链接在它上面是灰的，这正是 capability 驱动菜单该做的事），**WebDAV 支持 rename 而对象存储都不支持**。

### 接 WebDAV 时找出的真 bug

`upload_from` 无条件请求 `.concurrent(8).chunk(8MB)`。对 `write_can_multi == false` 的后端（WebDAV 就是），**任何超过 8MB 的上传都会失败**：

```
Unsupported (persistent) at write => OneShotWriter doesn't support multiple write
```

8MB 是日常大小，不是边缘情况 —— 这会真实地影响用户，而且五个里程碑都没发现，因为之前只在支持分片的后端上测过。

修法是按 `write_can_multi` 分流：一次性写入的后端走单请求路径（`upload_from` 与跨后端 `pump` 两处都要）。代价是**正文必须整份进内存**，所以加了 `ONE_SHOT_LIMIT`（512MB）上限，超过就报一句清楚的话而不是把进程 OOM 掉；这类后端的进度也只能从 0 直接跳到完成，因为没有分片可数。

回归测试不依赖网络：用 `CapabilityOverrideLayer` 把本地 fs 的 `write_can_multi` 改成 false 就能复现。

### fake-gcs-server 的缺口（未验证的部分）

OpenDAL 的 GCS 并发 writer 走 **XML API** 的 multipart 端点（`POST ...?uploads`），而 fake-gcs-server 对它返回 404 —— 它实现的是 JSON API 的 resumable upload。真实 GCS 两者都支持，所以这是 emulator 的缺口而不是缺陷，但**GCS 的大对象路径因此在本地无法验证**。那条测试保留着，用 `ROAM_GCS_MULTIPART=1` 开启，指向真实 GCS 时才跑。

### M6：版本浏览

菜单项由 `capability.list_with_versions` 门控 —— 本地 fs 上是灰的并写明原因，S3 上可用。目录永远没有版本历史（版本是对象级的，前缀不是能有版本的东西）。

三个模型上的决定：

- **删除标记是历史的一部分。** 列举时带 `deleted(true)`，否则「删过又恢复」的对象看起来像从没被删过。删除标记是事件而不是内容，所以它没有大小 —— UI 那一列显示「删除标记」而不是 `0 B`。
- **恢复是追加而不是回滚。** 版本化存储没有 revert 操作，恢复旧版本就是把旧字节写回去，这本身又产生一个新版本。所以提示写的是「已恢复该版本（作为新版本写入）」，而不是含糊的「已恢复」。有测试断言恢复后版本数 +1。
- **当前版本置顶。** 那是普通读取会拿到的那一个，应该在视线落点上。

### 在版本化 bucket 上发现的行为差异

启用 versioning 后，一条原本通过的测试失败了：**递归删除目录后，目录行仍然在列举里**。

我用原始 XML 查清了原因：删除是**成功的** —— `doomed/` 下每个对象的最新版本都是 DeleteMarker，没有活对象。但 `ListObjectsV2` 依然把 `rmrf/doomed/` 作为 CommonPrefix 返回。所以用户删掉一个目录、界面刷新、目录还在，看起来像失败了。

我没有用客户端过滤去粉饰它 —— 那是对后端状态撒谎，而且下次真刷新时它又会回来。做法是**把结果解释清楚**：在版本化后端上删除目录后，提示写「已删除目录内容；该后端保留版本历史，空目录名可能仍会显示」。非版本化后端保持原来简短的「删除完成」。

### 版本化 bucket 上的测试隔离

**版本历史是 append-only 的，删除也清不掉。** 所以固定前缀的测试第二次运行必然失败（我第一次就撞上了：期望 3 个版本、实际 6 个）。版本相关的测试改用带纳秒时间戳的唯一前缀，并实际验证了连跑两次都通过。

这不只是测试技巧 —— 它意味着在版本化 bucket 上，任何「清理」的假设都是错的。

### 断点续传

**下载能真续传，上传只能重试。** 这个区分不是偷懒：OpenDAL 不暴露 multipart upload id 与已上传分片的 etag，没有它无法接续一个已开始的上传会话。所以传输面板上「重试」对下载是续传、对上传是重来 —— 代码注释和 UI 行为都按这个说。

下载续传的设计：

- 字节先写进 `{名字}.roampart`，**只有全部到位才改名成真名**。所以一个下了一半的文件永远不会占用真实文件名 —— 看起来完整的半截文件比没有文件更糟。
- **取消删掉分片**（用户说停了），**失败保留分片**（这才让重试变成续传）。
- 续传前必须确认对象没变：第一次下载把 etag 写进 `{分片}.etag` 侧车文件，续传请求带 `If-Match`。拿不到可用 etag 就从零重下，而不是冒险把两个版本拼在一起。

### 续传踩到的两个真实陷阱

**弱 etag 不能用作续传凭据。** Apache 默认对刚修改的文件发 `W/"..."` 形式的弱 etag。弱 etag 只承诺语义等价、不承诺字节一致 —— 而追加写入恰恰需要后者；而且 RFC 7232 规定 `If-Match` 用强比较，弱 etag 永远匹配不上，服务器直接回 412。所以 `strong_etag()` 会把 `W/` 前缀的 etag 当作「没有 etag」。

**服务器可能在两次请求之间改变 etag。** 更麻烦的是：WebDAV 上 `stat` 返回强 etag，紧接着带 `If-Match` 的 GET 却回 412，而响应里的 etag 已经变成弱的、mtime 后缀也不同 —— Apache 因为文件刚被修改而切换了 etag 形式。

如果把 412 当失败，一个本来可以续传的下载就变成硬错误。**412 的正确含义是「你的分片过期了」，所以应该退回整体重下。** 现在就是这么做的：最坏情况是重新下载，永远不会因此失败。

这两条都是只有真实服务器才会暴露的东西 —— 我第一版实现在 MinIO 上全绿，接上 Apache 才炸。顺带说明另一件事：我最初还在**每次**下载都发 `If-Match`（包括没有分片要保护的全新下载），那是我引入的回归，会让所有 WebDAV 下载失败。现在只在真正续传时才发。

WebDAV 那条集成测试因此**断言不变量而不是路径**：无论走续传（强 etag）还是重下（弱 etag），落盘文件必须是对象的精确字节。断言走哪条路会让测试依赖服务器的时序。

### 重命名目录

没有任何后端能一次调用重命名目录（`Operator::rename` 对以 `/` 结尾的路径一律拒绝）。所以它是**遍历 + 复制 + 删除**，做成传输面板里可见、有进度、可取消的任务，而不是伪装成一次原子重命名。

关键设计是**一个任务而不是一批**：引擎没有任务依赖，如果把「一堆复制」和「一个删除」分别入队，删除可能在复制完成前就跑起来。合成单任务同时保证了另一件更重要的事 —— **删除只在每个复制都成功之后才执行**。

于是失败与取消的语义是安全的：**源目录完好，目标处有部分副本，什么都没丢**，重跑会覆盖那些副本。这一条在本地 fs 和真实 S3 上都有测试断言。

文件的重命名仍然走原生 `rename`。文件恰恰是「非原子」换不来任何好处的情况：`copy` 本身就能把它放到想要的位置，所以「创建副本 + 删除」是更诚实的两步，而不是把它包装成重命名。

菜单门控也随之变了：目录的重命名要求 `list && copy && delete`，而不是 `rename`。

### sftp：唯一不走 HTTP 的后端

`opendal-service-sftp` 建立在 `openssh` crate 上，而后者**调用系统的 `ssh` 二进制**。这不是一个纯 Rust 客户端，带来四个和其他后端不同的约束：

- **Windows 上根本不存在这个后端。** `openssh` 是 Unix-only 的，连编译都不过，所以 `services-sftp` 不能写在 workspace 清单里 —— 它挂在 `crates/roam-core/Cargo.toml` 的 `[target.'cfg(not(windows))'.dependencies]` 下。代码这一侧跟着走同一条 cfg：`service::SERVICES` 在 Windows 上少一项（表单因此不会给出一个连不上的选项，`for_scheme` 也就不再认识这个 scheme），`sftp_auth` 整个模块不编译，`Vfs::from_profile` 遇到 `sftp://` 直接给出「Windows 版不支持 SFTP」而不是 OpenDAL 的「unsupported scheme」—— 后者说的是现象，不是原因，而这个原因用户改不了。旧的 profile 仍然留在侧栏里（`load` 从不校验），点开时才解释自己。
- 运行时依赖宿主机 `PATH` 里有可用的 `ssh` / `sftp`；
- 主机密钥校验是系统的，所以连一台新服务器需要 `known_hosts_strategy` 策略（测试里用 `accept`）；
- 认证走 ssh 的方式，**密钥必须是磁盘上的文件**，所以 profile 里存的是密钥**路径**而不是密钥内容。这原本是「凭据只进钥匙串」原则的一个例外；那条原则现在已经不在了（见 §5），所以它也不再是例外 —— 表单把它渲染成一个路径字段。

它的 capability 也和别人都不一样：**唯一同时支持分片写和原生重命名的后端**。

| | `write_can_multi` | `copy` | `rename` | `presign` |
| --- | --- | --- | --- | --- |
| s3 / gcs | ✅ | ✅ | ❌ | ✅ |
| azblob | ✅ | ✅ | ❌ | ❌ |
| webdav | ❌ | ✅ | ✅ | ❌ |
| **sftp** | **✅** | ✅ | **✅** | ❌ |

### 起测试服务时踩到的三个坑

都是脚本层面的，但每一个都会让「集成测试跑不起来」看起来像代码坏了：

- **macOS 的 `TMPDIR` 不能拿来做 Docker bind mount。** 它解析成 `/var/folders/...`，而 Docker Desktop 默认不共享这个路径 —— mount 会静默失效，MinIO 报 `Unable to use the drive /data: drive not found`。数据目录改到仓库内的 `target/test-backends`（已被 gitignore，且必然在共享范围内）。
- **只有正在运行的容器值得复用。** 原来的脚本对已存在的容器一律 `docker start`，但 `down` 会删掉数据目录，于是旧容器的 bind mount 指向一个已经不是原样的路径。现在停止的容器一律重建。
- **`set -o pipefail` 会吃掉 `cmd | grep -q` 的成功。** sftp 的就绪探测是 `ssh ... | grep -q "sftp connections only"`；`ssh` 在这里必然非零退出，pipefail 就让整条管道失败，即使 grep 匹配上了。改成先把输出收进变量再判断。

### clippy

第一次跑 `cargo clippy --workspace --all-targets` 挑出 6 处，都是整洁度问题而非缺陷：两处过于复杂的闭包类型（提成 `type` 别名）、一次 `Copy` 类型上的 `clone`、一处多余的 `to_string`、一个可以用 `sort_by_key` 的比较闭包、一个多余闭包，以及两个不必要的 `mut`。其中一个 `mut` 的根因是访问器多要了 `&mut self` —— 那才是真正该改的地方，而不是在调用处加 `mut`。现在 clippy 全清（只余第三方 crate 的 future-incompat 提示）。

### 10 万级目录：把声称的性能真的测了

§12 一直把「10 万级目录卡顿」列为风险，缓解写的是「压测」，但我从没测过 —— 这是文档里唯一**声称已缓解却没有数字**的地方。现在有了。

release 构建，Apple Silicon，10 万条目：

| 操作 | 耗时 |
| --- | --- |
| 按名称排序（含目录优先） | 83 ms |
| 按大小排序 | 5.9 ms |
| 过滤 | 6.6 ms |
| 清空过滤 | 82 ms |
| 反复排序十次的最差单次 | 95 ms |
| 索引视图内存 | 390 KB（条目本身 9375 KB，不含字符串） |
| 10 万 / 1 万 耗时比 | **12.3×**（n log n 预测 ~12×） |

那个 12.3× 是最有意义的一条：它排除了二次行为。如果排序或过滤在某处偷偷克隆了每行，这个比值会是 100× 量级。

真实磁盘上的 10 万文件目录（`ROAM_SCALE_FS=1` 开启，会真的建 10 万个文件）：

| | |
| --- | --- |
| **首批到达** | **6.9 ms** |
| 全部扫完 | 1.13 s |
| 批数 | 200 |

**首批 6.9 ms、全量 1.13 s** —— 相差 163 倍。这正是流式分批要达到的效果：面板几乎立刻有内容，而不是等一秒多的白屏。

UI 侧（`EntriesDelegate`）：10 万行分 200 批流入共 **8 ms**，过滤 22 ms，清空 10 ms。8 ms 这个数字本身就是证据 —— 如果 `extend` 每批都重排一次，200 次 × 递增规模会让它变成秒级。

最后是真实 app：用 release 构建打开一个 **6 万文件**的目录，窗口正常起来，列举完成后**空闲 CPU 0.4%、RSS 148 MB**。空闲 CPU 这一项是特意看的 —— 传输面板有 10Hz 轮询、GPUI 有渲染循环，两者任一没有正确停下来都会表现为持续占用 CPU。

这些阈值故意设得很松（多为 5 秒）。它们要抓的是**成本形状的变化**（多出一次每行克隆、退化成二次），不是在忙碌机器上计较几毫秒。`--nocapture` 会打印真实数字。

### 工程卫生：格式、CI

**rustfmt 之前也从没跑过**（和 clippy 一样）。`cargo fmt --all` 改了 82 处；测试与 clippy 在格式化后仍全绿，确认是纯格式变更。顺手发现 `plan_move_dir` 漏在了顶层 re-export 之外 —— 测试是通过 `roam_core::transfer::` 直接引用的，所以没暴露。

CI（`.github/workflows/ci.yml`）沿着代码本身的接缝切分：

- **core（Linux）** —— `roam-core` 不依赖 gpui，所以在没有任何图形栈的 runner 上就能构建。后端集成测试也放这里，因为它们需要的服务全是 Linux 容器。
- **ui（macOS）** —— 视图层需要 gpui，跑在 app 真正发布的平台上。

写 CI 时修掉了两处会让它**静默坏掉**的问题：

- `up` 原本把进度点和变量一起打到 stdout，而 CI 要把 stdout 追加进 `$GITHUB_ENV`。进度改走 stderr，并加了输出裸 `KEY=value`（`$GITHUB_ENV` 不接受 `export`）的形式。
- azblob 的 key 在人类可 `eval` 的形式里带单引号（它含 `/` 和 `+`）。`$GITHUB_ENV` 取字面值，引号会变成 key 的一部分 —— CI 上会得到一个签名错误，而错误信息完全不指向真正的原因。bare 形式现在会去掉引号，并有长度断言（88 而非 90）确认。
- 另外把 `-D warnings` 从全局 `RUSTFLAGS` 移到只传给 clippy。全局的话它会作用于整棵依赖树，一次上游发布就能让 CI 因与本项目无关的原因失败，而失败看起来像是我们的问题。

### 把 workflow 真的在 runner 上跑了一遍

上面那段原本写着「这份 workflow 从未在 runner 上执行过，YAML 接线仍是未验证的」。这个缺口不需要 remote 也能消除 —— `act` 能用 Docker 在本地跑真的 GitHub Actions。跑完 `core` job：**`Job succeeded`**，全部 step 通过。

顺带验证了一件此前只是**声称**的事：`roam-core` 不依赖 gpui、能在没有任何图形栈的 Linux 上构建。这是 CI 拆分的前提，但在此之前从没在 Linux 上验证过。runner 上（`aarch64-unknown-linux-gnu`，rustc 1.97.1）的实际结果：

| step | 结果 |
| --- | --- |
| `fmt --check` / `clippy -D warnings` | ✅ |
| `cargo test -p roam-core` | ✅ 163 + 22 + 23 + 7 |
| 起五个后端服务 | ✅ 15.7s |
| S3 集成 | ✅ 23 |
| azblob / gcs / webdav / sftp 集成 | ✅ 22 |
| release scale（10 万条目） | ✅ 首批 13.3 ms，总计 3.12 s |
| `if: always()` 收尾 | ✅ 前一步失败时也确实执行了 |

scale 在容器里比 macOS 慢（13.3 ms / 3.12 s，对 6.08 ms / 1.16 s），但比值形状不变 —— 阈值本来就是按「成本形状」设的，所以照样过。

**这一轮找出三个真 bug，同一个根因**：`docker run -v` 的源路径是在 **daemon 的**文件系统里解析的，不是在执行 step 的那个文件系统里。当 step 直接跑在宿主上（本地 macOS、或 GitHub 的普通 runner）两者恰好相同，问题完全不可见；一旦 step 跑在容器里（嵌套 runner、self-hosted 的容器化 runner），bind mount 就会静默地交付一个**空目录**。用一个最小实验单独确认过：在挂了宿主 `docker.sock` 的容器里写一个文件，再把该路径挂进另一个容器 —— 内层看得见，挂进去是空的。

- **sftp 起不来**（唯一会报错的一个，因为它必须**读到**我们刚生成的公钥）。改成 `docker cp` 把公钥送进容器，走 Docker API，两种情况都成立。数据目录挂成空的无害，公钥挂成空的就是「sshd 永远不接受我们、然后就绪探测超时，而且没有任何信息指向真正的原因」。
- **MinIO 的 bucket 不存在** —— 原来靠「在数据目录里 mkdir 一个同名目录」来建桶。改成走 S3 API（`PUT /bucket`）。顺带这也去掉了对「目录 = bucket」这个 MinIO 后端布局实现细节的依赖。
- **fake-gcs 的 bucket 不存在** —— 同上，改成走 JSON API。

还修掉一个**假信号**：原来 versioning 的 PUT 在 bucket 并不存在时也打印 `versioning enabled (200)`，脚本看上去成功、然后 18 个测试全挂在 `NoSuchBucket` 上，错误指向测试而不是脚本。现在建桶是显式的一步，失败就非零退出并打出响应体。

**仍未验证的**：这是 `act` 在 arm64 Linux 容器里跑的，不是 GitHub 托管的 x86_64 `ubuntu-latest`；`Swatinem/rust-cache` 在本地没有 cache 服务，实际是空转。**`ui` job 用 act 验证没有意义** —— act 没有 macOS runner 镜像，它会去问「用哪个 Linux 镜像」，而拿 Linux 跑 gpui 只会给出一个假通过；不过那个 job 的宿主就是 macOS，它的每条命令本来就在本机直接跑过。

### 升级到 gpui-component git main：UI 测试在 macOS 上被上游挡住

依赖已切到上游 main（`gpui-component` / `gpui-base` / `gpui-component-assets` @ `bd83329`，`gpui` / `gpui_platform` 来自 zed @ `bc538de`）。**app 本身正常**：构建通过、真机跑起来、窗口在屏、`roam-core` 229 个测试全绿。

但 `roam-ui` 的 92 个测试里有 75 个在 macOS 上 panic：

```
not implemented: Test Windows are not backed by a real platform window
  gpui::platform::test::window: <TestWindow as HasWindowHandle>::window_handle
  gpui_base::macos_accessibility::install_window_hit_test_forwarder
  gpui_component::root::Root::new          ← 每个建 Root 的测试都会走到
```

链条是清楚的：`Root::new` 在 macOS 上装一个无障碍 hit-test 转发器，需要真实 NSView；gpui 的测试窗口没有，于是 `unimplemented!()`。

**两边各有一处问题，而且都不在我们这边：**

- 上游 gpui-component 其实**写得很稳**：`ns_view()` 用的是 `HasWindowHandle::window_handle(window).ok()?`，本来就容错。是 **zed 的 `TestWindow` 用 `unimplemented!()` panic，而不是按 trait 契约返回 `Err`** —— 一共两行（`platform/test/window.rs:55` 与 `:63`）。zed 当前 `origin/main` 上这两行依然如此，所以升级 zed 也解决不了。
- 上游确实加了守卫 `#[cfg(all(target_os = "macos", not(test)))]`，但 **`cfg(test)` 是按 crate 生效的**：gpui-base 作为我们测试二进制的普通依赖被编译时并没有 `test`，所以那个守卫只保护上游自己的单测，保护不到任何下游使用者。

**为什么没有免 fork 的绕法**（都查过了）：`Root::new` 是唯一构造入口；gpui-component / gpui-base 都没有可关掉无障碍的 feature；`TestAppContext::build` 里 `TestPlatform` 是硬编码的，不能注入平台；去掉脚手架里的 `Root` 会伤到大半测试 —— `connect`、`delete_entry`、`create_folder`、`rename_entry`、`download`、`save_form` 这些核心路径都会推送通知或开对话框，全都要求窗口第一层是 `Root`。

按 cfg 推断，**Linux 上不受影响**（那段整体是 `cfg(target_os = "macos")`）——但这一条**未验证**，在容器里构建 Linux 版 gpui 需要 x11/wayland 等系统库，没有实际跑过。

**处理方式(用户选择：保留 git main，等上游修)**：CI 的 macOS `ui` job 把 `cargo test --workspace` 标为 `continue-on-error`，并在它后面加了一条**强制**的 `cargo test --workspace --no-run`。理由是后者仍然会编译每个视图和两个测试脚手架 —— 也就是说 roam-ui 里的编译回归照样会让 CI 失败，不会躲在这个已知问题后面。这一点是验过的：故意在一个视图里塞一个不存在的类型，`--no-run` 报 4 个错误；还原后归零。

用 `continue-on-error` 而不是删掉那一步，是为了让它继续跑：上游哪天修好，这一步会自己变绿，不需要谁记得回来打开它。

### 「文件夹一多就特别卡」：四处真原因

之前 §12 的规模测试量的是 `roam-core` 的排序/过滤/流式列举，数字都很好 —— 但那些**不是**卡顿的来源。用户报的是真实使用中的卡，所以重新按「每帧要做多少活」去查渲染路径，找到四处，其中三处与目录/文件数量成正比。

**1. 侧边栏目录树每帧重建整棵树（主因）**

`DirTreeView::render` 直接调 `DirTree::visible()`，它遍历整棵展开树、每个节点分配一个 `TreeRow`（两个 `Arc<str>`）；然后为**每一行**构建 gpui 元素（两个 `SharedString::from(format!(...))`、两个 Icon、几个闭包）—— 而面板只有 260px 高，约 12 行可见。

量出来的每帧成本，只算重建行列表这一步（元素构建的开销还要大得多）：

| 展开的目录数 | `visible()` 单次 | 10 帧 |
| --- | --- | --- |
| 1 000 | 153 µs | 1.72 ms |
| 10 000 | 962 µs | 7.48 ms |
| 50 000 | 2.68 ms | 34.5 ms |

两处都改了:行列表**缓存**在视图里，只在树变化时重建（`refresh_rows`，四个调用点）；渲染换成 gpui 的 `uniform_list`，只构建可见区间。元素 id 也从 `format!("tree-{path}")` 改成索引 —— 那是每行每帧一次字符串分配。

**在真 app 上验证**（5000 个子目录）:临时打印 `uniform_list` 给的区间，得到 `range=0..12 (12 rows built) of 5001 total`。改之前是每帧 5001 个元素。之后撤掉打印。

**2. 列举缓存完全没有上界**

`ListingCache` 是个裸 `DashMap`，从不淘汰:每个访问过的目录都驻留到进程退出。浏览 60 个各 1 万条目的目录 = 60 万个 `DirEntry` 常驻。现在按**条目数**（不是目录数）限制在 25 万，淘汰最久未加载的，且**永不淘汰刚打开的那个目录**（否则来回导航会次次重列）。实测:投 60 万，保留 25 万（25 个目录，约 22 MB 仅 `DirEntry` 本身）。按条目而非目录计，是因为一个 20 万对象的前缀顶得上两百个普通文件夹 —— 而正是大目录让「无上界」变得难受。

**3. 展开侧边栏节点会把该目录下所有文件拉进内存**

树只要子目录，却走 `list_all` 先收齐全部条目再筛。加了 `Vfs::list_dirs`，流式丢弃文件。**这是内存收益，不是延迟收益** —— 实测 50k 文件的目录 766 ms 对 700 ms，瓶颈在文件系统遍历；差别在于一个持有 5 万条目、另一个持有 20 个。

**4. 传输面板以 10 Hz 轮询全部任务**

`plan_upload` **每个文件一个任务**，所以拖进一个 10 万文件的文件夹就是 10 万个任务。面板渲染本来就有 50 行上界，但 `snapshot()` 会克隆**每一条**记录、每条抢 3 把锁 —— 而它在传输进行时每秒被调 10 次。新增 `TransferEngine::overview(limit)`:计数 + 最新 N 条，`describe()` 由两条路径共用以免漂移。实测 21 000 个任务:`snapshot()` 2.24 ms → `overview(50)` 163 µs，**14 倍**；10 Hz 下是每秒 22 ms 对 1.6 ms，且不再每秒分配 20 万个快照对象。

**量了但故意不动的一处**:导航时会把整份列表深拷贝两次（一次喂缓存、一次交给表格）。10 万条目实测 1.66 ms —— 不值得为它重构，所以留着。

**回归防护**:缓存淘汰有 4 个单测（含「当前目录不被淘汰」和「少量浏览不触发淘汰」）；树缓存有 3 个 gpui 测试。后者是**不开窗口**写的，因为 `Root` 在 macOS 测试平台下会 panic（见上文），而 `DirTreeView` 只需要 `App` —— 顺带发现 zed 新的测试调度器还会把我们 tokio 线程上的活动判为「非确定性」，所以这些测试也刻意不触发任何列举。

### SFTP 密码认证:为什么只有一条路可走

用户要求 sftp 支持账号密码。**OpenDAL 的 sftp service 没有 password 选项** —— 它的配置只有 `endpoint`、`root`、`user`、`key`、`known_hosts_strategy`、`enable_copy`;底下建的是 `openssh::SessionBuilder`,而它的认证设置只有 `keyfile`、`user`、`ssh_auth_sock`。原因是它并不自己说 SSH 协议:它 fork 系统 `ssh`,而 `ssh` 按设计从不接受命令行密码。

`ssh` 接受的是**助手程序**:设了 `SSH_ASKPASS` 且 `SSH_ASKPASS_REQUIRE=force` 时,它向该程序索取密码而不是读终端(OpenSSH 8.4+ 无需 `DISPLAY`;实测 macOS 自带的 10.3 可用)。OpenDAL fork 的 `ssh` 是我们的子进程,继承我们的环境 —— 这是不改 OpenDAL 就能走的门。

**但只有这一半不够。** `openssh` 的命令行里写死了 `-o BatchMode=yes`,而 `BatchMode` 恰恰就是关掉密码提示(含 askpass)的那个开关。实测:同一个连接不带它能成功,带上就得到 `Permission denied (publickey,password,keyboard-interactive)`。而且**从外面覆盖不掉** —— `ssh` 对同一选项取**首个**值,它已经在命令行上了。

所以第二半:`openssh` 是通过 **PATH** 找 `ssh` 的,我们在自己进程的 PATH 前面放一个同名 shim。它只摘掉那一个选项,然后 `exec` 真正的 `ssh`,其余一字不动 —— argv 是**逐个轮转**而不是拼字符串重建的,因为控制 socket 路径带空格。shim 只在 profile 真的配了密码时才装,所以纯密钥连接保留 `BatchMode` 与它的快速失败。

**密码怎么找到对应的连接。** `SSH_ASKPASS` 是进程全局的,而 app 可以同时开多个 sftp 会话。`ssh` 把提示词作为唯一参数传给助手(`pwuser@127.0.0.1's password: `),所以助手能判断是**谁**在问,按 `user@host` 去环境变量里取 —— 密码因此不落任何文件,包括助手脚本自己。端口不出现在提示里,所以推导键名时必须剥掉端口(实测在 12222 上确认)。

写这段时踩到的:

- 助手里的 shell 变换和 Rust 里的 `env_key` 必须推出同一个名字。不一致的表现是「密码为空」→ 看起来像凭据错误而不是 bug,所以有一条测试**真的执行那个脚本**来比对。
- 我自己的测试先撞上了这个设计的边界:两个测试用同一个 `user@host`,而环境是全局的,先跑的「错密码」那条**覆盖**了后跑的正确密码。改成用不同用户 —— 这也说明同一 `user@host` 配两个不同密码是这套机制的真实限制。
- endpoint 必须写成 `ssh://host:port`。`host:port` 会被 openssh 当作**主机名**整体传给 ssh,失败信息是「连不上」,与认证无关。
- 测试容器要用 `user:pass:[e]:uid:gid:dirs`,把 `upload` 写到 gid 位会让容器直接退出(`Invalid GID`)。
- 容器重建后主机密钥变了,`StrictHostKeyChecking=no` **不会**放过「密钥变更」(它只自动接受新主机),所以 `up sftp-pw` 会先清掉那条 known_hosts 记录。

**代价**:密码在 app 进程的环境里存活。其他用户读不到,同一用户能读到 —— 与 `profiles.toml` 已有的暴露面相同(§5),不新增风险类别,但值得知道而不是假设。

验证:`scripts/test-backends.sh up sftp-pw` 起一台密码认证的 atmoz/sftp,两条集成测试跑真上传+列举+读回,以及一条「错密码被拒」。

### 显示/隐藏点文件

过滤放在索引视图里,和排序、文本过滤复合,不克隆条目也不重新列举 —— 条目已经在内存里,为一个显示设置去重新拉一遍大目录是完全错的取舍。

`view_indices` 的参数从四个位置参数改成了 `ViewOptions`,因为其中两个是 `bool` 且相邻:`view_indices(.., true, false)` 写反了照样编译,结果是「本该升序排列,却把所有东西藏了起来」。

- 判定就是**名字以点开头**。别无依据:S3 和其他对象存储都没有 hidden 标志,而点前缀是 `ls` 所指的隐藏,也是用户敲 `.git` 时期待能找到的东西。
- 默认隐藏,`⌘⇧.`(Finder 的键)切换,工具栏按钮用 `Eye`/`EyeOff` 并反映当前状态。
- **`extend` 有独立的过滤路径**(流式批次),所以它也得判 —— 否则大目录加载期间点文件会从那条路漏进来。这条有专门的测试。
- 设置在新列举后保留,否则每次进目录都像是自己把开关关了。

这四条行为的测试都是**无窗口**的(delegate 是普通结构体),所以在上游 `Root` 阻塞的情况下照样能跑。

### 两个测试抓出来的真问题

1. **`connect_local` 从 `browser.vfs()` 取本机 session** —— 但切到远端后那已经是远端的 Vfs，"回到本机" 实际上在重新列举远端。Workspace 必须自己保留本机 Vfs。
2. **测试窗口没有包 `Root`** —— `window.push_notification` 和对话框层都会 `Root::update` 并在缺失时 panic。测试现在和 main.rs 一样包一层 `Root`，否则测的就不是真实结构。

### 未做


- 断点续传：目前失败只靠 RetryLayer 覆盖网络抖动，没有基于 multipart upload id 的持久化续传。
- 重命名目录仍未做：需要 copy + delete 的组合，且要在传输引擎里做成可见任务而非伪装成一次重命名。
- `fill_metadata` 有实现和单测，但要等接上元数据不全的对象存储才会在真实场景触发 —— 本地 `fs` 和 S3 的 list 都返回完整元数据。
- `services-sftp`、跨后端复制、传输引擎（M4）。

### 尚未验证的部分

**四个后端都已对本地兼容服务验证**（MinIO / Azurite / fake-gcs-server / WebDAV），见上文。仍然没有覆盖的是：

- **真实的云服务本身。** 兼容实现不等同：真实 AWS 有 SigV4 区域细节、更严格的限流和最终一致性；真实 GCS/Azure 也各有自己的行为。这些只能用真账号验证。
- **GCS 的大对象路径**（fake-gcs-server 缺 XML multipart 端点，见上）。


**没有截图。** 这台机器的 `screencapture` 被屏幕录制权限挡住（全屏和指定窗口都返回 `could not create image`）。验证方式是：启动进程确认存活 + 通过 `CGWindowListCopyWindowInfo` 确认窗口在屏幕上并有真实尺寸，再用 `TestAppContext` 驱动视图断言行为。视觉效果需要你自己跑一次看。

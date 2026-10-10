//! One update state for every workspace and settings window.
use crate::dialog::DialogButtons;
use futures::StreamExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, Sizable, WindowExt, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    App, AppContext, ClipboardItem, Context, Entity, Global, InteractiveElement, IntoElement,
    ParentElement, PromptLevel, Render, StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use roam_core::{Rt, TransferEngine};
use roam_updater::{Channel, Preferences, Proxy};
use roam_updater::{Client, Downloaded, Installation, PreparedInstall, Release};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub(crate) enum Status {
    Idle,
    Checking,
    Current,
    Available(Release),
    Downloading {
        release: Release,
        received: u64,
        total: u64,
    },
    Ready(Arc<Downloaded>),
    Installing,
    Failed {
        message: String,
        release: Option<Release>,
    },
}
pub(crate) struct Updater {
    rt: Rt,
    pub preferences: Preferences,
    preferences_path: PathBuf,
    pub engine: Option<TransferEngine>,
    pub status: Status,
    pub installation: Installation,
    pub last_checked: Option<std::time::SystemTime>,
    pub receipt: Option<String>,
    cache: PathBuf,
    generation: u64,
    abort: Option<tokio::task::AbortHandle>,
    task: Option<Task<()>>,
    timer: Option<Task<()>>,
    _prepared: Option<PreparedInstall>,
}
struct SharedUpdater(Entity<Updater>);
impl Global for SharedUpdater {}
pub(crate) fn maybe_store(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<SharedUpdater>().map(|g| g.0.clone())
}
pub(crate) fn store(cx: &App) -> Entity<Updater> {
    cx.global::<SharedUpdater>().0.clone()
}

pub fn init(rt: Rt, directory: PathBuf, cx: &mut App) {
    let preferences_path = directory.join("updates.toml");
    let (preferences, receipt) = match Preferences::load(&preferences_path) {
        Ok(preferences) => (preferences, None),
        Err(error) => (
            Preferences::default(),
            Some(format!("无法读取更新设置：{error:#}")),
        ),
    };
    let cache = directory.join("updates");
    let updater = cx.new(|_| Updater {
        rt,
        preferences,
        preferences_path,
        engine: None,
        status: Status::Idle,
        installation: Installation::detect(),
        cache: cache.clone(),
        generation: 0,
        last_checked: None,
        receipt,
        abort: None,
        task: None,
        timer: None,
        _prepared: None,
    });
    cx.set_global(SharedUpdater(updater.clone()));
    updater.update(cx, |view, cx| {
        let reading = view.rt.handle().spawn(async move {
            tokio::task::spawn_blocking(move || roam_updater::read_install_result(&cache))
                .await
                .ok()
                .flatten()
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(message)) = reading.await {
                let _ = this.update(cx, |view, cx| {
                    view.receipt = Some(message);
                    cx.notify();
                });
            }
        })
        .detach();
        view.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(15))
                .await;
            loop {
                if this
                    .update(cx, |view, cx| {
                        if view.preferences.auto_check
                            && matches!(
                                view.status,
                                Status::Idle | Status::Current | Status::Failed { .. }
                            )
                        {
                            view.check(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(24 * 60 * 60))
                    .await;
            }
        }));
    });
}

pub fn open_settings(window: &mut Window, cx: &mut App) {
    let Some(updater) = maybe_store(cx) else {
        return;
    };
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .title("设置 · 应用更新")
            .w(px(640.).min(window.viewport_size().width - px(48.)))
            .max_h((window.viewport_size().height - px(96.)).max(px(240.)))
            .child(updater.clone())
            .dismiss("关闭")
    });
}
impl Updater {
    fn client(&self) -> anyhow::Result<Client> {
        Client::new(Proxy::System, roam_updater::PUBLIC_KEY)
    }
    fn save_preferences(&mut self, next: Preferences, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Installing) {
            return;
        }
        if let Err(error) = next.save(&self.preferences_path) {
            window.push_notification(format!("无法保存更新设置：{error:#}"), cx);
            return;
        }
        let channel_changed = next.channel != self.preferences.channel;
        let enable_download = next.auto_download && !self.preferences.auto_download;
        self.preferences = next;
        if channel_changed {
            self.cancel(cx);
            self.status = Status::Idle;
            if self.preferences.auto_check {
                self.check(cx);
            }
        } else if enable_download
            && matches!(self.status, Status::Available(_))
            && self.installation.can_install()
        {
            self.download(cx);
        }
        cx.notify();
    }
    fn transfers_active(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|engine| engine.is_active())
    }
    fn error(
        &mut self,
        error: impl std::fmt::Display,
        release: Option<Release>,
        cx: &mut Context<Self>,
    ) {
        let message = error.to_string();
        tracing::warn!(%message, "application update failed");
        self.status = Status::Failed { message, release };
        self.abort = None;
        cx.notify();
    }
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Installing) {
            return;
        }
        self.generation += 1;
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.task = None;
        self.status = match &self.status {
            Status::Downloading { release, .. } => Status::Available(release.clone()),
            _ => Status::Idle,
        };
        cx.notify();
    }
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.status,
            Status::Checking | Status::Downloading { .. } | Status::Installing | Status::Ready(_)
        ) {
            return;
        }
        let client = match self.client() {
            Ok(c) => c,
            Err(e) => {
                self.error(e, None, cx);
                return;
            }
        };
        self.cancel(cx);
        let generation = self.generation;
        self.status = Status::Checking;
        cx.notify();
        let installation = self.installation.clone();
        let channel = self.preferences.channel;
        let checking = self.rt.handle().spawn(async move {
            client
                .check(env!("CARGO_PKG_VERSION"), channel, &installation)
                .await
        });
        self.abort = Some(checking.abort_handle());
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = checking.await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.abort = None;
                view.last_checked = Some(std::time::SystemTime::now());
                match result {
                    Ok(Ok(Some(release))) => {
                        tracing::info!(version = %release.version, "application update available");
                        view.status = Status::Available(release);
                        if view.preferences.auto_download && view.installation.can_install() {
                            view.download(cx);
                        }
                    }
                    Ok(Ok(None)) => view.status = Status::Current,
                    Ok(Err(e)) => view.error(format!("{e:#}"), None, cx),
                    Err(e) => view.error(e, None, cx),
                }
                cx.notify();
            });
        }));
    }
    pub fn download(&mut self, cx: &mut Context<Self>) {
        let release = match &self.status {
            Status::Available(r)
            | Status::Failed {
                release: Some(r), ..
            } => r.clone(),
            _ => return,
        };
        if !self.installation.can_install() {
            return;
        }
        let client = match self.client() {
            Ok(c) => c,
            Err(e) => {
                self.error(e, Some(release), cx);
                return;
            }
        };
        self.cancel(cx);
        let generation = self.generation;
        let cache = self.cache.clone();
        self.status = Status::Downloading {
            release: release.clone(),
            received: 0,
            total: release.asset.size,
        };
        cx.notify();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let downloading = self.rt.handle().spawn(async move {
            let result = client
                .download(release, cache, |received, total| {
                    let _ = tx.unbounded_send(Progress::Bytes(received, total));
                })
                .await;
            let _ = tx.unbounded_send(Progress::Done(
                result.map(Arc::new).map_err(|e| format!("{e:#}")),
            ));
        });
        self.abort = Some(downloading.abort_handle());
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Some(progress) = rx.next().await {
                let done = matches!(progress, Progress::Done(_));
                if this
                    .update(cx, |view, cx| {
                        if view.generation != generation {
                            return;
                        }
                        match progress {
                            Progress::Bytes(n, size) => {
                                if let Status::Downloading {
                                    received, total, ..
                                } = &mut view.status
                                {
                                    *received = n;
                                    *total = size;
                                }
                            }
                            Progress::Done(Ok(download)) => {
                                view.abort = None;
                                view.status = Status::Ready(download);
                            }
                            Progress::Done(Err(error)) => {
                                let release =
                                    if let Status::Downloading { release, .. } = &view.status {
                                        Some(release.clone())
                                    } else {
                                        None
                                    };
                                view.error(error, release, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                    || done
                {
                    break;
                }
            }
        }));
    }
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Status::Ready(download) = &self.status else {
            return;
        };
        let download = download.clone();
        if self.transfers_active() {
            window.push_notification("请等待文件传输完成，或取消传输后再安装更新", cx);
            return;
        }
        let detail = format!(
            "安装 Roam {} 并重启应用？打开的标签页将关闭。",
            download.release.version
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            "重启并安装更新？",
            Some(&detail),
            &["取消", "重启并安装"],
            cx,
        );
        let installation = self.installation.clone();
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(1) {
                return;
            }
            let allowed = this.update(cx, |view, cx| {
                let same_package = matches!(&view.status, Status::Ready(ready) if Arc::ptr_eq(ready, &download));
                if view.transfers_active() || !same_package {
                    return false;
                }
                view.status = Status::Installing;
                cx.notify();
                true
            }).unwrap_or(false);
            if !allowed {
                return;
            }
            let preparing = cx.update(|cx| {
                store(cx).read(cx).rt.handle().spawn(async move {
                    tokio::task::spawn_blocking(move || {
                        roam_updater::prepare_install(download.clone(), installation)
                            .map_err(|error| (format!("{error:#}"), download))
                    }).await
                })
            });
            let result = preparing.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(Ok(Ok(guard))) => {
                        // Transfers may have started while the helper was copied.
                        // Launch and quit happen in one foreground callback.
                        if view.transfers_active() {
                            view.status = Status::Ready(guard.download());
                            view.receipt = Some("准备更新期间出现了新的文件传输，请完成或取消后重试。".into());
                            cx.notify();
                        } else if let Err(error) = guard.launch() {
                            tracing::warn!(%error, "could not launch update helper");
                            view.status = Status::Ready(guard.download());
                            view.receipt = Some(format!("{error:#}"));
                            cx.notify();
                        } else {
                            tracing::info!("restarting for application update");
                            view._prepared = Some(guard);
                            cx.quit();
                        }
                    }
                    Ok(Ok(Err((error, download)))) => {
                        tracing::warn!(%error, "could not prepare application update");
                        view.status = Status::Ready(download);
                        view.receipt = Some(error);
                        cx.notify();
                    }
                    Ok(Err(error)) => view.error(error, None, cx),
                    Err(error) => view.error(error, None, cx),
                }
            });
        }).detach();
    }
}
enum Progress {
    Bytes(u64, u64),
    Done(Result<Arc<Downloaded>, String>),
}

impl Drop for Updater {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

impl Render for Updater {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = matches!(
            self.status,
            Status::Checking | Status::Downloading { .. } | Status::Installing
        );
        let ready = matches!(self.status, Status::Ready(_));
        let installing = matches!(self.status, Status::Installing);
        let can_install = self.installation.can_install();
        let (message, release, can_download) = match &self.status {
            Status::Idle => ("尚未检查更新".into(), None, false),
            Status::Checking => ("正在检查更新…".into(), None, false),
            Status::Current => ("当前已是此渠道的最新版本".into(), None, false),
            Status::Available(release) => (
                format!(
                    "发现 Roam {} · {:.1} MB",
                    release.version,
                    release.asset.size as f64 / 1_000_000.
                ),
                Some(release),
                true,
            ),
            Status::Downloading {
                release,
                received,
                total,
            } => (
                format!(
                    "正在下载 {} · {:.1} / {:.1} MB · {}%",
                    release.version,
                    *received as f64 / 1_000_000.,
                    *total as f64 / 1_000_000.,
                    received.saturating_mul(100) / (*total).max(1)
                ),
                Some(release),
                false,
            ),
            Status::Ready(download) => (
                format!("Roam {} 已下载，签名验证通过", download.release.version),
                Some(&download.release),
                false,
            ),
            Status::Installing => ("正在准备安装并重启…".into(), None, false),
            Status::Failed { message, release } => (
                format!("更新失败：{message}"),
                release.as_ref(),
                release.is_some(),
            ),
        };
        let page = release.map(|r| r.page.to_string());
        let notes = release
            .map(|r| r.notes.chars().take(8192).collect::<String>())
            .filter(|notes| !notes.is_empty());
        v_flex().gap_4().w_full().debug_selector(|| "update-settings".into())
            .child(h_flex().gap_3().items_center().justify_between()
                .child(v_flex().gap_1()
                    .child(div().text_lg().child(format!("Roam {}", env!("CARGO_PKG_VERSION"))))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("后台检查，安装前确认重启")))
                .child(Button::new("check-updates").outline().small().label("检查更新").disabled(busy || ready)
                    .on_click(cx.listener(|this, _, _, cx| this.check(cx)))))
            .child(h_flex().items_center().justify_between()
                .child(div().text_sm().child("自动检查更新"))
                .child(Switch::new("auto-check-updates").checked(self.preferences.auto_check).disabled(installing)
                    .on_click(cx.listener(|this, checked, window, cx| {
                        let mut next = this.preferences.clone(); next.auto_check = *checked;
                        this.save_preferences(next, window, cx);
                    }))))
            .child(h_flex().items_center().justify_between()
                .child(div().text_sm().child("自动下载更新包"))
                .child(Switch::new("auto-download-updates").checked(self.preferences.auto_download).disabled(installing || !can_install)
                    .on_click(cx.listener(|this, checked, window, cx| {
                        let mut next = this.preferences.clone(); next.auto_download = *checked;
                        this.save_preferences(next, window, cx);
                    }))))
            .child(h_flex().items_center().justify_between()
                .child(div().text_sm().child("更新渠道"))
                .child(h_flex().gap_2().children([(Channel::Stable, "稳定版"), (Channel::Development, "开发版")].into_iter().map(|(channel, label)| {
                    Button::new(label).outline().small().label(label).selected(self.preferences.channel == channel).disabled(installing)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let mut next = this.preferences.clone(); next.channel = channel;
                            this.save_preferences(next, window, cx);
                        }))
                }))))
            .child(div().text_xs().text_color(cx.theme().muted_foreground)
                .child("启动 15 秒后检查，此后每 24 小时检查一次。开发渠道包含预发布版本，更新不会降级。"))
            .when(!can_install, |el| el.child(div().text_xs().text_color(cx.theme().muted_foreground)
                .child("当前为源码、便携或包管理器安装，请通过发布页面下载，或使用系统包管理器更新。")))
            .child(div().text_sm().text_color(if matches!(self.status, Status::Failed { .. }) { cx.theme().danger } else { cx.theme().foreground }).child(message.clone()))
            .when_some(self.receipt.clone(), |el, receipt| el.child(div().text_xs().child(receipt)))
            .child(h_flex().gap_2().flex_wrap()
                .when(busy && !installing, |el| el.child(div().debug_selector(|| "update-cancel".into()).child(Button::new("cancel-update").ghost().small().label("取消")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))))))
                .when(can_download && can_install, |el| el.child(div().debug_selector(|| "update-download".into()).child(Button::new("download-update").primary().small().label("下载更新")
                    .on_click(cx.listener(|this, _, _, cx| this.download(cx))))))
                .when(ready, |el| el.child(Button::new("install-update").primary().small().label("重启并安装")
                    .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx)))))
                .when_some(page, |el, url| el.child(Button::new("update-release-page").ghost().small().label("查看发布说明")
                    .on_click(move |_, _, cx| cx.open_url(&url))))
                .when(matches!(self.status, Status::Failed { .. }), |el| el.child(Button::new("copy-update-error").ghost().small().label("复制错误")
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(message.clone()))))))
            .when_some(notes, |el, notes| el.child(div().text_xs().max_h(px(160.)).id("update-notes").overflow_y_scroll().child(notes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::Root;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext};

    fn harness(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<Updater>, VisualTestContext) {
        cx.background_executor.allow_parking();
        let directory = tempfile::tempdir().unwrap();
        Preferences {
            auto_check: false,
            auto_download: false,
            channel: Channel::Stable,
        }
        .save(&directory.path().join("updates.toml"))
        .unwrap();
        let path = directory.path().to_path_buf();
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(Rt::new().unwrap(), path, cx);
        });
        let updater = cx.read(store);
        let view = updater.clone();
        let window = cx.add_window(move |window, cx| Root::new(view.clone(), window, cx));
        let visual = VisualTestContext::from_window(window.into(), cx);
        (directory, updater, visual)
    }

    fn release() -> Release {
        Release {
            version: "9.0.0".parse().unwrap(),
            notes: String::new(),
            page: "https://github.com/yoogoc/roam/releases/tag/v9.0.0"
                .parse()
                .unwrap(),
            asset: roam_updater::Asset {
                url: "https://github.com/yoogoc/roam/releases/download/v9.0.0/Roam.app.tar.gz"
                    .parse()
                    .unwrap(),
                signature: String::new(),
                size: 100,
                format: roam_updater::Format::App,
            },
        }
    }

    #[gpui_kit::test]
    fn managed_installations_show_release_information_without_an_install_button(
        cx: &mut TestAppContext,
    ) {
        let (_directory, updater, mut visual) = harness(cx);
        updater.update(&mut visual, |view, cx| {
            view.installation = Installation::Managed;
            view.status = Status::Available(release());
            cx.notify();
        });
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(visual.debug_bounds("update-settings").unwrap().size.height > px(100.));
        assert!(visual.debug_bounds("update-download").is_none());
        updater.update(&mut visual, |view, cx| {
            view.installation = Installation::MacApp {
                bundle: "/tmp/Roam.app".into(),
                executable: "/tmp/Roam.app/Contents/MacOS/roam".into(),
            };
            cx.notify();
        });
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(visual.debug_bounds("update-download").is_some());
    }

    #[gpui_kit::test]
    fn cancelling_download_from_the_visible_button_aborts_io_and_keeps_the_release(
        cx: &mut TestAppContext,
    ) {
        let (_directory, updater, mut visual) = harness(cx);
        let rt = Rt::new().unwrap();
        let network = rt.handle().spawn(std::future::pending::<()>());
        let abort = network.abort_handle();
        updater.update(&mut visual, |view, cx| {
            view.abort = Some(abort);
            view.status = Status::Downloading {
                release: release(),
                received: 20,
                total: 100,
            };
            cx.notify();
        });
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let button = visual.debug_bounds("update-cancel").unwrap().center();
        visual.simulate_click(button, Modifiers::default());
        assert!(updater.read_with(&visual, |view, _| matches!(
            view.status,
            Status::Available(_)
        )));
        assert!(
            futures::executor::block_on(network)
                .unwrap_err()
                .is_cancelled()
        );
    }

    #[gpui_kit::test]
    fn channel_changes_are_saved_and_discard_the_previous_channels_download(
        cx: &mut TestAppContext,
    ) {
        let (directory, updater, mut visual) = harness(cx);
        visual.update(|window, cx| {
            updater.update(cx, |view, cx| {
                view.status = Status::Downloading {
                    release: release(),
                    received: 20,
                    total: 100,
                };
                let next = Preferences {
                    auto_check: false,
                    auto_download: true,
                    channel: Channel::Development,
                };
                view.save_preferences(next.clone(), window, cx);
                assert_eq!(view.preferences, next);
                assert!(matches!(view.status, Status::Idle));
            });
        });
        let saved = Preferences::load(&directory.path().join("updates.toml")).unwrap();
        assert_eq!(saved.channel, Channel::Development);
        assert!(saved.auto_download);
    }

    #[gpui_kit::test]
    fn failed_settings_write_retains_the_previous_preferences(cx: &mut TestAppContext) {
        let (_directory, updater, mut visual) = harness(cx);
        visual.update(|window, cx| {
            updater.update(cx, |view, cx| {
                std::fs::remove_file(&view.preferences_path).unwrap();
                std::fs::create_dir(&view.preferences_path).unwrap();
                let before = view.preferences.clone();
                let mut next = before.clone();
                next.channel = Channel::Development;
                view.save_preferences(next, window, cx);
                assert_eq!(view.preferences, before);
                assert!(matches!(view.status, Status::Idle));
            });
        });
    }
}

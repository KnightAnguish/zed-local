use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use auto_update::{AutoUpdateEvent, AutoUpdateStatus, AutoUpdater, UpdateCheckType};
use gpui::{Empty, Render, Task};
use semver::Version;
use ui::{Tooltip, UpdateButton, prelude::*};

const UP_TO_DATE_DISPLAY_DURATION: Duration = Duration::from_secs(4);

pub struct UpdateVersion {
    status: AutoUpdateStatus,
    update_check_type: UpdateCheckType,
    dismissed_status: Option<AutoUpdateStatus>,
    showing_up_to_date: bool,
    _up_to_date_timer: Option<Task<()>>,
}

impl UpdateVersion {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(auto_updater) = AutoUpdater::get(cx) {
            cx.observe(&auto_updater, |this, auto_update, cx| {
                let auto_update = auto_update.read(cx);
                this.status = auto_update.status();
                this.update_check_type = auto_update.update_check_type();
                this.dismissed_status = auto_update.dismissed_status();
                if !matches!(this.status, AutoUpdateStatus::Idle) {
                    this.hide_up_to_date();
                }
                cx.notify();
            })
            .detach();
            cx.subscribe(&auto_updater, |this, _, event, cx| match event {
                AutoUpdateEvent::UpToDate => this.show_up_to_date(cx),
            })
            .detach();
            Self {
                status: auto_updater.read(cx).status(),
                update_check_type: UpdateCheckType::Automatic,
                dismissed_status: auto_updater.read(cx).dismissed_status(),
                showing_up_to_date: false,
                _up_to_date_timer: None,
            }
        } else {
            Self {
                status: AutoUpdateStatus::Idle,
                update_check_type: UpdateCheckType::Automatic,
                dismissed_status: None,
                showing_up_to_date: false,
                _up_to_date_timer: None,
            }
        }
    }

    fn show_up_to_date(&mut self, cx: &mut Context<Self>) {
        self.showing_up_to_date = true;
        self._up_to_date_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(UP_TO_DATE_DISPLAY_DURATION)
                .await;
            this.update(cx, |this, cx| {
                this.hide_up_to_date();
                cx.notify();
            })
            .ok();
        }));
    }

    fn hide_up_to_date(&mut self) {
        self.showing_up_to_date = false;
        self._up_to_date_timer = None;
    }

    pub fn update_simulation(&mut self, cx: &mut Context<Self>) {
        let next_state = match self.status {
            AutoUpdateStatus::Idle => AutoUpdateStatus::Checking,
            AutoUpdateStatus::Available { .. } => AutoUpdateStatus::Idle,
            AutoUpdateStatus::Checking => AutoUpdateStatus::Downloading {
                version: Version::new(1, 99, 0),
                progress: Some(0.5),
            },
            AutoUpdateStatus::Downloading { .. } => AutoUpdateStatus::Installing {
                version: Version::new(1, 99, 0),
            },
            AutoUpdateStatus::Installing { .. } => AutoUpdateStatus::Updated {
                version: Version::new(1, 99, 0),
            },
            AutoUpdateStatus::Updated { .. } => AutoUpdateStatus::Errored {
                error: Arc::new(anyhow!("Network timeout")),
            },
            AutoUpdateStatus::Errored { .. } => AutoUpdateStatus::Idle,
        };

        self.status = next_state;
        self.update_check_type = UpdateCheckType::Manual;
        self.dismissed_status = None;
        self.hide_up_to_date();
        cx.notify()
    }

    pub fn show_update_in_menu_bar(&self) -> bool {
        self.is_dismissed() && self.status.is_updated()
    }

    fn is_dismissed(&self) -> bool {
        self.dismissed_status.as_ref() == Some(&self.status)
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.dismissed_status = Some(self.status.clone());
        if let Some(auto_updater) = AutoUpdater::get(cx) {
            let status = self.status.clone();
            auto_updater.update(cx, |auto_updater, cx| {
                auto_updater.dismiss_status(status, cx)
            });
        }
        cx.notify()
    }

    fn version_tooltip_message(version: &Version) -> String {
        UpdateButton::version_tooltip_message(display_version(version))
    }
}

/// A short version label for the title bar.
///
/// The full semver build metadata (`+stable.<sha>`, `+dev.<sha>`) is noise for
/// users; local packages carry a readable `+local.<stamp>` part instead, which
/// is the only build metadata worth showing.
fn display_version(version: &Version) -> String {
    let mut label = format!("{}.{}.{}", version.major, version.minor, version.patch);
    if !version.pre.is_empty() {
        label.push('-');
        label.push_str(version.pre.as_str());
    }
    if let Some(stamp) = version.build.as_str().strip_prefix("local.") {
        label.push_str(" · ");
        label.push_str(stamp);
    }
    label
}

impl Render for UpdateVersion {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.is_dismissed() {
            return Empty.into_any_element();
        }
        if self.showing_up_to_date {
            return UpdateButton::up_to_date().into_any_element();
        }
        match &self.status {
            AutoUpdateStatus::Available { version } => {
                let is_fork = AutoUpdater::is_local_fork();
                let url = format!("https://zed.dev/releases/stable/{version}");
                let button = if is_fork {
                    UpdateButton::new(
                        IconName::Download,
                        format!("ZedLocal {} available", display_version(version)),
                    )
                    .tooltip("Download and install this ZedLocal release; it applies after a restart.")
                    .on_click(move |_, _, cx| {
                        if let Some(auto_updater) = AutoUpdater::get(cx) {
                            auto_updater.update(cx, |updater, cx| updater.download_fork_update(cx));
                        }
                    })
                } else {
                    UpdateButton::new(IconName::Download, format!("Zed {} available", display_version(version)))
                        .tooltip("Official stable release available. Review and merge into Zed Local; this build will not be overwritten.")
                        .on_click(move |_, _, cx| cx.open_url(&url))
                };
                button
                    .with_dismiss()
                    .on_dismiss(cx.listener(|this, _, _window, cx| this.dismiss(cx)))
                    .into_any_element()
            }
            AutoUpdateStatus::Checking if self.update_check_type.is_manual() => {
                UpdateButton::checking().into_any_element()
            }
            AutoUpdateStatus::Downloading { version, progress } => {
                let rendered_version = version.clone();
                let tooltip = Tooltip::element(move |_, cx| {
                    let status = AutoUpdater::get(cx).map(|updater| updater.read(cx).status());
                    let message = match &status {
                        Some(AutoUpdateStatus::Downloading { version, progress }) => {
                            UpdateButton::downloading_tooltip_message(
                                display_version(version),
                                *progress,
                            )
                        }
                        _ => Self::version_tooltip_message(&rendered_version),
                    };
                    Label::new(message).into_any_element()
                });
                UpdateButton::downloading(*progress)
                    .tooltip_fn(tooltip)
                    .into_any_element()
            }
            AutoUpdateStatus::Installing { version } => {
                let version = Self::version_tooltip_message(version);
                UpdateButton::installing(version).into_any_element()
            }
            AutoUpdateStatus::Updated { version } => {
                let version = Self::version_tooltip_message(version);
                UpdateButton::updated(version)
                    .on_click(|_, _, cx| {
                        workspace::reload(cx);
                    })
                    .on_dismiss(cx.listener(|this, _, _window, cx| this.dismiss(cx)))
                    .into_any_element()
            }
            AutoUpdateStatus::Errored { error } => {
                let error_str = error.to_string();
                UpdateButton::errored(error_str)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(workspace::OpenLog), cx);
                    })
                    .on_dismiss(cx.listener(|this, _, _window, cx| this.dismiss(cx)))
                    .into_any_element()
            }
            AutoUpdateStatus::Idle | AutoUpdateStatus::Checking { .. } => Empty.into_any_element(),
        }
    }
}
#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    #[test]
    fn test_version_tooltip_message() {
        let message = UpdateVersion::version_tooltip_message(&Version::new(1, 0, 0));

        assert_eq!(message, "Update to Version: 1.0.0");

        let message = UpdateVersion::version_tooltip_message(
            &"1.0.0+nightly.14d9a4189f058d8736339b06ff2340101eaea5af"
                .parse()
                .unwrap(),
        );

        assert_eq!(
            message,
            "Update to Version: 1.0.0+nightly.14d9a4189f058d8736339b06ff2340101eaea5af"
        );
    }

    #[test]
    fn display_version_hides_build_hashes() {
        assert_eq!(
            display_version(&"1.28.1+stable.076fe5e19f7f54d9756f71f67528b37e276806b0".parse().unwrap()),
            "1.28.1"
        );
        assert_eq!(
            display_version(&"1.28.1+dev.1d35c814066c26bea893427d8b3eb5e4d8f0ba92".parse().unwrap()),
            "1.28.1"
        );
        assert_eq!(
            display_version(&"1.28.1+local.20261116.5".parse().unwrap()),
            "1.28.1 · 20261116.5"
        );
        assert_eq!(
            display_version(&"2.0.0-beta.3+nightly.abcdef".parse().unwrap()),
            "2.0.0-beta.3"
        );
    }

    #[test]
    fn test_downloading_tooltip_message() {
        let version = Version::new(1, 0, 0);

        let message = UpdateButton::downloading_tooltip_message(&version, None);
        assert_eq!(message, "Update to Version: 1.0.0");

        let message = UpdateButton::downloading_tooltip_message(&version, Some(0.454));
        assert_eq!(message, "Update to Version: 1.0.0 (45% downloaded)");

        let message = UpdateButton::downloading_tooltip_message(&version, Some(1.5));
        assert_eq!(message, "Update to Version: 1.0.0 (100% downloaded)");
    }
}

use crate::{
    core::config::RightPanelTab,
    daemon::ssh::host_info::HostInfo,
    ui::{
        app::Tty7App,
        i18n::{L10nKey, t},
        right_panel::{HEADING, META, TEXT},
        sftp::SftpRoute,
    },
};
use gpui::{AnyElement, Context, Window, div, prelude::*, px, rems};
use gpui_component::Disableable as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::{ActiveTheme as _, Sizable as _, button::Button, h_flex, v_flex};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct OverviewState {
    entries: HashMap<String, Entry>,
    active: Option<String>,
    pub ports_open: bool,
}
#[derive(Default)]
struct Entry {
    data: Option<HostInfo>,
    attempted: Option<Instant>,
    updated: Option<Instant>,
    inventory_updated: Option<Instant>,
    loading: bool,
    failed: bool,
}
impl Entry {
    fn merge(&mut self, info: HostInfo, full: bool) {
        if full || self.data.is_none() {
            self.data = Some(info);
        } else if let Some(data) = &mut self.data {
            // Missing dynamic fields must not silently retain an old value.
            for key in ["CPU_PERCENT", "MEM_TOTAL", "MEM_AVAILABLE"] {
                data.values.remove(key);
            }
            data.values.extend(info.values);
            data.disks = info.disks;
        }
        self.updated = Some(Instant::now());
        if full {
            self.inventory_updated = self.updated;
        }
        self.failed = false;
    }
}
impl Tty7App {
    pub(crate) fn sync_host_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let route = (|| {
            if !self.right_panel_visible || self.right_panel_tab != RightPanelTab::Info {
                return None;
            }
            let pane = self.tabs.get(self.active)?.detail_pane(window, cx)?;
            let view = pane.read(cx);
            view.ssh_spec()?;
            Some(SftpRoute::new(view.pane_id, view.workspace().cloned()))
        })();
        let state = &mut self.right_panel.overview;
        state.active = route.as_ref().map(SftpRoute::connection_key);
        let Some(route) = route else {
            return;
        };
        let key = route.connection_key();
        if state.entries.len() > 64 {
            state.entries.retain(|k, e| k == &key || e.loading);
        }
        let entry = state.entries.entry(key.clone()).or_default();
        if entry.loading
            || entry
                .attempted
                .is_some_and(|t| t.elapsed() < Duration::from_secs(15))
        {
            return;
        }
        let full = entry
            .inventory_updated
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(300));
        entry.loading = true;
        entry.attempted = Some(Instant::now());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { route.host_info(full) })
                .await;
            let _ = this.update(cx, |app, cx| {
                if let Some(entry) = app.right_panel.overview.entries.get_mut(&key) {
                    entry.loading = false;
                    entry.attempted = Some(Instant::now());
                    match result {
                        Ok(info) => entry.merge(info, full),
                        Err(error) => {
                            log::debug!("host overview request failed: {error}");
                            entry.failed = true;
                        }
                    }
                }
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_secs(15))
                .await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    pub(crate) fn render_host_overview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = &self.right_panel.overview;
        let entry = state.entries.get(state.active.as_ref()?)?;
        let muted = cx.theme().muted_foreground;
        let badge = |text: String, installed: bool| {
            div()
                .px_2()
                .py(px(2.))
                .rounded_md()
                .text_size(rems(META))
                .bg(if installed {
                    cx.theme().success.opacity(0.15)
                } else {
                    cx.theme().secondary
                })
                .text_color(if installed { cx.theme().success } else { muted })
                .child(text)
        };
        let row = |label: &str, content: AnyElement| {
            h_flex()
                .items_start()
                .gap_3()
                .py_3()
                .child(
                    div()
                        .w(px(56.))
                        .flex_shrink_0()
                        .text_size(rems(HEADING))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(label.to_uppercase()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(cx.theme().font_family.clone())
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_size(rems(TEXT))
                        .text_color(cx.theme().sidebar_foreground)
                        .child(content),
                )
        };
        let separator = || div().h(px(1.)).bg(cx.theme().border);
        let refresh = Button::new("host-overview-refresh")
            .label(t(if entry.loading {
                L10nKey::HostCollecting
            } else {
                L10nKey::HostRefresh
            }))
            .ghost()
            .small()
            .disabled(entry.loading)
            .on_click(cx.listener(|this, _, _, cx| {
                let state = &mut this.right_panel.overview;
                if let Some(entry) = state
                    .active
                    .as_ref()
                    .and_then(|key| state.entries.get_mut(key))
                {
                    entry.attempted = None;
                    entry.inventory_updated = None;
                }
                cx.notify();
            }));
        let mut root = v_flex()
            .px_3()
            .py_2()
            .font_family(cx.theme().font_family.clone())
            .font_weight(gpui::FontWeight::NORMAL)
            .text_size(rems(TEXT))
            .text_color(cx.theme().sidebar_foreground)
            .border_t_1()
            .border_color(cx.theme().border);
        if let Some(info) = &entry.data {
            let value = |key: &str| {
                info.values
                    .get(key)
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "—".into())
            };
            root = root
                .child(row(
                    t(L10nKey::HostSystemType),
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(value("OS"))
                        .child(badge(value("ARCH"), false))
                        .into_any_element(),
                ))
                .child(separator());
            let cpu = info
                .values
                .get("CPU_PERCENT")
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v));
            let mut cpu_content = v_flex().gap_2().child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(format!("{} {}", value("CORES"), t(L10nKey::HostCores)))
                    .child(
                        cpu.map(|v| format!("{v:.0}%"))
                            .unwrap_or_else(|| "—".into()),
                    ),
            );
            if let Some(cpu) = cpu {
                cpu_content = cpu_content.child(meter(cpu / 100., cx));
            }
            root = root.child(row("CPU", cpu_content.into_any_element()));
            let memory = match (number(info, "MEM_TOTAL"), number(info, "MEM_AVAILABLE")) {
                (Some(total), Some(available)) if total > 0 && available <= total => {
                    let used = total - available;
                    let ratio = used as f32 / total as f32;
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .flex_wrap()
                                .justify_between()
                                .gap_2()
                                .child(format!("{} / {} GiB", gib(used), gib(total)))
                                .child(
                                    div()
                                        .text_size(rems(TEXT))
                                        .text_color(cx.theme().sidebar_foreground)
                                        .child(format!("{:.0}%", ratio * 100.)),
                                ),
                        )
                        .child(meter(ratio, cx))
                        .into_any_element()
                }
                _ => div().child("—").into_any_element(),
            };
            root = root.child(row(t(L10nKey::HostMemory), memory));
            if info.disks.is_empty() {
                root = root.child(row(
                    t(L10nKey::HostDisk),
                    div()
                        .text_color(muted)
                        .child(t(L10nKey::HostDisksUnavailable))
                        .into_any_element(),
                ));
            }
            for (index, disk) in info.disks.iter().enumerate() {
                let disk_content = v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(format!(
                                "{} / {} GiB",
                                gib(disk.used_kb),
                                gib(disk.total_kb)
                            ))
                            .child(
                                div()
                                    .text_size(rems(TEXT))
                                    .text_color(cx.theme().sidebar_foreground)
                                    .child(format!("{}%", disk.percent)),
                            ),
                    )
                    .child(meter(disk.percent as f32 / 100., cx))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(badge(
                                format!("{}: {}", t(L10nKey::HostDirectory), disk.mount),
                                false,
                            ))
                            .child(badge(disk.kind.clone(), false)),
                    );
                root = root.child(row(
                    if index == 0 { t(L10nKey::HostDisk) } else { "" },
                    disk_content.into_any_element(),
                ));
            }
            let mut software = v_flex().gap_3();
            for (name, state_key, version_key) in [
                ("Docker", "docker_STATE", "docker_VERSION"),
                ("Kubernetes", "KUBE_STATE", "KUBE_VERSION"),
                ("rsync", "rsync_STATE", "rsync_VERSION"),
            ] {
                let version = info
                    .values
                    .get(version_key)
                    .and_then(|v| software_version(v));
                let installed = version.is_some()
                    || matches!(
                        info.values.get(state_key).map(String::as_str),
                        Some("installed" | "ready" | "restricted" | "unavailable")
                    );
                let state = if installed {
                    badge(
                        version.unwrap_or_else(|| t(L10nKey::HostInstalled).into()),
                        true,
                    )
                    .into_any_element()
                } else {
                    div()
                        .text_size(rems(TEXT))
                        .text_color(cx.theme().sidebar_foreground)
                        .child(status(&value(state_key)))
                        .into_any_element()
                };
                software = software.child(
                    h_flex()
                        .flex_wrap()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        .child(name)
                        .child(state),
                );
            }
            root = root.child(separator()).child(row(
                t(L10nKey::HostEnvironment),
                software.into_any_element(),
            ));
        } else {
            root = root.child(div().py_3().text_color(muted).child(t(if entry.failed {
                L10nKey::HostCollectFailed
            } else {
                L10nKey::HostCollecting
            })));
        }
        let mut footer = h_flex().justify_end().gap_2().items_center();
        if entry.failed && entry.data.is_some() {
            footer = footer.child(
                div()
                    .text_size(rems(TEXT))
                    .text_color(muted)
                    .child(t(L10nKey::HostStale)),
            );
        }
        Some(root.child(footer.child(refresh)).into_any_element())
    }
}
fn software_version(raw: &str) -> Option<String> {
    raw.split_whitespace().find_map(|token| {
        let version = token.trim_end_matches(',').trim_start_matches('v');
        (version.as_bytes().first().is_some_and(u8::is_ascii_digit) && version.contains('.'))
            .then(|| format!("v{version}"))
    })
}
fn gib(kb: u64) -> String {
    format!("{:.1}", kb as f64 / 1048576.)
}
fn number(info: &HostInfo, key: &str) -> Option<u64> {
    info.values.get(key)?.parse().ok()
}
fn status(value: &str) -> &'static str {
    t(match value {
        "ready" => L10nKey::HostReady,
        "restricted" => L10nKey::HostRestricted,
        "missing" | "undetected" => L10nKey::HostMissing,
        "installed" => L10nKey::HostInstalled,
        "unavailable" => L10nKey::HostUnavailable,
        "failed" => L10nKey::HostCollectFailed,
        _ => L10nKey::HostUnknown,
    })
}
fn meter(ratio: f32, cx: &Context<Tty7App>) -> impl IntoElement {
    div()
        .w_full()
        .h(px(4.))
        .rounded_full()
        .bg(cx.theme().secondary)
        .child(
            div()
                .h_full()
                .w(gpui::relative(ratio.clamp(0., 1.)))
                .rounded_full()
                .bg(cx.theme().success),
        )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn software_badge_extracts_version_without_build_text() {
        assert_eq!(
            software_version("Docker version 26.1.3, build b72abbb"),
            Some("v26.1.3".into())
        );
        assert_eq!(
            software_version("rsync version 3.4.1 protocol version 32"),
            Some("v3.4.1".into())
        );
        assert_eq!(
            software_version("k3s version v1.32.1+k3s1"),
            Some("v1.32.1+k3s1".into())
        );
        assert_eq!(software_version("permission denied"), None);
    }
    #[test]
    fn resource_refresh_keeps_inventory_but_removes_missing_metrics() {
        let mut entry = Entry::default();
        entry.merge(
            crate::daemon::ssh::host_info::parse(
                "OS=Debian\nCPU_SAMPLE=1000 800 1200 950\nMEM_TOTAL=100\nMEM_AVAILABLE=20\nTTY7_INFO_DONE=1",
            )
            .unwrap(),
            true,
        );
        entry.merge(
            crate::daemon::ssh::host_info::parse("MEM_TOTAL=100\nTTY7_INFO_DONE=1").unwrap(),
            false,
        );
        let data = entry.data.unwrap();
        assert_eq!(data.values["OS"], "Debian");
        assert!(!data.values.contains_key("MEM_AVAILABLE"));
        assert!(!data.values.contains_key("CPU_PERCENT"));
    }
}

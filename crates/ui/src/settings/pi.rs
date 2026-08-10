//! Settings → Pi: device-local runtime overview, provider credentials,
//! packages/resources, and advanced diagnostics.
//!
//! The page always names both relevant contexts: the device that launches Pi
//! and the native Pi scope (global or the selected project). Mutations travel
//! through device-routable Nova RPCs; the viewport never reads dotfiles or
//! receives credential material.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use nova_proto::{
    PiPackageInfo, PiProviderStatus, PiResourceInfo, PiSettingsScope, PiSettingsSnapshot,
};
use nova_rpc::methods;

use crate::composer::ComposerInput;
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiSection {
    Overview,
    Providers,
    Packages,
    Advanced,
}

impl PiSection {
    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Pi overview",
            Self::Providers => "Provider credentials",
            Self::Packages => "Packages & resources",
            Self::Advanced => "Pi advanced",
        }
    }
}

enum PiDialog {
    ApiKey {
        provider: String,
        provider_name: String,
        input: Entity<ComposerInput>,
    },
    OpenAiCompatible {
        base_url: Entity<ComposerInput>,
        api_key: Entity<ComposerInput>,
        has_stored_key: bool,
    },
    InstallPackage {
        input: Entity<ComposerInput>,
    },
    RemoveCredential {
        provider: String,
        provider_name: String,
    },
    RemovePackage {
        source: String,
        scope: PiSettingsScope,
    },
}

pub struct PiSettingsPage {
    state: Entity<AppState>,
    section: PiSection,
    scope: PiSettingsScope,
    /// `None` means this viewport's directly connected engine.
    target_device: Option<String>,
    device_menu_open: bool,
    menu_dismissed_at: Option<Instant>,
    snapshot: Loadable<PiSettingsSnapshot>,
    busy: Option<SharedString>,
    error: Option<SharedString>,
    copied: Option<String>,
    dialog: Option<PiDialog>,
    task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _observe: Subscription,
}

impl PiSettingsPage {
    pub fn new(state: Entity<AppState>, section: PiSection, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            section,
            scope: PiSettingsScope::Global,
            target_device: None,
            device_menu_open: false,
            menu_dismissed_at: None,
            snapshot: Loadable::Idle,
            busy: None,
            error: None,
            copied: None,
            dialog: None,
            task: None,
            copy_task: None,
            _observe: observe,
        };
        page.load(cx);
        page
    }

    pub fn set_section(&mut self, section: PiSection, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            self.dialog = None;
            self.error = None;
            cx.notify();
        }
    }

    fn selected_project(&self, cx: &Context<Self>) -> Option<(String, String, String)> {
        let state = self.state.read(cx);
        let project = state.selected_project_row()?;
        Some((
            project.path.clone(),
            project.device_id.clone(),
            project.display_name().to_string(),
        ))
    }

    fn effective_device(&self, cx: &Context<Self>) -> Option<String> {
        if self.scope == PiSettingsScope::Project {
            return self.selected_project(cx).map(|(_, device, _)| device);
        }
        self.target_device
            .clone()
            .or_else(|| self.state.read(cx).local_device_id.clone())
    }

    fn params(&self, cx: &Context<Self>) -> serde_json::Value {
        let project_path = (self.scope == PiSettingsScope::Project)
            .then(|| self.selected_project(cx).map(|(path, _, _)| path))
            .flatten();
        let mut params = serde_json::json!({
            "scope": self.scope,
            "projectPath": project_path,
        });
        let local = self.state.read(cx).local_device_id.clone();
        if let Some(target) = self.effective_device(cx)
            && Some(target.as_str()) != local.as_deref()
        {
            params["targetDeviceId"] = serde_json::json!(target);
        }
        params
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        let params = self.params(cx);
        self.snapshot = Loadable::Loading;
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::GET_PI_SETTINGS, params).await;
            this.update(cx, |page, cx| {
                page.snapshot = decode_snapshot(result);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn call_snapshot(
        &mut self,
        method: &'static str,
        mut params: serde_json::Value,
        busy: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let base = self.params(cx);
        if let (Some(target), Some(object)) = (base.get("targetDeviceId"), params.as_object_mut()) {
            object.insert("targetDeviceId".into(), target.clone());
        }
        if let Some(object) = params.as_object_mut() {
            object.insert("scope".into(), base["scope"].clone());
            object.insert("projectPath".into(), base["projectPath"].clone());
        }
        self.busy = Some(busy.into());
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                match decode_snapshot(result) {
                    Loadable::Ready(snapshot) => page.snapshot = Loadable::Ready(snapshot),
                    Loadable::Error(error) => page.error = Some(error.into()),
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_setting(&mut self, key: &'static str, value: serde_json::Value, cx: &mut Context<Self>) {
        self.call_snapshot(
            methods::SET_PI_SETTING,
            serde_json::json!({ "key": key, "value": value }),
            "Saving Pi settings…",
            cx,
        );
    }

    fn set_scope(&mut self, scope: PiSettingsScope, cx: &mut Context<Self>) {
        if scope == PiSettingsScope::Project && self.selected_project(cx).is_none() {
            return;
        }
        if self.scope != scope {
            self.scope = scope;
            self.device_menu_open = false;
            self.load(cx);
        }
    }

    fn set_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let local = self.state.read(cx).local_device_id.clone();
        self.target_device = (Some(device_id.as_str()) != local.as_deref()).then_some(device_id);
        self.scope = PiSettingsScope::Global;
        self.device_menu_open = false;
        self.load(cx);
    }

    fn open_api_key(&mut self, provider: &PiProviderStatus, cx: &mut Context<Self>) {
        let input = cx.new(|cx| ComposerInput::new("Paste API key", cx));
        self.dialog = Some(PiDialog::ApiKey {
            provider: provider.id.clone(),
            provider_name: provider.name.clone(),
            input,
        });
        cx.notify();
    }

    fn submit_api_key(&mut self, cx: &mut Context<Self>) {
        let Some(PiDialog::ApiKey {
            provider, input, ..
        }) = &self.dialog
        else {
            return;
        };
        let key = input.read(cx).text().trim().to_string();
        if key.is_empty() {
            self.error = Some("Enter an API key first".into());
            cx.notify();
            return;
        }
        let provider = provider.clone();
        self.dialog = None;
        self.call_snapshot(
            methods::SET_PI_CREDENTIAL,
            serde_json::json!({ "provider": provider, "key": key }),
            "Saving provider credential…",
            cx,
        );
    }

    fn open_openai_compatible(&mut self, snapshot: &PiSettingsSnapshot, cx: &mut Context<Self>) {
        let base_url = cx.new(|cx| ComposerInput::new("https://api.example.com/v1", cx));
        let api_key = cx.new(|cx| ComposerInput::new("API key", cx));
        if let Some(value) = snapshot.openai_compatible.base_url.as_deref() {
            base_url.update(cx, |input, cx| input.set_text(value, cx));
        }
        if snapshot.openai_compatible.has_stored_key {
            api_key.update(cx, |input, cx| {
                input.set_placeholder("Leave blank to keep the saved key", cx)
            });
        }
        self.dialog = Some(PiDialog::OpenAiCompatible {
            base_url,
            api_key,
            has_stored_key: snapshot.openai_compatible.has_stored_key,
        });
        cx.notify();
    }

    fn submit_openai_compatible(&mut self, cx: &mut Context<Self>) {
        let Some(PiDialog::OpenAiCompatible {
            base_url, api_key, ..
        }) = &self.dialog
        else {
            return;
        };
        let base_url = base_url.read(cx).text().trim().to_string();
        let api_key = api_key.read(cx).text().trim().to_string();
        if base_url.is_empty() {
            self.error = Some("Enter a base URL".into());
            cx.notify();
            return;
        }
        self.dialog = None;
        self.call_snapshot(
            methods::SET_PI_OPENAI_COMPATIBLE,
            serde_json::json!({
                "baseUrl": base_url,
                "apiKey": (!api_key.is_empty()).then_some(api_key),
            }),
            "Loading model registry…",
            cx,
        );
    }

    fn submit_package(&mut self, cx: &mut Context<Self>) {
        let Some(PiDialog::InstallPackage { input }) = &self.dialog else {
            return;
        };
        let source = input.read(cx).text().trim().to_string();
        if source.is_empty() {
            self.error = Some("Enter an npm, Git, or local package source".into());
            cx.notify();
            return;
        }
        self.dialog = None;
        self.package_action("install", source, self.scope, cx);
    }

    fn package_action(
        &mut self,
        action: &'static str,
        source: String,
        scope: PiSettingsScope,
        cx: &mut Context<Self>,
    ) {
        let mut params = serde_json::json!({ "action": action, "source": source, "scope": scope });
        // Removing a project package must retain the row's own scope even if a
        // stale modal survived a scope change.
        if let Some(object) = params.as_object_mut() {
            object.insert("scope".into(), serde_json::json!(scope));
        }
        self.call_snapshot(
            methods::PI_PACKAGE_ACTION,
            params,
            match action {
                "install" => "Installing Pi package…",
                "remove" => "Removing Pi package…",
                _ => "Updating Pi package…",
            },
            cx,
        );
    }

    fn confirm_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            PiDialog::RemoveCredential { provider, .. } => self.call_snapshot(
                methods::REMOVE_PI_CREDENTIAL,
                serde_json::json!({ "provider": provider }),
                "Removing provider credential…",
                cx,
            ),
            PiDialog::RemovePackage { source, scope } => {
                self.package_action("remove", source, scope, cx)
            }
            other => self.dialog = Some(other),
        }
    }

    fn copy_path(&mut self, path: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
        self.copied = Some(path);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1400))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_context(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (devices, local_id) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let effective = self.effective_device(cx).or_else(|| local_id.clone());
        let selected_name: SharedString = devices
            .iter()
            .find(|device| Some(device.id.as_str()) == effective.as_deref())
            .map(|device| device.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let project = self.selected_project(cx);
        let project_enabled = project.is_some();
        let scope = self.scope;
        let menu_open = self.device_menu_open;
        let just_dismissed = self
            .menu_dismissed_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(400));

        let mut device_trigger = div()
            .id("pi-device-trigger")
            .relative()
            .h(px(32.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if menu_open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.wash(if menu_open { 0.07 } else { 0.03 }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.wash(0.06)).border_color(theme.border_strong))
            .on_click(cx.listener(move |this, _, _, cx| {
                if just_dismissed {
                    this.menu_dismissed_at = None;
                } else {
                    this.device_menu_open = !this.device_menu_open;
                }
                cx.notify();
            }))
            .child(
                crate::icons::icon(crate::icons::LAPTOP)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .max_w(px(190.0))
                    .truncate()
                    .text_size(px(12.5))
                    .child(selected_name),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            );
        if menu_open {
            let menu = popover::popover_card(theme)
                .w(px(240.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    this.menu_dismissed_at = Some(Instant::now());
                    cx.notify();
                }))
                .child(popover::menu_heading(theme, "Pi runs on"))
                .children(devices.into_iter().enumerate().map(|(ix, device)| {
                    let selected = Some(device.id.as_str()) == effective.as_deref();
                    let id = device.id.clone();
                    popover::menu_row(theme, selected, format!("pi-device-{ix}"))
                        .id(("pi-device", ix))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_device(id.clone(), cx)),
                        )
                        .child(
                            crate::icons::icon(crate::icons::MONITOR)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(device.name)),
                        )
                        .when(selected, |el| el.child(popover::menu_check(theme)))
                }));
            device_trigger = device_trigger.child(popover::anchored_menu(
                "pi-device-menu",
                menu.into_any_element(),
            ));
        }

        let scope_control = div()
            .h(px(32.0))
            .p(px(3.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.wash(0.03))
            .flex()
            .flex_row()
            .children(
                [
                    (PiSettingsScope::Global, "Global"),
                    (
                        PiSettingsScope::Project,
                        project
                            .as_ref()
                            .map(|(_, _, name)| name.as_str())
                            .unwrap_or("Project"),
                    ),
                ]
                .into_iter()
                .map(|(option, label)| {
                    let selected = scope == option;
                    let enabled = option == PiSettingsScope::Global || project_enabled;
                    div()
                        .id(SharedString::from(format!("pi-scope-{option:?}")))
                        .px(px(10.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .bg(if selected {
                            theme.wash(0.12)
                        } else {
                            gpui::transparent_black()
                        })
                        .opacity(if enabled { 1.0 } else { 0.35 })
                        .when(enabled, |el| {
                            el.cursor_pointer().on_click(
                                cx.listener(move |this, _, _, cx| this.set_scope(option, cx)),
                            )
                        })
                        .child(SharedString::from(label.to_string()))
                }),
            );

        div()
            .mt(px(18.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(device_trigger)
            .child(scope_control)
            .into_any_element()
    }

    fn render_overview(
        &mut self,
        snapshot: &PiSettingsSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = snapshot.settings.clone();
        let runtime_detail = if snapshot.runtime.installed {
            format!(
                "{} · {}",
                snapshot
                    .runtime
                    .version
                    .as_deref()
                    .unwrap_or("Version unavailable"),
                snapshot
                    .runtime
                    .executable
                    .as_deref()
                    .unwrap_or("Executable unavailable")
            )
        } else {
            "Pi was not found on this device".into()
        };
        let default_model = match (&settings.default_provider, &settings.default_model) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            (_, Some(model)) => model.clone(),
            _ => "Resolved by Pi".into(),
        };
        let thinking = settings
            .default_thinking_level
            .clone()
            .unwrap_or_else(|| "Model default".into());
        let transport = settings.transport.clone();
        let transport_for_action = transport.clone();
        let trust = settings.default_project_trust.clone();
        let next_trust = match trust.as_str() {
            "ask" => "always",
            "always" => "never",
            _ => "ask",
        };

        crate::settings::widgets::section_card(theme)
            .child(info_row(theme, "Pi runtime", &runtime_detail, Some(crate::icons::PI_MARK), None))
            .child(info_row(theme, "Default model", &default_model, Some(crate::icons::GLOBAL), None))
            .child(info_row(theme, "Thinking", &thinking, Some(crate::icons::TUNING), None))
            .child(toggle_row(theme, "Automatic compaction", "Summarize older context before the model window fills.", settings.auto_compaction, "pi-compaction", cx.listener(|this, _, _, cx| {
                let next = this.snapshot.ready().is_none_or(|s| !s.settings.auto_compaction);
                this.set_setting("compaction.enabled", serde_json::json!(next), cx);
            })))
            .child(toggle_row(theme, "Automatic retry", "Retry transient provider failures with Pi's backoff policy.", settings.auto_retry, "pi-retry", cx.listener(|this, _, _, cx| {
                let next = this.snapshot.ready().is_none_or(|s| !s.settings.auto_retry);
                this.set_setting("retry.enabled", serde_json::json!(next), cx);
            })))
            .child(action_row(theme, "Project trust", "Controls whether non-interactive Pi sessions load project-local settings and executable extensions.", &trust, "pi-trust", cx.listener(move |this, _, _, cx| {
                this.set_setting("defaultProjectTrust", serde_json::json!(next_trust), cx);
            })))
            .child(action_row(theme, "Transport", "Provider transport preference for sessions started after this change.", &transport, "pi-transport", cx.listener(move |this, _, _, cx| {
                let next = match transport_for_action.as_str() { "auto" => "sse", "sse" => "websocket", _ => "auto" };
                this.set_setting("transport", serde_json::json!(next), cx);
            })))
            .into_any_element()
    }

    fn render_providers(
        &mut self,
        snapshot: &PiSettingsSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compatible = snapshot.openai_compatible.clone();
        let compatible_snapshot = snapshot.clone();
        let compatible_detail = match (compatible.base_url.as_deref(), compatible.models.len()) {
            (Some(url), 1) => format!("{url} · 1 model"),
            (Some(url), count) => format!("{url} · {count} models"),
            _ => "Use any OpenAI Chat Completions endpoint".into(),
        };
        let compatible_row = crate::settings::widgets::card_row(theme, true)
            .child(crate::settings::widgets::row_tile(
                theme,
                crate::icons::OPENAI_MARK,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(crate::settings::widgets::row_title(
                        theme,
                        SharedString::from("OpenAI compatible"),
                    ))
                    .child(
                        div()
                            .mt(px(3.0))
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(compatible_detail)),
                    ),
            )
            .child(if compatible.configured {
                crate::settings::widgets::badge_active("Configured")
            } else {
                crate::settings::widgets::badge(theme, "Available")
            })
            .child(
                popover::btn_primary(
                    theme,
                    if compatible.configured {
                        "Configure"
                    } else {
                        "Set up"
                    },
                )
                .id("pi-openai-compatible")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_openai_compatible(&compatible_snapshot, cx)
                })),
            )
            .into_any_element();

        let rows = snapshot
            .providers
            .iter()
            .filter(|provider| provider.id != "openai-compatible")
            .enumerate()
            .map(|(ix, provider)| {
                let configured = provider.configured;
                let stored = provider.source.starts_with("Stored");
                let provider_for_add = provider.clone();
                let provider_for_remove = provider.clone();
                let source: SharedString = provider.source.clone().into();
                let name: SharedString = provider.name.clone().into();
                crate::settings::widgets::card_row(theme, false)
                    .child(crate::settings::widgets::row_tile(
                        theme,
                        crate::icons::KEY_MINIMALISTIC,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(crate::settings::widgets::row_title(theme, name))
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(source),
                            ),
                    )
                    .child(if configured {
                        crate::settings::widgets::badge_active("Configured")
                    } else {
                        crate::settings::widgets::badge(theme, "Available")
                    })
                    .when(!configured, |el| {
                        el.child(
                            popover::btn_primary(theme, "Add API key")
                                .id(("pi-provider-add", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_api_key(&provider_for_add, cx)
                                })),
                        )
                    })
                    .when(stored, |el| {
                        el.child(
                            popover::btn_ghost(theme, "Remove", format!("pi-provider-remove-{ix}"))
                                .id(("pi-provider-remove", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dialog = Some(PiDialog::RemoveCredential {
                                        provider: provider_for_remove.id.clone(),
                                        provider_name: provider_for_remove.name.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .into_any_element()
            });
        crate::settings::widgets::section_card(theme)
            .child(compatible_row)
            .children(rows)
            .into_any_element()
    }

    fn render_packages(
        &mut self,
        snapshot: &PiSettingsSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let visible_packages: Vec<PiPackageInfo> = snapshot
            .packages
            .iter()
            .filter(|package| package.scope == self.scope)
            .cloned()
            .collect();
        let package_rows: Vec<AnyElement> = visible_packages
            .into_iter()
            .enumerate()
            .map(|(ix, package)| {
                let update_source = package.source.clone();
                let remove_source = package.source.clone();
                let remove_scope = package.scope;
                crate::settings::widgets::card_row(theme, ix == 0)
                    .child(crate::settings::widgets::row_tile(
                        theme,
                        crate::icons::WIDGET,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(crate::settings::widgets::row_title(
                                theme,
                                SharedString::from(package.source),
                            ))
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(format!(
                                        "{} · {:?}{}",
                                        package.kind,
                                        package.scope,
                                        if package.pinned { " · pinned" } else { "" }
                                    ))),
                            ),
                    )
                    .child(
                        popover::btn_ghost(theme, "Update", format!("pi-package-update-{ix}"))
                            .id(("pi-package-update", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.package_action(
                                    "update",
                                    update_source.clone(),
                                    remove_scope,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        popover::btn_ghost(theme, "Remove", format!("pi-package-remove-{ix}"))
                            .id(("pi-package-remove", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.dialog = Some(PiDialog::RemovePackage {
                                    source: remove_source.clone(),
                                    scope: remove_scope,
                                });
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let resources: Vec<PiResourceInfo> = snapshot
            .resources
            .iter()
            .filter(|resource| resource.scope == self.scope)
            .cloned()
            .collect();
        let has_resources = !resources.is_empty();
        let resource_rows = resources.into_iter().enumerate().map(|(ix, resource)| {
            crate::settings::widgets::card_row(theme, ix == 0)
                .child(crate::settings::widgets::badge(
                    theme,
                    SharedString::from(resource.kind),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(resource.path)),
                )
                .child(crate::settings::widgets::badge(
                    theme,
                    SharedString::from(format!("{:?}", resource.scope)),
                ))
                .into_any_element()
        });

        let packages_card: AnyElement = if package_rows.is_empty() {
            empty_card(
                theme,
                "No packages in this scope",
                "Install from npm, Git, or a local path. Pi packages can bundle extensions, skills, prompts, and themes.",
            )
        } else {
            crate::settings::widgets::section_card(theme)
                .children(package_rows)
                .into_any_element()
        };
        let resources_card: AnyElement = if !has_resources {
            empty_card(
                theme,
                "No loose resources",
                "Resources installed by a package appear inside its package row; direct paths appear here.",
            )
        } else {
            crate::settings::widgets::section_card(theme)
                .children(resource_rows)
                .into_any_element()
        };

        div().flex().flex_col()
            .child(crate::settings::widgets::warning_strip(theme, "Extensions execute with the full permissions of Pi on this device. Install only sources you trust."))
            .child(div().mt(px(18.0)).flex().flex_row().justify_end().child(
                popover::btn_primary(theme, "Install package")
                    .id("pi-install-package")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let input = cx.new(|cx| ComposerInput::new("npm:@scope/package, Git URL, or local path", cx));
                        this.dialog = Some(PiDialog::InstallPackage { input });
                        cx.notify();
                    }))
            ))
            .child(packages_card)
            .child(div().mt(px(24.0)).text_size(px(13.0)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme.text).child(SharedString::from("Loose local resources")))
            .child(resources_card)
            .into_any_element()
    }

    fn render_advanced(
        &mut self,
        snapshot: &PiSettingsSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut paths = vec![
            ("Global settings", snapshot.global_settings_path.clone()),
            ("Provider credentials", snapshot.auth_path.clone()),
            ("Custom models", snapshot.models_path.clone()),
        ];
        if let Some(path) = snapshot.project_settings_path.clone() {
            paths.insert(1, ("Project settings", path));
        }
        let copied = self.copied.clone();
        let rows = paths.into_iter().enumerate().map(|(ix, (label, path))| {
            let click_path = path.clone();
            let is_copied = copied.as_deref() == Some(path.as_str());
            crate::settings::widgets::card_row(theme, ix == 0)
                .child(crate::settings::widgets::row_tile(
                    theme,
                    crate::icons::DOCUMENT,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(crate::settings::widgets::row_title(
                            theme,
                            SharedString::from(label),
                        ))
                        .child(
                            div()
                                .mt(px(3.0))
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_muted.opacity(0.65))
                                .child(SharedString::from(path)),
                        ),
                )
                .child(
                    popover::btn_ghost(
                        theme,
                        if is_copied { "Copied" } else { "Copy path" },
                        format!("pi-copy-path-{ix}"),
                    )
                    .id(("pi-copy-path", ix))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.copy_path(click_path.clone(), cx)),
                    ),
                )
                .into_any_element()
        });

        div().flex().flex_col()
            .child(crate::settings::widgets::section_card(theme).children(rows))
            .child(crate::settings::widgets::warning_strip(theme, "Nova runs Pi in RPC mode. Tool and event extensions work; Pi TUI themes, custom editors, footers, and overlays do not render inside Nova."))
            .child(div().mt(px(24.0)).text_size(px(13.0)).font_weight(gpui::FontWeight::MEDIUM).text_color(theme.text).child(SharedString::from("Effective settings")))
            .child(div().id("pi-effective-settings").mt(px(10.0)).max_h(px(360.0)).overflow_y_scroll().rounded(px(12.0)).border_1().border_color(theme.border).bg(theme.surface)
                .p(px(16.0)).font_family(theme.font_mono.clone()).text_size(px(11.5)).line_height(px(18.0)).text_color(theme.text_muted)
                .child(SharedString::from(snapshot.effective_settings_json.clone())))
            .into_any_element()
    }

    fn render_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.dialog.as_ref()?;
        let (title, body, primary, destructive): (&str, AnyElement, &str, bool) = match dialog {
            PiDialog::ApiKey { provider_name, input, .. } => (
                "Add provider API key",
                div().mt(px(10.0)).flex().flex_col()
                    .child(popover::dialog_body(&theme, format!("Store an API key for {provider_name} in Pi's device-local auth.json. The key never leaves this device.")))
                    .child(div().mt(px(12.0)).child(popover::dialog_field(input.clone().into_any_element()).font_family(theme.font_mono.clone())))
                    .into_any_element(),
                "Save API key",
                false,
            ),
            PiDialog::OpenAiCompatible {
                base_url,
                api_key,
                has_stored_key,
            } => (
                "OpenAI-compatible provider",
                div()
                    .mt(px(10.0))
                    .flex()
                    .flex_col()
                    .child(popover::dialog_body(
                        &theme,
                        "Connect an endpoint that implements OpenAI Chat Completions. Nova loads its /models registry automatically; an API key is optional for local endpoints and stays in device-local auth.json when supplied.",
                    ))
                    .child(dialog_input(&theme, "Base URL", base_url.clone()))
                    .child(dialog_input(
                        &theme,
                        if *has_stored_key {
                            "API key (leave blank to keep saved key)"
                        } else {
                            "API key (optional)"
                        },
                        api_key.clone(),
                    ))
                    .into_any_element(),
                "Save provider",
                false,
            ),
            PiDialog::InstallPackage { input } => (
                "Install Pi package",
                div().mt(px(10.0)).flex().flex_col()
                    .child(popover::dialog_body(&theme, "Enter an npm package, Git URL, or local path. Project installs are written to the selected project's .pi/settings.json."))
                    .child(div().mt(px(12.0)).child(popover::dialog_field(input.clone().into_any_element()).font_family(theme.font_mono.clone())))
                    .into_any_element(),
                "Install",
                false,
            ),
            PiDialog::RemoveCredential { provider_name, .. } => (
                "Remove provider credential?",
                popover::dialog_body(&theme, format!("Pi will forget the stored {provider_name} credential on this device. Environment credentials are not changed.")).into_any_element(),
                "Remove credential",
                true,
            ),
            PiDialog::RemovePackage { source, .. } => (
                "Remove Pi package?",
                popover::dialog_body(&theme, format!("Remove {source} from this Pi scope? Existing Pi sessions keep their currently loaded runtime until restarted.")).into_any_element(),
                "Remove package",
                true,
            ),
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "pi-dialog-cancel")
                            .id("pi-dialog-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(if destructive {
                        popover::btn_danger(&theme, primary)
                            .id("pi-dialog-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_dialog(cx)))
                    } else {
                        popover::btn_primary(&theme, primary)
                            .id("pi-dialog-confirm")
                            .on_click(cx.listener(|this, _, _, cx| match this.dialog {
                                Some(PiDialog::ApiKey { .. }) => this.submit_api_key(cx),
                                Some(PiDialog::OpenAiCompatible { .. }) => {
                                    this.submit_openai_compatible(cx)
                                }
                                Some(PiDialog::InstallPackage { .. }) => this.submit_package(cx),
                                _ => {}
                            }))
                    }),
            );
        Some(popover::modal(
            "pi-settings-dialog",
            viewport,
            card.into_any_element(),
        ))
    }
}

fn decode_snapshot(
    result: Result<serde_json::Value, nova_rpc::RpcError>,
) -> Loadable<PiSettingsSnapshot> {
    match result {
        Ok(value) => serde_json::from_value(value)
            .map(Loadable::Ready)
            .unwrap_or_else(|error| {
                Loadable::Error(format!("Malformed Pi settings reply: {error}"))
            }),
        Err(error) => Loadable::Error(error.to_string()),
    }
}

fn dialog_input(theme: &Theme, label: &'static str, input: Entity<ComposerInput>) -> AnyElement {
    div()
        .mt(px(12.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(SharedString::from(label)),
        )
        .child(popover::dialog_field(input.into_any_element()).font_family(theme.font_mono.clone()))
        .into_any_element()
}

impl Render for PiSettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let dialog = self.render_dialog(window.viewport_size(), cx);
        let title = self.section.title();
        let subtitle = match self.section {
            PiSection::Overview => {
                "Runtime behavior on the device that launches this project's Pi sessions."
            }
            PiSection::Providers => {
                "Provider authentication used by Pi on this device. Secret values are never displayed."
            }
            PiSection::Packages => {
                "Install and inspect extensions, skills, prompts, and themes in Pi's native scopes."
            }
            PiSection::Advanced => {
                "Configuration files, RPC compatibility, and the effective merged Pi settings."
            }
        };

        let content: AnyElement = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => {
                popover::skeleton_rows("pi-settings-loading", &theme, 5).into_any_element()
            }
            Loadable::Error(error) => {
                let message: SharedString = error.clone().into();
                crate::settings::widgets::error_strip(message)
                    .id("pi-settings-retry")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(px(11.5))
                            .child(SharedString::from("Click to retry")),
                    )
                    .into_any_element()
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                match self.section {
                    PiSection::Overview => self.render_overview(&snapshot, &theme, cx),
                    PiSection::Providers => self.render_providers(&snapshot, &theme, cx),
                    PiSection::Packages => self.render_packages(&snapshot, &theme, cx),
                    PiSection::Advanced => self.render_advanced(&snapshot, &theme, cx),
                }
            }
        };

        div()
            .id("pi-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                crate::settings::widgets::page_column()
                    .child(crate::settings::widgets::page_header(&theme, title, None))
                    .child(crate::settings::widgets::page_subtitle(&theme, subtitle))
                    .child(self.render_context(&theme, cx))
                    .when_some(self.busy.clone(), |el, busy| {
                        el.child(
                            div()
                                .mt(px(14.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(busy),
                        )
                    })
                    .when_some(self.error.clone(), |el, error| {
                        el.child(crate::settings::widgets::error_strip(error))
                    })
                    .child(content),
            )
            .children(dialog)
    }
}

fn info_row(
    theme: &Theme,
    title: &str,
    detail: &str,
    icon: Option<&'static str>,
    badge: Option<&str>,
) -> AnyElement {
    crate::settings::widgets::card_row(theme, title == "Pi runtime")
        .when_some(icon, |el, icon| {
            el.child(crate::settings::widgets::row_tile(theme, icon))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(crate::settings::widgets::row_title(
                    theme,
                    SharedString::from(title.to_string()),
                ))
                .child(
                    div()
                        .mt(px(3.0))
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(SharedString::from(detail.to_string())),
                ),
        )
        .when_some(badge, |el, badge| {
            el.child(crate::settings::widgets::badge(
                theme,
                SharedString::from(badge.to_string()),
            ))
        })
        .into_any_element()
}

fn toggle_row(
    theme: &Theme,
    title: &str,
    description: &str,
    on: bool,
    id: &'static str,
    listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    crate::settings::widgets::card_row(theme, false)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(crate::settings::widgets::row_title(
                    theme,
                    SharedString::from(title.to_string()),
                ))
                .child(
                    div()
                        .mt(px(3.0))
                        .text_size(px(11.5))
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(SharedString::from(description.to_string())),
                ),
        )
        .child(toggle(theme, on).id(id).on_click(listener))
        .into_any_element()
}

fn action_row(
    theme: &Theme,
    title: &str,
    description: &str,
    value: &str,
    id: &'static str,
    listener: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    crate::settings::widgets::card_row(theme, false)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(crate::settings::widgets::row_title(
                    theme,
                    SharedString::from(title.to_string()),
                ))
                .child(
                    div()
                        .mt(px(3.0))
                        .text_size(px(11.5))
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(SharedString::from(description.to_string())),
                ),
        )
        .child(
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .rounded(px(7.0))
                .px(px(9.0))
                .py(px(5.0))
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(theme.text_muted)
                .hover(|s| s.bg(theme.wash(0.06)).text_color(theme.text))
                .on_click(listener)
                .child(SharedString::from(value.to_string()))
                .child(
                    crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                        .size(px(12.0))
                        .text_color(theme.text_muted),
                ),
        )
        .into_any_element()
}

fn toggle(theme: &Theme, on: bool) -> gpui::Div {
    div()
        .relative()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { theme.text } else { theme.wash(0.10) })
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if on { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if on { theme.bg } else { theme.text_muted }),
        )
}

fn empty_card(theme: &Theme, title: &str, body: &str) -> AnyElement {
    crate::settings::widgets::section_card(theme)
        .child(
            div()
                .px(px(20.0))
                .py(px(22.0))
                .flex()
                .flex_col()
                .child(crate::settings::widgets::row_title(
                    theme,
                    SharedString::from(title.to_string()),
                ))
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(body.to_string())),
                ),
        )
        .into_any_element()
}

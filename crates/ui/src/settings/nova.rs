//! Settings → Nova devices: direct pairing, discovery, routing, and peer configuration.

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum FoundPeer {
    Nova {
        addr: String,
        device_id: String,
        name: String,
        ticket: String,
        trusted: bool,
    },
    Open {
        addr: String,
    },
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerView {
    device_id: String,
    name: String,
    platform: String,
    endpoint: String,
    role: String,
    revoked: bool,
    paired_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalDevice {
    device_id: String,
    name: String,
    port: u16,
    ticket: String,
}

struct EditPeer {
    device_id: String,
    name: Entity<ComposerInput>,
    endpoint: Entity<ComposerInput>,
    role: String,
}

pub struct NovaPage {
    state: Entity<AppState>,
    local: Option<LocalDevice>,
    endpoint_input: Entity<ComposerInput>,
    code_input: Entity<ComposerInput>,
    ranges_input: Entity<ComposerInput>,
    peers: Vec<PeerView>,
    found: Vec<FoundPeer>,
    pairing_code: Option<String>,
    edit: Option<EditPeer>,
    error: Option<SharedString>,
    status: Option<SharedString>,
    busy: bool,
    task: Option<Task<()>>,
    peer_watch: Option<Task<()>>,
    _observe: Subscription,
    _input_events: Vec<Subscription>,
}

impl NovaPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let endpoint_input = cx.new(|cx| ComposerInput::new("nova-iroh ticket or endpoint id", cx));
        let code_input = cx.new(|cx| ComposerInput::new("6-digit pairing code", cx));
        let ranges_input = cx.new(|cx| ComposerInput::new("one cidr per line", cx));
        ranges_input.update(cx, |input, cx| input.set_text("10.0.0.0/24", cx));
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let input_events = [&endpoint_input, &code_input, &ranges_input]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |_: &mut Self, _, event, cx| {
                    if matches!(
                        event,
                        ComposerInputEvent::Edited | ComposerInputEvent::Submitted
                    ) {
                        cx.notify();
                    }
                })
            })
            .collect();
        let mut page = Self {
            state,
            local: None,
            endpoint_input,
            code_input,
            ranges_input,
            peers: Vec::new(),
            found: Vec::new(),
            pairing_code: None,
            edit: None,
            error: None,
            status: None,
            busy: false,
            task: None,
            peer_watch: None,
            _observe: observe,
            _input_events: input_events,
        };
        page.refresh(cx);
        page.watch_peers(cx);
        page
    }

    fn watch_peers(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.peer_watch = Some(cx.spawn(async move |this, cx| {
            let Ok(mut stream) = engine
                .client()
                .subscribe("NovaWatchPeers", serde_json::Value::Null)
                .await
            else {
                return;
            };
            while let Some(value) = stream.recv().await {
                let Ok(peers) = serde_json::from_value::<Vec<PeerView>>(value) else {
                    continue;
                };
                if this
                    .update(cx, |page, cx| {
                        let gained_peer = peers.iter().any(|peer| {
                            !page
                                .peers
                                .iter()
                                .any(|known| known.device_id == peer.device_id)
                        });
                        page.peers = peers;
                        if gained_peer {
                            // An inbound pairing consumes this engine's single-use code.
                            // Clear the displayed value as soon as the trust watch reports
                            // the new peer so the UI never suggests that code is reusable.
                            page.pairing_code = None;
                            page.status = Some("device paired".into());
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let state = self.state.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let (local, peers) = futures::join!(
                engine
                    .client()
                    .call("NovaLocalDevice", serde_json::Value::Null),
                engine
                    .client()
                    .call("NovaListPeers", serde_json::Value::Null),
            );
            let local = local
                .ok()
                .and_then(|value| serde_json::from_value(value).ok());
            let peers = peers
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<PeerView>>(value).ok());
            if let Some(peers) = peers.as_ref() {
                let devices = peers
                    .iter()
                    .filter(|peer| !peer.revoked)
                    .map(|peer| comet_proto::Device {
                        id: peer.device_id.clone(),
                        name: peer.name.clone(),
                        platform: peer.platform.clone(),
                        last_seen_at: None,
                        created_at: Some(peer.paired_at),
                        version: None,
                    })
                    .collect();
                state.update(cx, |state, cx| {
                    state.apply_nova_devices(devices);
                    cx.notify();
                });
            }
            this.update(cx, |page, cx| {
                page.local = local;
                if let Some(peers) = peers {
                    page.peers = peers;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn begin_pairing(&mut self, cx: &mut Context<Self>) {
        self.call(
            "NovaBeginPairing",
            serde_json::Value::Null,
            "pairing code ready",
            cx,
        );
    }

    fn pair(&mut self, cx: &mut Context<Self>) {
        let endpoint = self.endpoint_input.read(cx).text().trim().to_string();
        let code = self.code_input.read(cx).text().trim().to_string();
        if endpoint.is_empty() || code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            self.error = Some("enter a peer's iroh ticket and 6-digit code".into());
            cx.notify();
            return;
        }
        self.call(
            "NovaPairPeer",
            serde_json::json!({"endpoint": endpoint, "code": code}),
            "device paired",
            cx,
        );
    }

    fn run_scan(&mut self, cx: &mut Context<Self>) {
        let ranges = self
            .ranges_input
            .read(cx)
            .text()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            self.error = Some("add at least one cidr range".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.status = None;
        self.busy = true;
        self.found.clear();
        let port = self.local.as_ref().map_or(27655, |local| local.port);
        self.task = Some(cx.spawn(async move |this, cx| {
            match engine
                .client()
                .subscribe(
                    "NovaScan",
                    serde_json::json!({"ranges": ranges, "port": port}),
                )
                .await
            {
                Ok(mut stream) => {
                    while let Some(value) = stream.recv().await {
                        if value.get("done").is_some() {
                            if let Some(error) = value.get("error").and_then(|error| error.as_str())
                            {
                                this.update(cx, |page, cx| {
                                    page.error = Some(format!("scan failed: {error}").into());
                                    cx.notify();
                                })
                                .ok();
                            }
                            break;
                        }
                        if let Ok(peer) = serde_json::from_value::<FoundPeer>(value) {
                            this.update(cx, |page, cx| {
                                page.found.push(peer);
                                cx.notify();
                            })
                            .ok();
                        }
                    }
                    this.update(cx, |page, cx| {
                        page.busy = false;
                        if page.error.is_none() {
                            page.status = Some("scan complete".into());
                        }
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.busy = false;
                        page.error = Some(format!("scan failed: {error}").into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
        cx.notify();
    }

    fn call(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        success: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = true;
        self.error = None;
        self.status = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(value) => {
                        if method == "NovaBeginPairing" {
                            page.pairing_code = value
                                .get("code")
                                .and_then(|code| code.as_str())
                                .map(str::to_string);
                        } else {
                            page.status = Some(success.into());
                            if method == "NovaPairPeer" {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                            }
                            page.refresh(cx);
                        }
                    }
                    Err(error) => page.error = Some(format!("{success} failed: {error}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn use_address(&mut self, address: String, cx: &mut Context<Self>) {
        self.endpoint_input
            .update(cx, |input, cx| input.set_text(address, cx));
    }

    fn copy_ticket(&mut self, cx: &mut Context<Self>) {
        let Some(local) = self.local.as_ref() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(local.ticket.clone()));
        self.status = Some("iroh ticket copied".into());
        cx.notify();
    }

    fn test_peer(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.call(
            "NovaTestPeer",
            serde_json::json!({"deviceId": device_id}),
            "direct connection verified",
            cx,
        );
    }

    fn revoke(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.call(
            "NovaRevokePeer",
            serde_json::json!({"deviceId": device_id}),
            "device revoked",
            cx,
        );
    }

    fn forget(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.call(
            "NovaForgetPeer",
            serde_json::json!({"deviceId": device_id}),
            "device forgotten",
            cx,
        );
    }

    fn open_edit(&mut self, peer: PeerView, cx: &mut Context<Self>) {
        let name = cx.new(|cx| ComposerInput::new("device name", cx));
        name.update(cx, |input, cx| input.set_text(peer.name, cx));
        let endpoint = cx.new(|cx| ComposerInput::new("nova-iroh ticket or endpoint id", cx));
        endpoint.update(cx, |input, cx| input.set_text(peer.endpoint, cx));
        self.edit = Some(EditPeer {
            device_id: peer.device_id,
            name,
            endpoint,
            role: peer.role,
        });
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        let name = edit.name.read(cx).text().trim().to_string();
        let endpoint = edit.endpoint.read(cx).text().trim().to_string();
        self.call(
            "NovaUpdatePeer",
            serde_json::json!({
                "deviceId": edit.device_id,
                "name": name,
                "endpoint": endpoint,
                "role": edit.role,
            }),
            "device updated",
            cx,
        );
    }

    fn render_edit_dialog(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let edit = self.edit.as_ref()?;
        let name = edit.name.clone();
        let endpoint = edit.endpoint.clone();
        let role = edit.role.clone();
        let next_role = if role == "admin" { "peer" } else { "admin" }.to_string();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Configure Nova device"))
            .child(labeled_input(&theme, "Name", name))
            .child(labeled_input(&theme, "Iroh ticket", endpoint))
            .child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .items_center()
                    .child(popover::dialog_body(&theme, format!("Role: {role}")))
                    .child(div().flex_1())
                    .child(
                        popover::btn_ghost(&theme, "Toggle role", "nova-toggle-role")
                            .id("nova-toggle-role")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(edit) = this.edit.as_mut() {
                                    edit.role = next_role.clone();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "nova-edit-cancel")
                            .id("nova-edit-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.edit = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Save")
                            .id("nova-edit-save")
                            .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal(
            "nova-edit-dialog",
            window.viewport_size(),
            card,
        ))
    }
}

fn labeled_input(theme: &Theme, label: &'static str, input: Entity<ComposerInput>) -> AnyElement {
    div()
        .mt(px(12.0))
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(label)),
        )
        .child(popover::dialog_field(input.into_any_element()))
        .into_any_element()
}

fn short_ticket(ticket: &str) -> String {
    if ticket.len() <= 32 {
        ticket.to_string()
    } else {
        format!("{}…{}", &ticket[..20], &ticket[ticket.len() - 8..])
    }
}

impl Render for NovaPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let emerald = crate::theme::oklch(0.765, 0.177, 163.223);
        let amber = crate::theme::oklch(0.78, 0.16, 70.0);
        let dialog = self.render_edit_dialog(window, cx);

        let local_line = self
            .local
            .as_ref()
            .map(|local| format!("{} · {} · port {}", local.name, local.device_id, local.port))
            .unwrap_or_else(|| "loading local identity…".into());
        let pairing = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "This Nova Engine"))
                            .child(widgets::row_description(&theme, local_line)),
                    )
                    .child(if let Some(code) = self.pairing_code.clone() {
                        widgets::badge_active(SharedString::from(code))
                    } else {
                        widgets::badge(&theme, "not pairing")
                    }),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_description(
                        &theme,
                        "Copy this device's iroh ticket, generate a single-use code, then enter both on the other Nova.",
                    ))
                    .child(div().flex_1())
                    .child(
                        popover::btn_ghost(&theme, "Copy ticket", "nova-copy-ticket")
                            .id("nova-copy-ticket")
                            .on_click(cx.listener(|this, _, _, cx| this.copy_ticket(cx))),
                    )
                    .child(
                        popover::btn_ghost(&theme, "New code", "nova-new-code")
                            .id("nova-new-code")
                            .on_click(cx.listener(|this, _, _, cx| this.begin_pairing(cx))),
                    ),
            );

        let connect = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true).child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(&theme, "Pair another Nova"))
                    .child(widgets::row_description(
                        &theme,
                        "Enter its encrypted iroh overlay ticket and the code shown on that device.",
                    ))
                    .child(labeled_input(
                        &theme,
                        "Iroh ticket",
                        self.endpoint_input.clone(),
                    ))
                    .child(labeled_input(
                        &theme,
                        "Pairing code",
                        self.code_input.clone(),
                    ))
                    .child(
                        div().mt(px(12.0)).flex().justify_end().child(
                            popover::btn_primary(
                                &theme,
                                if self.busy {
                                    "Connecting…"
                                } else {
                                    "Pair device"
                                },
                            )
                            .id("nova-pair-device")
                            .on_click(cx.listener(|this, _, _, cx| this.pair(cx))),
                        ),
                    ),
            ),
        );

        let discovery = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true).child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(&theme, "LAN discovery"))
                    .child(widgets::row_description(
                        &theme,
                        "Scan explicit private cidrs. discovered devices are never trusted automatically.",
                    ))
                    .child(labeled_input(&theme, "CIDR ranges", self.ranges_input.clone()))
                    .child(
                        div()
                            .mt(px(12.0))
                            .flex()
                            .justify_end()
                            .child(
                                popover::btn_ghost(
                                    &theme,
                                    if self.busy { "Scanning…" } else { "Scan" },
                                    "nova-scan",
                                )
                                .id("nova-scan")
                                .on_click(cx.listener(|this, _, _, cx| this.run_scan(cx))),
                            ),
                    ),
            ),
        );

        let mut found_card = widgets::section_card(&theme);
        for (index, peer) in self.found.iter().enumerate() {
            let (title, detail, ticket, trusted) = match peer {
                FoundPeer::Nova {
                    addr,
                    device_id,
                    name,
                    ticket,
                    trusted,
                } => (
                    name.clone(),
                    format!("{device_id} · {addr}"),
                    Some(ticket.clone()),
                    *trusted,
                ),
                FoundPeer::Open { addr } => ("Unknown service".into(), addr.clone(), None, false),
            };
            found_card = found_card.child(
                widgets::card_row(&theme, index == 0)
                    .child(div().size(px(8.0)).rounded_full().bg(if trusted {
                        emerald
                    } else {
                        amber
                    }))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, title))
                            .child(widgets::row_description(&theme, detail)),
                    )
                    .when_some(ticket, |row, ticket| {
                        row.child(
                            popover::btn_ghost(&theme, "Use ticket", "nova-use-address")
                                .id(("nova-use-address", index))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.use_address(ticket.clone(), cx)
                                })),
                        )
                    }),
            );
        }

        let mut peers = widgets::section_card(&theme);
        if self.peers.is_empty() {
            peers = peers.child(
                widgets::card_row(&theme, true).child(widgets::row_description(
                    &theme,
                    "No paired Nova devices yet.",
                )),
            );
        } else {
            for (index, peer) in self.peers.clone().into_iter().enumerate() {
                let test_id = peer.device_id.clone();
                let revoke_id = peer.device_id.clone();
                let forget_id = peer.device_id.clone();
                let edit_peer = peer.clone();
                peers = peers.child(
                    widgets::card_row(&theme, index == 0)
                        .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(&theme, peer.name))
                                .child(widgets::row_description(
                                    &theme,
                                    format!(
                                        "{} · {} · {}{}",
                                        peer.platform,
                                        peer.role,
                                        short_ticket(&peer.endpoint),
                                        if peer.revoked { " · revoked" } else { "" }
                                    ),
                                )),
                        )
                        .child(
                            popover::btn_ghost(&theme, "Test", "nova-test")
                                .id(("nova-test", index))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.test_peer(test_id.clone(), cx)
                                })),
                        )
                        .child(
                            popover::btn_ghost(&theme, "Edit", "nova-edit")
                                .id(("nova-edit", index))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_edit(edit_peer.clone(), cx)
                                })),
                        )
                        .child(
                            popover::btn_ghost(&theme, "Revoke", "nova-revoke")
                                .id(("nova-revoke", index))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.revoke(revoke_id.clone(), cx)
                                })),
                        )
                        .child(
                            popover::btn_ghost(&theme, "Forget", "nova-forget")
                                .id(("nova-forget", index))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.forget(forget_id.clone(), cx)
                                })),
                        ),
                );
            }
        }

        let mut page = widgets::page_column().child(widgets::page_header(
            &theme,
            "Nova devices",
            Some(self.peers.len()),
        ));
        if let Some(error) = self.error.clone() {
            page = page.child(widgets::warning_strip(&theme, error));
        }
        if let Some(status) = self.status.clone() {
            page = page.child(
                div()
                    .mb(px(8.0))
                    .text_size(px(12.0))
                    .text_color(emerald)
                    .child(status),
            );
        }
        page = page.child(pairing).child(connect).child(discovery);
        if !self.found.is_empty() {
            page = page.child(found_card);
        }
        page.child(peers).children(dialog)
    }
}

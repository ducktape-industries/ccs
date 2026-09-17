//! Native GPUI Kit dashboard. Network and credential work stays off the UI thread.
mod backend;
mod format;
mod messenger;
mod platform;

use backend::{Account, Prefs};
use ccs::model::Provider;
use ccs::routing::{Routing, Rule};
use futures::StreamExt;
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::{
    button::*,
    checkbox::Checkbox,
    input::{Input, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    progress::Progress,
    switch::Switch,
    tooltip::Tooltip,
    *,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

actions!(ccs, [Quit]);

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Accounts,
    Routes,
    Settings,
    Messenger,
}

struct Dashboard {
    messenger: Entity<messenger::Messenger>,
    accounts: Vec<Account>,
    prefs: Prefs,
    routes: Routing,
    page: Page,
    error: String,
    status: String,
    gateway_status: String,
    busy: bool,
    refreshing: bool,
    routes_ready: bool,
    confirming: Option<String>,
    port: Entity<InputState>,
    model: Entity<InputState>,
    codex_models: Vec<String>,
    claude_models: Vec<String>,
    models_loading: bool,
    model_error: String,
    provider: Provider,
    selected: Vec<String>,
    editing: Option<usize>,
    editor_open: bool,
    scroll: ScrollHandle,
    #[cfg(target_os = "macos")]
    tray: Option<tray_icon::TrayIcon>,
}

impl Dashboard {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prefs = backend::prefs();
        let port =
            cx.new(|cx| InputState::new(window, cx).default_value(prefs.gateway_port.clone()));
        let model = cx.new(|cx| InputState::new(window, cx).placeholder("Model ID or prefix*"));
        backend::set_watch(
            format::pool_for(prefs.rotation_on, &prefs.pool),
            prefs.notifications_on,
        );
        cx.on_app_quit(|_, _| async {
            backend::shutdown();
        })
        .detach();
        let messenger = cx.new(|cx| messenger::Messenger::new(window, cx));
        let mut this = Self {
            messenger,
            accounts: vec![],
            prefs,
            routes: Routing::default(),
            page: if std::env::args().any(|a| a == "--messenger") {
                Page::Messenger
            } else {
                Page::Accounts
            },
            error: String::new(),
            status: "Loading…".into(),
            gateway_status: "Off".into(),
            busy: false,
            refreshing: false,
            routes_ready: false,
            confirming: None,
            port,
            model,
            codex_models: vec![],
            claude_models: vec![],
            models_loading: true,
            model_error: String::new(),
            provider: Provider::Claude,
            selected: vec![],
            editing: None,
            editor_open: false,
            scroll: ScrollHandle::new(),
            #[cfg(target_os = "macos")]
            tray: None,
        };
        #[cfg(all(target_os = "macos", not(test)))]
        {
            match tray_icon::Icon::from_rgba(include_bytes!("../assets/tray.rgba").to_vec(), 22, 22)
                .map_err(|e| e.to_string())
                .and_then(|icon| {
                    tray_icon::TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_icon_as_template(true)
                        .with_title("ccs")
                        .with_tooltip("CCS — account usage")
                        .build()
                        .map_err(|e| e.to_string())
                }) {
                Ok(tray) => this.tray = Some(tray),
                Err(e) => this.error = format!("Menu bar: {e}"),
            }
            let mut clicks = backend::tray_clicks();
            let handle = window.window_handle();
            cx.spawn(async move |_, cx| {
                while clicks.next().await.is_some() {
                    let _ = handle.update(cx, |_, window, cx| {
                        platform::follow_active_space();
                        window.activate_window();
                        cx.activate(true);
                    });
                }
            })
            .detach();
            window.on_window_should_close(cx, |_, cx| {
                cx.hide();
                false
            });
        }
        cx.spawn(async |this, cx| {
            let accounts = backend::load().await;
            let routes = backend::load_routes().await;
            let _ = this.update(cx, |this, cx| {
                match accounts {
                    Ok(a) => this.set_accounts(a),
                    Err(e) => this.error = e.message,
                }
                match routes {
                    Ok(r) => {
                        this.routes = r;
                        this.routes_ready = true;
                    }
                    Err(e) => this.error = e.message,
                }
                this.status = "".into();
                cx.notify();
            });
        })
        .detach();
        cx.spawn(async |this, cx| {
            let (claude, codex) = futures::join!(
                backend::load_models(Provider::Claude),
                backend::load_models(Provider::Codex),
            );
            let _ = this.update(cx, |this, cx| {
                this.set_models(claude, codex);
                cx.notify();
            });
        })
        .detach();
        if !std::env::args().any(|a| a == "--preview") {
            let mut watch = Box::pin(backend::watch(90.0));
            cx.spawn(async move |this, cx| {
                while let Some(turn) = watch.next().await {
                    if this
                        .update(cx, |this, cx| {
                            match turn {
                                Ok(turn) => {
                                    this.set_accounts(turn.accounts);
                                    this.status =
                                        format::watcher_said(&turn.notices, &turn.rotated);
                                }
                                Err(e) => this.status = e.message,
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            if this.prefs.gateway_on {
                this.gateway(cx);
            }
        }
        if this.page == Page::Messenger {
            this.messenger.update(cx, |view, cx| view.refresh(cx));
        }
        this
    }

    fn set_accounts(&mut self, accounts: Vec<Account>) {
        self.accounts = accounts;
        #[cfg(target_os = "macos")]
        if let Some(tray) = &self.tray {
            tray.set_title(Some(format::bar_label(&self.accounts)));
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::Messenger {
            self.messenger.update(cx, |view, cx| view.refresh(cx));
            return;
        }
        if self.busy || self.models_loading {
            return;
        }
        self.busy = true;
        self.refreshing = true;
        self.models_loading = true;
        self.error.clear();
        self.status = "Refreshing…".into();
        cx.notify();
        cx.spawn(async |this, cx| {
            let accounts = backend::refresh().await;
            let (claude, codex) = futures::join!(
                backend::load_models(Provider::Claude),
                backend::load_models(Provider::Codex),
            );
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.refreshing = false;
                this.set_models(claude, codex);
                match accounts {
                    Ok(accounts) => {
                        let failed = accounts.iter().filter(|a| !a.note.is_empty()).count();
                        this.set_accounts(accounts);
                        this.status = if failed == 0 {
                            if this.model_error.is_empty() {
                                "Updated".into()
                            } else {
                                "Usage updated · model list could not refresh".into()
                            }
                        } else {
                            format!("Updated · {failed} account(s) could not refresh")
                        };
                    }
                    Err(error) => {
                        this.error = error.message;
                        this.status.clear();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn persist(&mut self) {
        if !backend::save_prefs(
            self.prefs.gateway_on,
            self.prefs.gateway_port.clone(),
            self.prefs.rotation_on,
            self.prefs.pool.clone(),
            self.prefs.notifications_on,
            self.prefs.launch_at_login,
        ) {
            self.error = "Could not save preferences".into();
        }
        backend::set_watch(
            format::pool_for(self.prefs.rotation_on, &self.prefs.pool),
            self.prefs.notifications_on,
        );
    }

    fn gateway(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        let (on, port, pool) = (
            self.prefs.gateway_on,
            self.prefs.gateway_port.clone(),
            format::pool_for(self.prefs.rotation_on, &self.prefs.pool),
        );
        cx.spawn(async move |this, cx| {
            let result = backend::gateway(on, port, pool).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(line) => this.gateway_status = line,
                    Err(e) => {
                        this.prefs.gateway_on = false;
                        this.gateway_status = e.message;
                    }
                }
                this.persist();
                cx.notify();
            });
        })
        .detach();
    }

    fn switch(&mut self, slug: String, force: bool, cx: &mut Context<Self>) {
        self.busy = true;
        self.error.clear();
        self.confirming = None;
        cx.spawn(async move |this, cx| {
            let result = backend::switch(slug, force).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(a) => this.set_accounts(a),
                    Err(e) if e.spent => this.confirming = Some(e.slug),
                    Err(e) => this.error = e.message,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn store_routes(&mut self, routes: Routing, cx: &mut Context<Self>) {
        self.busy = true;
        self.error.clear();
        cx.spawn(async move |this, cx| {
            let result = backend::save_routes(routes.clone()).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.routes = routes;
                        this.selected.clear();
                        this.editing = None;
                        this.editor_open = false;
                        this.status = "Saved".into();
                    }
                    Err(e) => this.error = e.message,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn accounts_page(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div().v_flex().gap_4();
        for provider in ["claude", "codex"] {
            let accounts: Vec<_> =
                self.accounts.iter().filter(|a| a.provider == provider).collect();
            let mut group = div().v_flex().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pb_2()
                    .child(div().font_semibold().child(if provider == "claude" {
                        "Claude"
                    } else {
                        "Codex"
                    }))
                    .child(muted(accounts.len().to_string())),
            );
            if accounts.is_empty() {
                group = group.child(row().child(muted(format!("ccs add --{provider}"))));
            }
            for a in accounts {
                let slug = a.slug.clone();
                let assigned = self
                    .routes
                    .rules
                    .iter()
                    .filter(|r| {
                        r.provider.to_string() == a.provider && r.accounts.contains(&a.slug)
                    })
                    .map(|r| {
                        let name = r
                            .model
                            .strip_prefix("claude-")
                            .unwrap_or(&r.model)
                            .trim_end_matches(['*', '-']);
                        if r.accounts.first() == Some(&a.slug) {
                            name.to_string()
                        } else {
                            format!("{name} (fallback)")
                        }
                    })
                    .collect::<Vec<_>>();
                let detail = if assigned.is_empty() {
                    a.plan.clone()
                } else {
                    format!("{} · {}", a.plan, assigned.join(", "))
                };
                let count = a.limits.len();
                let available = f32::from(window.viewport_size().width) - 362.;
                let columns =
                    (((available + 16.) / 128.).floor() as usize).max(1).min(count.max(1)) as u16;
                let mut limits = div().grid().grid_cols(columns).flex_1().min_w_0().gap_4();
                for (i, limit) in a.limits.iter().enumerate() {
                    let label = limit.column.clone();
                    limits = limits.child(
                        div()
                            .id(SharedString::from(format!("metric-{}-{i}", a.slug)))
                            .test_support()
                            .tooltip(move |window, cx| {
                                Tooltip::new(label.clone()).build(window, cx)
                            })
                            .v_flex()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .text_xs()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_ellipsis()
                                            .child(limit.column.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .child(format::percent_label(limit.percent)),
                                    ),
                            )
                            .child(
                                Progress::new((SharedString::from(a.slug.clone()), i))
                                    .value(limit.percent as f32)
                                    .xsmall()
                                    .accessibility_label(format!(
                                        "{} {} usage",
                                        a.email, limit.column
                                    ))
                                    .color(rgb(0x525252)),
                            )
                            .child(muted(limit.resets_in.clone()).text_xs()),
                    );
                }
                if count == 0 {
                    limits = limits.child(muted(if a.note.is_empty() {
                        "No usage data".into()
                    } else {
                        a.note.clone()
                    }));
                }
                group = group.child(
                    row()
                        .items_center()
                        .gap_5()
                        .child(
                            div()
                                .v_flex()
                                .w(px(210.))
                                .flex_shrink_0()
                                .gap_1()
                                .child(a.email.clone())
                                .child(muted(detail).text_xs()),
                        )
                        .child(limits)
                        .child(
                            Button::new(SharedString::from(format!("switch-{}", a.slug)))
                                .w(px(64.))
                                .small()
                                .ghost()
                                .label(if a.active { "Active" } else { "Use" })
                                .disabled(a.active || self.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.switch(slug.clone(), false, cx)
                                })),
                        ),
                );
            }
            content = content.child(group);
        }
        content
    }

    fn set_models(
        &mut self,
        claude: Result<Vec<String>, backend::Failure>,
        codex: Result<Vec<String>, backend::Failure>,
    ) {
        self.models_loading = false;
        let mut errors = vec![];
        for (name, result, models) in
            [("Claude", claude, &mut self.claude_models), ("Codex", codex, &mut self.codex_models)]
        {
            match result {
                Ok(ids) => *models = ids,
                Err(error) => errors.push(format!("{name}: {}", error.message)),
            }
        }
        self.model_error = errors.join(" · ");
    }

    fn model_options(&self) -> Vec<String> {
        let mut options = match self.provider {
            Provider::Claude => self.claude_models.clone(),
            Provider::Codex => self.codex_models.clone(),
        };
        for model in
            self.routes.rules.iter().filter(|r| r.provider == self.provider).map(|r| &r.model)
        {
            if !options.contains(model) {
                options.push(model.clone());
            }
        }
        options
    }

    fn routes_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div().v_flex().gap_4().child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(Button::new("copy-launch").small().ghost().label("ccs claude  ⧉").on_click(
                    |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string("ccs claude".into()))
                    },
                ))
                .child(
                    Button::new("new-route")
                        .small()
                        .label("Add route")
                        .disabled(self.busy || !self.routes_ready)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.editing = None;
                            this.editor_open = true;
                            this.scroll.scroll_to_bottom();
                            this.selected.clear();
                            this.model.update(cx, |s, cx| s.set_value("", window, cx));
                            cx.notify();
                        })),
                ),
        );
        let mut table = div().v_flex().child(
            row()
                .py_2()
                .text_xs()
                .text_color(rgb(0x737373))
                .child(div().w(px(230.)).child("Model"))
                .child(div().flex_1().child("Primary → fallback")),
        );
        if self.routes.rules.is_empty() {
            table = table.child(row().child(muted("No routes")));
        }
        for (i, rule) in self.routes.rules.iter().enumerate() {
            let r = rule.clone();
            let targets = rule
                .accounts
                .iter()
                .map(|slug| {
                    self.accounts
                        .iter()
                        .find(|a| a.slug == *slug)
                        .map(|a| a.email.as_str())
                        .unwrap_or(slug)
                })
                .collect::<Vec<_>>()
                .join(" → ");
            table = table.child(
                row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .v_flex()
                            .w(px(218.))
                            .flex_shrink_0()
                            .gap_1()
                            .child(rule.model.clone())
                            .child(muted(rule.provider.to_string()).text_xs()),
                    )
                    .child(div().flex_1().min_w_0().child(targets))
                    .child(
                        Button::new(("edit", i))
                            .small()
                            .ghost()
                            .label("Edit")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.provider = r.provider;
                                this.selected = r.accounts.clone();
                                this.editing = Some(i);
                                this.editor_open = true;
                                this.scroll.scroll_to_bottom();
                                this.model
                                    .update(cx, |s, cx| s.set_value(r.model.clone(), window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(("delete", i))
                            .small()
                            .ghost()
                            .label("Remove")
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut routes = this.routes.clone();
                                routes.rules.remove(i);
                                this.store_routes(routes, cx);
                            })),
                    ),
            );
        }
        content = content.child(table);
        if !self.editor_open {
            return content;
        }
        let options = self.model_options();
        let model_input = self.model.clone();
        let mut editor = div()
            .v_flex()
            .gap_3()
            .p_4()
            .border_1()
            .border_color(rgb(0xe5e5e5))
            .rounded_md()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(280.)).child(Input::new(&self.model).id("model-input")))
                    .child(
                        Button::new("model-options")
                            .small()
                            .label(if self.models_loading { "Loading…" } else { "Models ▾" })
                            .disabled(self.busy || self.models_loading || options.is_empty())
                            .dropdown_menu(move |mut menu, _, cx| {
                                for option in &options {
                                    let model = option.clone();
                                    let input = model_input.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(option.clone())
                                            .checked(
                                                model_input.read(cx).value().as_ref() == option,
                                            )
                                            .on_click(move |_, window, cx| {
                                                input.update(cx, |state, cx| {
                                                    state.set_value(model.clone(), window, cx)
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .children(Provider::ALL.into_iter().map(|p| {
                        Button::new(SharedString::from(format!("provider-{p}")))
                            .small()
                            .ghost()
                            .label(p.to_string())
                            .selected(self.provider == p)
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.provider != p {
                                    this.provider = p;
                                    this.selected.clear();
                                    this.model.update(cx, |s, cx| s.set_value("", window, cx));
                                    cx.notify();
                                }
                            }))
                    })),
            )
            .child(
                muted("Choose a model or type an ID. Use a trailing * to match a prefix.")
                    .text_xs(),
            )
            .when(!self.model_error.is_empty(), |editor| {
                editor.child(muted(self.model_error.clone()).text_xs())
            })
            .child(muted("Select accounts in priority order.").text_xs());
        for a in self.accounts.iter().filter(|a| a.provider == self.provider.to_string()) {
            let slug = a.slug.clone();
            let order = self.selected.iter().position(|s| s == &a.slug);
            editor = editor.child(
                account_checkbox(
                    format!("route-account-{slug}"),
                    format!(
                        "{}{}",
                        order.map(|n| format!("{}. ", n + 1)).unwrap_or_default(),
                        a.email
                    ),
                )
                .checked(order.is_some())
                .disabled(self.busy)
                .on_click(cx.listener(move |this, on, _, cx| {
                    this.selected = format::toggled(&this.selected, slug.clone(), *on);
                    cx.notify();
                })),
            );
        }
        editor = editor.child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("save-route")
                        .small()
                        .primary()
                        .label("Save")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let rule = Rule {
                                provider: this.provider,
                                model: this.model.read(cx).value().trim().to_string(),
                                accounts: this.selected.clone(),
                            };
                            let mut routes = this.routes.clone();
                            if let Some(i) = this.editing {
                                routes.rules[i] = rule;
                            } else {
                                routes.rules.push(rule);
                            }
                            this.store_routes(routes, cx);
                        })),
                )
                .child(
                    Button::new("cancel-route")
                        .small()
                        .ghost()
                        .label("Cancel")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.editor_open = false;
                            this.editing = None;
                            this.selected.clear();
                            cx.notify();
                        })),
                ),
        );
        content.child(editor)
    }

    fn settings_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div()
            .v_flex()
            .child(
                row().justify_between().items_center().child("Local proxy").child(
                    Switch::new("gateway")
                        .accessibility_label("Local proxy")
                        .checked(self.prefs.gateway_on)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, on, _, cx| {
                            this.prefs.gateway_on = *on;
                            this.gateway(cx);
                            cx.notify();
                        })),
                ),
            )
            .child(row().justify_between().items_center().child("Port").child(
                div().flex().gap_2().child(div().w(px(100.)).child(Input::new(&self.port))).child(
                    Button::new("port").small().label("Apply").disabled(self.busy).on_click(
                        cx.listener(|this, _, _, cx| {
                            let port = this.port.read(cx).value().to_string();
                            if !format::is_port(&port) {
                                this.error = "Port must be 1–65535".into();
                            } else {
                                this.error.clear();
                                this.prefs.gateway_port = port;
                                this.gateway(cx);
                            }
                            cx.notify();
                        }),
                    ),
                ),
            ))
            .child(
                row().justify_between().items_center().child("Auto rotate at 90%").child(
                    Switch::new("rotation")
                        .accessibility_label("Automatic rotation")
                        .checked(self.prefs.rotation_on)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, on, _, cx| {
                            this.prefs.rotation_on = *on;
                            this.persist();
                            this.gateway(cx);
                            cx.notify();
                        })),
                ),
            );
        if self.prefs.rotation_on {
            for a in &self.accounts {
                let slug = a.slug.clone();
                content = content.child(
                    row().pl_4().child(
                        account_checkbox(
                            format!("pool-{slug}"),
                            format!("{} · {}", a.provider, a.email),
                        )
                        .checked(self.prefs.pool.contains(&slug))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, on, _, cx| {
                            this.prefs.pool = format::toggled(&this.prefs.pool, slug.clone(), *on);
                            this.persist();
                            this.gateway(cx);
                            cx.notify();
                        })),
                    ),
                );
            }
        }
        content
            .child(
                row().justify_between().items_center().child("Notifications").child(
                    Switch::new("notifications")
                        .accessibility_label("Notifications")
                        .checked(self.prefs.notifications_on)
                        .on_click(cx.listener(|this, on, _, cx| {
                            this.prefs.notifications_on = *on;
                            this.persist();
                            cx.notify();
                        })),
                ),
            )
            .child(
                row().justify_between().items_center().child("Launch at login").child(
                    Switch::new("login")
                        .accessibility_label("Launch at login")
                        .checked(self.prefs.launch_at_login)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, on, _, cx| {
                            let on = *on;
                            this.busy = true;
                            cx.spawn(async move |this, cx| {
                                let result = backend::launch_at_login(on).await;
                                let _ = this.update(cx, |this, cx| {
                                    this.busy = false;
                                    match result {
                                        Ok(on) => {
                                            this.prefs.launch_at_login = on;
                                            this.persist();
                                        }
                                        Err(e) => this.error = e.message,
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        })),
                ),
            )
            .child(div().pt_3().child(muted(self.gateway_status.clone()).text_xs()))
    }
}

fn account_checkbox(id: String, label: String) -> Checkbox {
    // The built-in label clips descenders with its 1em line height.
    Checkbox::new(SharedString::from(id))
        .small()
        .items_center()
        .accessibility_label(label.clone())
        .child(div().line_height(px(20.)).child(label))
}

fn muted(text: impl Into<SharedString>) -> Div {
    div().text_sm().text_color(rgb(0x737373)).child(text.into())
}
fn row() -> Div {
    div().flex().py_3().border_b_1().border_color(rgb(0xe5e5e5))
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tabs = div()
            .flex()
            .items_center()
            .gap_1()
            .pb_3()
            .border_b_1()
            .border_color(rgb(0xe5e5e5))
            .when(!cfg!(target_os = "macos"), |tabs| {
                tabs.child(div().font_semibold().mr_6().child("ccs"))
            })
            .when(cfg!(target_os = "macos") && !window.is_fullscreen(), |tabs| tabs.pl(px(70.)));
        for (id, page, label) in [
            ("accounts", Page::Accounts, "Accounts"),
            ("routes", Page::Routes, "Routes"),
            ("messenger", Page::Messenger, "Messenger"),
            ("settings", Page::Settings, "Settings"),
        ] {
            tabs = tabs.child(
                Button::new(id).small().ghost().label(label).selected(self.page == page).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        if page == Page::Messenger {
                            this.messenger.update(cx, |view, cx| view.refresh(cx));
                        }
                        this.scroll.set_offset(point(px(0.), px(0.)));
                        this.status.clear();
                        cx.notify();
                    }),
                ),
            );
        }
        tabs = tabs
            .child(
                div()
                    .id("window-drag")
                    .flex_1()
                    .h(px(24.))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .on_double_click(|_, window, _| window.titlebar_double_click()),
            )
            .child(
                Button::new("refresh")
                    .small()
                    .ghost()
                    .label(if self.refreshing { "Refreshing…" } else { "Refresh" })
                    .disabled(self.page != Page::Messenger && (self.busy || self.models_loading))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                Button::new("quit").small().ghost().label("Quit").on_click(|_, _, cx| cx.quit()),
            );
        let body = match self.page {
            Page::Accounts => self.accounts_page(window, cx).into_any_element(),
            Page::Routes => self.routes_page(cx).into_any_element(),
            Page::Settings => self.settings_page(cx).into_any_element(),
            Page::Messenger => self.messenger.clone().into_any_element(),
        };
        let mut main = div()
            .v_flex()
            .size_full()
            .p_6()
            .when(cfg!(target_os = "macos"), |main| main.pt_3())
            .gap_4()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x171717))
            .text_sm()
            .child(tabs);
        if !self.error.is_empty() && self.page != Page::Messenger {
            main = main.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_3()
                    .border_1()
                    .border_color(rgb(0xa3a3a3))
                    .child(self.error.clone())
                    .child(Button::new("dismiss-error").small().ghost().label("Dismiss").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.error.clear();
                            cx.notify();
                        }),
                    )),
            );
        }
        if let Some(slug) = self.confirming.clone() {
            main = main.child(
                row()
                    .gap_3()
                    .items_center()
                    .child(div().flex_1().child(format::confirm_question(&self.accounts, &slug)))
                    .child(Button::new("force").small().label("Switch anyway").on_click(
                        cx.listener(move |this, _, _, cx| this.switch(slug.clone(), true, cx)),
                    ))
                    .child(Button::new("cancel-force").small().ghost().label("Cancel").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirming = None;
                            cx.notify();
                        }),
                    )),
            );
        }
        main.child(
            div()
                .id("page-scroll")
                .track_scroll(&self.scroll)
                .overflow_y_scroll()
                .flex_1()
                .min_h_0()
                .child(body),
        )
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    muted(if self.page == Page::Messenger {
                        String::new()
                    } else {
                        format!("Local CCS · {}", format::polled_line(&self.accounts))
                    })
                    .text_xs(),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(muted(self.status.clone()).text_xs())
                        .when(self.page != Page::Messenger, |footer| {
                            footer.child(
                                Button::new("open-remote-messenger")
                                    .small()
                                    .ghost()
                                    .label("View remote CCS →")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.page = Page::Messenger;
                                        this.scroll.set_offset(point(px(0.), px(0.)));
                                        this.messenger.update(cx, |view, cx| view.open_remote(cx));
                                        cx.notify();
                                    })),
                            )
                        }),
                ),
        )
    }
}

// Reserve four account rows; grow through eight, including wrapped usage limits.
fn initial_height(accounts: &[Account]) -> f32 {
    let extra_rows: usize = ["claude", "codex"]
        .into_iter()
        .flat_map(|provider| accounts.iter().filter(move |a| a.provider == provider))
        .take(8)
        .map(|a| a.limits.len().div_ceil(3).saturating_sub(1))
        .sum();
    200. + accounts.len().clamp(4, 8) as f32 * 80. + extra_rows as f32 * 64.
}

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        #[cfg(not(target_os = "macos"))]
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let accounts = backend::load().await.unwrap_or_default();
            let (bounds, minimum_height) = cx.update(|cx| {
                let available_height = cx
                    .primary_display()
                    .map_or(900., |display| f32::from(display.visible_bounds().size.height) - 24.);
                (
                    Bounds::centered(
                        None,
                        size(px(860.), px(initial_height(&accounts).min(available_height))),
                        cx,
                    ),
                    px(initial_height(&accounts[..accounts.len().min(6)]).min(available_height)),
                )
            });
            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("ccs".into()),
                        appears_transparent: cfg!(target_os = "macos"),
                        traffic_light_position: cfg!(target_os = "macos")
                            .then(|| point(px(20.), px(16.))),
                    }),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(780.), minimum_height)),
                    ..Default::default()
                },
                |window, cx| {
                    let dashboard = cx.new(|cx| Dashboard::new(window, cx));
                    cx.new(|cx| Root::new(dashboard, window, cx))
                },
            )
            .expect("open CCS window");
            cx.update(|cx| cx.activate(true));
        })
        .detach();
    });
}

#[cfg(test)]
mod ui_tests {
    use super::{Dashboard, Page};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, TestAppContext};

    #[gpui_kit::test]
    fn refresh_updates_accounts_without_discarding_route_edits(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut dashboard = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Dashboard::new(window, cx));
            dashboard = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let dashboard = dashboard.unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("routes", cx);
            window.click("new-route", cx);
            window.click("model-input", cx);
            window.input("custom-model-v2", cx);
            window.click("route-account-agent", cx);
        })
        .unwrap();
        dashboard.update(cx, |view, cx| {
            view.accounts.clear();
            cx.notify();
        });
        cx.update_window(handle.into(), |_, window, cx| window.click("refresh", cx)).unwrap();
        cx.run_until_parked();
        dashboard.update(cx, |view, cx| {
            assert!(!view.accounts.is_empty());
            assert_eq!(view.model.read(cx).value(), "custom-model-v2");
            assert_eq!(view.selected, ["agent"]);
            assert!(view.editor_open);
            assert!(!view.busy);
            assert!(!view.refreshing);
            assert_eq!(view.status, "Updated");
        });
    }

    #[gpui_kit::test]
    fn model_dropdown_selects_and_saves_with_provider_specific_options(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut dashboard = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Dashboard::new(window, cx));
            dashboard = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let dashboard = dashboard.unwrap();
        dashboard.update(cx, |view, _| {
            view.codex_models = vec!["gpt-example".into()];
            view.claude_models = vec!["claude-new-model-v9".into()];
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("routes", cx);
            window.click("new-route", cx);
            window.click("model-options", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        dashboard.update(cx, |view, cx| {
            assert_eq!(view.model.read(cx).value(), "claude-new-model-v9");
            assert!(!view.model_options().contains(&"gpt-example".into()));
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("route-account-agent", cx);
            window.click("provider-codex", cx);
        })
        .unwrap();
        dashboard.update(cx, |view, cx| {
            assert!(view.model.read(cx).value().is_empty());
            assert!(view.selected.is_empty());
            assert_eq!(view.model_options(), ["gpt-example"]);
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("model-options", cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        dashboard.update(cx, |view, cx| assert_eq!(view.model.read(cx).value(), "gpt-example"));
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("provider-claude", cx);
            window.click("model-options", cx);
            window.press("down", cx);
            window.press("enter", cx);
            window.click("route-account-agent", cx);
            window.click("save-route", cx);
        })
        .unwrap();
        cx.run_until_parked();
        dashboard.update(cx, |view, _| {
            assert_eq!(view.routes.rules[0].model, "claude-new-model-v9");
            assert_eq!(view.routes.rules[0].accounts, ["agent"]);
            view.set_models(
                Err(super::backend::Failure {
                    message: "offline".into(),
                    slug: String::new(),
                    spent: false,
                }),
                Ok(vec!["codex-next-model".into()]),
            );
            assert_eq!(view.model_options(), ["claude-new-model-v9"]);
            assert_eq!(view.codex_models, ["codex-next-model"]);
            assert!(view.model_error.contains("offline"));
            view.set_models(Ok(vec![]), Ok(vec![]));
            assert_eq!(view.model_options(), ["claude-new-model-v9"], "saved IDs remain editable");
        });
    }

    #[gpui_kit::test]
    fn six_accounts_fit_without_scroll_at_minimum_width(cx: &mut TestAppContext) {
        use gpui_kit::{px, size};
        cx.update(gpui_kit::init);
        let mut dashboard = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Dashboard::new(window, cx));
            dashboard = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let dashboard = dashboard.unwrap();
        let height = dashboard.update(cx, |view, cx| {
            let template = view.accounts[0].clone();
            view.accounts = (0..6)
                .map(|i| {
                    let mut account = template.clone();
                    account.slug = format!("account-{i}");
                    account.provider = if i < 4 { "claude" } else { "codex" }.into();
                    account.limits = (0..3)
                        .map(|j| super::backend::Limit {
                            column: format!("limit-{j}"),
                            percent: 10.,
                            resets_in: "2h".into(),
                            health: "ok".into(),
                        })
                        .collect();
                    account
                })
                .collect();
            cx.notify();
            super::initial_height(&view.accounts)
        });
        for width in [780., 860.] {
            cx.simulate_window_resize(handle.into(), size(px(width), px(height)));
            cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx)).unwrap();
            dashboard.update(cx, |view, _| assert_eq!(view.scroll.max_offset().y, px(0.)));
        }
    }

    #[gpui_kit::test]
    fn usage_grid_reflows_extra_limits_when_resized(cx: &mut TestAppContext) {
        use gpui_kit::{SharedString, px, size};
        cx.update(gpui_kit::init);
        let mut dashboard = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Dashboard::new(window, cx));
            dashboard = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let dashboard = dashboard.unwrap();
        let slug = dashboard.update(cx, |view, cx| {
            view.accounts.truncate(1);
            let account = &mut view.accounts[0];
            account.limits = (0..7)
                .map(|i| super::backend::Limit {
                    column: if i == 2 {
                        "A future model with a very long scoped limit name".into()
                    } else {
                        format!("limit {i}")
                    },
                    percent: i as f64 * 13.,
                    resets_in: "2h".into(),
                    health: "ok".into(),
                })
                .collect();
            cx.notify();
            account.slug.clone()
        });
        let mut last_y = Vec::new();
        for width in [780., 1100.] {
            cx.simulate_window_resize(handle.into(), size(px(width), px(700.)));
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                if cfg!(target_os = "macos") {
                    assert!(window.find("accounts").bounds().left() >= px(94.));
                }
                assert!(
                    window.find("settings").bounds().right() < window.find("quit").bounds().left()
                );
                let first = window.find(SharedString::from(format!("metric-{slug}-0"))).bounds();
                let second = window.find(SharedString::from(format!("metric-{slug}-1"))).bounds();
                assert_eq!(first.top(), second.top());
                for i in 0..7 {
                    let metric = window.find(SharedString::from(format!("metric-{slug}-{i}")));
                    assert!(metric.bounds().size.width > px(0.));
                    assert!(metric.bounds().right() <= window.viewport_size().width - px(24.));
                }
                last_y.push(
                    window.find(SharedString::from(format!("metric-{slug}-6"))).bounds().top(),
                );
            })
            .unwrap();
        }
        assert!(last_y[0] > last_y[1], "narrow windows must use more rows: {last_y:?}");
    }

    #[gpui_kit::test]
    fn route_editor_saves_account_order_and_closes_after_save(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut dashboard = None;
        let handle = cx.add_window(|window, cx| {
            let view = cx.new(|cx| Dashboard::new(window, cx));
            dashboard = Some(view.clone());
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        let dashboard = dashboard.unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("routes", cx);
            window.click("new-route", cx);
            window.click("model-input", cx);
            window.input("claude-opus-*", cx);
            window.click("route-account-hong", cx);
            window.click("route-account-agent", cx);
            window.click("save-route", cx);
        })
        .unwrap();
        cx.run_until_parked();
        dashboard.update(cx, |view, _| {
            assert!(view.page == Page::Routes);
            assert_eq!(view.routes.rules.len(), 1, "{}", view.error);
            assert_eq!(view.routes.rules[0].model, "claude-opus-*");
            assert_eq!(view.routes.rules[0].accounts, ["hong", "agent"]);
            assert!(!view.editor_open);
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.click(("edit", 0_usize), cx);
            window.click("route-account-hong", cx);
            window.click("save-route", cx);
        })
        .unwrap();
        cx.run_until_parked();
        dashboard.update(cx, |view, _| assert_eq!(view.routes.rules[0].accounts, ["agent"]));
        cx.update_window(handle.into(), |_, window, cx| window.click(("delete", 0_usize), cx))
            .unwrap();
        cx.run_until_parked();
        dashboard.update(cx, |view, _| assert!(view.routes.rules.is_empty()));
    }
}

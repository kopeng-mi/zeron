//! Settings → Providers → Add provider: any agent from the ACP registry, or
//! any local command that speaks ACP, added to the target device's provider
//! list. Built as the New project palette is (search header, Cmd+K rows, a
//! key-hint footer with the primary action on its right edge), with a second
//! step for a custom command.
//!
//! The engine does the work (`ListAcpRegistry` / `AddAcpAgent`, forwarded to
//! the target device like every other provider RPC): availability depends
//! on the device (its platform, whether npm/uv exist) and adding installs the
//! agent there before it joins the list.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable as _, IntoElement, ScrollHandle,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use zeron_engine::registry::{AcpRegistryListing, HarnessDescriptor};
use zeron_rpc::methods;

use super::HarnessesPage;
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover::{self, Loadable};
use crate::shell::command_palette;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Registry,
    Custom,
}

pub(super) struct AddProvider {
    pub(super) step: Step,
    search: Entity<ComposerInput>,
    name: Entity<ComposerInput>,
    command: Entity<ComposerInput>,
    env: Entity<ComposerInput>,
    focus: FocusHandle,
    /// Focus the step's first field on the next render.
    focus_pending: bool,
    registry: Loadable<Vec<AcpRegistryListing>>,
    active: usize,
    scroll: ScrollHandle,
    /// The registry id being added, or `""` for the custom command.
    adding: Option<String>,
    error: Option<String>,
    load_task: Option<Task<()>>,
    add_task: Option<Task<()>>,
    _events: Vec<Subscription>,
}

/// A registry row's state on the target device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RowState {
    Addable,
    Added,
    BuiltIn,
    Unavailable,
}

fn row_state(listing: &AcpRegistryListing) -> RowState {
    if listing.built_in.is_some() {
        RowState::BuiltIn
    } else if listing.added {
        RowState::Added
    } else if listing.unavailable.is_some() {
        RowState::Unavailable
    } else {
        RowState::Addable
    }
}

/// Rows matching `query` (name, id, then description), best match first;
/// addable agents ahead of the ones already added, built in or unavailable.
fn filtered<'a>(listings: &'a [AcpRegistryListing], query: &str) -> Vec<&'a AcpRegistryListing> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<(u8, RowState, String, &AcpRegistryListing)> = listings
        .iter()
        .filter_map(|listing| {
            let name = listing.name.to_lowercase();
            let rank = if query.is_empty() {
                0
            } else if name.starts_with(&query) || listing.id.starts_with(&query) {
                0
            } else if name.contains(&query) || listing.id.contains(&query) {
                1
            } else if listing.description.to_lowercase().contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, row_state(listing), name, listing))
        })
        .collect();
    rows.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    rows.into_iter().map(|(.., listing)| listing).collect()
}

impl HarnessesPage {
    pub(super) fn open_add_provider(&mut self, cx: &mut Context<Self>) {
        // Form fields edit like the app's dialog fields (caret keys stay in
        // the field, Enter submits); only the search uses the palette's
        // navigation-bubbling context.
        let field = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                ComposerInput::new(placeholder, cx)
                    .with_single_line()
                    .with_tab_stop()
            })
        };
        let search = cx.new(|cx| {
            ComposerInput::with_context("Search the ACP registry", "PaletteSearch", cx)
                .with_accessibility_role(gpui::Role::SearchInput)
                .with_single_line()
        });
        let name = field("Name (optional)", cx);
        let command = field("my-agent --acp", cx);
        let env = field("API_KEY=… (optional)", cx);
        let mut events = vec![cx.subscribe(&search, |page: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited)
                && let Some(flow) = page.add_provider.as_mut()
            {
                flow.active = 0;
                flow.scroll.scroll_to_item(0);
                cx.notify();
            }
        })];
        for input in [&name, &command, &env] {
            events.push(cx.subscribe(input, |page: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    page.submit_custom_agent(cx);
                }
            }));
        }
        self.add_provider = Some(AddProvider {
            step: Step::Registry,
            search,
            name,
            command,
            env,
            focus: cx.focus_handle(),
            focus_pending: true,
            registry: Loadable::Loading,
            active: 0,
            scroll: ScrollHandle::new(),
            adding: None,
            error: None,
            load_task: None,
            add_task: None,
            _events: events,
        });
        self.load_registry(false, cx);
        cx.notify();
    }

    pub(super) fn close_add_provider(&mut self, cx: &mut Context<Self>) {
        // An install in flight keeps running on the device; its reply still
        // lands in the list (see `add_agent`).
        if self.add_provider.take().is_some() {
            cx.notify();
        }
    }

    fn load_registry(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(serde_json::json!({ "force": force }));
        let target = self.target_device.clone();
        let Some(flow) = self.add_provider.as_mut() else {
            return;
        };
        flow.registry = Loadable::Loading;
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ACP_REGISTRY, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<AcpRegistryListing>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                if let Some(flow) = page.add_provider.as_mut() {
                    flow.registry = match result {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error),
                    };
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn add_registry_agent(&mut self, registry_id: String, cx: &mut Context<Self>) {
        self.add_agent(
            registry_id.clone(),
            serde_json::json!({ "registryId": registry_id }),
            cx,
        );
    }

    fn submit_custom_agent(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.add_provider.as_mut() else {
            return;
        };
        let name = flow.name.read(cx).text().trim().to_string();
        let command = flow.command.read(cx).text().to_string();
        let env = flow.env.read(cx).text().to_string();
        let parsed = zeron_harness::acp_agents::split_command_line(&command).and_then(|words| {
            let mut words = words.into_iter();
            let program = words
                .next()
                .ok_or_else(|| "Enter the command that starts the agent".to_string())?;
            let env = zeron_harness::acp_agents::parse_env_assignments(&env)?;
            Ok((program, words.collect::<Vec<_>>(), env))
        });
        let (program, args, env) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                flow.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.add_agent(
            String::new(),
            serde_json::json!({ "name": name, "program": program, "args": args, "env": env }),
            cx,
        );
    }

    /// `AddAcpAgent` on the target device. The reply is the device's fresh
    /// catalog, so the new provider shows up (enabled) in one round trip.
    fn add_agent(&mut self, key: String, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.with_target(params);
        let target = self.target_device.clone();
        let Some(flow) = self.add_provider.as_mut() else {
            return;
        };
        if flow.adding.is_some() {
            return;
        }
        flow.adding = Some(key);
        flow.error = None;
        flow.add_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ADD_ACP_AGENT, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<Vec<HarnessDescriptor>>(value)
                        .map_err(|error| error.to_string())
                });
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                match result {
                    Ok(list) => {
                        page.set_catalog(list, cx);
                        crate::pickers::bump_harness_catalog(cx);
                        page.add_provider = None;
                    }
                    Err(error) => {
                        if let Some(flow) = page.add_provider.as_mut() {
                            flow.adding = None;
                            flow.error = Some(error);
                        } else {
                            page.error = Some(format!("Couldn't add the provider — {error}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn add_provider_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(flow) = self.add_provider.as_mut() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if flow.step == Step::Custom {
                self.add_provider_step(Step::Registry, cx);
            } else {
                self.close_add_provider(cx);
            }
            cx.stop_propagation();
            return;
        }
        if flow.step != Step::Registry {
            return;
        }
        let Some(listings) = flow.registry.ready() else {
            return;
        };
        let query = flow.search.read(cx).text().to_string();
        let rows = filtered(listings, &query);
        match key {
            "down" | "up" if !rows.is_empty() => {
                let last = rows.len() - 1;
                flow.active = if key == "down" {
                    (flow.active + 1).min(last)
                } else {
                    flow.active.saturating_sub(1)
                };
                flow.scroll.scroll_to_item(flow.active);
                cx.stop_propagation();
                cx.notify();
            }
            "enter" => {
                let pick = rows
                    .get(flow.active)
                    .filter(|row| row_state(row) == RowState::Addable)
                    .map(|row| row.id.clone());
                if let Some(id) = pick {
                    self.add_registry_agent(id, cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn add_provider_step(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(flow) = self.add_provider.as_mut()
            && flow.adding.is_none()
        {
            flow.step = step;
            flow.error = None;
            flow.focus_pending = true;
            cx.notify();
        }
    }

    pub(super) fn render_add_provider(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let viewport = window.viewport_size();
        let theme = Theme::of(cx).for_popup();
        let flow = self.add_provider.as_mut()?;
        if std::mem::take(&mut flow.focus_pending) {
            let field = match flow.step {
                Step::Registry => &flow.search,
                Step::Custom => &flow.command,
            };
            window.focus(&field.focus_handle(cx), cx);
        }
        let step = flow.step;
        let focus = flow.focus.clone();
        let error = flow.error.clone();
        let busy = flow.adding.is_some();
        let body = match step {
            Step::Registry => self.render_registry_step(viewport, &theme, cx),
            Step::Custom => self.render_custom_step(&theme),
        };
        let footer = self.render_add_provider_footer(step, busy, &theme, cx);
        let header = match step {
            Step::Registry => {
                let search = self.add_provider.as_ref()?.search.clone();
                command_palette::palette_header(&theme, search.into_any_element(), div())
                    .into_any_element()
            }
            Step::Custom => custom_header(&theme, cx).into_any_element(),
        };
        let card = command_palette::palette_card("add-provider-palette", &focus, viewport, &theme)
            .role(gpui::Role::Dialog)
            .aria_label("Add provider")
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                page.add_provider_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_add_provider(cx)))
            .child(header)
            .child(body)
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .px(px(16.0))
                        .pb(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(footer);
        Some(command_palette::palette_overlay(viewport, card))
    }

    fn render_registry_step(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(flow) = self.add_provider.as_ref() else {
            return div().into_any_element();
        };
        let query = flow.search.read(cx).text().to_string();
        let scroll = flow.scroll.clone();
        let adding = flow.adding.clone();
        let registry = match &flow.registry {
            Loadable::Ready(list) => Ok(list.clone()),
            Loadable::Error(error) => Err(Some(error.clone())),
            Loadable::Idle | Loadable::Loading => Err(None),
        };
        let mut results = div()
            .id("add-provider-results")
            .min_h_0()
            .max_h(px(command_palette::palette_results_height(viewport)))
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .flex()
            .flex_col()
            .gap(px(crate::shell::SIDEBAR_LIST_GAP));
        match registry {
            Err(None) => {
                results = results.child(div().p(px(8.0)).child(popover::skeleton_rows(
                    "add-provider-loading",
                    theme,
                    6,
                    cx.entity_id(),
                    cx,
                )));
            }
            Err(Some(message)) => {
                results = results.child(
                    popover::error_row(theme, &message).p(px(16.0)).child(
                        popover::btn_ghost(theme, "Retry", "add-provider-retry")
                            .id("add-provider-retry")
                            .on_click(cx.listener(|page, _, _, cx| page.load_registry(true, cx))),
                    ),
                );
            }
            Ok(listings) => {
                let rows = filtered(&listings, &query);
                let active = flow.active.min(rows.len().saturating_sub(1));
                let count = rows.len();
                if count == 0 {
                    results = results.child(command_palette::palette_empty(
                        theme,
                        "No agents found",
                        "Try another name, or add a custom command.",
                    ));
                }
                for (ix, listing) in rows.into_iter().enumerate() {
                    let row =
                        self.registry_row(ix, ix == active, listing, &query, &adding, theme, cx);
                    // End spacing scrolls with the content (Cmd+K).
                    results = results.child(
                        div()
                            .flex_none()
                            .px(px(8.0))
                            .when(ix == 0, |row| row.pt(px(8.0)))
                            .when(ix + 1 == count, |row| row.pb(px(8.0)))
                            .child(row),
                    );
                }
            }
        }
        command_palette::palette_results_fade(results, &scroll).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn registry_row(
        &self,
        ix: usize,
        active: bool,
        listing: &AcpRegistryListing,
        query: &str,
        adding: &Option<String>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = row_state(listing);
        let installing = adding.as_deref() == Some(listing.id.as_str());
        let mark = crate::acp_icons::mark_for_url(listing.icon.as_deref(), cx);
        let trailing: AnyElement = if installing {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_color(theme.text_muted)
                .child(crate::loaders::mini_mono_spinner(
                    format!("add-provider-spinner-{ix}"),
                    1.5,
                    theme.text_muted,
                    cx.entity_id(),
                    cx,
                ))
                .child("Installing…")
                .into_any_element()
        } else {
            let label: SharedString = match state {
                RowState::Addable => format!("v{}", listing.version).into(),
                RowState::Added => "Added".into(),
                RowState::BuiltIn => "Built in".into(),
                RowState::Unavailable => listing.unavailable.clone().unwrap_or_default().into(),
            };
            div()
                .text_color(theme.text_muted.opacity(0.75))
                .child(label)
                .into_any_element()
        };
        let dimmed = state != RowState::Addable && !installing;
        let id = listing.id.clone();
        let name = listing.name.clone();
        popover::menu_row(theme, active, format!("add-provider-row-{ix}"))
            .id(("add-provider-row", ix))
            .rounded(px(popover::PALETTE_ITEM_RADIUS))
            .min_h(px(30.0))
            .py(px(4.0))
            .role(gpui::Role::Button)
            .aria_label(format!("Add {name}"))
            .on_mouse_move(cx.listener(move |page, _: &gpui::MouseMoveEvent, _, cx| {
                if let Some(flow) = page.add_provider.as_mut()
                    && flow.active != ix
                {
                    flow.active = ix;
                    cx.notify();
                }
            }))
            .when(state == RowState::Addable && adding.is_none(), |row| {
                row.on_click(
                    cx.listener(move |page, _, _, cx| page.add_registry_agent(id.clone(), cx)),
                )
            })
            .when(state != RowState::Addable, |row| row.cursor_default())
            .child(
                icon(mark)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(200.0))
                    .truncate()
                    .when(dimmed, |el| el.text_color(theme.text_muted))
                    .child(popover::search_highlight(
                        listing.name.clone().into(),
                        Some(query),
                        theme,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.75))
                    .child(listing.description.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(crate::typography::ui_rems(11.0))
                    .child(trailing),
            )
            .into_any_element()
    }

    fn render_custom_step(&self, theme: &Theme) -> AnyElement {
        let Some(flow) = self.add_provider.as_ref() else {
            return div().into_any_element();
        };
        let field = |label: &'static str, hint: &'static str, input: &Entity<ComposerInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(8.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .child(div().text_color(theme.text).child(label))
                        .child(div().text_color(theme.text_muted).child(hint)),
                )
                .child(popover::dialog_field(input.clone().into_any_element()))
        };
        div()
            .px(px(16.0))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(field(
                "Command",
                "Starts the agent speaking ACP over stdio",
                &flow.command,
            ))
            .child(field("Name", "Shown in the provider list", &flow.name))
            .child(field("Environment", "KEY=value pairs", &flow.env))
            .into_any_element()
    }

    fn render_add_provider_footer(
        &mut self,
        step: Step,
        busy: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let (label, busy_label) = match step {
            Step::Registry => ("Custom command", "Installing…"),
            Step::Custom => ("Add agent", "Adding…"),
        };
        let action = div()
            .id("add-provider-action")
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(3.0))
            .my(px(-3.0))
            .mr(px(-8.0))
            .rounded(px(8.0))
            .role(gpui::Role::Button)
            .aria_label(label)
            .when(!busy, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(crate::theme::card_selected_bg()))
                    .active(|s| s.opacity(0.8))
                    .on_click(cx.listener(move |page, _, _, cx| match step {
                        Step::Registry => page.add_provider_step(Step::Custom, cx),
                        Step::Custom => page.submit_custom_agent(cx),
                    }))
            })
            .when(busy, |el| el.opacity(0.5))
            .when(step == Step::Custom, |el| {
                el.child(popover::kbd_hint(theme, "↵"))
            })
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(if busy && step == Step::Custom {
                        busy_label
                    } else {
                        label
                    }),
            );
        command_palette::palette_footer()
            .when(step == Step::Registry, |el| {
                el.child(command_palette::command_key_hint(theme, "↑ ↓", "Navigate"))
                    .child(command_palette::command_key_hint(theme, "↵", "Add"))
                    .child(command_palette::command_key_hint(theme, "Esc", "Close"))
            })
            .when(step == Step::Custom, |el| {
                el.child(command_palette::command_key_hint(theme, "Esc", "Back"))
            })
            .child(div().flex_1())
            .child(action)
    }
}

/// The custom step's top bar: back to the registry, and the step's title
/// (the New project palette's crumb bar, one crumb deep).
fn custom_header(theme: &Theme, cx: &mut Context<HarnessesPage>) -> gpui::Div {
    div()
        .h(px(44.0))
        .flex_none()
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .border_b_1()
        .border_color(crate::theme::hairline(0.06))
        .text_size(crate::typography::ui_rems(13.0))
        .child(
            div()
                .id("add-provider-back")
                .group("add-provider-back")
                .size(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label("Back")
                .tooltip(crate::settings::widgets::text_tooltip("Back"))
                .hover(|s| s.bg(theme.element_hover))
                .on_click(cx.listener(|page, _, _, cx| page.add_provider_step(Step::Registry, cx)))
                .child(
                    icon(icons::ARROW_LEFT)
                        .size(px(16.0))
                        .text_color(theme.text_muted)
                        .group_hover("add-provider-back", |s| s.text_color(theme.text)),
                ),
        )
        .child(div().text_color(theme.text).child("Custom ACP command"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(id: &str, name: &str, description: &str) -> AcpRegistryListing {
        AcpRegistryListing {
            id: id.into(),
            name: name.into(),
            version: "1.0.0".into(),
            description: description.into(),
            icon: None,
            website: None,
            added: false,
            built_in: None,
            unavailable: None,
        }
    }

    #[test]
    fn search_ranks_name_matches_then_addable_agents_first() {
        let mut codex = listing("codex-acp", "Codex", "OpenAI's coding agent");
        codex.built_in = Some(zeron_proto::HarnessId::Codex);
        let mut goose = listing("goose", "goose", "An open source agent");
        goose.added = true;
        let mut kimi = listing("kimi", "Kimi CLI", "Moonshot's agent");
        kimi.unavailable = Some("No build for this device".into());
        let all = vec![
            codex,
            goose,
            kimi,
            listing("gemini", "Gemini CLI", "Google's official CLI"),
            listing("qwen-code", "Qwen Code", "Agent for Qwen models"),
        ];
        let ids = |query: &str| -> Vec<String> {
            filtered(&all, query).iter().map(|l| l.id.clone()).collect()
        };
        assert_eq!(
            ids(""),
            ["gemini", "qwen-code", "goose", "codex-acp", "kimi"]
        );
        // Name/id prefix, then substring, then description.
        assert_eq!(
            ids("g"),
            ["gemini", "goose", "qwen-code", "codex-acp", "kimi"]
        );
        assert_eq!(ids("cli"), ["gemini", "kimi"]);
        assert_eq!(ids("moonshot"), ["kimi"]);
        assert!(ids("nothing-matches").is_empty());
    }
}

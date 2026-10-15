//! The composer footer's ring cluster. The plan-usage ring shows how much of
//! the active account's rate limit the session's harness has used, beside
//! the context ring. Clicking it opens the harness's accounts — each with its
//! usage meters — and clicking one switches to it, the same `ActivateAgentAccount` Settings →
//! Accounts runs. Both views share [`AccountsSnapshotCache`], so a switch in
//! either shows up in the other.
use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use zeron_proto::{AgentAccount, AgentAccountsSnapshot, HarnessId};
use zeron_rpc::methods;

use crate::popover;
use crate::settings::accounts::{
    self, AccountsSnapshotCache, UsageLevel, render_usage_meter, reports_usage, signs_in,
    usage_color, usage_level,
};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// Forced probes hit the provider; the engine throttles them too, but the
/// ring re-probes on every hover, so don't even ask more often than this.
const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// Background re-probe while a composer is alive: usage moves as turns run.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often the status line repaints its cache-warm countdown.
const STATUS_TICK: Duration = Duration::from_secs(30);

/// Overlay the plan windows Claude reported on its last turn onto the
/// account's polled ones (`Session` = five-hour, `Week` = seven-day). Turns
/// report them live, so the ring stops lagging the 5-minute poll. A window
/// whose reset has passed is dropped as stale. Pure.
pub fn with_live_windows(
    mut account: AgentAccount,
    live: Option<zeron_proto::ContextUsage>,
    now: chrono::DateTime<chrono::Utc>,
) -> AgentAccount {
    let Some(live) = live else {
        return account;
    };
    for (label, window) in [("Session", live.five_hour), ("Week", live.seven_day)] {
        let Some(window) = window else { continue };
        let resets_at = chrono::DateTime::from_timestamp(window.resets_at, 0);
        if resets_at.is_some_and(|at| at <= now) {
            continue;
        }
        let fresh = zeron_proto::AgentUsageWindow {
            label: label.to_owned(),
            used_fraction: f32::from(window.used_permille) / 1000.0,
            resets_at,
        };
        match account.usage_windows.iter_mut().find(|w| w.label == label) {
            Some(existing) => *existing = fresh,
            None => account.usage_windows.push(fresh),
        }
    }
    account
}

/// The status line's text: prompt-cache hit rate and how long the cache
/// stays warm, e.g. `cache 91% · warm 58m`. `None` before a turn reports.
/// `142 MB`, `1.3 GB`: an agent's resident memory, short. Pure.
pub fn memory_label(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{} MB", mb.round() as u64)
    }
}

pub fn cache_status(
    usage: Option<zeron_proto::ContextUsage>,
    memory_bytes: Option<u64>,
    now_ms: i64,
) -> Option<String> {
    let memory = memory_bytes.map(memory_label);
    let cost = usage.and_then(|u| u.cost_micros).map(cost_label);
    let text = cache_text(usage, memory, now_ms);
    match (text, cost) {
        (Some(text), Some(cost)) => Some(format!("{text} · {cost}")),
        (text, cost) => text.or(cost),
    }
}

/// `$0.41`, or `<$0.01` for a session that has barely started.
fn cost_label(micros: u64) -> String {
    if micros < 10_000 {
        "<$0.01".into()
    } else {
        format!("${:.2}", micros as f64 / 1_000_000.0)
    }
}

fn cache_text(
    usage: Option<zeron_proto::ContextUsage>,
    memory: Option<String>,
    now_ms: i64,
) -> Option<String> {
    let Some(cache) = usage
        .and_then(|u| u.cache)
        .filter(|c| c.hit_fraction().is_some())
    else {
        return memory;
    };
    let hit = cache.hit_fraction()?;
    let mut text = match memory {
        Some(memory) => format!("{memory} · cache {}%", (hit * 100.0).round() as u32),
        None => format!("cache {}%", (hit * 100.0).round() as u32),
    };
    if let Some(left) = cache.warm_ms_left(now_ms) {
        let mins = left / 60_000;
        text.push_str(&if left == 0 {
            " · cold".to_owned()
        } else if mins >= 1 {
            format!(" · warm {mins}m")
        } else {
            " · warm <1m".to_owned()
        });
    }
    Some(text)
}

/// The binding limit: the most-used window of the account. Pure.
pub fn used_fraction(account: &AgentAccount) -> Option<f32> {
    account
        .usage_windows
        .iter()
        .map(|window| window.used_fraction.clamp(0.0, 1.0))
        .reduce(f32::max)
}

/// The harness's live account, when it reports usage. Pure.
pub fn active_account(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Option<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .find(|account| account.harness == harness && account.active)
}

/// Loads (and switches) the accounts of the composer's target device.
pub struct AccountUsage {
    state: Entity<AppState>,
    /// `None` = this device, as in [`AccountsSnapshotCache`].
    target: Option<String>,
    harness: Option<HarnessId>,
    /// Once any list has been asked for the current target.
    loaded: bool,
    last_forced: Option<Instant>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    popup: popover::Popup<FooterCard>,
    _poll: Task<()>,
    _cache: Subscription,
    _state: Subscription,
}

impl AccountUsage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // The warm countdown moves with the clock, not with events.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STATUS_TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update(cx, |usage, cx| {
                        if usage.harness.is_some() {
                            usage.load(true, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            _state: cx.observe(&state, |_, _, cx| cx.notify()),
            state,
            target: None,
            harness: None,
            loaded: false,
            last_forced: None,
            error: None,
            load_task: None,
            action_task: None,
            popup: popover::Popup::default(),
            _poll: poll,
            // Settings → Accounts writes the same cache.
            _cache: cx.observe_global::<AccountsSnapshotCache>(|_, cx| cx.notify()),
        }
    }

    /// Point at the session's harness and device; loads on first sight of a
    /// device. Cheap enough to call every render.
    pub fn track(
        &mut self,
        harness: Option<HarnessId>,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let harness = harness.filter(|h| signs_in(*h) && reports_usage(*h));
        self.harness = harness;
        if self.target != target {
            self.target = target;
            self.loaded = false;
            self.last_forced = None;
            self.error = None;
        }
        if harness.is_some() && !self.loaded {
            self.loaded = true;
            self.load(true, cx);
        }
    }

    fn snapshot<'a>(&self, cx: &'a gpui::App) -> Option<&'a AgentAccountsSnapshot> {
        cx.try_global::<AccountsSnapshotCache>()?
            .0
            .get(&self.target)
    }

    fn params(&self, mut value: serde_json::Value) -> serde_json::Value {
        if let (Some(target), Some(object)) = (&self.target, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// Plain list first when nothing is cached (the engine's persisted usage
    /// paints at once), then the forced probe replaces it.
    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        if force_usage {
            if self
                .last_forced
                .is_some_and(|at| at.elapsed() < FORCE_MIN_INTERVAL)
            {
                return;
            }
            self.last_forced = Some(Instant::now());
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let paint_first = force_usage && self.snapshot(cx).is_none();
        let plain = self.params(serde_json::json!({ "forceUsage": false }));
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let fetch = |params: serde_json::Value| {
                let engine = engine.clone();
                async move {
                    engine
                        .client()
                        .call(methods::LIST_AGENT_ACCOUNTS, params)
                        .await
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<AgentAccountsSnapshot>(value).ok()
                        })
                }
            };
            let store = |snapshot: AgentAccountsSnapshot, cx: &mut gpui::AsyncApp| {
                let key = key.clone();
                this.update(cx, |_, cx| {
                    cx.default_global::<AccountsSnapshotCache>()
                        .0
                        .insert(key, snapshot);
                })
                .ok();
            };
            if paint_first && let Some(snapshot) = fetch(plain).await {
                store(snapshot, cx);
            }
            if let Some(snapshot) = fetch(params).await {
                store(snapshot, cx);
            }
        }));
    }

    /// Switch optimistically, like Settings → Accounts: the rows flip at once
    /// and the engine's reply replaces them; a refusal restores the list.
    fn switch(&mut self, account: &AgentAccount, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let key = self.target.clone();
        let cache = &mut cx.default_global::<AccountsSnapshotCache>().0;
        let previous = cache.get(&key).cloned();
        if let Some(snapshot) = cache.get_mut(&key) {
            // Per-provider agents keep one live login per provider: only the
            // switched-to row's group changes.
            accounts::mark_switched(snapshot, account);
        }
        self.error = None;
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ACTIVATE_AGENT_ACCOUNT, params)
                .await;
            this.update(cx, |usage, cx| {
                match result.map(serde_json::from_value::<AgentAccountsSnapshot>) {
                    Ok(Ok(snapshot)) => {
                        cx.default_global::<AccountsSnapshotCache>()
                            .0
                            .insert(key, snapshot);
                    }
                    Ok(Err(_)) => usage.load(false, cx),
                    Err(err) => {
                        if let Some(previous) = previous {
                            cx.default_global::<AccountsSnapshotCache>()
                                .0
                                .insert(key, previous);
                        }
                        usage.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The ring's reading: the live account's most-used window, when the
    /// harness has a live account with usage to show.
    fn fraction(&self, cx: &gpui::App) -> Option<f32> {
        let account = active_account(self.snapshot(cx)?, self.harness?)?.clone();
        used_fraction(&self.live(account, cx))
    }

    /// `account` with Claude's per-turn windows laid over the polled ones.
    fn live(&self, account: AgentAccount, cx: &gpui::App) -> AgentAccount {
        if !account.active || account.harness != HarnessId::ClaudeCode {
            return account;
        }
        with_live_windows(
            account,
            self.state.read(cx).context_usage,
            chrono::Utc::now(),
        )
    }

    /// A trigger click: open this ring's popover, or close it when the
    /// press found it open (the card's mouse-down-out already began that
    /// close — see `Popup::note_trigger_press`).
    fn toggle(&mut self, card: FooterCard, cx: &mut Context<Self>) {
        if self.popup.take_press_was_open() || self.popup.as_open() == Some(&card) {
            self.dismiss(cx);
            return;
        }
        if card == FooterCard::Accounts {
            // Opening the card is the moment someone cares: re-probe.
            self.load(true, cx);
        }
        self.popup.open(card);
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.popup.begin_close() {
            popover::reap_popup(cx, |usage: &mut Self| &mut usage.popup);
        }
        cx.notify();
    }

    /// A footer ring that opens `card` above itself, right-aligned like the
    /// model picker.
    fn trigger(
        &self,
        chip: gpui::Stateful<gpui::Div>,
        card: FooterCard,
        content: impl FnOnce(&Self, &mut Context<Self>) -> gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let chip = chip
            .relative()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |usage, _, _, _| {
                    usage
                        .popup
                        .note_trigger_press_matching(|open| *open == card);
                }),
            )
            .on_click(cx.listener(move |usage, _, _, cx| usage.toggle(card, cx)));
        if self.popup.get() != Some(&card) {
            return chip.into_any_element();
        }
        let content = content(self, cx)
            .on_mouse_down_out(cx.listener(|usage, _, _, cx| usage.dismiss(cx)))
            .into_any_element();
        chip.child(popover::anchored_menu_above_end(
            card.id(),
            content,
            self.popup.closing_since(),
        ))
        .into_any_element()
    }

    fn accounts_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = &Theme::of(cx).for_popup();
        let harness = self.harness;
        let now = chrono::Utc::now();
        let rows: Vec<AgentAccount> = match (harness, self.snapshot(cx)) {
            (Some(harness), Some(snapshot)) => accounts::provider_accounts(snapshot, harness)
                .into_iter()
                .cloned()
                .map(|account| self.live(account, cx))
                .collect(),
            _ => Vec::new(),
        };
        let title = format!(
            "{} accounts",
            harness.map_or("Agent", accounts::provider_name)
        );
        popover::popover_card(theme)
            .w(px(400.0))
            .flex()
            .flex_col()
            .child(popover::menu_heading(theme, &title))
            .children(rows.iter().enumerate().map(|(ix, account)| {
                let email: SharedString = account
                    .email
                    .clone()
                    .or_else(|| account.display_name.clone())
                    .unwrap_or_else(|| "Unknown account".into())
                    .into();
                let can_switch = !account.active && account.switchable;
                let mut meta: Vec<gpui::AnyElement> = account
                    .plan_label
                    .iter()
                    .map(|plan| {
                        div()
                            .child(SharedString::from(plan.clone()))
                            .into_any_element()
                    })
                    .collect();
                if account.active {
                    meta.push(
                        div()
                            .text_color(theme.accent)
                            .child(SharedString::from("In use"))
                            .into_any_element(),
                    );
                } else if let Some(reason) = account
                    .usage_error
                    .clone()
                    .filter(|_| account.usage_windows.is_empty())
                {
                    meta.push(div().child(SharedString::from(reason)).into_any_element());
                }
                let switch_to = account.clone();
                popover::menu_row(theme, account.active, format!("account-usage-row-{ix}"))
                    .id(("account-usage-row", ix))
                    .when(!can_switch, |row| row.cursor_default())
                    .when(can_switch, |row| {
                        row.on_click(cx.listener(move |usage, _, _, cx| {
                            usage.switch(&switch_to, cx);
                        }))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(email),
                            )
                            .when(!meta.is_empty(), |el| {
                                el.child(crate::settings::widgets::meta_line(theme, meta))
                            }),
                    )
                    .child(
                        div()
                            .id(("account-usage-meters", ix))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .when(!account.usage_windows.is_empty(), |meters| {
                                meters.tooltip(widgets::text_tooltip(accounts::usage_resets(
                                    &account.usage_windows,
                                    now,
                                )))
                            })
                            .children(
                                account
                                    .usage_windows
                                    .iter()
                                    .take(2)
                                    .map(|window| render_usage_meter(window, theme)),
                            ),
                    )
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger)
                    .child(error)
            }))
    }
}

/// Which footer ring's popover is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCard {
    Accounts,
    Context,
}

impl FooterCard {
    fn id(self) -> &'static str {
        match self {
            FooterCard::Accounts => "account-usage-menu",
            FooterCard::Context => "context-usage-menu",
        }
    }
}

/// The footer's ring cluster: account usage (accent arc, so the two read
/// apart), then context occupancy. Each opens its popover on click.
impl Render for AccountUsage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let context = self.state.read(cx).context_usage;
        let account = self.fraction(cx).map(|fraction| {
            let level = usage_level(fraction);
            let chip = crate::context_usage::ring_chip(
                "account-usage",
                fraction,
                usage_color(level, &theme),
                match level {
                    UsageLevel::Normal => theme.text_muted,
                    _ => usage_color(level, &theme),
                },
                format!("{}%", (fraction * 100.0).round() as u32),
                self.popup.get() == Some(&FooterCard::Accounts),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Accounts,
                |usage, cx| usage.accounts_card(cx),
                cx,
            )
        });
        let context = crate::context_usage::has_window(context).then(|| {
            let chip = crate::context_usage::chip(
                context,
                self.popup.get() == Some(&FooterCard::Context),
                &theme,
            );
            self.trigger(
                chip,
                FooterCard::Context,
                move |_, cx| crate::context_usage::card(context, &Theme::of(cx).for_popup()),
                cx,
            )
        });
        let status = {
            let state = self.state.read(cx);
            let memory = state
                .selected_chat
                .as_deref()
                .and_then(|chat| state.agent_memory.get(chat).copied());
            cache_status(state.context_usage, memory, chrono::Utc::now().timestamp_millis())
        }
        .map(|text| {
            div()
                .flex_none()
                .pr(px(4.0))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(text))
        });
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(status)
            .children(account)
            .children(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentUsageWindow;

    fn account(harness: HarnessId, active: bool, used: &[f32]) -> AgentAccount {
        serde_json::from_value(serde_json::json!({
            "id": format!("{harness:?}-{active}"),
            "harness": harness,
            "email": null,
            "planLabel": null,
            "active": active,
            "switchable": true,
            "usageWindows": used
                .iter()
                .map(|fraction| AgentUsageWindow {
                    label: "5h".into(),
                    used_fraction: *fraction,
                    resets_at: None,
                })
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn live_windows_replace_the_polled_ones_until_they_reset() {
        let now = chrono::Utc::now();
        let mut polled = account(HarnessId::ClaudeCode, true, &[0.1, 0.2]);
        polled.usage_windows[0].label = "Session".into();
        polled.usage_windows[1].label = "Week".into();
        let live = zeron_proto::ContextUsage {
            five_hour: Some(zeron_proto::RateWindow {
                used_permille: 410,
                resets_at: now.timestamp() + 3600,
            }),
            seven_day: Some(zeron_proto::RateWindow {
                used_permille: 900,
                resets_at: now.timestamp() - 1,
            }),
            ..Default::default()
        };
        let merged = with_live_windows(polled, Some(live), now);
        assert_eq!(merged.usage_windows[0].used_fraction, 0.41);
        // A window past its reset keeps the polled value.
        assert_eq!(merged.usage_windows[1].used_fraction, 0.2);
    }

    #[test]
    fn cache_status_reads_hits_and_warmth() {
        let usage = |ttl: Option<u32>| zeron_proto::ContextUsage {
            cache: Some(zeron_proto::CacheStats {
                read: 90,
                write: 5,
                input: 5,
                ttl_secs: ttl,
                at_ms: 0,
            }),
            ..Default::default()
        };
        assert_eq!(cache_status(Some(usage(Some(3600))), None, 120_000).unwrap(), "cache 90% · warm 58m");
        assert_eq!(cache_status(Some(usage(Some(300))), None, 400_000).unwrap(), "cache 90% · cold");
        assert_eq!(cache_status(Some(usage(None)), None, 0).unwrap(), "cache 90%");
        assert_eq!(cache_status(None, None, 0), None);
        assert_eq!(
            cache_status(Some(usage(Some(3600))), Some(142 * 1024 * 1024), 120_000).unwrap(),
            "142 MB · cache 90% · warm 58m"
        );
        assert_eq!(cache_status(None, Some(3 * 1024 * 1024 * 1024), 0).unwrap(), "3.0 GB");
        let costly = zeron_proto::ContextUsage {
            cost_micros: Some(412_345),
            ..usage(Some(3600))
        };
        assert_eq!(
            cache_status(Some(costly), Some(142 * 1024 * 1024), 120_000).unwrap(),
            "142 MB · cache 90% · warm 58m · $0.41"
        );
        assert_eq!(
            cache_status(Some(zeron_proto::ContextUsage { cost_micros: Some(5), ..Default::default() }), None, 0).unwrap(),
            "<$0.01"
        );
    }

    #[test]
    fn ring_shows_the_most_used_window() {
        assert_eq!(used_fraction(&account(HarnessId::Codex, true, &[])), None);
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[0.12, 0.64])),
            Some(0.64)
        );
        assert_eq!(
            used_fraction(&account(HarnessId::Codex, true, &[1.4])),
            Some(1.0)
        );
    }

    #[test]
    fn active_account_is_scoped_to_the_harness() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account(HarnessId::ClaudeCode, true, &[0.3]),
                account(HarnessId::Codex, false, &[0.1]),
                account(HarnessId::Codex, true, &[0.2]),
            ],
            warnings: Vec::new(),
        };
        assert_eq!(
            active_account(&snapshot, HarnessId::Codex).map(|a| a.id.as_str()),
            Some("Codex-true")
        );
        assert!(active_account(&snapshot, HarnessId::Cursor).is_none());
    }
}

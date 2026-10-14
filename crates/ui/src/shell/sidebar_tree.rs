//! A sidebar chat's children, drawn as a tree hanging off its row: the chats
//! it spawned (side chats and Zeron MCP `create_chat` children, which carry
//! `parent_chat_id`) and, for the open chat only, its harness subagents.
//!
//! Subagents live in a chat's transcript, and only the open chat's
//! transcript is loaded; other rows say how many are running instead of
//! loading every transcript to list them.
//!
//! The tree starts collapsed and opens while anything in it runs. A click
//! flips that default, remembered per parent. Settled subagents fold into
//! one "N finished" line, closed until opened. The layout is planned here as
//! data so the row heights the virtualized sidebar needs are testable.

use super::*;
use std::collections::HashSet;
use crate::files::{ChildChatRow, SubagentRow, child_chat_rows, subagent_rows};
use zeron_doc::SubagentStatus;
use zeron_proto::ChatIndicator;

/// The "N subagents" line under the parent row.
const TOGGLE_HEIGHT: f32 = 22.0;
/// One child, fold or note line.
const ROW_HEIGHT: f32 = 24.0;
/// Where the tree's trunk runs, under the parent row's status glyph.
const TRUNK_X: f32 = 16.0;
/// The curve's reach from the trunk to a child's glyph, and its radius.
const ELBOW_WIDTH: f32 = 9.0;
const ELBOW_RADIUS: f32 = 6.0;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TreeItem {
    Chat(ChildChatRow),
    Subagent(SubagentRow),
    /// The fold over settled subagents.
    Finished { done: usize, failed: usize, open: bool },
    /// Subagents running under a chat whose transcript isn't loaded.
    Elsewhere(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TreePlan {
    pub label: String,
    pub open: bool,
    pub items: Vec<TreeItem>,
}

impl TreePlan {
    /// The sidebar slot the tree adds under its parent row.
    pub fn height(&self) -> f32 {
        TOGGLE_HEIGHT
            + if self.open {
                self.items.len() as f32 * ROW_HEIGHT
            } else {
                0.0
            }
    }
}

pub(super) fn finished_key(parent: &str) -> String {
    format!("{parent}:finished")
}

/// `None` when the chat has no children to show.
pub(super) fn plan(
    chats: Vec<ChildChatRow>,
    subagents: Vec<SubagentRow>,
    running_elsewhere: u32,
    tree_flipped: bool,
    finished_flipped: bool,
) -> Option<TreePlan> {
    let running = chats
        .iter()
        .filter(|c| c.status == ChatIndicator::Working)
        .count()
        + subagents
            .iter()
            .filter(|s| s.status == Some(SubagentStatus::Running))
            .count()
        + running_elsewhere as usize;
    let total = chats.len()
        + if subagents.is_empty() {
            running_elsewhere as usize
        } else {
            subagents.len()
        };
    if total == 0 {
        return None;
    }
    let noun = if total == 1 { "subagent" } else { "subagents" };
    let label = if running > 0 {
        format!("{total} {noun} · {running} running")
    } else {
        format!("{total} {noun}")
    };
    let open = (running > 0) != tree_flipped;

    let mut items: Vec<TreeItem> = chats.into_iter().map(TreeItem::Chat).collect();
    let (settled, active): (Vec<_>, Vec<_>) = subagents.into_iter().partition(|s| s.frozen());
    items.extend(active.into_iter().map(TreeItem::Subagent));
    if !settled.is_empty() {
        let failed = settled
            .iter()
            .filter(|s| s.status == Some(SubagentStatus::Failed))
            .count();
        items.push(TreeItem::Finished {
            done: settled.len() - failed,
            failed,
            open: finished_flipped,
        });
        if finished_flipped {
            items.extend(settled.into_iter().map(TreeItem::Subagent));
        }
    }
    if running_elsewhere > 0 && !items.iter().any(|i| matches!(i, TreeItem::Subagent(_))) {
        items.push(TreeItem::Elsewhere(running_elsewhere));
    }
    Some(TreePlan { label, open, items })
}

impl Shell {
    /// Chats with spawned children, gathered once per frame so rows without
    /// any skip the per-row scan of every chat.
    pub(super) fn sidebar_tree_parents(&self, cx: &App) -> HashSet<String> {
        self.state
            .read(cx)
            .chats
            .iter()
            .filter(|chat| !chat.archived)
            .filter_map(|chat| chat.parent_chat_id.clone())
            .collect()
    }

    pub(super) fn sidebar_tree_plan(
        &self,
        parent: &str,
        parents: &HashSet<String>,
        cx: &App,
    ) -> Option<TreePlan> {
        let now = Utc::now();
        let state = self.state.read(cx);
        if !parents.contains(parent)
            && state.selected_chat.as_deref() != Some(parent)
            && state.running_subagents_for(parent, now) == 0
        {
            return None;
        }
        let subagents = subagent_rows(state, parent);
        let elsewhere = if subagents.is_empty() {
            state.running_subagents_for(parent, now)
        } else {
            0
        };
        plan(
            child_chat_rows(state, parent, now),
            subagents,
            elsewhere,
            self.sidebar_tree_flipped.contains(parent),
            self.sidebar_tree_flipped.contains(&finished_key(parent)),
        )
    }

    fn flip_sidebar_tree(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.sidebar_tree_flipped.remove(&key) {
            self.sidebar_tree_flipped.insert(key);
        }
        cx.notify();
    }

    /// The tree under `parent`'s row, drawn in `plan.height()`.
    pub(super) fn render_sidebar_tree(
        &self,
        parent: &str,
        plan: TreePlan,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = theme.text_muted.opacity(0.28);
        let count = if plan.open { plan.items.len() } else { 0 };
        let toggle_key = parent.to_owned();
        let toggle = branch(
            line,
            TOGGLE_HEIGHT,
            count == 0,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(10.0))
                        .flex_none()
                        .text_color(spaces::sidebar_header_glyph(theme))
                        .when(plan.open, |el| {
                            el.with_transformation(gpui::Transformation::rotate(gpui::percentage(
                                0.25,
                            )))
                        }),
                )
                .child(muted(theme, plan.label))
                .into_any_element(),
        )
        .id(SharedString::from(format!("sidebar-tree-{parent}")))
        .debug_selector({
            let parent = parent.to_owned();
            move || format!("sidebar-tree-{parent}")
        })
        .role(gpui::Role::Button)
        .aria_label(if plan.open {
            "Hide subagents"
        } else {
            "Show subagents"
        })
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.flip_sidebar_tree(toggle_key.clone(), cx);
        }));

        let rows = plan
            .items
            .into_iter()
            .take(count)
            .enumerate()
            .map(|(index, item)| {
                let last = index + 1 == count;
                self.render_tree_item(parent, index, item, last, line, theme, cx)
            });
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(toggle)
            .children(rows)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tree_item(
        &self,
        parent: &str,
        index: usize,
        item: TreeItem,
        last: bool,
        line: gpui::Hsla,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("sidebar-tree-{parent}-{index}"));
        match item {
            TreeItem::Chat(chat) => {
                let selected = self.side_chat_open_here(&chat.chat_id, cx);
                let parent_id = parent.to_owned();
                let chat_id = chat.chat_id.clone();
                branch(
                    line,
                    ROW_HEIGHT,
                    last,
                    row_content(
                        dot(spaces::status_dot_color(chat.status, theme)),
                        title(theme, chat.title, selected),
                        Some(muted(theme, chat.time_ago.to_string())),
                    ),
                )
                .id(id)
                .cursor_pointer()
                .rounded(px(5.0))
                .when(selected, |el| el.bg(theme.glass_hover()))
                .hover(|el| el.bg(theme.glass_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    let state = this.state.read(cx);
                    if state.selected_chat.as_deref() != Some(parent_id.as_str()) {
                        this.open_chat(parent_id.clone(), cx);
                    }
                    this.open_child_chat_tab(&chat_id, cx);
                }))
                .into_any_element()
            }
            TreeItem::Subagent(agent) => {
                let color = match agent.status {
                    Some(SubagentStatus::Running) => {
                        spaces::status_dot_color(ChatIndicator::Working, theme)
                    }
                    Some(SubagentStatus::Done) => {
                        spaces::status_dot_color(ChatIndicator::Completed, theme)
                    }
                    Some(SubagentStatus::Failed) => {
                        spaces::status_dot_color(ChatIndicator::Errored, theme)
                    }
                    None => spaces::status_dot_color(ChatIndicator::Idle, theme),
                };
                let parent_id = parent.to_owned();
                let frozen = agent.frozen();
                let doc_id = agent.doc_id.clone();
                let tab_title = agent.title.to_string();
                branch(
                    line,
                    ROW_HEIGHT,
                    last,
                    row_content(
                        dot(color),
                        title(theme, agent.title, false),
                        Some(tag(theme, "subagent")),
                    ),
                )
                .id(id)
                .cursor_pointer()
                .rounded(px(5.0))
                .hover(|el| el.bg(theme.glass_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.add_subagent_surface(
                        parent_id.clone(),
                        doc_id.clone(),
                        tab_title.clone(),
                        frozen,
                        cx,
                    );
                }))
                .into_any_element()
            }
            TreeItem::Finished { done, failed, open } => {
                let key = finished_key(parent);
                let label = if failed > 0 {
                    format!("{} finished · {failed} failed", done + failed)
                } else {
                    format!("{done} finished")
                };
                branch(
                    line,
                    ROW_HEIGHT,
                    last,
                    row_content(
                        icon(icons::ALT_ARROW_RIGHT)
                            .size(px(10.0))
                            .flex_none()
                            .text_color(spaces::sidebar_header_glyph(theme))
                            .when(open, |el| {
                                el.with_transformation(gpui::Transformation::rotate(
                                    gpui::percentage(0.25),
                                ))
                            })
                            .into_any_element(),
                        muted(theme, label),
                        None,
                    ),
                )
                .id(id)
                .cursor_pointer()
                .rounded(px(5.0))
                .hover(|el| el.bg(theme.glass_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.flip_sidebar_tree(key.clone(), cx);
                }))
                .into_any_element()
            }
            TreeItem::Elsewhere(running) => branch(
                line,
                ROW_HEIGHT,
                last,
                row_content(
                    dot(spaces::status_dot_color(ChatIndicator::Working, theme)),
                    muted(theme, format!("{running} running · open the chat to see them")),
                    None,
                ),
            )
            .id(id)
            .into_any_element(),
        }
    }
}

/// One line of the tree: a curved elbow off the trunk, and the trunk running
/// on below unless this is the last line.
fn branch(line: gpui::Hsla, height: f32, last: bool, content: AnyElement) -> gpui::Div {
    div()
        .relative()
        .w_full()
        .h(px(height))
        .flex_none()
        .child(
            div()
                .absolute()
                .left(px(TRUNK_X))
                .top_0()
                .w(px(ELBOW_WIDTH))
                .h(px(height / 2.0))
                .border_l_1()
                .border_b_1()
                .rounded_bl(px(ELBOW_RADIUS))
                .border_color(line),
        )
        .when(!last, |el| {
            el.child(
                div()
                    .absolute()
                    .left(px(TRUNK_X))
                    .top(px(height / 2.0))
                    .bottom_0()
                    .border_l_1()
                    .border_color(line),
            )
        })
        .child(
            div()
                .absolute()
                .left(px(TRUNK_X + ELBOW_WIDTH + 4.0))
                .right(px(6.0))
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .child(content),
        )
}

fn row_content(lead: AnyElement, title: AnyElement, trail: Option<AnyElement>) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(lead)
        .child(div().flex_1().min_w_0().child(title))
        .children(trail.map(|el| div().flex_none().child(el)))
        .into_any_element()
}

fn dot(color: gpui::Hsla) -> AnyElement {
    div()
        .size(px(6.0))
        .flex_none()
        .rounded_full()
        .bg(color)
        .into_any_element()
}

fn title(theme: &Theme, text: SharedString, selected: bool) -> AnyElement {
    div()
        .truncate()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(if selected { theme.text } else { theme.text_muted })
        .child(text)
        .into_any_element()
}

fn muted(theme: &Theme, text: String) -> AnyElement {
    div()
        .truncate()
        .text_size(crate::typography::ui_rems(11.0))
        .text_color(theme.text_muted.opacity(0.75))
        .child(SharedString::from(text))
        .into_any_element()
}

fn tag(theme: &Theme, text: &'static str) -> AnyElement {
    div()
        .px(px(5.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .bg(crate::theme::wash(0.08))
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text_muted.opacity(0.8))
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, status: ChatIndicator) -> ChildChatRow {
        ChildChatRow::for_test(id, status)
    }

    fn agent(id: &str, status: Option<SubagentStatus>) -> SubagentRow {
        SubagentRow {
            doc_id: id.into(),
            title: id.to_owned().into(),
            status,
            spawned_at: Utc::now(),
        }
    }

    #[test]
    fn no_children_means_no_tree() {
        assert_eq!(plan(vec![], vec![], 0, false, false), None);
    }

    #[test]
    fn collapsed_when_idle_and_open_while_running() {
        let idle = plan(vec![chat("a", ChatIndicator::Idle)], vec![], 0, false, false).unwrap();
        assert!(!idle.open);
        assert_eq!(idle.label, "1 subagent");
        assert_eq!(idle.height(), TOGGLE_HEIGHT);

        let busy = plan(vec![chat("a", ChatIndicator::Working)], vec![], 0, false, false).unwrap();
        assert!(busy.open);
        assert_eq!(busy.label, "1 subagent · 1 running");
        assert_eq!(busy.height(), TOGGLE_HEIGHT + ROW_HEIGHT);

        // A click flips either default.
        assert!(plan(vec![chat("a", ChatIndicator::Idle)], vec![], 0, true, false).unwrap().open);
        assert!(!plan(vec![chat("a", ChatIndicator::Working)], vec![], 0, true, false).unwrap().open);
    }

    #[test]
    fn settled_subagents_fold_until_opened() {
        let agents = vec![
            agent("run", Some(SubagentStatus::Running)),
            agent("ok", Some(SubagentStatus::Done)),
            agent("bad", Some(SubagentStatus::Failed)),
        ];
        let closed = plan(vec![], agents.clone(), 0, false, false).unwrap();
        assert_eq!(closed.label, "3 subagents · 1 running");
        assert_eq!(closed.items.len(), 2);
        assert_eq!(
            closed.items[1],
            TreeItem::Finished { done: 1, failed: 1, open: false }
        );
        let opened = plan(vec![], agents, 0, false, true).unwrap();
        assert_eq!(opened.items.len(), 4);
        assert_eq!(opened.height(), TOGGLE_HEIGHT + 4.0 * ROW_HEIGHT);
    }

    #[test]
    fn unloaded_chats_count_running_subagents_without_listing_them() {
        let tree = plan(vec![chat("a", ChatIndicator::Idle)], vec![], 2, false, false).unwrap();
        assert_eq!(tree.label, "3 subagents · 2 running");
        assert!(tree.open);
        assert_eq!(tree.items.last(), Some(&TreeItem::Elsewhere(2)));
    }
}

//! A sidebar chat's children, drawn as a tree hanging off its row: the chats
//! it spawned (side chats and Zeron MCP `create_chat` children, which carry
//! `parent_chat_id`) and, for the open chat only, its harness subagents.
//!
//! Spawned chats are full sidebar rows (harness, status, time, project,
//! branch, PR), indented under curved tree lines. Subagents live in a chat's
//! transcript, and only the open chat's transcript is loaded, so other rows
//! say how many are running instead of loading every transcript to list them.
//! The tree is always shown: nothing collapses.

use super::*;
use crate::files::{SubagentRow, child_chat_rows, subagent_rows};
use spaces::ActiveChatRow;
use std::collections::HashSet;
use zeron_doc::SubagentStatus;
use zeron_proto::ChatIndicator;

/// A subagent or note line.
const LINE_HEIGHT: f32 = 26.0;
/// Where the trunk runs: under the parent row's leading glyph.
const TRUNK_X: f32 = 14.0;
/// The curve's reach from the trunk to a child, and its radius.
const ELBOW_WIDTH: f32 = 10.0;
const ELBOW_RADIUS: f32 = 7.0;
/// A child row's left edge, past the curve.
const CHILD_INSET: f32 = TRUNK_X + ELBOW_WIDTH + 2.0;

/// What the tree lists, in order. Kept as ids and rows so it tests without a
/// window.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TreeItem<C> {
    Chat(C),
    Subagent(SubagentRow),
    /// Subagents running under a chat whose transcript isn't loaded.
    Elsewhere(u32),
}

/// Spawned chats first (newest activity leading), then subagents (running
/// first), then the running count when subagents can't be listed.
pub(super) fn plan<C>(
    chats: Vec<C>,
    subagents: Vec<SubagentRow>,
    running_elsewhere: u32,
) -> Vec<TreeItem<C>> {
    let listed = !subagents.is_empty();
    let mut items: Vec<TreeItem<C>> = chats.into_iter().map(TreeItem::Chat).collect();
    items.extend(subagents.into_iter().map(TreeItem::Subagent));
    if running_elsewhere > 0 && !listed {
        items.push(TreeItem::Elsewhere(running_elsewhere));
    }
    items
}

/// A planned tree with each line's height resolved.
pub(super) struct SidebarTree {
    lines: Vec<(TreeItem<ActiveChatRow>, f32)>,
}

impl SidebarTree {
    /// The sidebar slot the tree adds under its parent row.
    pub fn height(&self) -> f32 {
        self.lines.iter().map(|(_, height)| height).sum()
    }
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

    pub(super) fn sidebar_tree(
        &self,
        parent: &str,
        parents: &HashSet<String>,
        cx: &App,
    ) -> Option<SidebarTree> {
        let now = Utc::now();
        let state = self.state.read(cx);
        if !parents.contains(parent)
            && state.selected_chat.as_deref() != Some(parent)
            && state.running_subagents_for(parent, now) == 0
        {
            return None;
        }
        let chats: Vec<ActiveChatRow> = child_chat_rows(state, parent, now)
            .into_iter()
            .filter_map(|row| {
                let chat = state.chats.iter().find(|c| c.id == row.chat_id)?.clone();
                Some(self.sidebar_chat_data(row.status, chat, state))
            })
            .collect();
        let subagents = subagent_rows(state, parent);
        let elsewhere = if subagents.is_empty() {
            state.running_subagents_for(parent, now)
        } else {
            0
        };
        let lines: Vec<_> = plan(chats, subagents, elsewhere)
            .into_iter()
            .map(|item| {
                let height = match &item {
                    TreeItem::Chat(row) => {
                        sidebar_row_height(
                            self.settings.sidebar_compact,
                            self.settings.sidebar_show_project_label,
                            row.branch.is_some(),
                            row.change_request.is_some(),
                        ) + SIDEBAR_LIST_GAP
                    }
                    TreeItem::Subagent(_) | TreeItem::Elsewhere(_) => LINE_HEIGHT,
                };
                (item, height)
            })
            .collect();
        (!lines.is_empty()).then_some(SidebarTree { lines })
    }

    /// The tree under `parent`'s row, drawn in `tree.height()`.
    pub(super) fn render_sidebar_tree(
        &self,
        parent: &str,
        tree: SidebarTree,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = theme.text_muted.opacity(0.3);
        let count = tree.lines.len();
        let selected = self.state.read(cx).selected_chat.clone();
        let parent_harness = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == parent)
            .and_then(|chat| chat.config.as_ref().map(|c| c.harness));
        let lines = tree
            .lines
            .into_iter()
            .enumerate()
            .map(|(index, (item, height))| {
                let last = index + 1 == count;
                match item {
                    TreeItem::Chat(row) => {
                        // The elbow meets the title line, the row's last.
                        let reach = height - 6.0 - 8.5;
                        let is_selected = selected.as_deref() == Some(row.chat.id.as_str())
                            || self.side_chat_open_here(&row.chat.id, cx);
                        let element = self.render_tree_chat(row, is_selected, theme, cx);
                        branch(line, height, reach, last, element)
                    }
                    TreeItem::Subagent(agent) => {
                        let element = self.render_tree_subagent(
                            parent,
                            index,
                            agent,
                            parent_harness,
                            theme,
                            cx,
                        );
                        branch(line, height, height / 2.0, last, element)
                    }
                    TreeItem::Elsewhere(running) => branch(
                        line,
                        height,
                        height / 2.0,
                        last,
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(Theme::SPACE_SM))
                            .child(dot(spaces::status_dot_color(ChatIndicator::Working, theme)))
                            .child(muted(
                                theme,
                                format!("{running} subagents running · open the chat to list them"),
                            ))
                            .into_any_element(),
                    ),
                }
            });
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(lines)
            .into_any_element()
    }

    /// A spawned chat, as the same row its parent wears.
    fn render_tree_chat(
        &self,
        row: ActiveChatRow,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ActiveChatRow {
            status,
            chat,
            folder,
            branch,
            change_request,
            ..
        } = row;
        let time_ago: SharedString =
            format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), Utc::now()).into();
        let harness = self
            .settings
            .sidebar_show_harness
            .then(|| chat.config.as_ref().map(|c| c.harness))
            .flatten();
        let title = transcript::single_line(&crate::files::child_chat_title(&chat));
        div()
            .pt(px(SIDEBAR_LIST_GAP))
            .child(self.render_chat_row(
                chat.id.clone(),
                title.into(),
                time_ago,
                folder.into(),
                branch.map(SharedString::from),
                change_request,
                harness,
                status,
                selected,
                false,
                false,
                None,
                None,
                false,
                None,
                theme,
                cx,
            ))
            .into_any_element()
    }

    fn render_tree_subagent(
        &self,
        parent: &str,
        index: usize,
        agent: SubagentRow,
        harness: Option<zeron_proto::HarnessId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let live = self
            .state
            .read(cx)
            .subagent_activity
            .get(&agent.doc_id)
            .cloned();
        let (color, label) = match (agent.status, live.as_deref()) {
            (Some(SubagentStatus::Running), Some("waiting")) => (
                spaces::status_dot_color(ChatIndicator::AwaitingInput, theme),
                "Waiting",
            ),
            (Some(SubagentStatus::Running), Some("idle")) => {
                (spaces::status_dot_color(ChatIndicator::Idle, theme), "Idle")
            }
            (status, _) => match status {
                Some(SubagentStatus::Running) => (
                    spaces::status_dot_color(ChatIndicator::Working, theme),
                    "Working",
                ),
                Some(SubagentStatus::Done) => (
                    spaces::status_dot_color(ChatIndicator::Completed, theme),
                    "Done",
                ),
                Some(SubagentStatus::Failed) => (
                    spaces::status_dot_color(ChatIndicator::Errored, theme),
                    "Failed",
                ),
                None => (spaces::status_dot_color(ChatIndicator::Idle, theme), ""),
            },
        };
        let parent_id = parent.to_owned();
        let frozen = agent.frozen();
        let doc_id = agent.doc_id.clone();
        let tab_title = agent.title.to_string();
        let time_ago = format_time_ago(agent.spawned_at, Utc::now());
        div()
            .id(SharedString::from(format!("sidebar-tree-{parent}-{index}")))
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme.glass_hover()))
            .child(dot(color))
            .when_some(
                harness.map(crate::pickers::harness_brand_icon),
                |el, (path, tint)| {
                    el.child(
                        icon(path)
                            .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                            .flex_none()
                            .text_color(tint.unwrap_or(theme.text_muted).opacity(0.8)),
                    )
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text.opacity(0.8))
                    .child(agent.title),
            )
            .child(tag(theme, "subagent"))
            .child(muted(
                theme,
                if label.is_empty() || label == "Done" {
                    time_ago
                } else {
                    label.to_owned()
                },
            ))
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
}

/// One line of the tree: a curved elbow off the trunk reaching down to
/// `reach`, the trunk running on below unless this is the last line.
fn branch(
    line: gpui::Hsla,
    height: f32,
    reach: f32,
    last: bool,
    content: AnyElement,
) -> AnyElement {
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
                .h(px(reach))
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
                    .top(px(reach))
                    .bottom_0()
                    .border_l_1()
                    .border_color(line),
            )
        })
        .child(
            div()
                .absolute()
                .left(px(CHILD_INSET))
                .right_0()
                .top_0()
                .bottom_0()
                .child(content),
        )
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

fn muted(theme: &Theme, text: String) -> AnyElement {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(10.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted.opacity(0.5))
        .child(SharedString::from(text))
        .into_any_element()
}

fn tag(theme: &Theme, text: &'static str) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .h(px(15.0))
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

    fn agent(id: &str, status: Option<SubagentStatus>) -> SubagentRow {
        SubagentRow {
            doc_id: id.into(),
            title: id.to_owned().into(),
            status,
            spawned_at: Utc::now(),
        }
    }

    #[test]
    fn no_children_means_no_lines() {
        assert!(plan::<&str>(vec![], vec![], 0).is_empty());
    }

    #[test]
    fn chats_lead_then_every_subagent_is_listed() {
        let items = plan(
            vec!["a", "b"],
            vec![
                agent("run", Some(SubagentStatus::Running)),
                agent("ok", Some(SubagentStatus::Done)),
            ],
            0,
        );
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], TreeItem::Chat("a"));
        assert!(matches!(&items[3], TreeItem::Subagent(s) if s.doc_id == "ok"));
    }

    #[test]
    fn unloaded_chats_note_running_subagents_without_listing_them() {
        let items = plan(vec!["a"], vec![], 2);
        assert_eq!(items.last(), Some(&TreeItem::Elsewhere(2)));
        // A loaded list replaces the note.
        let items = plan::<&str>(vec![], vec![agent("x", None)], 2);
        assert!(!items.iter().any(|i| matches!(i, TreeItem::Elsewhere(_))));
    }
}

//! Context occupancy is read from the replicated chat snapshot, never local CLI state.
use crate::theme::Theme;
use gpui::{IntoElement, PathBuilder, SharedString, canvas, div, point, prelude::*, px};
use zeron_proto::ContextUsage;

/// The context ring's trigger chip; the footer ([`crate::account_usage`])
/// opens [`card`] from it on click.
pub fn chip(usage: Option<ContextUsage>, open: bool, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    let fraction = usage.and_then(ContextUsage::fraction);
    let color = match fraction {
        Some(f) if f >= 0.9 => theme.danger,
        Some(f) if f >= 0.75 => theme.warning,
        Some(_) => theme.text_muted,
        None => theme.text_faint,
    };
    let label = fraction
        .map(|f| format!("{:.0}%", f * 100.0))
        .unwrap_or_else(|| "—".into());
    ring_chip(
        "context-usage",
        fraction.unwrap_or(0.0) as f32,
        color,
        color,
        label,
        open,
        theme,
    )
}

/// One footer ring indicator: ring + percent, identical geometry for every
/// ring so they sit side by side as equals. `arc` colours the ring's fill,
/// `text` the label; `open` holds the hover wash while its popover is up.
pub(crate) fn ring_chip(
    id: &'static str,
    fraction: f32,
    arc: gpui::Hsla,
    text: gpui::Hsla,
    label: String,
    open: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .when(open, |s| s.bg(crate::theme::ink(0.05)))
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .child(ring(fraction, arc, theme))
        .child(SharedString::from(label))
}

/// The footer's 16px progress ring: a faint full track under a `fraction` arc
/// starting at twelve o'clock. Shared with the account usage indicator.
pub(crate) fn ring(fraction: f32, color: gpui::Hsla, theme: &Theme) -> impl IntoElement {
    let track = theme.text_faint.opacity(0.25);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let mut arc = |fraction: f32, color| {
                if fraction <= 0.0 {
                    return;
                }
                let steps = (64.0 * fraction).ceil().max(2.0) as usize;
                let mut path = PathBuilder::stroke(px(1.8));
                for i in 0..=steps {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
                    let p = point(
                        center.x + px(6.0 * angle.cos()),
                        center.y + px(6.0 * angle.sin()),
                    );
                    if i == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            arc(1.0, track);
            arc(fraction.clamp(0.0, 1.0), color);
        },
    )
    .size(px(16.0))
}

fn with_separators(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// whether the indicator has anything to measure against: harnesses that
/// never report a window (antigravity) get no indicator at all, rather than a
/// permanently empty ring.
pub fn has_window(usage: Option<ContextUsage>) -> bool {
    usage
        .and_then(|usage| usage.window)
        .is_some_and(|window| window > 0)
}

fn details(usage: Option<ContextUsage>) -> String {
    match usage.unwrap_or_default() {
        ContextUsage {
            tokens: Some(tokens),
            window: Some(window),
            ..
        } if window > 0 => {
            format!(
                "{} / {} tokens\n{} tokens remaining",
                with_separators(tokens),
                with_separators(window),
                with_separators(window.saturating_sub(tokens))
            )
        }
        ContextUsage {
            tokens: Some(tokens),
            ..
        } => format!(
            "{} tokens used\nContext limit not reported",
            with_separators(tokens)
        ),
        ContextUsage {
            window: Some(window),
            ..
        } if window > 0 => format!(
            "{} token capacity\nWaiting for context usage",
            with_separators(window)
        ),
        _ => "Context usage not reported by this harness yet".into(),
    }
}

/// `12.3k`-style short token count.
fn short_tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", count as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}k", (count as f64 / 1_000.0).round() as u64),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

/// The breakdown's rows, largest first, with each part's share of the used
/// tokens. Empty parts are left out.
fn breakdown_rows(usage: Option<ContextUsage>) -> Vec<(&'static str, String, u32)> {
    let Some(b) = usage.and_then(|u| u.breakdown) else {
        return Vec::new();
    };
    let mut parts = [
        ("Messages", b.messages),
        ("System prompt", b.system),
        ("Tools", b.tools),
        ("MCP tools", b.mcp),
        ("Memory files", b.memory),
        ("Skills & agents", b.skills),
        ("Reminders", b.reminders),
        ("Tool schemas & output", b.other),
    ];
    parts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let total: u64 = parts.iter().map(|(_, n)| n).sum();
    parts
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(label, n)| {
            let share = (n as f64 * 100.0 / total.max(1) as f64).round() as u32;
            (label, short_tokens(n), share)
        })
        .collect()
}

/// The context ring's popover content.
pub fn card(usage: Option<ContextUsage>, theme: &Theme) -> gpui::Div {
    let rows = breakdown_rows(usage);
    crate::popover::popover_card(theme)
        .flex()
        .flex_col()
        .child(crate::popover::menu_heading(theme, "Context window"))
        .child(
            div()
                .px(px(8.0))
                .pb(px(6.0))
                .text_size(px(12.0))
                .line_height(px(19.0))
                .whitespace_nowrap()
                .text_color(theme.text_muted)
                .child(SharedString::from(details(usage))),
        )
        .when(!rows.is_empty(), |card| {
            card.child(crate::popover::menu_heading(theme, "What fills it (estimated)"))
                .child(
                    div()
                        .px(px(8.0))
                        .pb(px(6.0))
                        .flex()
                        .flex_col()
                        .text_size(px(12.0))
                        .line_height(px(19.0))
                        .whitespace_nowrap()
                        .children(rows.into_iter().map(|(label, tokens, share)| {
                            div()
                                .flex()
                                .gap(px(12.0))
                                .child(div().flex_1().text_color(theme.text_muted).child(label))
                                .child(div().text_color(theme.text).child(SharedString::from(tokens)))
                                .child(
                                    div()
                                        .w(px(34.0))
                                        .flex()
                                        .justify_end()
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(format!("{share}%"))),
                                )
                        })),
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indicator_needs_a_reported_window() {
        assert!(!has_window(None));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: None,
            ..Default::default()
        })));
        assert!(!has_window(Some(ContextUsage {
            tokens: Some(1_200),
            window: Some(0),
            ..Default::default()
        })));
        assert!(has_window(Some(ContextUsage {
            tokens: None,
            window: Some(200_000),
            ..Default::default()
        })));
    }

    #[test]
    fn missing_usage_is_distinct_from_zero_and_overflow() {
        assert!(details(None).contains("not reported"));
        assert!(
            details(Some(ContextUsage {
                tokens: Some(0),
                window: Some(200),
                ..Default::default()
            }))
            .contains("200 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(250),
                window: Some(200),
                ..Default::default()
            }))
            .contains("0 tokens remaining")
        );
        assert!(
            details(Some(ContextUsage {
                tokens: Some(10),
                window: Some(0),
                ..Default::default()
            }))
            .contains("limit not reported")
        );
    }

    #[test]
    fn breakdown_lists_parts_largest_first() {
        let usage = ContextUsage {
            breakdown: Some(zeron_proto::ContextBreakdown {
                system: 2_000,
                tools: 12_400,
                messages: 25_600,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            breakdown_rows(Some(usage)),
            [
                ("Messages", "26k".to_owned(), 64),
                ("Tools", "12k".to_owned(), 31),
                ("System prompt", "2.0k".to_owned(), 5),
            ]
        );
        assert!(breakdown_rows(None).is_empty());
        assert_eq!(short_tokens(999), "999");
        assert_eq!(short_tokens(1_260_000), "1.3M");
    }

    #[test]
    fn token_counts_are_grouped_by_thousands() {
        assert_eq!(with_separators(0), "0");
        assert_eq!(with_separators(999), "999");
        assert_eq!(with_separators(5417), "5,417");
        assert_eq!(with_separators(1_048_576), "1,048,576");
        assert_eq!(
            details(Some(ContextUsage {
                tokens: Some(5417),
                window: Some(1_048_576),
                ..Default::default()
            })),
            "5,417 / 1,048,576 tokens\n1,043,159 tokens remaining"
        );
    }
}

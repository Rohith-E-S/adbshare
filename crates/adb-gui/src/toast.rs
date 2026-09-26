//! Transient in-app notifications.
//!
//! The GTK build sent device connect/disconnect and transfer results to the
//! desktop with `gio::Notification`. Pulling GLib in behind a GPUI window would
//! drag a second main loop into the process, so notifications are drawn by the
//! app instead — which also means they respect the app's own theme and can carry
//! an inline action.

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{AnyElement, IntoElement, Styled, Window, div, px};

use crate::icons::{self, names};
use crate::theme::Palette;
use crate::ui;

/// How long a toast stays before it can be dismissed.
const LIFETIME: Duration = Duration::from_secs(4);
/// Extra time a toast lingers, so it stays clickable to the very end.
const GRACE: Duration = Duration::from_millis(600);
/// A burst of events must not fill the screen.
const MAX_VISIBLE: usize = 4;

/// Severity, which picks the accent colour and icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastTone {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastTone {
    fn icon(self) -> &'static str {
        match self {
            ToastTone::Info => names::DIALOG_INFORMATION,
            ToastTone::Success => names::OBJECT_SELECT,
            ToastTone::Warning => names::DIALOG_WARNING,
            ToastTone::Error => names::DIALOG_ERROR,
        }
    }

    fn accent(self, t: &Palette) -> gpui::Rgba {
        match self {
            ToastTone::Info => t.text_dim,
            ToastTone::Success => t.success,
            ToastTone::Warning => t.warning,
            ToastTone::Error => t.danger,
        }
    }
}

/// Identifies a toast action so one click handler can serve them all, rather
/// than a closure per toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    /// Re-queue every failed transfer.
    RetryFailed,
    /// Reveal the local downloads folder, offered when a queue call fails.
    OpenDownloads,
    /// Open the diagnostics report, offered when the daemon stops answering.
    CopyDiagnostics,
}

/// One in-flight notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub tone: ToastTone,
    /// An optional trailing button, as `(label, id)`.
    pub action: Option<(String, ActionId)>,
    expires_at: Instant,
}

/// The stack of live toasts, oldest first.
#[derive(Debug, Default, Clone)]
pub struct ToastStack {
    items: Vec<Toast>,
}

impl ToastStack {
    /// Show a message.
    pub fn push(&mut self, message: impl Into<String>, tone: ToastTone) {
        self.push_with_action(message, tone, None);
    }

    /// Show a message with a trailing action button.
    pub fn push_with_action(
        &mut self,
        message: impl Into<String>,
        tone: ToastTone,
        action: Option<(String, ActionId)>,
    ) {
        let message = message.into();
        // A repeating failure (a poll that keeps erroring) must not stack up, so
        // a toast with the same text is replaced outright. The newest state
        // wins, even when only one of the two had an action attached.
        self.items.retain(|t| t.message != message);
        self.items.push(Toast {
            message,
            tone,
            action,
            expires_at: Instant::now() + LIFETIME + GRACE,
        });
        if self.items.len() > MAX_VISIBLE {
            let excess = self.items.len() - MAX_VISIBLE;
            self.items.drain(0..excess);
        }
    }

    /// Drop everything that has run out. Returns true when something was
    /// removed, so the caller knows to re-render.
    pub fn prune(&mut self) -> bool {
        let now = Instant::now();
        let before = self.items.len();
        self.items.retain(|t| t.expires_at > now);
        self.items.len() != before
    }

    /// Remove the toast carrying `id`, after its action has run.
    pub fn dismiss_action(&mut self, id: ActionId) {
        self.items
            .retain(|t| t.action.as_ref().is_none_or(|(_, a)| *a != id));
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many toasts are showing. Used by the tests to assert on the stack.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Draw the stack in the bottom-right corner, above everything else.
    pub fn render<F>(&self, t: &Palette, on_action: F) -> Option<AnyElement>
    where
        // `Clone` so each card can own its own handle to the same closure; the
        // alternative is boxing, which every call site would have to unwind.
        F: Fn(ActionId, &mut Window, &mut gpui::App) + Clone + 'static,
    {
        if self.is_empty() {
            return None;
        }
        let cards = self
            .items
            .iter()
            .map(|toast| render_toast(t, toast, on_action.clone()))
            .collect::<Vec<_>>();

        Some(
            ui::on_top(
                div()
                    .absolute()
                    .bottom(px(48.0))
                    .right(px(16.0))
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(8.0))
                    .w(px(340.0))
                    .children(cards),
            )
            .into_any_element(),
        )
    }
}

/// One toast card: an accent rule, an icon, the message and an optional action.
fn render_toast<F>(t: &Palette, toast: &Toast, on_action: F) -> AnyElement
where
    F: Fn(ActionId, &mut Window, &mut gpui::App) + Clone + 'static,
{
    let action = toast.action.clone();
    let accent = toast.tone.accent(t);

    div()
        .id(gpui::ElementId::from(gpui::SharedString::from(format!(
            "toast:{}",
            toast.message
        ))))
        .flex()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .py(px(10.0))
        .pl(px(12.0))
        .pr(px(12.0))
        .rounded(px(10.0))
        .bg(t.card)
        .border_1()
        .border_color(t.border)
        .shadow_lg()
        .child(div().w(px(3.0)).h(px(20.0)).rounded_full().bg(accent))
        .child(icons::icon(toast.tone.icon(), 15.0, accent))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(t.text_primary)
                .line_clamp(3)
                .child(toast.message.clone()),
        )
        .when_some(action, move |d, (label, id)| {
            d.child(
                div()
                    .id(gpui::ElementId::from(gpui::SharedString::from(format!(
                        "toast-action:{id:?}",
                    ))))
                    .flex()
                    .items_center()
                    .h(px(24.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(t.text_header)
                    .bg(t.hover)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.pressed))
                    .child(label.clone())
                    .on_click(move |_, window, cx| on_action(id, window, cx)),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_messages_do_not_stack() {
        let mut stack = ToastStack::default();
        stack.push("device disconnected", ToastTone::Warning);
        stack.push("device disconnected", ToastTone::Warning);
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn a_later_toast_replaces_an_earlier_one_with_the_same_text() {
        let mut stack = ToastStack::default();
        stack.push_with_action(
            "no device",
            ToastTone::Warning,
            Some(("Retry".into(), ActionId::RetryFailed)),
        );
        stack.push("no device", ToastTone::Info);
        assert_eq!(stack.len(), 1, "the newer message wins");
        assert!(stack.items[0].action.is_none());
    }

    #[test]
    fn a_repeated_message_keeps_its_action() {
        let mut stack = ToastStack::default();
        stack.push_with_action(
            "no device",
            ToastTone::Warning,
            Some(("Retry".into(), ActionId::RetryFailed)),
        );
        stack.push_with_action(
            "no device",
            ToastTone::Warning,
            Some(("Retry".into(), ActionId::RetryFailed)),
        );
        assert_eq!(stack.len(), 1, "replaced, not appended");
        assert_eq!(
            stack.items[0].action.as_ref().map(|(_, id)| *id),
            Some(ActionId::RetryFailed)
        );
    }

    #[test]
    fn distinct_messages_accumulate() {
        let mut stack = ToastStack::default();
        stack.push("first", ToastTone::Info);
        stack.push("second", ToastTone::Error);
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn bursts_are_capped_dropping_the_oldest() {
        let mut stack = ToastStack::default();
        for i in 0..10 {
            stack.push(format!("message {i}"), ToastTone::Info);
        }
        assert_eq!(stack.len(), MAX_VISIBLE);
        assert_eq!(stack.items.last().unwrap().message, "message 9");
        assert_eq!(stack.items.first().unwrap().message, "message 6");
    }

    #[test]
    fn pruning_keeps_toasts_within_their_lifetime() {
        let mut stack = ToastStack::default();
        stack.push("hello", ToastTone::Info);
        assert_eq!(stack.len(), 1);
        assert!(!stack.prune(), "nothing has expired yet");
    }

    #[test]
    fn dismissing_an_action_removes_only_that_toast() {
        let mut stack = ToastStack::default();
        stack.push("plain", ToastTone::Info);
        stack.push_with_action(
            "retry?",
            ToastTone::Warning,
            Some(("Retry".into(), ActionId::RetryFailed)),
        );
        assert_eq!(stack.len(), 2);
        stack.dismiss_action(ActionId::RetryFailed);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.items[0].message, "plain");
    }

    #[test]
    fn every_tone_has_an_icon_and_a_visible_accent() {
        let t = Palette::one_dark();
        for tone in [
            ToastTone::Info,
            ToastTone::Success,
            ToastTone::Warning,
            ToastTone::Error,
        ] {
            let path = format!("icons/{}.svg", tone.icon());
            assert!(
                crate::icons::ASSETS.iter().any(|(name, _)| *name == path),
                "{tone:?} icon {path} is not embedded"
            );
            let accent = tone.accent(&t);
            assert!(accent.a > 0.0, "{tone:?} accent would be invisible");
        }
    }
}

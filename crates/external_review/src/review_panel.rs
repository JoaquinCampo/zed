//! A dock panel, on the right by default, that shows the review: every change still
//! waiting for review, with keep and reject on each hunk.

use crate::{ReviewPane, ReviewSessions, ToggleFocus, session_for};
use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, WeakEntity, Window, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub struct ReviewPanel {
    workspace: WeakEntity<Workspace>,
    review: Entity<ReviewPane>,
    position: DockPosition,
    _subscription: Subscription,
}

impl ReviewPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            let project = workspace.project().clone();
            cx.new(|cx| Self::new(weak_workspace, project, window, cx))
        })
    }

    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let review = cx.new(|cx| ReviewPane::new(workspace.clone(), project, window, cx));
        Self {
            workspace,
            review,
            position: DockPosition::Right,
            // The dock button shows only while a review runs.
            _subscription: cx.observe_global::<ReviewSessions>(|_, cx| cx.notify()),
        }
    }

    fn is_reviewing(&self, cx: &App) -> bool {
        session_for(self.workspace.entity_id(), cx).is_some()
    }
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.review.read(cx).root_focus_handle()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.review.clone())
    }
}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "ExternalReviewPanel"
    }

    fn panel_key() -> &'static str {
        "ExternalReviewPanel"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.review.focus_handle(cx)
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(560.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        self.is_reviewing(cx).then_some(IconName::Diff)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Changes to Review")
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        let files = session_for(self.workspace.entity_id(), cx)?
            .read(cx)
            .pending_counts(cx)
            .files;
        (files > 0).then(|| files.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

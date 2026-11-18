//! A dock panel, on the left by default, that shows the review: every change still
//! waiting for review, with keep and reject on each hunk.

use crate::{ReviewPane, ReviewSessions, ToggleFocus, session_for};
use anyhow::Result;
use db::kvp::KeyValueStore;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, WeakEntity, Window, px,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

/// Where the panel was last docked, kept across restarts.
const POSITION_KEY: &str = "external_review_panel_position";

fn saved_position(cx: &App) -> DockPosition {
    match KeyValueStore::global(cx).read_kvp(POSITION_KEY).log_err().flatten().as_deref() {
        Some("right") => DockPosition::Right,
        _ => DockPosition::Left,
    }
}

pub struct ReviewPanel {
    workspace: WeakEntity<Workspace>,
    review: Entity<ReviewPane>,
    position: DockPosition,
    /// The dock's panel before the review opened, shown again once it closes.
    return_to: Option<&'static str>,
    /// Whether the review is showing because it was asked for.
    requested: bool,
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
            position: saved_position(cx),
            return_to: None,
            requested: false,
            // The dock button shows only while a review runs.
            _subscription: cx.observe_global::<ReviewSessions>(|_, cx| cx.notify()),
        }
    }

    fn is_reviewing(&self, cx: &App) -> bool {
        session_for(self.workspace.entity_id(), cx).is_some()
    }
}

/// Shows the review panel on purpose, remembering the dock's panel to go back to.
pub fn show(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.panel::<ReviewPanel>(cx) {
        let position = panel.read(cx).position;
        let dock = workspace.dock_at_position(position).read(cx);
        let previous = dock
            .active_panel()
            .filter(|active| active.panel_id() != panel.entity_id())
            .map(|active| active.persistent_name());
        panel.update(cx, |panel, _| {
            if previous.is_some() {
                panel.return_to = previous;
            }
            panel.requested = true;
        });
    }
    workspace.focus_panel::<ReviewPanel>(window, cx);
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
        let value = match position {
            DockPosition::Right => "right",
            DockPosition::Left | DockPosition::Bottom => "left",
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(POSITION_KEY.to_string(), value.to_string())
                .await
                .log_err();
        })
        .detach();
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

    /// The review shows only when asked for (cmd-shift-b, its dock button, bb, the
    /// command palette). When its dock is toggled instead (cmd-b), the dock shows
    /// its panel from before the review, or closes if the review is its only panel.
    /// After the review closes, that earlier panel is made the dock's active one
    /// again, so toggling the dock brings it back.
    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Showing a panel activates it more than once, so the request lasts until
        // the review closes.
        if active && self.requested {
            return;
        }
        if !active {
            self.requested = false;
        }
        let workspace = self.workspace.clone();
        let position = self.position;
        let return_to = self.return_to;
        let panel_id = cx.entity_id();
        // Called while the dock is being updated, so look at it afterwards.
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let dock = workspace.read(cx).dock_at_position(position).clone();
            dock.update(cx, |dock, cx| {
                let review_is_active = dock
                    .active_panel()
                    .is_some_and(|panel| panel.panel_id() == panel_id);
                // Closed after being shown, or shown without being asked for.
                if !review_is_active || dock.is_open() != active {
                    return;
                }
                let review_index = dock.active_panel_index();
                let other = return_to
                    .and_then(|name| dock.panel_index_for_persistent_name(name, cx))
                    .or_else(|| dock.first_enabled_panel_idx(cx).ok())
                    .filter(|index| Some(*index) != review_index);
                match other {
                    Some(index) => dock.activate_panel(index, window, cx),
                    None if active => dock.set_open(false, window, cx),
                    None => {}
                }
            });
        });
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

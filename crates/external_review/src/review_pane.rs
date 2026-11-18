//! A tab listing every change still waiting for review, and a status bar item that
//! counts them.

use crate::{
    ExternalReviewAddon, KeepAll, OpenReview, RejectAll, ReviewHunkRenderer, ReviewKind,
    ReviewSession, ReviewSessions, review_ranges, session_for,
};
use anyhow::Result;
use collections::HashSet;
use editor::{Editor, EditorEvent, MultiBuffer, SelectionEffects, multibuffer_context_lines};
use gpui::{
    Action as _, AnyElement, App, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use language::Capability;
use multi_buffer::PathKey;
use project::{Project, ProjectPath};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{Divider, KeyBinding, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, Item, ItemHandle, ItemNavHistory, StatusItemView, Workspace,
    item::{ItemEvent, SaveOptions, TabContentParams},
    searchable::SearchableItemHandle,
};

/// What a review still has to go through: files with changes and their hunk count.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct PendingCounts {
    pub files: usize,
    pub hunks: usize,
}

impl PendingCounts {
    fn files_label(&self) -> String {
        match self.files {
            1 => "1 file".to_string(),
            files => format!("{files} files"),
        }
    }

    fn hunks_label(&self) -> String {
        match self.hunks {
            1 => "1 change".to_string(),
            hunks => format!("{hunks} changes"),
        }
    }
}

/// Every change of the workspace's review in one scrolling editor, with keep and
/// reject on each hunk and for all of them at once.
pub struct ReviewPane {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    session: Option<Entity<ReviewSession>>,
    multibuffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewPane {
    /// Opens the Review tab of the workspace, or focuses it if it is already open.
    pub fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let existing = workspace.items_of_type::<ReviewPane>(cx).next();
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let weak_workspace = workspace.weak_handle();
        let project = workspace.project().clone();
        let pane = cx.new(|cx| ReviewPane::new(weak_workspace, project, window, cx));
        workspace.add_item_to_center(Box::new(pane), window, cx);
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.register_addon(ExternalReviewAddon);
            editor
        });
        let subscriptions = vec![
            cx.observe_global_in::<ReviewSessions>(window, |this, window, cx| {
                this.bind_session(window, cx)
            }),
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                if event == &(EditorEvent::SelectionsChanged { local: true }) {
                    cx.emit(event.clone())
                }
            }),
        ];
        let mut this = Self {
            workspace,
            project,
            session: None,
            multibuffer,
            editor,
            focus_handle: cx.focus_handle(),
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        this.bind_session(window, cx);
        this
    }

    /// Follows the workspace's review, which is replaced when it starts again with
    /// another base.
    fn bind_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = session_for(self.workspace.entity_id(), cx);
        if session.as_ref().map(Entity::entity_id) == self.session.as_ref().map(Entity::entity_id)
        {
            return;
        }
        self.multibuffer.update(cx, |multibuffer, cx| multibuffer.clear(cx));
        self._session_subscription = session.as_ref().map(|session| {
            let action_log = session.read(cx).action_log.clone();
            cx.observe_in(&action_log, window, |this, _, window, cx| {
                this.update_excerpts(window, cx)
            })
        });
        let renderer = session.as_ref().map(|session| {
            Arc::new(ReviewHunkRenderer {
                session: session.downgrade(),
            }) as Arc<dyn editor::DiffHunkRenderer>
        });
        self.editor
            .update(cx, |editor, cx| editor.set_diff_hunk_renderer(renderer, cx));
        self.session = session;
        self.update_excerpts(window, cx);
    }

    fn update_excerpts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .session
            .as_ref()
            .map(|session| session.read(cx).pending_changes(cx))
            .unwrap_or_default();

        let mut buffers_to_remove = self
            .multibuffer
            .read(cx)
            .snapshot(cx)
            .excerpts()
            .map(|excerpt| excerpt.context.start.buffer_id)
            .collect::<HashSet<_>>();

        for change in pending {
            let buffer_id = change.buffer.read(cx).remote_id();
            buffers_to_remove.remove(&buffer_id);
            let path_key = PathKey::for_buffer(&change.buffer, cx);
            let was_empty = self.multibuffer.read(cx).is_empty();
            self.multibuffer.update(cx, |multibuffer, cx| {
                multibuffer.update_excerpts_for_path(
                    path_key,
                    change.buffer.clone(),
                    change.hunk_ranges,
                    multibuffer_context_lines(cx),
                    cx,
                );
                if multibuffer
                    .diff_for(buffer_id)
                    .is_none_or(|diff| diff.entity_id() != change.diff.entity_id())
                {
                    multibuffer.add_diff(change.diff, cx);
                }
            });
            if was_empty {
                self.select_first_hunk(window, cx);
            }
        }

        self.multibuffer.update(cx, |multibuffer, cx| {
            for buffer_id in buffers_to_remove {
                multibuffer.remove_excerpts_for_buffer(buffer_id, cx);
            }
        });

        let is_empty = self.multibuffer.read(cx).is_empty();
        if is_empty && self.editor.focus_handle(cx).contains_focused(window, cx) {
            self.focus_handle.focus(window, cx);
        } else if !is_empty && self.focus_handle.is_focused(window) {
            self.editor.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn select_first_hunk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let multibuffer = self.multibuffer.clone();
        self.editor.update(cx, |editor, cx| {
            let first_hunk = editor
                .diff_hunks_in_ranges(
                    &[editor::Anchor::Min..editor::Anchor::Max],
                    &multibuffer.read(cx).read(cx),
                )
                .next();
            if let Some(first_hunk) = first_hunk {
                let start = first_hunk.multi_buffer_range.start;
                editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([start..start]);
                });
            }
        });
    }

    fn counts(&self, cx: &App) -> PendingCounts {
        self.session
            .as_ref()
            .map(|session| session.read(cx).pending_counts(cx))
            .unwrap_or_default()
    }

    fn review_all(&mut self, kind: ReviewKind, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            review_ranges(
                editor,
                &session,
                kind,
                vec![editor::Anchor::Min..editor::Anchor::Max],
                cx,
            );
        });
    }

    fn render_header(&self, counts: PendingCounts, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.editor.focus_handle(cx);
        let has_changes = counts.hunks > 0;
        h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background)
            .child(Icon::new(IconName::Diff).size(IconSize::Small).color(Color::Muted))
            .child(Label::new("Changes to review").size(LabelSize::Small))
            .when(has_changes, |header| {
                header
                    .child(Divider::vertical())
                    .child(
                        Label::new(format!(
                            "{} · {}",
                            counts.files_label(),
                            counts.hunks_label()
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
            })
            .child(div().flex_1())
            .when(has_changes, |header| {
                header
                    .child(
                        Button::new("external-review-reject-all", "Reject All")
                            .label_size(LabelSize::Small)
                            .key_binding(
                                KeyBinding::for_action_in(&RejectAll, &focus_handle, cx)
                                    .map(|binding| binding.size(rems_from_px(12_f32))),
                            )
                            .tooltip(Tooltip::text("Restore every file to the review's base"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.review_all(ReviewKind::Reject, cx)
                            })),
                    )
                    .child(
                        Button::new("external-review-keep-all", "Keep All")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .key_binding(
                                KeyBinding::for_action_in(&KeepAll, &focus_handle, cx)
                                    .map(|binding| binding.size(rems_from_px(12_f32))),
                            )
                            .tooltip(Tooltip::text("Keep every change"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.review_all(ReviewKind::Keep, cx)
                            })),
                    )
            })
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, detail) = if self.session.is_some() {
            (
                "No changes to review",
                "Changes made outside Zed since the review started show up here.",
            )
        } else {
            ("Review is off", "Start it from bb or with external review: toggle.")
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(cx.theme().colors().editor_background)
            .child(
                Icon::new(if self.session.is_some() {
                    IconName::Check
                } else {
                    IconName::Diff
                })
                .size(IconSize::Medium)
                .color(Color::Muted),
            )
            .child(Label::new(title))
            .child(Label::new(detail).size(LabelSize::Small).color(Color::Muted))
    }
}

impl Render for ReviewPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.multibuffer.read(cx).is_empty();
        let counts = self.counts(cx);
        v_flex()
            .track_focus(&self.focus_handle)
            .key_context("ExternalReviewPane")
            .size_full()
            .child(self.render_header(counts, cx))
            .child(if is_empty {
                self.render_empty_state(cx).into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.editor.clone())
                    .into_any_element()
            })
    }
}

impl EventEmitter<EditorEvent> for ReviewPane {}

impl Focusable for ReviewPane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.multibuffer.read(cx).is_empty() {
            self.focus_handle.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Item for ReviewPane {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.counts(cx).files {
            0 => "Review".into(),
            files => format!("Review ({files})").into(),
        }
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .when(!params.selected, |label| label.color(Color::Muted))
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some("Changes made outside Zed".into())
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        Task::ready(Some(cx.new(|cx| {
            Self::new(self.workspace.clone(), self.project.clone(), window, cx)
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.multibuffer.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(options, project, window, cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.reload(project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

/// Status bar button that counts the files left to review and opens the Review tab.
pub struct ReviewIndicator {
    workspace_id: EntityId,
    session: Option<EntityId>,
    _session_subscription: Option<Subscription>,
    _global_subscription: Subscription,
}

impl ReviewIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            workspace_id: workspace.weak_handle().entity_id(),
            session: None,
            _session_subscription: None,
            _global_subscription: cx.observe_global::<ReviewSessions>(|this, cx| {
                this.bind_session(cx)
            }),
        };
        this.bind_session(cx);
        this
    }

    fn bind_session(&mut self, cx: &mut Context<Self>) {
        let session = session_for(self.workspace_id, cx);
        if session.as_ref().map(Entity::entity_id) == self.session {
            return;
        }
        self.session = session.as_ref().map(Entity::entity_id);
        self._session_subscription = session.map(|session| {
            let action_log = session.read(cx).action_log.clone();
            cx.observe(&action_log, |_, _, cx| cx.notify())
        });
        cx.notify();
    }
}

impl Render for ReviewIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = session_for(self.workspace_id, cx) else {
            return gpui::Empty.into_any_element();
        };
        let counts = session.read(cx).pending_counts(cx);
        let (label, icon, color) = if counts.files == 0 {
            ("Reviewed".to_string(), IconName::Check, Color::Muted)
        } else {
            (
                format!("Review {}", counts.files_label()),
                IconName::Diff,
                Color::Default,
            )
        };
        Button::new("external-review-indicator", label)
            .label_size(LabelSize::Small)
            .color(color)
            .start_icon(Icon::new(icon).size(IconSize::XSmall).color(color))
            .tooltip(Tooltip::for_action_title("Open Changes to Review", &OpenReview))
            .on_click(|_, window, cx| window.dispatch_action(OpenReview.boxed_clone(), cx))
            .into_any_element()
    }
}

impl StatusItemView for ReviewIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    /// Shown only while a review is running, which the user starts on purpose.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

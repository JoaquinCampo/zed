//! Review changes that other programs make to a project's files.
//!
//! While a review is active, the git repository's state at the start of the review
//! is the base. Every file that differs from it is shown with its hunks expanded,
//! and each hunk can be kept or rejected, the same way agent edits are reviewed.
//! Edits typed in Zed are not part of the review.
//!
//! The Review tab lists every change still waiting for review, and a status bar
//! item counts them. While a review runs, its counts are also written to
//! `zed-review.json` in the repository's git directory, for other programs to show.

mod review_pane;
mod review_panel;

pub use review_pane::{PendingCounts, ReviewIndicator, ReviewPane};
pub use review_panel::{ReviewPanel, show as show_panel};

use action_log::ActionLog;
use anyhow::{Context as _, Result};
use buffer_diff::{BufferDiff, DiffHunkStatus};
use collections::HashMap;
use editor::{DiffHunkRenderer, Editor};
use gpui::{
    AnyElement, App, AppContext as _, Entity, EntityId, Focusable as _, Global, Pixels,
    Subscription, Task, TaskExt as _, WeakEntity, Window, actions,
};
use language::{Buffer, OffsetRangeExt as _, Point};
use project::{Project, buffer_store::BufferStoreEvent};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use text::{LineEnding, Rope};
use ui::{KeyBinding, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, dock::Panel as _};

actions!(
    external_review,
    [
        /// Starts or stops reviewing changes that other programs make to the project.
        Toggle,
        /// Opens the panel with every change waiting for review.
        OpenReview,
        /// Opens every change waiting for review in a tab.
        OpenReviewTab,
        /// Opens or closes the review panel.
        ToggleFocus,
        /// Keeps the external changes in the selected hunks.
        Keep,
        /// Rejects the external changes in the selected hunks, restoring the base text.
        Reject,
        /// Keeps every external change in the current file, or in the Review tab.
        KeepAll,
        /// Rejects every external change in the current file, or in the Review tab.
        RejectAll,
        /// Undoes the last keep or reject in the current file (in the Review tab, the
        /// last one anywhere), showing its changes for review again. Falls back to the
        /// editor's undo when there is none to undo.
        Undo,
    ]
);

/// How long to wait after a file system event before comparing the repository with the base.
const SCAN_DELAY: Duration = Duration::from_millis(150);

/// How long to wait after a review change before writing the status file.
const STATUS_DELAY: Duration = Duration::from_millis(200);

/// Name of the file in the git directory that holds the running review's counts.
const STATUS_FILE_NAME: &str = "zed-review.json";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, _, cx| {
            let workspace_id = cx.entity_id();
            if is_active(workspace_id, cx) {
                stop(workspace_id, cx);
            } else {
                let project = workspace.project().clone();
                start(cx.weak_entity(), project, None, None, cx).detach_and_log_err(cx);
            }
        });
        workspace.register_action(|workspace, _: &OpenReview, window, cx| {
            review_panel::show(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenReviewTab, window, cx| {
            ReviewPane::deploy(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            // Opens the panel when it is hidden and closes it when it is showing,
            // wherever the focus is.
            let showing = workspace.panel::<ReviewPanel>(cx).is_some_and(|panel| {
                let position = panel.read(cx).position(window, cx);
                workspace
                    .dock_at_position(position)
                    .read(cx)
                    .visible_panel()
                    .is_some_and(|visible| visible.panel_id() == panel.entity_id())
            });
            if showing {
                workspace.close_panel::<ReviewPanel>(window, cx);
            } else {
                review_panel::show(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &Keep, _, cx| {
            review_active_editor(workspace, ReviewKind::Keep, false, cx);
        });
        workspace.register_action(|workspace, _: &Reject, _, cx| {
            review_active_editor(workspace, ReviewKind::Reject, false, cx);
        });
        workspace.register_action(|workspace, _: &KeepAll, _, cx| {
            review_active_editor(workspace, ReviewKind::Keep, true, cx);
        });
        workspace.register_action(|workspace, _: &RejectAll, _, cx| {
            review_active_editor(workspace, ReviewKind::Reject, true, cx);
        });
        workspace.register_action(|workspace, _: &Undo, _, cx| {
            let session = session_for(cx.entity_id(), cx);
            let editor = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .filter(|editor| editor.read(cx).addon::<ExternalReviewAddon>().is_some());
            let undone = match (session, editor) {
                (Some(session), Some(editor)) => {
                    let buffer = editor.read(cx).buffer().read(cx).as_singleton();
                    session.update(cx, |session, cx| match buffer {
                        Some(buffer) => session.undo_review(Some(&buffer), cx),
                        // The Review tab shows every file, so it undoes the latest action.
                        None => session.undo_review(None, cx),
                    })
                }
                _ => false,
            };
            if !undone {
                cx.propagate();
            }
        });
    })
    .detach();
}

#[derive(Default)]
struct ReviewSessions(HashMap<EntityId, Entity<ReviewSession>>);

impl Global for ReviewSessions {}

fn session_for(workspace_id: EntityId, cx: &App) -> Option<Entity<ReviewSession>> {
    cx.try_global::<ReviewSessions>()?
        .0
        .get(&workspace_id)
        .cloned()
}

pub fn is_active(workspace_id: EntityId, cx: &App) -> bool {
    session_for(workspace_id, cx).is_some()
}

/// Starts reviewing external changes in the git repository containing `path`, or the
/// repository of the workspace's first worktree. `base` is a git tree or commit to diff
/// against; without it, the repository's current state (including untracked files) is used.
/// Starting again with the same repository and base keeps the running review, with
/// what was kept and the undo history; anything else replaces it. Safe to call while
/// the workspace is being updated, since the workspace is only used once the review
/// is ready.
pub fn start(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    path: Option<PathBuf>,
    base: Option<String>,
    cx: &mut App,
) -> Task<Result<()>> {
    let path = path.or_else(|| {
        project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    });
    let Some(path) = path else {
        return Task::ready(Err(anyhow::anyhow!("no folder is open")));
    };
    let worktree_roots = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect::<Vec<_>>();
    let workspace_id = workspace.entity_id();
    cx.spawn(async move |cx| {
        let (repo_root, git_dir) = cx
            .background_spawn(async move {
                let repo_root = repository_root(&path, &worktree_roots).await?;
                let git_dir =
                    PathBuf::from(git_line(&repo_root, &["rev-parse", "--absolute-git-dir"]).await?);
                anyhow::Ok((repo_root, git_dir))
            })
            .await?;
        let running_base = cx.update(|cx| {
            session_for(workspace_id, cx)
                .map(|session| session.read(cx))
                .filter(|session| session.repo_root == repo_root)
                .map(|session| session.base.clone())
        });
        let base = match (base, running_base) {
            // Without a base, a running review of the same repository goes on.
            (None, Some(_)) => return Ok(()),
            (Some(base), running_base) => {
                let repo_root = repo_root.clone();
                let base = cx
                    .background_spawn(async move { resolve_tree(&repo_root, &base).await })
                    .await?;
                if running_base.as_ref() == Some(&base) {
                    return Ok(());
                }
                base
            }
            (None, None) => {
                let repo_root = repo_root.clone();
                cx.background_spawn(async move { snapshot_tree(&repo_root).await })
                    .await?
            }
        };
        let workspace = workspace.upgrade().context("workspace was closed")?;
        cx.update(|cx| {
            stop(workspace.entity_id(), cx);
            log::info!("external review started in {repo_root:?} against {base}");
            let session = cx.new(|cx| {
                ReviewSession::new(workspace.clone(), project, repo_root, git_dir, base, cx)
            });
            cx.default_global::<ReviewSessions>()
                .0
                .insert(workspace.entity_id(), session);
        });
        Ok(())
    })
}

pub fn stop(workspace_id: EntityId, cx: &mut App) {
    let session = cx
        .default_global::<ReviewSessions>()
        .0
        .remove(&workspace_id);
    if let Some(session) = session {
        session.update(cx, |session, cx| {
            session.clear_editors(cx);
            session.remove_status_file(cx);
        });
    }
}

/// One file with changes waiting for review.
struct PendingChange {
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    hunk_ranges: Vec<Range<Point>>,
}

struct ReviewSession {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo_root: PathBuf,
    git_dir: PathBuf,
    base: String,
    action_log: Entity<ActionLog>,
    reviewing_editors: HashMap<WeakEntity<Editor>, Entity<Buffer>>,
    /// Keeps and rejects that can be undone, oldest first. Each entry is one action,
    /// which can span several buffers.
    undo_stack: Vec<Vec<ReviewUndo>>,
    scan_task: Option<Task<()>>,
    scan_again: bool,
    status_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewSession {
    fn new(
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        repo_root: PathBuf,
        git_dir: PathBuf,
        base: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let action_log =
            cx.new(|_| ActionLog::new(project.clone()).with_disk_changes_as_agent_edits());
        let buffer_store = project.read(cx).buffer_store().clone();
        let subscriptions = vec![
            cx.observe(&action_log, |this, _, cx| {
                this.update_editors(cx);
                this.schedule_status_write(cx);
            }),
            cx.subscribe(&workspace, |this, _, event: &workspace::Event, cx| {
                if let workspace::Event::ItemAdded { .. } = event {
                    this.update_editors(cx);
                }
            }),
            cx.subscribe(&project, |this, project, event: &project::Event, cx| {
                if let project::Event::WorktreeUpdatedEntries(worktree_id, entries) = event {
                    let worktree = project.read(cx).worktree_for_id(*worktree_id, cx);
                    let relevant = entries.iter().any(|(path, _, _)| {
                        let in_git_dir = path
                            .components()
                            .next()
                            .is_some_and(|component| component == ".git");
                        let ignored = worktree.as_ref().is_some_and(|worktree| {
                            worktree
                                .read(cx)
                                .entry_for_path(path)
                                .is_some_and(|entry| entry.is_ignored)
                        });
                        !in_git_dir && !ignored
                    });
                    if relevant {
                        this.schedule_scan(cx);
                    }
                }
            }),
            // Buffers opened in Zed are tracked too, so that edits typed into them count
            // as the user's and are not shown as external changes after saving.
            cx.subscribe(&buffer_store, |this, _, event: &BufferStoreEvent, cx| {
                if let BufferStoreEvent::BufferAdded(buffer) = event {
                    this.track_buffer(buffer.clone(), cx);
                }
            }),
        ];

        let mut this = Self {
            workspace: workspace.downgrade(),
            project: project.clone(),
            repo_root,
            git_dir,
            base,
            action_log,
            reviewing_editors: HashMap::default(),
            undo_stack: Vec::new(),
            scan_task: None,
            scan_again: false,
            status_task: None,
            _subscriptions: subscriptions,
        };
        for buffer in project.read(cx).opened_buffers(cx) {
            this.track_buffer(buffer, cx);
        }
        this.schedule_scan(cx);
        this.schedule_status_write(cx);
        this
    }

    /// Files with changes waiting for review, sorted by path, with their hunks.
    fn pending_changes(&self, cx: &App) -> Vec<PendingChange> {
        let mut changes = self
            .action_log
            .read(cx)
            .changed_buffers(cx)
            .filter(|(buffer, _)| buffer.read(cx).file().is_some())
            .filter_map(|(buffer, diff)| {
                let snapshot = buffer.read(cx).snapshot();
                let hunk_ranges = diff
                    .read(cx)
                    .snapshot(cx)
                    .hunks_intersecting_range(
                        language::Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                        &snapshot,
                    )
                    .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>();
                (!hunk_ranges.is_empty()).then_some(PendingChange {
                    buffer,
                    diff,
                    hunk_ranges,
                })
            })
            .collect::<Vec<_>>();
        changes.sort_by_cached_key(|change| {
            change
                .buffer
                .read(cx)
                .file()
                .map(|file| file.path().clone())
        });
        changes
    }

    fn pending_counts(&self, cx: &App) -> PendingCounts {
        let changes = self.pending_changes(cx);
        PendingCounts {
            files: changes.len(),
            hunks: changes.iter().map(|change| change.hunk_ranges.len()).sum(),
        }
    }

    fn status_file(&self) -> PathBuf {
        self.git_dir.join(STATUS_FILE_NAME)
    }

    /// Writes the review's counts for other programs, such as bb, to show.
    fn schedule_status_write(&mut self, cx: &mut Context<Self>) {
        let status_file = self.status_file();
        self.status_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STATUS_DELAY).await;
            let Ok(status) = this.read_with(cx, |this, cx| {
                let changes = this.pending_changes(cx);
                let paths = changes
                    .iter()
                    .filter_map(|change| this.repo_path_for_buffer(&change.buffer, cx))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "repo": this.repo_root,
                    "base": this.base,
                    "files": changes.len(),
                    "hunks": changes.iter().map(|change| change.hunk_ranges.len()).sum::<usize>(),
                    "paths": paths,
                })
            }) else {
                return;
            };
            cx.background_spawn(async move {
                let temp_file = status_file.with_extension("json.tmp");
                std::fs::write(&temp_file, status.to_string())?;
                std::fs::rename(&temp_file, &status_file)?;
                anyhow::Ok(())
            })
            .await
            .log_err();
        }));
    }

    fn remove_status_file(&mut self, cx: &mut Context<Self>) {
        self.status_task = None;
        let status_file = self.status_file();
        cx.background_spawn(async move { std::fs::remove_file(status_file).ok() })
            .detach();
    }

    /// Compares the repository with the base and tracks every changed file.
    fn schedule_scan(&mut self, cx: &mut Context<Self>) {
        if self.scan_task.is_some() {
            self.scan_again = true;
            return;
        }
        let repo_root = self.repo_root.clone();
        let base = self.base.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCAN_DELAY).await;
            let changes = cx
                .background_spawn(async move { changed_paths(&repo_root, &base).await })
                .await;
            this.update(cx, |this, cx| {
                this.scan_task = None;
                match changes {
                    Ok(changes) => {
                        for repo_path in changes {
                            this.track_repo_path(repo_path, cx);
                        }
                    }
                    Err(error) => log::error!("external review scan failed: {error:#}"),
                }
                if std::mem::take(&mut this.scan_again) {
                    this.schedule_scan(cx);
                }
            })
            .ok();
        }));
    }

    fn track_repo_path(&mut self, repo_path: String, cx: &mut Context<Self>) {
        let abs_path = self.repo_root.join(&repo_path);
        let Some(project_path) = self.project.read(cx).find_project_path(&abs_path, cx) else {
            return;
        };
        let open_buffer = self
            .project
            .update(cx, |project, cx| project.open_buffer(project_path, cx));
        cx.spawn(async move |this, cx| {
            let buffer = open_buffer.await?;
            this.update(cx, |this, cx| this.track_buffer(buffer, cx))
        })
        .detach_and_log_err(cx);
    }

    fn track_buffer(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        if self.action_log.read(cx).is_tracking(&buffer) {
            return;
        }
        let Some(repo_path) = self.repo_path_for_buffer(&buffer, cx) else {
            return;
        };
        let repo_root = self.repo_root.clone();
        let base = self.base.clone();
        cx.spawn(async move |this, cx| {
            let base_text = cx
                .background_spawn(async move { base_text(&repo_root, &base, &repo_path).await })
                .await?;
            this.update(cx, |this, cx| {
                this.action_log.update(cx, |action_log, cx| {
                    action_log.track_disk_change(buffer, base_text.map(Rope::from), cx)
                })
            })
        })
        .detach_and_log_err(cx);
    }

    fn repo_path_for_buffer(&self, buffer: &Entity<Buffer>, cx: &App) -> Option<String> {
        let abs_path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
        let repo_path = abs_path.strip_prefix(&self.repo_root).ok()?;
        let repo_path = repo_path.to_str()?;
        (!repo_path.is_empty()).then(|| repo_path.replace('\\', "/"))
    }

    /// Shows the review diff in every editor whose buffer has external changes, and
    /// restores the regular git diff in editors that no longer have any.
    fn update_editors(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let changed_buffers = self
            .action_log
            .read(cx)
            .changed_buffers(cx)
            .collect::<HashMap<_, _>>();
        let editors = workspace
            .read(cx)
            .items_of_type::<Editor>(cx)
            .collect::<Vec<_>>();

        let renderer: Arc<dyn DiffHunkRenderer> = Arc::new(ReviewHunkRenderer {
            session: cx.weak_entity(),
        });

        let mut still_reviewing = HashMap::default();
        for editor in editors {
            let buffer = {
                let editor = editor.read(cx);
                if !editor.mode().is_full() {
                    continue;
                }
                editor.buffer().read(cx).as_singleton()
            };
            let Some(buffer) = buffer else {
                continue;
            };
            // A file whose changes were all reviewed stays in review while its last
            // keep or reject can still be undone.
            let diff = changed_buffers.get(&buffer).cloned().or_else(|| {
                self.has_undo_for(&buffer)
                    .then(|| self.action_log.read(cx).tracked_diff(&buffer))
                    .flatten()
            });
            let Some(diff) = diff else {
                continue;
            };
            let weak_editor = editor.downgrade();
            let was_reviewing = self.reviewing_editors.remove(&weak_editor).is_some();
            editor.update(cx, |editor, cx| {
                editor.buffer().update(cx, |multibuffer, cx| {
                    multibuffer.add_diff(diff.clone(), cx);
                });
                if !was_reviewing {
                    editor.set_diff_hunk_renderer(Some(renderer.clone()), cx);
                    editor.set_expand_all_diff_hunks(cx);
                    editor.register_addon(ExternalReviewAddon);
                }
            });
            still_reviewing.insert(weak_editor, buffer);
        }

        let finished = std::mem::replace(&mut self.reviewing_editors, still_reviewing);
        for editor in finished.into_keys() {
            reset_editor(&editor, cx);
        }
    }

    fn has_undo_for(&self, buffer: &Entity<Buffer>) -> bool {
        self.undo_stack
            .iter()
            .flatten()
            .any(|entry| &entry.buffer == buffer)
    }

    fn push_undo(&mut self, entries: Vec<ReviewUndo>) {
        const MAX_UNDO_ENTRIES: usize = 200;
        if entries.is_empty() {
            return;
        }
        self.undo_stack.push(entries);
        if self.undo_stack.len() > MAX_UNDO_ENTRIES {
            self.undo_stack.remove(0);
        }
    }

    /// Undoes the latest keep or reject that touched `buffer`, or the latest of all
    /// without a buffer, if its buffers still have the text that action left. Returns
    /// whether anything was undone.
    fn undo_review(&mut self, buffer: Option<&Entity<Buffer>>, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.undo_stack.iter().rposition(|entries| {
            buffer.is_none_or(|buffer| entries.iter().any(|entry| &entry.buffer == buffer))
        }) else {
            return false;
        };
        let unchanged_since = self.undo_stack[ix]
            .iter()
            .all(|entry| entry.buffer.read(cx).text() == entry.text_after);
        if !unchanged_since {
            return false;
        }
        for entry in self.undo_stack.remove(ix) {
            let text_changed = entry.text_before != entry.text_after;
            if text_changed {
                entry.buffer.update(cx, |buffer, cx| {
                    let edits = language::text_diff(&entry.text_after, &entry.text_before);
                    buffer.edit(edits, None, cx);
                });
            }
            self.action_log.update(cx, |action_log, cx| {
                if action_log.is_tracking(&entry.buffer) {
                    action_log.restore_review_base(&entry.buffer, entry.base, cx);
                } else {
                    // Rejecting a created file deleted it and ended its tracking.
                    action_log.track_disk_change(entry.buffer.clone(), None, cx);
                }
            });
            if text_changed {
                self.project
                    .update(cx, |project, cx| project.save_buffer(entry.buffer, cx))
                    .detach_and_log_err(cx);
            }
        }
        // The action log notifies its observers, which brings the editors back into review.
        true
    }

    fn clear_editors(&mut self, cx: &mut Context<Self>) {
        for editor in std::mem::take(&mut self.reviewing_editors).into_keys() {
            reset_editor(&editor, cx);
        }
    }
}

fn reset_editor(editor: &WeakEntity<Editor>, cx: &mut App) {
    editor
        .update(cx, |editor, cx| {
            editor.set_diff_hunk_renderer(None, cx);
            editor.unregister_addon::<ExternalReviewAddon>();
        })
        .ok();
}

/// What a keep or reject changed in one buffer, to undo it.
struct ReviewUndo {
    buffer: Entity<Buffer>,
    base: Rope,
    text_before: String,
    text_after: String,
}

/// Adds the `external_review` key context, so that review key bindings only apply to
/// editors with external changes.
struct ExternalReviewAddon;

impl editor::Addon for ExternalReviewAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn extend_key_context(&self, key_context: &mut gpui::KeyContext, _: &App) {
        key_context.add("external_review");
    }
}

#[derive(Clone, Copy)]
enum ReviewKind {
    Keep,
    Reject,
}

fn review_active_editor(
    workspace: &mut Workspace,
    kind: ReviewKind,
    whole_file: bool,
    cx: &mut Context<Workspace>,
) {
    let session = session_for(cx.entity_id(), cx);
    let editor = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx));
    let (Some(session), Some(editor)) = (session, editor) else {
        cx.propagate();
        return;
    };
    if editor.read(cx).addon::<ExternalReviewAddon>().is_none() {
        cx.propagate();
        return;
    }
    let reviewed = editor.update(cx, |editor, cx| {
        let ranges = if whole_file {
            vec![editor::Anchor::Min..editor::Anchor::Max]
        } else {
            editor.selections.disjoint_anchor_ranges().collect()
        };
        review_ranges(editor, &session, kind, ranges, cx)
    });
    // Away from a hunk, the key does what it normally does (cmd-n creates a file).
    if !reviewed {
        cx.propagate();
    }
}

fn review_ranges(
    editor: &mut Editor,
    session: &Entity<ReviewSession>,
    kind: ReviewKind,
    ranges: Vec<Range<editor::Anchor>>,
    cx: &mut Context<Editor>,
) -> bool {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let hunks = editor
        .diff_hunks_in_ranges(&ranges, &snapshot)
        .collect::<Vec<_>>();
    if hunks.is_empty() {
        return false;
    }
    let multibuffer = editor.buffer().clone();

    let mut ranges_by_buffer = HashMap::<Entity<Buffer>, Vec<_>>::default();
    for hunk in hunks {
        if let Some(buffer) = multibuffer.read(cx).buffer(hunk.buffer_id) {
            ranges_by_buffer
                .entry(buffer)
                .or_default()
                .push(hunk.buffer_range);
        }
    }

    let action_log = session.read(cx).action_log.clone();
    let mut undo_entries = Vec::new();
    for (buffer, ranges) in ranges_by_buffer {
        let Some(base) = action_log.read(cx).review_base(&buffer) else {
            continue;
        };
        let text_before = buffer.read(cx).text();
        action_log.update(cx, |action_log, cx| match kind {
            ReviewKind::Keep => {
                for range in ranges {
                    action_log.keep_edits_in_range(buffer.clone(), range, None, cx);
                }
            }
            ReviewKind::Reject => {
                let (task, _undo) =
                    action_log.reject_edits_in_ranges(buffer.clone(), ranges, None, cx);
                task.detach_and_log_err(cx);
            }
        });
        let text_after = buffer.read(cx).text();
        undo_entries.push(ReviewUndo {
            buffer,
            base,
            text_before,
            text_after,
        });
    }
    session.update(cx, |session, _| session.push_undo(undo_entries));
    true
}

struct ReviewHunkRenderer {
    session: WeakEntity<ReviewSession>,
}

impl DiffHunkRenderer for ReviewHunkRenderer {
    /// Draws a small keep chip and a reject chip at the right edge of the hunk's first
    /// line, labeled with their key bindings.
    fn render_hunk_controls(
        &self,
        row: u32,
        _status: &DiffHunkStatus,
        hunk_range: Range<editor::Anchor>,
        _is_created_file: bool,
        line_height: Pixels,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let focus_handle = editor.read(cx).focus_handle(cx);
        let status_colors = cx.theme().status();
        let chip = |id: &'static str, kind: ReviewKind, background: gpui::Hsla, cx: &App| {
            let editor = editor.clone();
            let session = self.session.clone();
            let hunk_start = hunk_range.start;
            let (key_binding, tooltip): (KeyBinding, &'static str) = match kind {
                ReviewKind::Keep => (KeyBinding::for_action_in(&Keep, &focus_handle, cx), "Keep"),
                ReviewKind::Reject => (
                    KeyBinding::for_action_in(&Reject, &focus_handle, cx),
                    "Reject",
                ),
            };
            div()
                .id((id, row as u64))
                .h_full()
                .px_1()
                .flex()
                .items_center()
                .rounded_sm()
                .bg(background)
                .cursor_pointer()
                .hover(|style| style.opacity(0.85))
                .tooltip(Tooltip::text(tooltip))
                .child(
                    key_binding
                        .size(rems_from_px(11_f32))
                        .color(Color::Custom(gpui::white())),
                )
                .on_click(move |_, _, cx| {
                    let Some(session) = session.upgrade() else {
                        return;
                    };
                    editor.update(cx, |editor, cx| {
                        review_ranges(editor, &session, kind, vec![hunk_start..hunk_start], cx);
                    });
                })
        };

        h_flex()
            .h(line_height)
            .py_0p5()
            .gap_0p5()
            .block_mouse_except_scroll()
            .child(chip(
                "external-review-keep",
                ReviewKind::Keep,
                status_colors.created.opacity(0.9),
                cx,
            ))
            .child(chip(
                "external-review-reject",
                ReviewKind::Reject,
                status_colors.deleted.opacity(0.9),
                cx,
            ))
            .into_any_element()
    }

    fn render_hunk_as_staged(&self, _status: &DiffHunkStatus, _cx: &App) -> bool {
        false
    }

    fn show_controls_on_every_hunk(&self) -> bool {
        true
    }
}

async fn git(repo: &Path, args: &[&str], index_file: Option<&Path>) -> Result<Vec<u8>> {
    let mut command = util::command::new_command("git");
    command.current_dir(repo).args(args);
    if let Some(index_file) = index_file {
        command.env("GIT_INDEX_FILE", index_file);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

async fn git_line(repo: &Path, args: &[&str]) -> Result<String> {
    let stdout = git(repo, args, None).await?;
    Ok(String::from_utf8(stdout)?.trim().to_string())
}

/// Returns the root of the repository containing `path`, written relative to one of the
/// project's worktree roots, so it can be compared with the paths of the project's files
/// even when symlinks lead to the same directory (`/tmp` is `/private/tmp` on macOS).
async fn repository_root(path: &Path, worktree_roots: &[PathBuf]) -> Result<PathBuf> {
    let directory = if path.is_dir() {
        path
    } else {
        path.parent().context("path has no parent directory")?
    };
    let top_level = PathBuf::from(git_line(directory, &["rev-parse", "--show-toplevel"]).await?);
    let Ok(real_top_level) = top_level.canonicalize() else {
        return Ok(top_level);
    };
    for worktree_root in worktree_roots {
        let Ok(real_worktree_root) = worktree_root.canonicalize() else {
            continue;
        };
        // The repository is inside the worktree.
        if let Ok(inside) = real_top_level.strip_prefix(&real_worktree_root) {
            return Ok(worktree_root.join(inside));
        }
        // The worktree is a folder inside the repository.
        if let Ok(inside) = real_worktree_root.strip_prefix(&real_top_level) {
            let depth = inside.components().count();
            if let Some(root) = worktree_root.ancestors().nth(depth) {
                return Ok(root.to_path_buf());
            }
        }
    }
    Ok(top_level)
}

async fn resolve_tree(repo_root: &Path, base: &str) -> Result<String> {
    git_line(
        repo_root,
        &["rev-parse", "--verify", &format!("{base}^{{tree}}")],
    )
    .await
}

/// Writes the working tree, including untracked files that are not ignored, as a git
/// tree object. A temporary index is used, so the repository's index is left untouched.
async fn snapshot_tree(repo_root: &Path) -> Result<String> {
    static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);
    let index_file = std::env::temp_dir().join(format!(
        "zed-external-review-{}-{}.index",
        std::process::id(),
        NEXT_INDEX.fetch_add(1, Ordering::Relaxed)
    ));
    // Starting from a copy of the real index keeps its file stat cache, so unchanged
    // files are not hashed again.
    let real_index =
        repo_root.join(git_line(repo_root, &["rev-parse", "--git-path", "index"]).await?);
    if real_index.exists() {
        std::fs::copy(&real_index, &index_file)?;
    }
    let tree = async {
        git(repo_root, &["add", "--all"], Some(&index_file)).await?;
        let tree = git(repo_root, &["write-tree"], Some(&index_file)).await?;
        anyhow::Ok(String::from_utf8(tree)?.trim().to_string())
    }
    .await;
    std::fs::remove_file(&index_file).ok();
    tree
}

/// Paths, relative to the repository root, of files that were added or modified since `base`.
async fn changed_paths(repo_root: &Path, base: &str) -> Result<Vec<String>> {
    let current = snapshot_tree(repo_root).await?;
    if current == base {
        return Ok(Vec::new());
    }
    let output = git(
        repo_root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            base,
            &current,
        ],
        None,
    )
    .await?;
    let output = String::from_utf8(output)?;
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if status != "D" {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

/// The text of `repo_path` in `base`, or `None` if the file did not exist there.
async fn base_text(repo_root: &Path, base: &str, repo_path: &str) -> Result<Option<String>> {
    let object = format!("{base}:{repo_path}");
    if git(repo_root, &["cat-file", "-e", &object], None)
        .await
        .is_err()
    {
        return Ok(None);
    }
    let blob = git(repo_root, &["cat-file", "blob", &object], None).await?;
    let mut text = String::from_utf8(blob).context("file in the review base is not UTF-8")?;
    LineEnding::normalize(&mut text);
    Ok(Some(text))
}

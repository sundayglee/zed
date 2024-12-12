use auto_update::{AutoUpdateStatus, AutoUpdater, DismissErrorMessage};
use editor::Editor;
use extension_host::ExtensionStore;
use futures::StreamExt;
use gpui::{
    actions, percentage, Animation, AnimationExt as _, AppContext, AppContext, CursorStyle,
    EventEmitter, InteractiveElement as _, Model, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement, Styled, Transformation, View, VisualContext as _,
};
use language::{LanguageRegistry, LanguageServerBinaryStatus, LanguageServerId};
use lsp::LanguageServerName;
use project::{EnvironmentErrorMessage, LanguageServerProgress, Project, WorktreeId};
use smallvec::SmallVec;
use std::{cmp::Reverse, fmt::Write, sync::Arc, time::Duration};
use ui::{prelude::*, ButtonLike, ContextMenu, PopoverMenu, PopoverMenuHandle, Tooltip};
use util::truncate_and_trailoff;
use workspace::{item::ItemHandle, StatusItemView, Workspace};

actions!(activity_indicator, [ShowErrorMessage]);

pub enum Event {
    ShowError {
        lsp_name: LanguageServerName,
        error: String,
    },
}

pub struct ActivityIndicator {
    statuses: Vec<LspStatus>,
    project: Model<Project>,
    auto_updater: Option<Model<AutoUpdater>>,
    context_menu_handle: PopoverMenuHandle<ContextMenu>,
}

struct LspStatus {
    name: LanguageServerName,
    status: LanguageServerBinaryStatus,
}

struct PendingWork<'a> {
    language_server_id: LanguageServerId,
    progress_token: &'a str,
    progress: &'a LanguageServerProgress,
}

struct Content {
    icon: Option<gpui::AnyElement>,
    message: String,
    on_click:
        Option<Arc<dyn Fn(&mut ActivityIndicator, &Model<ActivityIndicator>, &mut AppContext)>>,
}

impl ActivityIndicator {
    pub fn new(
        workspace: &mut Workspace,
        languages: Arc<LanguageRegistry>,
        model: &Model<Workspace>,
        cx: &mut AppContext,
    ) -> Model<ActivityIndicator> {
        let project = workspace.project().clone();
        let auto_updater = AutoUpdater::get(cx);
        let this = cx.new_model(|model: &Model<Self>, cx: &mut AppContext| {
            let mut status_events = languages.language_server_binary_statuses();
            model
                .spawn(cx, |this, mut cx| async move {
                    while let Some((name, status)) = status_events.next().await {
                        this.update(&mut cx, |this, model, cx| {
                            this.statuses.retain(|s| s.name != name);
                            this.statuses.push(LspStatus { name, status });
                            model.notify(cx);
                        })?;
                    }
                    anyhow::Ok(())
                })
                .detach();
            cx.observe(&project, |_, _, cx| model.notify(cx)).detach();

            if let Some(auto_updater) = auto_updater.as_ref() {
                cx.observe(auto_updater, |_, _, cx| model.notify(cx))
                    .detach();
            }

            Self {
                statuses: Default::default(),
                project: project.clone(),
                auto_updater,
                context_menu_handle: Default::default(),
            }
        });

        cx.subscribe(&this, move |_, _, event, cx| match event {
            Event::ShowError { lsp_name, error } => {
                let create_buffer =
                    project.update(cx, |project, model, cx| project.create_buffer(model, cx));
                let project = project.clone();
                let error = error.clone();
                let lsp_name = lsp_name.clone();
                cx.spawn(|workspace, mut cx| async move {
                    let buffer = create_buffer.await?;
                    buffer.update(&mut cx, |buffer, cx| {
                        buffer.edit(
                            [(
                                0..0,
                                format!("Language server error: {}\n\n{}", lsp_name, error),
                            )],
                            None,
                            cx,
                        );
                        buffer.set_capability(language::Capability::ReadOnly, cx);
                    })?;
                    workspace.update(&mut cx, |workspace, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(cx.new_model(|model, cx| {
                                Editor::for_buffer(buffer, Some(project.clone()), model, cx)
                            })),
                            None,
                            true,
                            cx,
                        );
                    })?;

                    anyhow::Ok(())
                })
                .detach();
            }
        })
        .detach();
        this
    }

    fn show_error_message(
        &mut self,
        _: &ShowErrorMessage,
        model: &Model<Self>,
        window: &mut Window,
        cx: &mut AppContext,
    ) {
        self.statuses.retain(|status| {
            if let LanguageServerBinaryStatus::Failed { error } = &status.status {
                model.emit(
                    cx,
                    Event::ShowError {
                        lsp_name: status.name.clone(),
                        error: error.clone(),
                    },
                );
                false
            } else {
                true
            }
        });

        model.notify(cx);
    }

    fn dismiss_error_message(
        &mut self,
        _: &DismissErrorMessage,
        model: &Model<Self>,
        window: &mut Window,
        cx: &mut AppContext,
    ) {
        if let Some(updater) = &self.auto_updater {
            updater.update(cx, |updater, model, cx| {
                updater.dismiss_error(model, cx);
            });
        }
        model.notify(cx);
    }

    fn pending_language_server_work<'a>(
        &self,
        cx: &'a AppContext,
    ) -> impl Iterator<Item = PendingWork<'a>> {
        self.project
            .read(cx)
            .language_server_statuses(cx)
            .rev()
            .filter_map(|(server_id, status)| {
                if status.pending_work.is_empty() {
                    None
                } else {
                    let mut pending_work = status
                        .pending_work
                        .iter()
                        .map(|(token, progress)| PendingWork {
                            language_server_id: server_id,
                            progress_token: token.as_str(),
                            progress,
                        })
                        .collect::<SmallVec<[_; 4]>>();
                    pending_work.sort_by_key(|work| Reverse(work.progress.last_update_at));
                    Some(pending_work)
                }
            })
            .flatten()
    }

    fn pending_environment_errors<'a>(
        &'a self,
        cx: &'a AppContext,
    ) -> impl Iterator<Item = (&'a WorktreeId, &'a EnvironmentErrorMessage)> {
        self.project.read(cx).shell_environment_errors(cx)
    }

    fn content_to_render(&mut self, model: &Model<Self>, cx: &mut AppContext) -> Option<Content> {
        // Show if any direnv calls failed
        if let Some((&worktree_id, error)) = self.pending_environment_errors(cx).next() {
            return Some(Content {
                icon: Some(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .into_any_element(),
                ),
                message: error.0.clone(),
                on_click: Some(Arc::new(move |this, cx| {
                    this.project.update(cx, |project, model, cx| {
                        project.remove_environment_error(cx, worktree_id);
                    });
                    cx.dispatch_action(Box::new(workspace::OpenLog));
                })),
            });
        }
        // Show any language server has pending activity.
        let mut pending_work = self.pending_language_server_work(cx);
        if let Some(PendingWork {
            progress_token,
            progress,
            ..
        }) = pending_work.next()
        {
            let mut message = progress
                .title
                .as_deref()
                .unwrap_or(progress_token)
                .to_string();

            if let Some(percentage) = progress.percentage {
                write!(&mut message, " ({}%)", percentage).unwrap();
            }

            if let Some(progress_message) = progress.message.as_ref() {
                message.push_str(": ");
                message.push_str(progress_message);
            }

            let additional_work_count = pending_work.count();
            if additional_work_count > 0 {
                write!(&mut message, " + {} more", additional_work_count).unwrap();
            }

            return Some(Content {
                icon: Some(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .with_animation(
                            "arrow-circle",
                            Animation::new(Duration::from_secs(2)).repeat(),
                            |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                        )
                        .into_any_element(),
                ),
                message,
                on_click: Some(Arc::new(Self::toggle_language_server_work_context_menu)),
            });
        }

        // Show any language server installation info.
        let mut downloading = SmallVec::<[_; 3]>::new();
        let mut checking_for_update = SmallVec::<[_; 3]>::new();
        let mut failed = SmallVec::<[_; 3]>::new();
        for status in &self.statuses {
            match status.status {
                LanguageServerBinaryStatus::CheckingForUpdate => {
                    checking_for_update.push(status.name.clone())
                }
                LanguageServerBinaryStatus::Downloading => downloading.push(status.name.clone()),
                LanguageServerBinaryStatus::Failed { .. } => failed.push(status.name.clone()),
                LanguageServerBinaryStatus::None => {}
            }
        }

        if !downloading.is_empty() {
            return Some(Content {
                icon: Some(
                    Icon::new(IconName::Download)
                        .size(IconSize::Small)
                        .into_any_element(),
                ),
                message: format!(
                    "Downloading {}...",
                    downloading.iter().map(|name| name.0.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    )
                ),
                on_click: Some(Arc::new(move |this, cx| {
                    this.statuses
                        .retain(|status| !downloading.contains(&status.name));
                    this.dismiss_error_message(&DismissErrorMessage, cx)
                })),
            });
        }

        if !checking_for_update.is_empty() {
            return Some(Content {
                icon: Some(
                    Icon::new(IconName::Download)
                        .size(IconSize::Small)
                        .into_any_element(),
                ),
                message: format!(
                    "Checking for updates to {}...",
                    checking_for_update.iter().map(|name| name.0.as_ref()).fold(
                        String::new(),
                        |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }
                    ),
                ),
                on_click: Some(Arc::new(move |this, cx| {
                    this.statuses
                        .retain(|status| !checking_for_update.contains(&status.name));
                    this.dismiss_error_message(&DismissErrorMessage, cx)
                })),
            });
        }

        if !failed.is_empty() {
            return Some(Content {
                icon: Some(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .into_any_element(),
                ),
                message: format!(
                    "Failed to run {}. Click to show error.",
                    failed
                        .iter()
                        .map(|name| name.0.as_ref())
                        .fold(String::new(), |mut acc, s| {
                            if !acc.is_empty() {
                                acc.push_str(", ");
                            }
                            acc.push_str(s);
                            acc
                        }),
                ),
                on_click: Some(Arc::new(|this, cx| {
                    this.show_error_message(&Default::default(), cx)
                })),
            });
        }

        // Show any formatting failure
        if let Some(failure) = self.project.read(cx).last_formatting_failure(cx) {
            return Some(Content {
                icon: Some(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .into_any_element(),
                ),
                message: format!("Formatting failed: {}. Click to see logs.", failure),
                on_click: Some(Arc::new(|indicator, cx| {
                    indicator.project.update(cx, |project, model, cx| {
                        project.reset_last_formatting_failure(cx);
                    });
                    cx.dispatch_action(Box::new(workspace::OpenLog));
                })),
            });
        }

        // Show any application auto-update info.
        if let Some(updater) = &self.auto_updater {
            return match &updater.read(cx).status() {
                AutoUpdateStatus::Checking => Some(Content {
                    icon: Some(
                        Icon::new(IconName::Download)
                            .size(IconSize::Small)
                            .into_any_element(),
                    ),
                    message: "Checking for Zed updates…".to_string(),
                    on_click: Some(Arc::new(|this, cx| {
                        this.dismiss_error_message(&DismissErrorMessage, cx)
                    })),
                }),
                AutoUpdateStatus::Downloading => Some(Content {
                    icon: Some(
                        Icon::new(IconName::Download)
                            .size(IconSize::Small)
                            .into_any_element(),
                    ),
                    message: "Downloading Zed update…".to_string(),
                    on_click: Some(Arc::new(|this, cx| {
                        this.dismiss_error_message(&DismissErrorMessage, cx)
                    })),
                }),
                AutoUpdateStatus::Installing => Some(Content {
                    icon: Some(
                        Icon::new(IconName::Download)
                            .size(IconSize::Small)
                            .into_any_element(),
                    ),
                    message: "Installing Zed update…".to_string(),
                    on_click: Some(Arc::new(|this, cx| {
                        this.dismiss_error_message(&DismissErrorMessage, cx)
                    })),
                }),
                AutoUpdateStatus::Updated { binary_path } => Some(Content {
                    icon: None,
                    message: "Click to restart and update Zed".to_string(),
                    on_click: Some(Arc::new({
                        let reload = workspace::Reload {
                            binary_path: Some(binary_path.clone()),
                        };
                        move |_, cx| workspace::reload(&reload, cx)
                    })),
                }),
                AutoUpdateStatus::Errored => Some(Content {
                    icon: Some(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .into_any_element(),
                    ),
                    message: "Auto update failed".to_string(),
                    on_click: Some(Arc::new(|this, cx| {
                        this.dismiss_error_message(&DismissErrorMessage, cx)
                    })),
                }),
                AutoUpdateStatus::Idle => None,
            };
        }

        if let Some(extension_store) =
            ExtensionStore::try_global(cx).map(|extension_store| extension_store.read(cx))
        {
            if let Some(extension_id) = extension_store.outstanding_operations().keys().next() {
                return Some(Content {
                    icon: Some(
                        Icon::new(IconName::Download)
                            .size(IconSize::Small)
                            .into_any_element(),
                    ),
                    message: format!("Updating {extension_id} extension…"),
                    on_click: Some(Arc::new(|this, cx| {
                        this.dismiss_error_message(&DismissErrorMessage, cx)
                    })),
                });
            }
        }

        None
    }

    fn toggle_language_server_work_context_menu(
        &mut self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) {
        self.context_menu_handle.toggle(model, cx);
    }
}

impl EventEmitter<Event> for ActivityIndicator {}

const MAX_MESSAGE_LEN: usize = 50;

impl Render for ActivityIndicator {
    fn render(
        &mut self,
        model: &Model<Self>,
        window: &mut gpui::Window,
        cx: &mut AppContext,
    ) -> impl IntoElement {
        let result = h_flex()
            .id("activity-indicator")
            .on_action(model.listener(Self::show_error_message))
            .on_action(model.listener(Self::dismiss_error_message));
        let Some(content) = self.content_to_render(model, cx) else {
            return result;
        };
        let this = model.downgrade();
        let truncate_content = content.message.len() > MAX_MESSAGE_LEN;
        result.gap_2().child(
            PopoverMenu::new("activity-indicator-popover")
                .trigger(
                    ButtonLike::new("activity-indicator-trigger").child(
                        h_flex()
                            .id("activity-indicator-status")
                            .gap_2()
                            .children(content.icon)
                            .map(|button| {
                                if truncate_content {
                                    button
                                        .child(
                                            Label::new(truncate_and_trailoff(
                                                &content.message,
                                                MAX_MESSAGE_LEN,
                                            ))
                                            .size(LabelSize::Small),
                                        )
                                        .tooltip(move |window, cx| {
                                            Tooltip::text(&content.message, cx)
                                        })
                                } else {
                                    button.child(Label::new(content.message).size(LabelSize::Small))
                                }
                            })
                            .when_some(content.on_click, |this, handler| {
                                this.on_click(model.listener(move |this, _, model, window, cx| {
                                    handler(this, cx);
                                }))
                                .cursor(CursorStyle::PointingHand)
                            }),
                    ),
                )
                .anchor(gpui::AnchorCorner::BottomLeft)
                .menu(move |window, cx| {
                    let strong_this = this.upgrade()?;
                    let mut has_work = false;
                    let menu = ContextMenu::build(window, cx, |mut menu, model, window, cx| {
                        for work in strong_this.read(cx).pending_language_server_work(cx) {
                            has_work = true;
                            let this = this.clone();
                            let mut title = work
                                .progress
                                .title
                                .as_deref()
                                .unwrap_or(work.progress_token)
                                .to_owned();

                            if work.progress.is_cancellable {
                                let language_server_id = work.language_server_id;
                                let token = work.progress_token.to_string();
                                let title = SharedString::from(title);
                                menu = menu.custom_entry(
                                    move |_| {
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(Label::new(title.clone()))
                                            .child(Icon::new(IconName::XCircle))
                                            .into_any_element()
                                    },
                                    move |cx| {
                                        this.update(cx, |this, model, cx| {
                                            this.project.update(cx, |project, model, cx| {
                                                project.cancel_language_server_work(
                                                    language_server_id,
                                                    Some(token.clone()),
                                                    model,
                                                    cx,
                                                );
                                            });
                                            this.context_menu_handle.hide(cx);
                                            model.notify(cx);
                                        })
                                        .ok();
                                    },
                                );
                            } else {
                                if let Some(progress_message) = work.progress.message.as_ref() {
                                    title.push_str(": ");
                                    title.push_str(progress_message);
                                }

                                menu = menu.label(title);
                            }
                        }
                        menu
                    });
                    has_work.then_some(menu)
                }),
        )
    }
}

impl StatusItemView for ActivityIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &Model<Self>,
        _: &mut AppContext,
    ) {
    }
}

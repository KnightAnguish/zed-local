use crate::literary_diff::{self, Comparison};
use buffer_diff::BufferDiff;
use db::kvp::KeyValueStore;
use gpui::{Context, Entity, HighlightStyle, Render, StyledText, Subscription, Task};
use language::Buffer;
use project::Project;
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::ResultExt;

const PAGE_SIZE: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
struct ReviewBase {
    git_text: String,
    reviewed_text: String,
}

pub struct LiteraryReview {
    buffer: Entity<Buffer>,
    diff: Option<Entity<BufferDiff>>,
    base: Option<ReviewBase>,
    comparison: Option<Comparison>,
    storage_key: Option<String>,
    error: Option<String>,
    page: usize,
    loading: bool,
    saving: bool,
    generation: u64,
    subscriptions: Vec<Subscription>,
    _load_task: Option<Task<()>>,
    compare_task: Option<Task<()>>,
}

impl LiteraryReview {
    pub fn new(buffer: Entity<Buffer>, project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let storage_key = buffer
            .read(cx)
            .file()
            .and_then(|file| file.as_local())
            .map(|file| format!("literary-review-v1:{}", file.abs_path(cx).display()));
        let diff_task = project.update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        });
        let store = KeyValueStore::global(cx);
        let load_key = storage_key.clone();
        let subscription = cx.observe(&buffer, |this, _, cx| this.refresh(cx));
        let load_task = cx.spawn(async move |this, cx| {
            let result = async {
                let diff = diff_task.await?;
                let saved = cx
                    .background_spawn(async move {
                        load_key
                            .map(|key| store.read_kvp(&key))
                            .transpose()
                            .map(Option::flatten)
                    })
                    .await?;
                let saved = saved
                    .map(|value| serde_json::from_str::<ReviewBase>(&value))
                    .transpose()?;
                anyhow::Ok((diff, saved))
            }
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((diff, saved)) => {
                        let git_text = diff
                            .read(cx)
                            .snapshot(cx)
                            .base_text_string()
                            .unwrap_or_default();
                        this.base = Some(
                            saved
                                .filter(|base| base.git_text == git_text)
                                .unwrap_or_else(|| ReviewBase {
                                    reviewed_text: git_text.clone(),
                                    git_text,
                                }),
                        );
                        this.subscriptions
                            .push(cx.observe(&diff, |this, _, cx| this.refresh(cx)));
                        this.diff = Some(diff);
                        this.refresh(cx);
                    }
                    Err(error) => {
                        this.error = Some(format!("Не удалось открыть правки: {error}"));
                        this.loading = false;
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        Self {
            buffer,
            diff: None,
            base: None,
            comparison: None,
            storage_key,
            error: None,
            page: 0,
            loading: true,
            saving: false,
            generation: 0,
            subscriptions: vec![subscription],
            _load_task: Some(load_task),
            compare_task: None,
        }
    }

    fn git_matches(&self, cx: &App) -> bool {
        self.base
            .as_ref()
            .zip(self.diff.as_ref())
            .is_some_and(|(base, diff)| {
                diff.read(cx)
                    .snapshot(cx)
                    .base_text_string()
                    .unwrap_or_default()
                    == base.git_text
            })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let Some(base) = &self.base else {
            return;
        };
        if !self.git_matches(cx) {
            self.compare_task = None;
            self.comparison = None;
            self.loading = false;
            self.error = Some(
                "Версия в Git изменилась. Нажмите «Снова от Git», чтобы начать новый просмотр."
                    .into(),
            );
            cx.notify();
            return;
        }
        let before = base.reviewed_text.clone();
        let after = self.buffer.read(cx).text();
        self.loading = true;
        self.compare_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { literary_diff::compare(before, after) })
                .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(comparison) => {
                        this.page = this
                            .page
                            .min(comparison.changes.len().saturating_sub(1) / PAGE_SIZE);
                        this.comparison = Some(comparison);
                        this.error = None;
                    }
                    Err(error) => {
                        this.comparison = None;
                        this.error = Some(error.into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn save_base(&mut self, base: ReviewBase, cx: &mut Context<Self>) {
        let Some(key) = self.storage_key.clone() else {
            self.base = Some(base);
            self.refresh(cx);
            return;
        };
        let value = match serde_json::to_string(&base) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let store = KeyValueStore::global(cx);
        self.saving = true;
        cx.spawn(async move |this, cx| {
            let result = store.write_kvp(key, value).await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.base = Some(base);
                        this.refresh(cx);
                    }
                    Err(error) => {
                        this.error = Some(format!("Не удалось сохранить отметку: {error}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn decide(&mut self, index: usize, accept: bool, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        if self.loading || self.saving || !self.git_matches(cx) {
            self.refresh(cx);
            return;
        }
        let Some(comparison) = &self.comparison else {
            return;
        };
        if self.buffer.read(cx).text() != comparison.after {
            self.refresh(cx);
            return;
        }
        if accept {
            if let Some(base) = &self.base {
                self.save_base(
                    ReviewBase {
                        git_text: base.git_text.clone(),
                        reviewed_text: comparison.accepted(index),
                    },
                    cx,
                );
            }
        } else {
            if !self.buffer.read(cx).capability().editable() {
                self.error = Some("Этот файл доступен только для чтения.".into());
                cx.notify();
                return;
            }
            let edits = comparison.replacements(index, false);
            self.buffer.update(cx, |buffer, cx| {
                // Reject must not merge into a preceding user/agent edit in the undo history.
                buffer.finalize_last_transaction();
                buffer.edit_non_coalesce(edits, None, cx);
                buffer.finalize_last_transaction();
            });
            self.refresh(cx);
        }
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if let Some(diff) = &self.diff {
            let git_text = diff
                .read(cx)
                .snapshot(cx)
                .base_text_string()
                .unwrap_or_default();
            self.save_base(
                ReviewBase {
                    reviewed_text: git_text.clone(),
                    git_text,
                },
                cx,
            );
        }
    }
}

impl Render for LiteraryReview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self
            .comparison
            .as_ref()
            .map_or(0, |comparison| comparison.changes.len());
        let disabled = self.loading || self.saving;
        let generation = self.generation;
        v_flex().size_full().min_h_0().gap_2().p_3()
            .child(h_flex().gap_2()
                .child(Label::new(format!("Литературные правки · {count}")))
                .child(Button::new("literary-reset", "Снова от Git").disabled(self.saving || self.diff.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.reset(cx)))))
            .child(Label::new("Принять — оставить и отметить. Отклонить — вернуть текст (Undo в редакторе). Git index и коммиты не меняются.")
                .size(LabelSize::Small).color(Color::Muted))
            .when(self.storage_key.is_none(), |element| element.child(Label::new("Отметки для удалённого файла сохраняются только до закрытия просмотра.").color(Color::Warning)))
            .when_some(self.error.clone(), |element, error| element.child(Label::new(error).color(Color::Error)))
            .when(disabled, |element| element.child(Label::new("Обновление…")))
            .when(!disabled && count == 0 && self.error.is_none(), |element| element.child(Label::new("Непросмотренных изменений нет.")))
            .child(v_flex().id("literary-review-scroll").flex_1().min_h_0().overflow_y_scroll().gap_3()
                .children(self.comparison.as_ref().into_iter().flat_map(|comparison| {
                    comparison.changes.iter().enumerate().skip(self.page * PAGE_SIZE).take(PAGE_SIZE)
                        .map(|(index, change)| {
                            let old = &comparison.before[change.before.clone()];
                            let new = &comparison.after[change.after.clone()];
                            let (old_ranges, new_ranges) = literary_diff::word_changes(old, new);
                            let title = if let Some(other) = change.moved_with {
                                format!("{} · Перемещение ↔ {} (решение для обеих частей)", index + 1, other + 1)
                            } else if old.is_empty() { format!("{} · Добавление", index + 1) }
                            else if new.is_empty() { format!("{} · Удаление", index + 1) }
                            else { format!("{} · Изменение", index + 1) };
                            v_flex().flex_none().gap_2().p_3().border_1().rounded_md().border_color(cx.theme().colors().border)
                                .child(Label::new(title))
                                .when(!old.is_empty(), |element| element
                                    .child(Label::new("Было").size(LabelSize::Small).color(Color::Muted))
                                    .child(StyledText::new(old.to_owned()).with_highlights(old_ranges.into_iter().map(|range| (range, HighlightStyle {
                                        color: Some(cx.theme().status().deleted), font_weight: Some(gpui::FontWeight::BOLD), ..Default::default()
                                    })))))
                                .when(!new.is_empty(), |element| element
                                    .child(Label::new("Стало").size(LabelSize::Small).color(Color::Muted))
                                    .child(StyledText::new(new.to_owned()).with_highlights(new_ranges.into_iter().map(|range| (range, HighlightStyle {
                                        color: Some(cx.theme().status().created), font_weight: Some(gpui::FontWeight::BOLD), ..Default::default()
                                    })))))
                                .child(h_flex().gap_2()
                                    .child(div().debug_selector(move || format!("literary-accept-{index}"))
                                        .child(Button::new(("literary-accept", index), "Принять").disabled(disabled)
                                        .on_click(cx.listener(move |this, _, _, cx| this.decide(index, true, generation, cx)))))
                                    .child(div().debug_selector(move || format!("literary-reject-{index}"))
                                        .child(Button::new(("literary-reject", index), "Отклонить").disabled(disabled)
                                        .on_click(cx.listener(move |this, _, _, cx| this.decide(index, false, generation, cx))))))
                        }).collect::<Vec<_>>()
                })))
            .child(h_flex().gap_2()
                .child(Button::new("literary-previous", "Назад").disabled(self.page == 0)
                    .on_click(cx.listener(|this, _, _, cx| { this.page = this.page.saturating_sub(1); cx.notify(); })))
                .child(Label::new(format!("{} / {}", self.page + 1, count.div_ceil(PAGE_SIZE).max(1))))
                .child(Button::new("literary-next", "Далее").disabled((self.page + 1) * PAGE_SIZE >= count)
                    .on_click(cx.listener(|this, _, _, cx| { this.page += 1; cx.notify(); }))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[gpui::test]
    async fn literary_loads_git_and_restores_review(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            workspace::AppState::test(cx);
            editor::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/literary"),
            serde_json::json!({
                ".git": {}, "chapter.md": "Новая глава.\n"
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            std::path::Path::new(util::path!("/literary/.git")),
            &[("chapter.md", "Старая глава.\n".into())],
        );
        let project = Project::test(fs, [std::path::Path::new(util::path!("/literary"))], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(util::path!("/literary/chapter.md"), cx)
            })
            .await
            .unwrap();
        let review = cx.new(|cx| LiteraryReview::new(buffer.clone(), project.clone(), cx));
        cx.run_until_parked();
        review.read_with(cx, |review, _| {
            assert_eq!(review.error, None);
            assert_eq!(review.base.as_ref().unwrap().git_text, "Старая глава.\n");
            assert_eq!(review.comparison.as_ref().unwrap().changes.len(), 1);
        });
        review.update(cx, |review, cx| {
            review.decide(0, true, review.generation, cx)
        });
        cx.run_until_parked();
        drop(review);
        let reopened = cx.new(|cx| LiteraryReview::new(buffer, project, cx));
        cx.run_until_parked();
        reopened.read_with(cx, |review, _| {
            assert_eq!(review.error, None);
            assert!(review.comparison.as_ref().unwrap().changes.is_empty());
            assert_eq!(
                review.base.as_ref().unwrap().reviewed_text,
                "Новая глава.\n"
            );
            assert_eq!(review.base.as_ref().unwrap().git_text, "Старая глава.\n");
        });
    }

    #[gpui::test]
    async fn literary_decisions_persist_and_reject_is_undoable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            workspace::AppState::test(cx);
            editor::init(cx);
        });
        let before = "Первый.\n\nВторой.\n";
        let after = "Первый новый.\n\nВторой другой.\n";
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(before, cx);
            buffer.edit([(0..before.len(), after)], None, cx);
            buffer
        });
        let diff = cx
            .new(|cx| BufferDiff::new_with_base_text(before, &buffer.read(cx).text_snapshot(), cx));
        let (review, cx) = cx.add_window_view(|_, _| LiteraryReview {
            buffer: buffer.clone(),
            diff: Some(diff),
            base: Some(ReviewBase {
                git_text: before.into(),
                reviewed_text: before.into(),
            }),
            comparison: Some(literary_diff::compare(before.into(), after.into()).unwrap()),
            storage_key: Some("literary-test".into()),
            error: None,
            page: 0,
            loading: false,
            saving: false,
            subscriptions: Vec::new(),
            _load_task: None,
            compare_task: None,
            generation: 0,
        });
        cx.run_until_parked();
        let accept = cx.debug_bounds("literary-accept-0").expect("accept button");
        cx.simulate_click(accept.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), after);
        let saved = cx.update(|_, cx| {
            KeyValueStore::global(cx)
                .read_kvp("literary-test")
                .unwrap()
                .unwrap()
        });
        let saved: ReviewBase = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved.reviewed_text, "Первый новый.\n\nВторой.\n");
        assert_eq!(
            review.read_with(cx, |review, _| review
                .comparison
                .as_ref()
                .unwrap()
                .changes
                .len()),
            1
        );
        let reject = cx.debug_bounds("literary-reject-0").expect("reject button");
        cx.simulate_click(reject.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            saved.reviewed_text
        );
        buffer.update(cx, |buffer, cx| {
            buffer.undo(cx);
        });
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), after);

        // An edit racing a click must never be overwritten by stale byte offsets.
        buffer.update(cx, |buffer, cx| {
            buffer.edit_non_coalesce([(0..0, "Свежая правка.\n\n")], None, cx);
        });
        let fresh = buffer.read_with(cx, |buffer, _| buffer.text());
        review.update(cx, |review, cx| {
            review.decide(0, false, review.generation, cx)
        });
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), fresh);
        review.update(cx, |review, cx| review.reset(cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), fresh);
        assert_eq!(
            review.read_with(cx, |review, _| review
                .base
                .as_ref()
                .unwrap()
                .reviewed_text
                .clone()),
            before
        );
    }
}

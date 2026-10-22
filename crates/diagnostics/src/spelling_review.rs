use anyhow::{Context as _, Result, bail};
use collections::HashSet;
use editor::{
    Editor, EditorEvent, MultiBufferOffset, SelectionEffects,
    actions::{Redo, Undo},
    scroll::Autoscroll,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Task, Window, actions, uniform_list,
};
use language::{Buffer, BufferEvent, BufferSnapshot, Capability, LanguageServerId, Point};
use project::{CodeAction, LspAction, Project, ProjectItem};
use std::ops::Range;
use text::{Anchor, ToOffset as _, ToPoint as _, ToPointUtf16 as _};
use ui::{Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, ItemHandle, SaveOptions},
};

actions!(
    diagnostics,
    [
        /// Reviews spelling problems in the current file without scrolling through it.
        ReviewSpelling,
    ]
);

#[derive(Clone)]
struct Occurrence {
    range: Range<Anchor>,
    server_id: LanguageServerId,
}

struct WordGroup {
    word: String,
    occurrences: Vec<Occurrence>,
    finding: Option<super::proofreader::Finding>,
}

fn collect_words(snapshot: &BufferSnapshot, excluded: &HashSet<String>) -> Vec<WordGroup> {
    let mut groups: Vec<WordGroup> = Vec::new();
    let mut indexes = collections::HashMap::default();
    for (server_id, entry) in snapshot
        .diagnostic_entries_in_range_with_server_id(Point::zero()..snapshot.max_point(), false)
    {
        if !entry.diagnostic.is_primary
            || !entry
                .diagnostic
                .source
                .as_deref()
                .is_some_and(|source| source.eq_ignore_ascii_case("cspell"))
        {
            continue;
        }
        let word: String = snapshot.text_for_range(entry.range.clone()).collect();
        if !valid_word(&word) || excluded.contains(&word) {
            continue;
        }
        if let Some(reported) = entry
            .diagnostic
            .data
            .as_ref()
            .and_then(|data| data.get("text"))
            .and_then(|text| text.as_str())
            && reported != word
        {
            continue;
        }
        // Anchors follow edits before a server republishes diagnostics. Don't reinterpret
        // the old CSpell message as a diagnostic for a newly corrected word.
        if let Some(message) = entry.diagnostic.message.as_str().strip_prefix('"')
            && let Some((reported, _)) = message.split_once("\":")
            && reported != word
        {
            continue;
        }
        let occurrence = Occurrence {
            range: entry.range.clone(),
            server_id,
        };
        if let Some(&index) = indexes.get(&word) {
            let group: &mut WordGroup = &mut groups[index];
            if !group
                .occurrences
                .iter()
                .any(|existing| existing.range == occurrence.range)
            {
                group.occurrences.push(occurrence);
            }
        } else {
            indexes.insert(word.clone(), groups.len());
            groups.push(WordGroup {
                word,
                occurrences: vec![occurrence],
                finding: None,
            });
        }
    }
    groups
}

fn valid_word(word: &str) -> bool {
    !word.is_empty() && word.chars().count() <= 64 && !word.chars().any(char::is_whitespace)
}

fn replacement(action: &CodeAction, uri: &lsp::Uri, range: lsp::Range) -> Option<String> {
    let LspAction::Action(raw) = &action.lsp_action else {
        return None;
    };
    if raw.command.is_some()
        || raw.disabled.is_some()
        || raw.kind.as_ref() != Some(&lsp::CodeActionKind::QUICKFIX)
    {
        return None;
    }
    let edit = raw.edit.as_ref()?;
    if edit.document_changes.is_some() {
        return None;
    }
    let changes = edit.changes.as_ref()?;
    if changes.len() != 1 {
        return None;
    }
    let [edit] = changes.get(uri)?.as_slice() else {
        return None;
    };
    (edit.range == range && valid_word(&edit.new_text)).then(|| edit.new_text.clone())
}

fn dictionary_command(
    action: &CodeAction,
    uri: &lsp::Uri,
    range: lsp::Range,
) -> Option<lsp::Command> {
    let command = match &action.lsp_action {
        LspAction::Action(raw) if raw.edit.is_none() && raw.disabled.is_none() => {
            raw.command.as_ref()?
        }
        LspAction::Command(command) => command,
        _ => return None,
    };
    if command.command != "AddToWorkspaceWordsConfig" {
        return None;
    }
    let [argument] = command.arguments.as_ref()?.as_slice() else {
        return None;
    };
    if argument.get("uri")?.as_str()? != uri.as_str()
        || serde_json::from_value::<lsp::Range>(argument.get("range")?.clone()).ok()? != range
    {
        return None;
    }
    Some(command.clone())
}

pub(crate) struct SpellingReview {
    project: Entity<Project>,
    source: Entity<Editor>,
    buffer: Entity<Buffer>,
    focus_handle: FocusHandle,
    groups: Vec<WordGroup>,
    selected: usize,
    occurrence: usize,
    skipped: HashSet<String>,
    accepted: HashSet<String>,
    suggestions: Vec<String>,
    dictionary_action: Option<CodeAction>,
    action_snapshot: Option<BufferSnapshot>,
    generation: usize,
    busy: bool,
    saving_dictionary: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    full_review: bool,
    checking: bool,
    checked: bool,
    internal_edit: bool,
    own_edit_snapshot: Option<BufferSnapshot>,
    edited_since_check: bool,
    check_generation: usize,
    check_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for SpellingReview {}

impl SpellingReview {
    pub fn register(workspace: &mut Workspace, _: Option<&mut Window>, _: &mut Context<Workspace>) {
        workspace.register_action(Self::deploy);
    }

    fn deploy(
        workspace: &mut Workspace,
        _: &ReviewSpelling,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(source) = workspace.active_item_as::<Editor>(cx) else {
            return;
        };
        let Some(buffer) = source.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|review| review.read(cx).buffer == buffer);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let review = cx.new(|cx| Self::new(project, source, buffer, window, cx));
        workspace.add_item_to_active_pane(Box::new(review), None, true, window, cx);
    }

    fn new(
        project: Entity<Project>,
        source: Entity<Editor>,
        buffer: Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe_in(&buffer, window, |this, _, event, window, cx| match event {
                BufferEvent::DiagnosticsUpdated if !this.full_review => {
                    this.refresh(false, window, cx)
                }
                BufferEvent::Edited { .. }
                    if this.full_review
                        && (this.internal_edit
                            || this.own_edit_snapshot.as_ref().is_some_and(|snapshot| {
                                this.buffer.read(cx).version() == snapshot.text.version
                            })) =>
                {
                    this.own_edit_snapshot = None;
                    cx.notify();
                }
                BufferEvent::Edited { .. }
                | BufferEvent::Reloaded
                | BufferEvent::FileHandleChanged => this.refresh(false, window, cx),
                BufferEvent::CapabilityChanged => cx.notify(),
                _ => {}
            }),
            cx.subscribe(&source, |_, _, event: &EditorEvent, cx| {
                cx.emit(event.clone())
            }),
        ];
        let mut review = Self {
            project,
            source,
            buffer,
            focus_handle: cx.focus_handle(),
            groups: Vec::new(),
            selected: 0,
            occurrence: 0,
            skipped: HashSet::default(),
            accepted: HashSet::default(),
            suggestions: Vec::new(),
            dictionary_action: None,
            action_snapshot: None,
            generation: 0,
            busy: false,
            saving_dictionary: false,
            error: None,
            task: None,
            full_review: false,
            checking: false,
            checked: false,
            internal_edit: false,
            own_edit_snapshot: None,
            edited_since_check: false,
            check_generation: 0,
            check_task: None,
            _subscriptions: subscriptions,
        };
        review.refresh(false, window, cx);
        review
    }

    fn current(&self) -> Option<(&WordGroup, &Occurrence)> {
        let group = self.groups.get(self.selected)?;
        Some((group, group.occurrences.get(self.occurrence)?))
    }

    fn refresh(&mut self, include_skipped: bool, window: &mut Window, cx: &mut Context<Self>) {
        // A diagnostics notification may arrive before executeCommand's response.
        // Do not drop the in-flight write or lose its success/error acknowledgement.
        if self.saving_dictionary {
            return;
        }
        if self.full_review {
            if self.internal_edit {
                return;
            }
            self.check_generation += 1;
            self.check_task = None;
            self.checking = false;
            self.checked = false;
            self.groups.clear();
            self.suggestions.clear();
            self.action_snapshot = None;
            self.error = None;
            if include_skipped {
                self.check_text(window, cx);
            }
            cx.notify();
            return;
        }
        if include_skipped {
            self.skipped.clear();
            self.accepted.clear();
        }
        let previous = self.current().map(|(group, _)| group.word.clone());
        let snapshot = self.buffer.read(cx).snapshot();
        let excluded = self.skipped.union(&self.accepted).cloned().collect();
        self.groups = collect_words(&snapshot, &excluded);
        let retained =
            previous.and_then(|word| self.groups.iter().position(|group| group.word == word));
        self.selected =
            retained.unwrap_or_else(|| self.selected.min(self.groups.len().saturating_sub(1)));
        if let Some(group) = self.groups.get(self.selected) {
            self.occurrence = if retained.is_some() {
                self.occurrence
                    .min(group.occurrences.len().saturating_sub(1))
            } else {
                0
            };
        } else {
            self.occurrence = 0;
        }
        self.request_actions(window, cx);
    }

    fn request_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        self.task = None;
        self.busy = false;
        self.suggestions.clear();
        self.dictionary_action = None;
        self.action_snapshot = None;
        self.error = None;
        if self.full_review {
            if let Some((group, _)) = self.current() {
                self.suggestions = group
                    .finding
                    .as_ref()
                    .map(|finding| finding.suggestions.clone())
                    .unwrap_or_default();
                self.action_snapshot = Some(self.buffer.read(cx).snapshot());
            }
            cx.notify();
            return;
        }
        let Some((group, occurrence)) = self.current() else {
            cx.notify();
            return;
        };
        let word = group.word.clone();
        let occurrence = occurrence.clone();
        let snapshot = self.buffer.read(cx).snapshot();
        let Some(file) = snapshot.file().and_then(|file| file.as_local()) else {
            self.error = Some("Подсказки доступны для локального файла.".into());
            cx.notify();
            return;
        };
        let uri = match lsp::Uri::from_file_path(file.abs_path(cx)) {
            Ok(uri) => uri,
            Err(()) => {
                self.error = Some("Не удалось определить адрес локального файла.".into());
                cx.notify();
                return;
            }
        };
        let range = lsp::Range::new(
            language::point_to_lsp(occurrence.range.start.to_point_utf16(&snapshot)),
            language::point_to_lsp(occurrence.range.end.to_point_utf16(&snapshot)),
        );
        let request = self.project.update(cx, |project, cx| {
            project.code_actions(
                &self.buffer,
                occurrence.range.start..occurrence.range.start,
                None,
                cx,
            )
        });
        self.busy = true;
        let generation = self.generation;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let response = request.await;
            this.update(cx, |this, cx| {
                if this.generation != generation
                    || this.buffer.read(cx).version() != snapshot.text.version
                {
                    return;
                }
                this.busy = false;
                match response {
                    Ok(actions) => {
                        for action in actions.unwrap_or_default() {
                            if action.server_id != occurrence.server_id {
                                continue;
                            }
                            if let Some(replacement) = replacement(&action, &uri, range)
                                && replacement != word
                                && !this.suggestions.contains(&replacement)
                                && this.suggestions.len() < 8
                            {
                                this.suggestions.push(replacement);
                            }
                            if dictionary_command(&action, &uri, range).is_some() {
                                this.dictionary_action = Some(action);
                            }
                        }
                        this.action_snapshot = Some(snapshot);
                    }
                    Err(error) => {
                        this.error = Some(format!("Не удалось получить варианты: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn validated_occurrence(&self, cx: &App) -> Result<(&WordGroup, &Occurrence)> {
        if self.saving_dictionary {
            bail!("Дождитесь записи словаря.");
        }
        if self
            .source
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .as_ref()
            != Some(&self.buffer)
        {
            bail!("Исходный редактор переключён на другой файл. Откройте проверку заново.");
        }
        if self.source.read(cx).read_only(cx) {
            bail!("Файл доступен только для чтения.");
        }
        let (group, occurrence) = self.current().context("Слово уже отсутствует в очереди.")?;
        let snapshot = self
            .action_snapshot
            .as_ref()
            .context("Дождитесь вариантов исправления.")?;
        if self.buffer.read(cx).version() != snapshot.text.version {
            bail!("Текст изменился. Обновите список.");
        }
        let current: String = self
            .buffer
            .read(cx)
            .snapshot()
            .text_for_range(occurrence.range.clone())
            .collect();
        if current != group.word {
            bail!("Слово изменилось. Обновите список.");
        }
        Ok((group, occurrence))
    }

    fn apply(&mut self, replacement: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result: Result<()> = (|| {
            let (_, occurrence) = self.validated_occurrence(cx)?;
            if !self
                .suggestions
                .iter()
                .any(|suggestion| suggestion == replacement)
            {
                bail!("Вариант устарел.");
            }
            let snapshot = self.buffer.read(cx).snapshot();
            let range = MultiBufferOffset(occurrence.range.start.to_offset(&snapshot))
                ..MultiBufferOffset(occurrence.range.end.to_offset(&snapshot));
            self.internal_edit = self.full_review;
            self.source.update(cx, |source, cx| {
                source.finalize_last_transaction(cx);
                source.transact(window, cx, |source, _, cx| {
                    source.edit([(range, replacement.to_owned())], cx)
                });
                source.finalize_last_transaction(cx);
            });
            self.internal_edit = false;
            if self.full_review {
                self.own_edit_snapshot = Some(self.buffer.read(cx).snapshot());
                self.groups.remove(self.selected);
                self.selected = self.selected.min(self.groups.len().saturating_sub(1));
                self.edited_since_check = true;
                self.request_actions(window, cx);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn accept_word(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.full_review {
            self.accept_checked_word(window, cx);
            return;
        }
        let result: Result<()> = (|| {
            let (group, _) = self.validated_occurrence(cx)?;
            let word = group.word.clone();
            let action = self
                .dictionary_action
                .clone()
                .context("CSpell не предложил запись в словарь.")?;
            let command = action
                .lsp_action
                .command()
                .context("Нет команды словаря.")?
                .clone();
            let store = self.project.read(cx).lsp_store();
            let request = store.update(cx, |store, cx| {
                store.execute_lsp_command(
                    action.server_id,
                    command.command,
                    command.arguments.unwrap_or_default(),
                    cx,
                )
            });
            self.busy = true;
            self.saving_dictionary = true;
            self.generation += 1;
            let generation = self.generation;
            self.task = Some(cx.spawn_in(window, async move |this, cx| {
                let response = request.await;
                this.update_in(cx, |this, window, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.busy = false;
                    this.saving_dictionary = false;
                    match response {
                        Ok(Some(value)) if value.get("error").is_some() => {
                            this.error = Some(format!("Словарь не изменён: {}", value["error"]))
                        }
                        Ok(Some(value)) if value.get("result").is_some() => {
                            this.accepted.insert(word);
                            this.refresh(false, window, cx);
                        }
                        Ok(_) => {
                            this.error =
                                Some("Сервер не подтвердил запись слова. Проверьте словарь.".into())
                        }
                        Err(error) => {
                            this.error = Some(format!("Не удалось записать слово: {error:#}"))
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn skip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        if self.full_review {
            if !self.groups.is_empty() {
                self.groups.remove(self.selected);
                self.selected = self.selected.min(self.groups.len().saturating_sub(1));
                self.request_actions(window, cx);
            }
            return;
        }
        if let Some((group, _)) = self.current() {
            self.skipped.insert(group.word.clone());
        }
        self.refresh(false, window, cx);
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        self.selected = index;
        self.occurrence = 0;
        self.request_actions(window, cx);
    }

    fn next_occurrence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        if let Some((group, _)) = self.current() {
            self.occurrence = (self.occurrence + 1) % group.occurrences.len();
        }
        self.request_actions(window, cx);
    }

    fn show_in_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        let Some((group, occurrence)) = self.current() else {
            return;
        };
        let word = group.word.clone();
        let range = occurrence.range.clone();
        let source = self.source.clone();
        let buffer = self.buffer.clone();
        let review = cx.entity().downgrade();
        // Activating another tab deactivates this item, which updates this entity.
        // Defer on the window, not Context<Self>, so this borrow is released first.
        window.defer(cx, move |window, cx| {
            let result: Result<()> = (|| {
                if source.read(cx).buffer().read(cx).as_singleton().as_ref() != Some(&buffer) {
                    bail!("Исходный редактор переключён на другой файл. Откройте проверку заново.");
                }
                let snapshot = buffer.read(cx).snapshot();
                let current: String = snapshot.text_for_range(range.clone()).collect();
                if current != word {
                    bail!("Слово изменилось. Обновите список.");
                }
                let workspace =
                    Workspace::for_window(window, cx).context("Рабочее окно уже закрыто.")?;
                let range = MultiBufferOffset(range.start.to_offset(&snapshot))
                    ..MultiBufferOffset(range.end.to_offset(&snapshot));
                source.update(cx, |source, cx| {
                    source.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([range]),
                    );
                });
                workspace.update(cx, |workspace, cx| {
                    if !workspace.activate_item(&source, true, true, window, cx) {
                        // Keep the same in-memory editor, including unsaved text and Undo,
                        // if its original tab was closed while the review remained open.
                        workspace.add_item_to_active_pane(Box::new(source), None, true, window, cx);
                    }
                });
                Ok(())
            })();
            if let Err(error) = result {
                review
                    .update(cx, |review, cx| {
                        review.error = Some(format!("Не удалось перейти к слову: {error:#}"));
                        cx.notify();
                    })
                    .log_err();
            }
        });
    }

    fn undo(&mut self, action: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        self.source
            .update(cx, |source, cx| source.undo(action, window, cx));
    }

    fn redo(&mut self, action: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary {
            return;
        }
        self.source
            .update(cx, |source, cx| source.redo(action, window, cx));
    }

    fn set_mode(&mut self, full: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving_dictionary || self.full_review == full {
            return;
        }
        self.check_generation += 1;
        self.check_task = None;
        self.checking = false;
        self.full_review = full;
        self.refresh(true, window, cx);
    }

    fn check_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.full_review || self.saving_dictionary {
            return;
        }
        self.check_generation += 1;
        self.check_task = None;
        self.task = None;
        self.busy = false;
        self.checked = false;
        self.edited_since_check = false;
        self.groups.clear();
        self.suggestions.clear();
        self.error = None;
        let snapshot = self.buffer.read(cx).snapshot();
        let Some(file) = snapshot.file().and_then(|file| file.as_local()) else {
            self.error = Some("Полная проверка доступна для локального файла проекта.".into());
            cx.notify();
            return;
        };
        let path = file.abs_path(cx);
        let text = snapshot.text();
        let timeout = cx
            .background_executor()
            .timer(std::time::Duration::from_secs(120));
        let request = cx.background_spawn(async move {
            futures_lite::future::or(
                super::proofreader::check(path, text),
                async move { timeout.await; bail!("Проверка превысила 120 секунд; процесс остановлен. Попробуйте файл меньшего размера.") },
            ).await
        });
        let generation = self.check_generation;
        self.checking = true;
        self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            let response = request.await;
            this.update_in(cx, |this, window, cx| {
                if this.check_generation != generation {
                    return;
                }
                this.checking = false;
                if this.buffer.read(cx).version() != snapshot.text.version {
                    this.error =
                        Some("Текст изменился во время проверки. Запустите её заново.".into());
                } else {
                    match response {
                        Ok(findings) => {
                            this.groups = findings
                                .into_iter()
                                .map(|finding| {
                                    let range = snapshot.anchor_after(finding.range.start)
                                        ..snapshot.anchor_before(finding.range.end);
                                    WordGroup {
                                        word: snapshot
                                            .text_for_range(finding.range.clone())
                                            .collect(),
                                        occurrences: vec![Occurrence {
                                            range,
                                            server_id: LanguageServerId(0),
                                        }],
                                        finding: Some(finding),
                                    }
                                })
                                .collect();
                            this.selected = 0;
                            this.occurrence = 0;
                            this.checked = true;
                            this.request_actions(window, cx);
                        }
                        Err(error) => {
                            this.error = Some(format!("Проверка не завершена: {error:#}"))
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn accept_checked_word(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result: Result<_> = (|| {
            let (group, _) = self.validated_occurrence(cx)?;
            ensure_dictionary_word(group)?;
            let snapshot = self.buffer.read(cx).snapshot();
            let file = snapshot
                .file()
                .and_then(|file| file.as_local())
                .context("Нет локального файла")?;
            Ok((file.abs_path(cx), group.word.clone()))
        })();
        let (path, word) = match result {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.saving_dictionary = true;
        let request = cx.background_spawn(async move {
            super::proofreader::accept_word(&path, &word)?;
            Ok::<_, anyhow::Error>(word)
        });
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.saving_dictionary = false;
                match result {
                    Ok(word) => {
                        this.groups.retain(|group| {
                            group.word != word
                                || !group
                                    .finding
                                    .as_ref()
                                    .is_some_and(|finding| finding.spelling)
                        });
                        this.selected = this.selected.min(this.groups.len().saturating_sub(1));
                        this.request_actions(window, cx);
                    }
                    Err(error) => this.error = Some(format!("Словарь не изменён: {error:#}")),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }
}

fn ensure_dictionary_word(group: &WordGroup) -> Result<()> {
    if !group
        .finding
        .as_ref()
        .is_some_and(|finding| finding.spelling)
        || !valid_word(&group.word)
        || !group
            .word
            .chars()
            .all(|character| character.is_alphabetic() || character == '-')
    {
        bail!("Это замечание к правилу, а не неизвестное слово.");
    }
    Ok(())
}

impl Focusable for SpellingReview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SpellingReview {
    type Event = EditorEvent;
    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        format!(
            "Проверка текста: {}",
            self.source.read(cx).tab_content_text(0, cx)
        )
        .into()
    }
    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        self.source.read(cx).tab_tooltip_text(cx)
    }
    fn to_item_events(event: &EditorEvent, emit: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, emit);
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.source.read(cx).is_dirty(cx)
    }
    fn capability(&self, cx: &App) -> Capability {
        self.source.read(cx).capability(cx)
    }
    fn can_save(&self, cx: &App) -> bool {
        self.source.read(cx).can_save(cx)
    }
    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.source.save(options, project, window, cx)
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        visit: &mut dyn FnMut(gpui::EntityId, &dyn ProjectItem),
    ) {
        self.source.for_each_project_item(cx, visit);
    }
    fn active_project_path(&self, cx: &App) -> Option<project::ProjectPath> {
        self.source.read(cx).active_project_path(cx)
    }
}

impl Render for SpellingReview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let read_only = self.source.read(cx).read_only(cx);
        let mut detail = v_flex().gap_3().p_4().flex_1().min_w_0();
        if let Some((group, occurrence)) = self.current() {
            let snapshot = self.buffer.read(cx).snapshot();
            let offset = occurrence.range.start.to_offset(&snapshot);
            let start = snapshot.clip_offset(offset.saturating_sub(160), language::Bias::Left);
            let end = snapshot.clip_offset(
                (occurrence.range.end.to_offset(&snapshot) + 160).min(snapshot.len()),
                language::Bias::Right,
            );
            let context: String = snapshot.text_for_range(start..end).collect();
            let line = occurrence.range.start.to_point(&snapshot).row + 1;
            detail = detail
                .child(Label::new(if group.word.is_empty() { "Вставить знак".into() } else { group.word.clone() }).size(LabelSize::Large))
                .when_some(group.finding.as_ref(), |detail, finding| detail
                    .child(div().child(finding.message.clone()))
                    .child(Label::new(format!("{} · {}", if finding.spelling { "Словарь" } else { "Грамматика / пунктуация" }, finding.rule)).color(Color::Muted)))
                .child(Label::new(format!("Строка {line} · вхождение {} из {}", self.occurrence + 1, group.occurrences.len())).color(Color::Muted))
                .child(div().p_3().rounded_md().bg(cx.theme().colors().element_background).child(context))
                .child(h_flex().gap_2()
                    .child(Button::new("spelling-next-occurrence", "Следующее вхождение").disabled(group.occurrences.len() < 2 || self.saving_dictionary).on_click(cx.listener(|this, _, window, cx| this.next_occurrence(window, cx))))
                    .child(div().debug_selector(|| "spelling-show-file".into()).child(Button::new("spelling-show-file", "Показать в файле").disabled(self.saving_dictionary).on_click(cx.listener(|this, _, window, cx| this.show_in_file(window, cx))))))
                .child(Label::new(if self.busy { "Подбираю исправления…" } else if self.suggestions.is_empty() { if self.full_review { "Нет готовых замен — можно пропустить замечание." } else { "Нет вариантов замены — можно принять слово или пропустить." } } else { "Выберите замену для этого вхождения:" }).color(Color::Muted))
                .children(self.suggestions.iter().enumerate().map(|(index, suggestion)| {
                    let suggestion = suggestion.clone();
                    div().debug_selector(move || format!("spelling-replacement-{index}")).child(
                        Button::new(("spelling-replace", index), if suggestion.is_empty() { "Удалить лишнее".to_owned() } else { suggestion.clone() }).full_width().disabled(read_only || self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| this.apply(&suggestion, window, cx)))
                    )
                }))
                .child(h_flex().gap_2()
                    .child(Button::new("spelling-accept", "Слово верное → в словарь").disabled(read_only || self.busy || self.saving_dictionary || if self.full_review { ensure_dictionary_word(group).is_err() } else { self.dictionary_action.is_none() })
                        .tooltip(Tooltip::text("В словарь CSpell проекта; полная проверка читает его при запуске. Сохраняется сразу; Undo отменяет только замены текста."))
                        .on_click(cx.listener(|this, _, window, cx| this.accept_word(window, cx))))
                    .child(Button::new("spelling-skip", if self.full_review { "Пропустить замечание" } else { "Пропустить слово" }).disabled(self.saving_dictionary).on_click(cx.listener(|this, _, window, cx| this.skip(window, cx)))));
        } else {
            detail = detail.child(Label::new(if self.full_review {
                if self.checking { "Проверяю русский текст локально…" } else if self.checked { "В очереди больше нет замечаний." } else { "Нажмите «Проверить текст»." }
            } else { "Найденных опечаток в очереди нет." }))
                .child(div().text_color(cx.theme().colors().text_muted).child(if self.full_review {
                    "Русский словарь, грамматика и пунктуация — LTeX+ / LanguageTool. Текст не отправляется в интернет. Отсутствие замечаний не гарантирует отсутствия ошибок; авторские обороты могут требовать пропуска."
                } else { "Список использует диагностику CSpell текущего файла. Если проверка ещё идёт или CSpell отключён, это не означает, что ошибок нет." }));
        }
        detail = detail
            .when_some(self.error.clone(), |detail, error| {
                detail.child(Label::new(error).color(Color::Error))
            })
            .when(read_only, |detail| {
                detail.child(Label::new("Файл доступен только для чтения.").color(Color::Warning))
            })
            .when(self.full_review && self.edited_since_check, |detail| {
                detail.child(div().text_color(cx.theme().status().warning).child("Текст исправлен. Остальные замечания относятся к предыдущей проверке; для проверки связанного контекста нажмите «Проверить текст» ещё раз."))
            });
        v_flex()
            .size_full()
            .key_context("Editor SpellingReview")
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .child(
                h_flex()
                    .p_3()
                    .gap_3()
                    .flex_wrap()
                    .child(
                        Button::new("spelling-mode-words", "Слова CSpell")
                            .toggle_state(!self.full_review)
                            .disabled(self.saving_dictionary)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.set_mode(false, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("spelling-mode-full", "Полная проверка")
                            .toggle_state(self.full_review)
                            .disabled(self.saving_dictionary)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.set_mode(true, window, cx)),
                            ),
                    )
                    .child(Label::new(format!("Осталось: {}", self.groups.len())))
                    .child(
                        Button::new(
                            "spelling-refresh",
                            if self.full_review {
                                "Проверить текст"
                            } else {
                                "Обновить / вернуть пропущенные"
                            },
                        )
                        .disabled(self.saving_dictionary || self.checking)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.refresh(true, window, cx)),
                        ),
                    )
                    .when(self.checking, |header| {
                        header.child(
                            Button::new("spelling-cancel-check", "Отменить проверку").on_click(
                                cx.listener(|this, _, window, cx| this.refresh(false, window, cx)),
                            ),
                        )
                    })
                    .child(
                        Button::new("spelling-undo", "Отменить замену")
                            .disabled(read_only || self.saving_dictionary)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.undo(&Undo, window, cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        uniform_list(
                            "spelling-words",
                            self.groups.len(),
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                range
                                    .filter_map(|index| {
                                        let group = this.groups.get(index)?;
                                        Some(
                                            Button::new(
                                                ("spelling-word", index),
                                                if let Some(finding) = &group.finding {
                                                    let label: String =
                                                        group.word.chars().take(28).collect();
                                                    format!(
                                                        "{}: {}",
                                                        if finding.spelling {
                                                            "Слово"
                                                        } else {
                                                            "Правило"
                                                        },
                                                        if label.is_empty() {
                                                            "Вставка"
                                                        } else {
                                                            &label
                                                        }
                                                    )
                                                } else {
                                                    format!(
                                                        "{} ({})",
                                                        group.word,
                                                        group.occurrences.len()
                                                    )
                                                },
                                            )
                                            .toggle_state(index == this.selected)
                                            .full_width()
                                            .disabled(this.saving_dictionary)
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.select(index, window, cx)
                                                }),
                                            ),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .w(gpui::px(250.))
                        .h_full()
                        .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .id("spelling-detail")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .child(detail),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn action(uri: lsp::Uri, range: lsp::Range) -> CodeAction {
        CodeAction {
            server_id: LanguageServerId(1),
            range: Anchor::min_max_range_for_buffer(language::BufferId::new(1).unwrap()),
            resolved: true,
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title: "ошибка".into(),
                kind: Some(lsp::CodeActionKind::QUICKFIX),
                edit: Some(lsp::WorkspaceEdit::new(
                    [(uri, vec![lsp::TextEdit::new(range, "ошибка".into())])]
                        .into_iter()
                        .collect(),
                )),
                ..Default::default()
            })),
        }
    }

    #[test]
    fn replacement_is_only_a_single_safe_word_edit() {
        let uri: lsp::Uri = "file:///book.md".parse().unwrap();
        let range = lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 7));
        let original = action(uri.clone(), range);
        assert_eq!(
            replacement(&original, &uri, range).as_deref(),
            Some("ошибка")
        );
        assert!(replacement(&original, &"file:///other.md".parse().unwrap(), range).is_none());
        assert!(replacement(&original, &uri, lsp::Range::default()).is_none());
        for text in ["", "два слова", "\n", &"я".repeat(65)] {
            let mut candidate = original.clone();
            if let LspAction::Action(raw) = &mut candidate.lsp_action {
                raw.edit
                    .as_mut()
                    .unwrap()
                    .changes
                    .as_mut()
                    .unwrap()
                    .get_mut(&uri)
                    .unwrap()[0]
                    .new_text = text.into();
            }
            assert!(replacement(&candidate, &uri, range).is_none());
        }
        let mut candidate = original;
        if let LspAction::Action(raw) = &mut candidate.lsp_action {
            raw.command = Some(lsp::Command {
                command: "execute".into(),
                ..Default::default()
            });
        }
        assert!(replacement(&candidate, &uri, range).is_none());
    }

    fn dictionary_action(uri: &lsp::Uri, range: lsp::Range) -> CodeAction {
        let mut action = action(uri.clone(), range);
        action.lsp_action = LspAction::Action(Box::new(lsp::CodeAction {
            title: "Add to workspace words in config".into(),
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            command: Some(lsp::Command {
                title: "Add".into(),
                command: "AddToWorkspaceWordsConfig".into(),
                arguments: Some(vec![json!({"uri":uri,"range":range})]),
            }),
            ..Default::default()
        }));
        action
    }

    #[test]
    fn dictionary_command_is_bound_to_this_uri_range_and_whitelist() {
        let uri: lsp::Uri = "file:///book.md".parse().unwrap();
        let range = lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 7));
        let mut action = dictionary_action(&uri, range);
        assert!(dictionary_command(&action, &uri, range).is_some());
        assert!(dictionary_command(&action, &uri, lsp::Range::default()).is_none());
        assert!(dictionary_command(&action, &"file:///other.md".parse().unwrap(), range).is_none());
        if let LspAction::Action(raw) = &mut action.lsp_action {
            raw.command.as_mut().unwrap().command = "AddToCustomDictionary".into();
        }
        assert!(dictionary_command(&action, &uri, range).is_none());
    }

    async fn fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (
        EditorLspTestContext,
        Entity<SpellingReview>,
        Arc<AtomicUsize>,
    ) {
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                code_action_provider: Some(lsp::CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: vec!["AddToWorkspaceWordsConfig".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state("ошибкка ошибкка\nопечтка\nˇ");
        cx.lsp
            .set_request_handler::<lsp::request::CodeActionRequest, _, _>(|params, _| async move {
                let Some(diagnostic) = params.context.diagnostics.first() else {
                    return Ok(None);
                };
                let uri = params.text_document.uri;
                let range = diagnostic.range;
                let replacement = if range.start.line == 0 {
                    "ошибка"
                } else {
                    "опечатка"
                };
                let mut fix = action(uri.clone(), range);
                if let LspAction::Action(raw) = &mut fix.lsp_action {
                    raw.edit
                        .as_mut()
                        .unwrap()
                        .changes
                        .as_mut()
                        .unwrap()
                        .get_mut(&uri)
                        .unwrap()[0]
                        .new_text = replacement.into();
                }
                Ok(Some(
                    vec![fix, dictionary_action(&uri, range)]
                        .into_iter()
                        .map(|action| match action.lsp_action {
                            LspAction::Action(action) => {
                                lsp::CodeActionOrCommand::CodeAction(*action)
                            }
                            _ => unreachable!(),
                        })
                        .collect(),
                ))
            });
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        cx.lsp
            .set_request_handler::<lsp::request::ExecuteCommand, _, _>(move |params, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(params.command, "AddToWorkspaceWordsConfig");
                    Ok(Some(json!({"result":"Added word to the dictionary."})))
                }
            });
        let diagnostics = [(0, 0, "ошибкка"), (0, 8, "ошибкка"), (1, 0, "опечтка")]
            .into_iter()
            .map(|(line, column, word)| lsp::Diagnostic {
                range: lsp::Range::new(
                    lsp::Position::new(line, column),
                    lsp::Position::new(line, column + word.chars().count() as u32),
                ),
                source: Some("cSpell".into()),
                severity: Some(lsp::DiagnosticSeverity::INFORMATION),
                message: format!("\"{word}\": Unknown word.").into(),
                data: Some(json!({"text":word})),
                ..Default::default()
            })
            .collect();
        cx.lsp
            .notify::<lsp::notification::PublishDiagnostics>(lsp::PublishDiagnosticsParams {
                uri: cx.buffer_lsp_url.clone(),
                diagnostics,
                version: None,
            });
        cx.run_until_parked();
        let workspace = cx.workspace.clone();
        let source = cx.editor.clone();
        let review = cx.update(|window, cx| {
            let buffer = source.read(cx).buffer().read(cx).as_singleton().unwrap();
            let project = workspace.read(cx).project().clone();
            cx.new(|cx| SpellingReview::new(project, source, buffer, window, cx))
        });
        cx.run_until_parked();
        (cx, review, calls)
    }

    #[gpui::test]
    async fn full_review_applies_phrases_keeps_remaining_anchors_and_invalidates_on_undo(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, _) = fixture(cx).await;
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                let snapshot = review.buffer.read(cx).snapshot();
                review.full_review = true;
                review.checked = true;
                review.task = None;
                review.busy = false;
                review.groups = [
                    (0.."ошибкка".len(), "ошибка, да"),
                    (
                        "ошибкка ошибкка\n".len().."ошибкка ошибкка\nопечтка".len(),
                        "",
                    ),
                ]
                .into_iter()
                .map(|(range, replacement)| WordGroup {
                    word: snapshot.text_for_range(range.clone()).collect(),
                    occurrences: vec![Occurrence {
                        range: snapshot.anchor_after(range.start)
                            ..snapshot.anchor_before(range.end),
                        server_id: LanguageServerId(0),
                    }],
                    finding: Some(super::super::proofreader::Finding {
                        range,
                        message: "Пунктуация".into(),
                        rule: "TEST".into(),
                        suggestions: vec![replacement.into()],
                        spelling: false,
                    }),
                })
                .collect();
                review.request_actions(window, cx);
            })
        });
        let workspace = cx.workspace.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(review.clone()), None, true, window, cx)
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let bounds = cx
            .debug_bounds("spelling-replacement-0")
            .expect("full-review phrase replacement rendered");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка, да ошибкка\nопечтка\n");
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                assert_eq!(review.groups.len(), 1);
                assert!(review.edited_since_check);
                assert!(ensure_dictionary_word(&review.groups[0]).is_err());
                review.apply("", window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка, да ошибкка\n\n");
        cx.update(|window, cx| review.update(cx, |review, cx| review.undo(&Undo, window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка, да ошибкка\nопечтка\n");
        cx.update(|_, cx| {
            assert!(review.read(cx).groups.is_empty());
            assert!(!review.read(cx).checked);
        });
    }

    #[gpui::test]
    async fn full_review_ignores_diagnostics_but_external_edits_clear_results(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, _) = fixture(cx).await;
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                review.full_review = true;
                review.checked = true;
                review
                    .buffer
                    .update(cx, |_, cx| cx.emit(BufferEvent::DiagnosticsUpdated));
                assert_eq!(review.groups.len(), 2);
                review.source.update(cx, |source, cx| {
                    source.edit([(MultiBufferOffset(0)..MultiBufferOffset(0), "ещё ")], cx)
                });
                review.apply("ошибка", window, cx);
                assert!(review.error.is_some());
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(review.read(cx).groups.is_empty());
            assert!(!review.read(cx).checked);
        });
    }

    #[gpui::test]
    async fn queue_groups_information_diagnostics_and_loads_without_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, calls) = fixture(cx).await;
        cx.update(|_, cx| {
            let review = review.read(cx);
            assert_eq!(review.groups.len(), 2);
            assert_eq!(review.groups[0].word, "ошибкка");
            assert_eq!(review.groups[0].occurrences.len(), 2);
            assert_eq!(review.suggestions, ["ошибка"]);
            assert!(review.dictionary_action.is_some());
        });
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn replacement_changes_one_occurrence_and_undo_restores_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, _) = fixture(cx).await;
        cx.update(|window, cx| review.update(cx, |review, cx| review.apply("ошибка", window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка ошибкка\nопечтка\n");
        cx.update(|_, cx| assert_eq!(review.read(cx).groups[0].occurrences.len(), 1));
        cx.update(|window, cx| review.update(cx, |review, cx| review.undo(&Undo, window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
        cx.update(|_, cx| assert_eq!(review.read(cx).groups[0].occurrences.len(), 2));
    }

    #[gpui::test]
    async fn rapid_replacements_remain_separate_undo_steps(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, _) = fixture(cx).await;
        for _ in 0..2 {
            cx.update(|window, cx| {
                review.update(cx, |review, cx| review.apply("ошибка", window, cx))
            });
            cx.run_until_parked();
        }
        assert_eq!(cx.buffer_text(), "ошибка ошибка\nопечтка\n");
        cx.update(|window, cx| review.update(cx, |review, cx| review.undo(&Undo, window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка ошибкка\nопечтка\n");
        cx.update(|window, cx| review.update(cx, |review, cx| review.undo(&Undo, window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
    }

    #[gpui::test]
    async fn skip_is_per_word_and_refresh_returns_skipped_words(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, calls) = fixture(cx).await;
        cx.update(|window, cx| review.update(cx, |review, cx| review.skip(window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(review.read(cx).groups[0].word, "опечтка"));
        cx.update(|window, cx| review.update(cx, |review, cx| review.refresh(true, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(review.read(cx).groups.len(), 2));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn readonly_and_unknown_suggestions_never_edit_or_write_dictionary(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, calls) = fixture(cx).await;
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                review.apply("произвольное", window, cx);
                assert!(review.error.is_some());
                review
                    .source
                    .update(cx, |source, _| source.set_read_only(true));
                review.apply("ошибка", window, cx);
                review.accept_word(window, cx);
                assert!(review.error.as_ref().unwrap().contains("чтения"));
            })
        });
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn dictionary_acceptance_waits_for_ack_and_does_not_edit_prose(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, calls) = fixture(cx).await;
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                review.accept_word(window, cx);
                let generation = review.generation;
                review.refresh(false, window, cx);
                assert_eq!(review.generation, generation);
                review.accept_word(window, cx);
                assert!(review.saving_dictionary);
            })
        });
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
        cx.update(|_, cx| {
            let review = review.read(cx);
            assert!(!review.saving_dictionary);
            assert_eq!(review.groups[0].word, "опечтка");
        });
    }

    #[gpui::test]
    async fn dictionary_error_keeps_word_visible(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, _) = fixture(cx).await;
        cx.lsp
            .set_request_handler::<lsp::request::ExecuteCommand, _, _>(|_, _| async move {
                Ok(Some(json!({"error":"Cannot safely read config"})))
            });
        cx.update(|window, cx| review.update(cx, |review, cx| review.accept_word(window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let review = review.read(cx);
            assert_eq!(review.groups[0].word, "ошибкка");
            assert!(
                review
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("Cannot safely read")
            );
            assert!(!review.saving_dictionary);
        });
    }

    #[gpui::test]
    async fn modified_buffer_rejects_stale_actions(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, calls) = fixture(cx).await;
        cx.update(|window, cx| {
            review.update(cx, |review, cx| {
                review.source.update(cx, |source, cx| {
                    source.edit(
                        [(
                            MultiBufferOffset(0)..MultiBufferOffset("ошибкка".len()),
                            "новое",
                        )],
                        cx,
                    )
                });
                review.apply("ошибка", window, cx);
                review.accept_word(window, cx);
                assert!(review.error.is_some());
            })
        });
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "новое ошибкка\nопечтка\n");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn mounted_queue_button_and_undo_action_work(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, _) = fixture(cx).await;
        let workspace = cx.workspace.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(review.clone()), None, true, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let bounds = cx
            .debug_bounds("spelling-replacement-0")
            .expect("replacement is rendered in the queue");
        assert!(bounds.size.width > gpui::px(0.) && bounds.size.height > gpui::px(0.));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибка ошибкка\nопечтка\n");
        cx.dispatch_action(Undo);
        cx.run_until_parked();
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
    }

    #[gpui::test]
    async fn show_in_file_activates_source_without_reentrant_review_borrow(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, _) = fixture(cx).await;
        let workspace = cx.workspace.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(review.clone()), None, true, window, cx)
            });
        });
        let source = cx.editor.clone();
        for _ in 0..2 {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&review, true, true, window, cx);
                });
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
            let bounds = cx
                .debug_bounds("spelling-show-file")
                .expect("show-in-file button is rendered");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.update(|_, cx| {
                assert_eq!(
                    workspace.read(cx).active_item_as::<Editor>(cx),
                    Some(source.clone())
                )
            });
        }
        cx.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).active_item_as::<Editor>(cx),
                Some(source.clone())
            );
            source.update(cx, |source, cx| {
                let snapshot = source.display_snapshot(cx);
                assert_eq!(
                    source
                        .selections
                        .newest::<MultiBufferOffset>(&snapshot)
                        .range(),
                    MultiBufferOffset(0)..MultiBufferOffset("ошибкка".len())
                );
            });
        });
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
    }

    #[gpui::test]
    async fn show_in_file_reopens_closed_source_with_unsaved_text(cx: &mut gpui::TestAppContext) {
        let (mut cx, review, _) = fixture(cx).await;
        let workspace = cx.workspace.clone();
        let source = cx.editor.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(review.clone()), None, true, window, cx)
            });
            let pane = workspace.read(cx).active_pane().clone();
            pane.update(cx, |pane, cx| {
                pane.remove_item(source.entity_id(), false, false, window, cx)
            });
            assert!(
                workspace
                    .read(cx)
                    .items_of_type::<Editor>(cx)
                    .all(|editor| editor != source)
            );
            assert!(source.read(cx).is_dirty(cx));
            review.update(cx, |review, cx| review.show_in_file(window, cx));
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).active_item_as::<Editor>(cx),
                Some(source.clone())
            );
            assert!(source.read(cx).is_dirty(cx));
        });
        assert_eq!(cx.buffer_text(), "ошибкка ошибкка\nопечтка\n");
    }

    #[gpui::test]
    async fn show_in_file_revalidates_word_before_deferred_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, review, _) = fixture(cx).await;
        let workspace = cx.workspace.clone();
        let source = cx.editor.clone();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(review.clone()), None, true, window, cx)
            });
            review.update(cx, |review, cx| review.show_in_file(window, cx));
            source.update(cx, |source, cx| {
                source.edit(
                    [(
                        MultiBufferOffset(0)..MultiBufferOffset("ошибкка".len()),
                        "новое",
                    )],
                    cx,
                )
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .active_item(cx)
                    .map(|item| item.item_id()),
                Some(review.entity_id())
            )
        });
        assert_eq!(cx.buffer_text(), "новое ошибкка\nопечтка\n");
    }
}

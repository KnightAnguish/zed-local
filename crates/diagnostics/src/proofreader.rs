use anyhow::{Context as _, Result, ensure};
use futures_lite::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use serde_json::{Value, json};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use util::command::{Stdio, new_command};

pub(super) struct Finding {
    pub range: Range<usize>,
    pub message: String,
    pub rule: String,
    pub suggestions: Vec<String>,
    pub spelling: bool,
}

pub(super) fn dictionary_path(file: &Path) -> Result<PathBuf> {
    find_dictionary(file)?
        .context("Не найден cspell.json проекта. Для сохранения слов нужен словарь проекта.")
}

fn find_dictionary(file: &Path) -> Result<Option<PathBuf>> {
    for directory in file.parent().context("У файла нет папки")?.ancestors() {
        let path = directory.join("cspell.json");
        if path.is_file() {
            return Ok(Some(path));
        }
        if directory.join(".git").exists() {
            break;
        }
    }
    Ok(None)
}

fn dictionary(file: &Path) -> Result<Value> {
    let Some(path) = find_dictionary(file)? else {
        return Ok(json!({}));
    };
    let content = std::fs::read_to_string(path).context("Не удалось прочитать словарь проекта")?;
    let value: Value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
        .context("Ошибка JSON в словаре проекта; файл не изменён")?;
    ensure!(
        value.is_object(),
        "Словарь проекта должен быть JSON-объектом"
    );
    Ok(value)
}

fn words(config: &Value) -> Vec<String> {
    ["words", "userWords"]
        .into_iter()
        .flat_map(|key| {
            config
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

pub(super) fn accept_word(file: &Path, word: &str) -> Result<()> {
    let backup_directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("Не найдена папка локальных данных")?
        .join("WorkbenchTools/proofreading/dictionary-backups");
    accept_word_with_backup(file, word, &backup_directory)
}

fn accept_word_with_backup(file: &Path, word: &str, backup_directory: &Path) -> Result<()> {
    use std::io::{Read as _, Seek as _, Write as _};
    ensure!(
        !word.is_empty()
            && word.chars().count() <= 64
            && word
                .chars()
                .all(|character| character.is_alphabetic() || character == '-'),
        "В словарь можно добавить только отдельное слово"
    );
    let path = dictionary_path(file)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // Keep another writer from replacing the config between reading and writing it.
        options.share_mode(0);
    }
    let mut handle = options
        .open(&path)
        .context("Словарь занят или недоступен для записи")?;
    let mut original = String::new();
    handle.read_to_string(&mut original)?;
    let mut config: Value = serde_json::from_str(original.trim_start_matches('\u{feff}'))?;
    let object = config
        .as_object_mut()
        .context("Словарь должен быть JSON-объектом")?;
    let list = object
        .entry("words")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("Поле words должно быть массивом")?;
    if list.iter().any(|value| value.as_str() == Some(word)) {
        return Ok(());
    }
    list.push(json!(word));
    let output = serde_json::to_vec_pretty(&config)?;
    std::fs::create_dir_all(&backup_directory)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut backup = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup_directory.join(format!("cspell-{stamp}.json")))?;
    backup.write_all(original.as_bytes())?;
    backup.sync_all()?;
    let write = (|| -> Result<()> {
        handle.rewind()?;
        handle.write_all(&output)?;
        handle.set_len(output.len() as u64)?;
        handle.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write {
        handle.rewind()?;
        handle.write_all(original.as_bytes())?;
        handle.set_len(original.len() as u64)?;
        handle.sync_all()?;
        return Err(error.context("Запись не удалась; исходный словарь восстановлен"));
    }
    Ok(())
}

async fn send(
    writer: &mut (impl futures_lite::io::AsyncWrite + Unpin),
    value: Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(&value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn receive(reader: &mut (impl futures_lite::io::AsyncBufRead + Unpin)) -> Result<Value> {
    let mut length = None;
    for _ in 0..16 {
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line).await? > 0,
            "Движок проверки завершился без ответа"
        );
        if line == "\r\n" || line == "\n" {
            break;
        }
        ensure!(line.len() < 1024, "Некорректный заголовок движка проверки");
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("В ответе движка нет Content-Length")?;
    ensure!(length <= 16 * 1024 * 1024, "Слишком большой ответ проверки");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn configuration(request: &Value, settings: &Value) -> Value {
    let items = request.pointer("/params/items").and_then(Value::as_array);
    Value::Array(
        items
            .into_iter()
            .flatten()
            .map(
                |item| match item.get("section").and_then(Value::as_str).unwrap_or("") {
                    "" => json!({"ltex":settings}),
                    "ltex" => settings.clone(),
                    section => section
                        .strip_prefix("ltex.")
                        .and_then(|key| settings.get(key))
                        .cloned()
                        .unwrap_or(Value::Null),
                },
            )
            .collect(),
    )
}

pub(super) async fn check(file: PathBuf, text: String) -> Result<Vec<Finding>> {
    ensure!(
        text.len() <= 2 * 1024 * 1024,
        "Для проверки выберите файл размером до 2 МБ"
    );
    let config = dictionary(&file)?;
    let settings = json!({"language":"ru-RU", "enabled":true, "checkFrequency":"save",
        "dictionary":{"ru-RU":words(&config)}, "additionalRules":{"enablePickyRules":false}});
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("Не найдена папка локальных данных")?
        .join("WorkbenchTools/proofreading/ltex-ls-plus-18.7.0");
    let java = root.join("jdk-21.0.10+7/bin/java.exe");
    ensure!(
        java.is_file(),
        "Локальный движок LTeX+ 18.7.0 не установлен"
    );
    let mut child = new_command(java)
        .args(["-Xmx768m", "-Dfile.encoding=UTF-8", "-cp"])
        .arg(root.join("lib/*"))
        .arg("org.bsplines.ltexls.LtexLanguageServerLauncher")
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut writer = child.stdin.take().context("Нет входного потока движка")?;
    let mut reader = BufReader::new(child.stdout.take().context("Нет ответа движка")?);
    let uri = lsp::Uri::from_file_path(&file)
        .map_err(|_| anyhow::anyhow!("Неверный путь файла"))?
        .to_string();
    send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "processId":std::process::id(), "rootUri":null,"locale":"ru-RU",
        "capabilities":{"workspace":{"configuration":true},"textDocument":{"codeAction":{"codeActionLiteralSupport":{"codeActionKind":{"valueSet":["quickfix"]}}}}}
    }})).await?;
    let mut diagnostics = None;
    let mut index = 0;
    let mut findings = Vec::new();
    let mut opened = false;
    loop {
        let message = receive(&mut reader).await?;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                let result = if method == "workspace/configuration" {
                    configuration(&message, &settings)
                } else {
                    Value::Null
                };
                send(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":id,"result":result}),
                )
                .await?;
            } else if method == "textDocument/publishDiagnostics"
                && message.pointer("/params/uri").and_then(Value::as_str) == Some(uri.as_str())
                && diagnostics.is_none()
            {
                let entries = message
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .context("Движок вернул неверный список ошибок")?
                    .clone();
                ensure!(
                    entries.len() <= 5000,
                    "Более 5000 замечаний: разделите файл на главы для проверки"
                );
                let Some(first) = entries.first() else {
                    child.kill()?;
                    child.status().await?;
                    return Ok(findings);
                };
                request_fixes(&mut writer, &uri, first).await?;
                diagnostics = Some(entries);
            }
        } else if message.get("id").and_then(Value::as_u64) == Some(1) && !opened {
            ensure!(
                message.get("error").is_none(),
                "Движок не смог запуститься: {message}"
            );
            send(
                &mut writer,
                json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
            )
            .await?;
            send(&mut writer, json!({"jsonrpc":"2.0","method":"workspace/didChangeConfiguration","params":{"settings":{"ltex":settings}}})).await?;
            send(&mut writer, json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{
                "uri":uri,"languageId":if file.extension().is_some_and(|ext| ext == "md") {"markdown"} else {"plaintext"},"version":1,"text":text
            }}})).await?;
            opened = true;
        } else if message.get("id").and_then(Value::as_u64) == Some(2) {
            ensure!(
                message.get("error").is_none(),
                "Не удалось получить исправления: {message}"
            );
            let entries: &Vec<Value> = diagnostics
                .as_ref()
                .context("Исправления пришли до диагностики")?;
            let entry = entries.get(index).context("Неверный индекс замечания")?;
            findings.extend(parse_findings(
                &text,
                std::slice::from_ref(entry),
                message.get("result"),
                &uri,
            )?);
            index += 1;
            if let Some(next) = entries.get(index) {
                request_fixes(&mut writer, &uri, next).await?;
                continue;
            }
            child.kill()?;
            child.status().await?;
            return Ok(findings);
        }
    }
}

async fn request_fixes(
    writer: &mut (impl futures_lite::io::AsyncWrite + Unpin),
    uri: &str,
    entry: &Value,
) -> Result<()> {
    // LTeX caps unique replacement strings per request, even across different rules.
    // A whole-document codeAction request silently loses most punctuation fixes.
    send(
        writer,
        json!({"jsonrpc":"2.0","id":2,"method":"textDocument/codeAction","params":{
            "textDocument":{"uri":uri},"range":entry.get("range").context("Нет диапазона ошибки")?,
            "context":{"diagnostics":[entry],"only":["quickfix.ltex.acceptSuggestions"]}
        }}),
    )
    .await
}

fn offset(text: &str, position: lsp::Position) -> Option<usize> {
    let mut base = 0;
    for _ in 0..position.line {
        base += text.get(base..)?.find('\n')? + 1;
    }
    let line = text.get(base..)?.split('\n').next()?.trim_end_matches('\r');
    let mut column = 0;
    for (index, character) in line.char_indices() {
        if column == position.character {
            return Some(base + index);
        }
        column += character.len_utf16() as u32;
    }
    (column == position.character).then_some(base + line.len())
}

fn parse_findings(
    text: &str,
    entries: &[Value],
    actions: Option<&Value>,
    uri: &str,
) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    ensure!(
        entries.len() <= 20_000,
        "Слишком много замечаний в одном файле"
    );
    for entry in entries {
        let range: lsp::Range =
            serde_json::from_value(entry.get("range").context("Нет диапазона ошибки")?.clone())?;
        let start = offset(text, range.start).context("Неверная позиция начала ошибки")?;
        let end = offset(text, range.end).context("Неверная позиция конца ошибки")?;
        ensure!(start <= end, "Перевёрнутый диапазон ошибки");
        let rule = entry
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("LanguageTool")
            .to_owned();
        let mut suggestions = Vec::new();
        for action in actions.and_then(Value::as_array).into_iter().flatten() {
            if action.get("command").is_some() || action.get("disabled").is_some() {
                continue;
            }
            for change in action
                .pointer("/edit/documentChanges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if change.pointer("/textDocument/uri").and_then(Value::as_str) != Some(uri) {
                    continue;
                }
                let Some(edits) = change.get("edits").and_then(Value::as_array) else {
                    continue;
                };
                let [edit] = edits.as_slice() else {
                    continue;
                };
                if edit.get("range") != entry.get("range") {
                    continue;
                }
                if let Some(replacement) = edit.get("newText").and_then(Value::as_str)
                    && replacement.len() <= 4096
                    && replacement != &text[start..end]
                    && !suggestions.iter().any(|existing| existing == replacement)
                    && suggestions.len() < 8
                {
                    suggestions.push(replacement.to_owned());
                }
            }
        }
        findings.push(Finding {
            range: start..end,
            spelling: rule.contains("MORFOLOGIK") || rule.contains("HUNSPELL"),
            rule,
            message: entry
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Замечание к тексту")
                .to_owned(),
            suggestions,
        });
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "zed-proofreader-test-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove owned test directory");
        }
    }

    #[test]
    fn dictionary_write_preserves_settings_and_backs_up_exact_original() {
        let directory = TestDirectory::new();
        let path = directory.0.join("cspell.json");
        let file = directory.0.join("book.md");
        let backup = directory.0.join("backups");
        let original = "\u{feff}{\"language\":\"en,ru\",\"import\":[\"russian.json\"],\"ignorePaths\":[\"tmp\"],\"words\":[\"Алхиматор\"],\"userWords\":[\"Роуз\"],\"custom\":true}";
        std::fs::write(&path, original).unwrap();
        accept_word_with_backup(&file, "Гиклопы", &backup).unwrap();
        let actual = dictionary(&file).unwrap();
        let mut expected: Value =
            serde_json::from_str(original.trim_start_matches('\u{feff}')).unwrap();
        expected["words"]
            .as_array_mut()
            .unwrap()
            .push(json!("Гиклопы"));
        assert_eq!(actual, expected);
        assert_eq!(words(&actual), ["Алхиматор", "Гиклопы", "Роуз"]);
        let backups: Vec<_> = std::fs::read_dir(&backup)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
        accept_word_with_backup(&file, "Гиклопы", &backup).unwrap();
        assert_eq!(std::fs::read_dir(&backup).unwrap().count(), 1);
    }

    #[test]
    fn malformed_dictionary_is_never_overwritten() {
        let directory = TestDirectory::new();
        let path = directory.0.join("cspell.json");
        let file = directory.0.join("book.md");
        let backup = directory.0.join("backups");
        for original in ["{broken", "[]", "{\"words\":false}"] {
            std::fs::write(&path, original).unwrap();
            assert!(accept_word_with_backup(&file, "Гиклопы", &backup).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            assert!(!backup.exists());
        }
        assert!(accept_word_with_backup(&file, "два слова", &backup).is_err());
    }

    #[test]
    fn dictionary_discovery_does_not_cross_git_boundary() {
        let directory = TestDirectory::new();
        std::fs::write(directory.0.join("cspell.json"), "{}").unwrap();
        let project = directory.0.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join(".git"), "gitdir: elsewhere").unwrap();
        assert!(find_dictionary(&project.join("book.md")).unwrap().is_none());
        assert_eq!(dictionary(&project.join("book.md")).unwrap(), json!({}));
    }
    #[test]
    #[ignore = "Requires the pinned local LTeX+ installation and PROOFREADER_TEST_FILE in a project with cspell.json"]
    fn local_engine_checks_russian_markdown_offline() {
        let file =
            PathBuf::from(std::env::var_os("PROOFREADER_TEST_FILE").expect("test file path"));
        let text = "# Проверка\n\nЭто ошипка. Кот вылез из под стола. Я думаю что он придёт. Он по-моему устал. Алхиматор.\n\n`xyzunknowncodeword`\n";
        let findings =
            futures_lite::future::block_on(check(file, text.to_owned())).expect("offline check");
        for finding in &findings {
            eprintln!(
                "{} {:?}: {} -> {:?}",
                finding.rule,
                &text[finding.range.clone()],
                finding.message,
                finding.suggestions
            );
        }
        assert!(findings.iter().any(|finding| {
            finding.spelling
                && text.get(finding.range.clone()) == Some("ошипка")
                && finding
                    .suggestions
                    .iter()
                    .any(|suggestion| suggestion == "ошибка")
        }));
        assert!(findings.iter().any(|finding| {
            !finding.spelling
                && finding
                    .suggestions
                    .iter()
                    .any(|suggestion| suggestion.contains("из-под"))
        }));
        assert!(
            findings
                .iter()
                .any(|finding| !finding.spelling && finding.message.contains("запят"))
        );
        assert!(
            !findings
                .iter()
                .any(|finding| text[finding.range.clone()].contains("xyzunknowncodeword"))
        );
        assert!(
            !findings
                .iter()
                .any(|finding| text.get(finding.range.clone()) == Some("Алхиматор"))
        );
    }
    #[test]
    fn utf16_positions_are_strict_and_handle_crlf_and_emoji() {
        let text = "я😀\r\nон";
        assert_eq!(offset(text, lsp::Position::new(0, 1)), Some(2));
        assert_eq!(offset(text, lsp::Position::new(0, 2)), None);
        assert_eq!(offset(text, lsp::Position::new(0, 3)), Some(6));
        assert_eq!(offset(text, lsp::Position::new(0, 4)), None);
        assert_eq!(offset(text, lsp::Position::new(1, 1)), Some(10));
        assert_eq!(offset(text, lsp::Position::new(2, 0)), None);
    }
    #[test]
    fn accepts_punctuation_deletions_and_only_exact_local_edits() {
        let range = json!({"start":{"line":0,"character":0},"end":{"line":0,"character":2}});
        let entries = vec![json!({"range":range,"code":"RULE","message":"Подсказка"})];
        let actions = json!([{"edit":{"documentChanges":[{"textDocument":{"uri":"file:///a.md","version":1},"edits":[{"range":range,"newText":""}]}]}}]);
        let findings = parse_findings("да", &entries, Some(&actions), "file:///a.md").unwrap();
        assert_eq!(findings[0].range, 0..4);
        assert_eq!(findings[0].suggestions, vec![""]);
        assert!(
            parse_findings("да", &entries, Some(&actions), "file:///b.md").unwrap()[0]
                .suggestions
                .is_empty()
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Followup {
    pub label: String,
    pub prompt: String,
}

pub fn extract(source: &str) -> (String, Vec<Followup>) {
    let mut markdown = String::new();
    let mut followups = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count();
        if let Some((opening, length)) = fence {
            if marker == opening && count >= length && trimmed[count..].trim().is_empty() {
                fence = None;
            }
        } else if matches!(marker, '`' | '~') && count >= 3 {
            fence = Some((marker, count));
        } else if !line.starts_with("    ") && !line.starts_with('\t') {
            if let Some(followup) = parse(trimmed) {
                followups.push(followup);
                markdown.push('\n');
                continue;
            }
        }
        markdown.push_str(line);
    }
    (markdown, followups)
}

fn parse(line: &str) -> Option<Followup> {
    let rest = line.strip_prefix(":codex-followup[")?;
    let (label, rest) = rest.split_once("]{")?;
    let value = rest
        .strip_suffix('}')?
        .trim()
        .strip_prefix("prompt")?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    let opening = value.chars().next()?;
    let closing = match opening {
        '"' => '"',
        '“' => '”',
        '\'' => '\'',
        _ => return None,
    };
    let body = value.strip_prefix(opening)?.strip_suffix(closing)?;
    let mut prompt = String::new();
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escaped = characters.next()?;
            match escaped {
                '\\' | '"' | '\'' => prompt.push(escaped),
                'n' => prompt.push('\n'),
                't' => prompt.push('\t'),
                _ => {
                    prompt.push('\\');
                    prompt.push(escaped);
                }
            }
        } else if character == closing {
            return None;
        } else {
            prompt.push(character);
        }
    }
    if label.trim().is_empty() || prompt.trim().is_empty() {
        return None;
    }
    Some(Followup {
        label: label.trim().to_owned(),
        prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_quotes_and_multiple_choices() {
        let (text, choices) = extract(
            "Ответ\n:codex-followup[Пробное запекание секции]{prompt=“Продолжай на копии.”}\n:codex-followup[B]{prompt=\"Next\"}",
        );
        assert_eq!(text, "Ответ\n\n\n");
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].prompt, "Продолжай на копии.");
    }

    #[test]
    fn preserves_examples_and_incomplete_stream() {
        for text in [
            "```text\n:codex-followup[A]{prompt=\"B\"}\n```",
            "~~~\n:codex-followup[A]{prompt=\"B\"}\n~~~",
            "    :codex-followup[A]{prompt=\"B\"}",
            "`:codex-followup[A]{prompt=\"B\"}`",
            ":codex-followup[A]{prompt=\"B",
            ":codex-followup[A]{prompt=\"B\" extra=\"C\"}",
        ] {
            assert_eq!(extract(text), (text.to_owned(), vec![]));
        }
    }

    #[test]
    fn escaped_quotes_and_backslashes() {
        let (_, choices) = extract(r#":codex-followup[A]{prompt="Say \"hi\" at C:\\test"}"#);
        assert_eq!(choices[0].prompt, "Say \"hi\" at C:\\test");
    }
}

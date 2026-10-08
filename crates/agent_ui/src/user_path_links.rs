use std::ops::Range;

fn is_path(value: &str) -> bool {
    if value.contains("://") && !value.starts_with("file://") {
        return false;
    }
    if value.starts_with("file://") {
        return true;
    }
    let absolute = value.starts_with('/')
        || value.starts_with("\\\\")
        || (value.as_bytes().get(1) == Some(&b':')
            && matches!(value.as_bytes().get(2), Some(b'/' | b'\\')));
    let name = value.rsplit(['/', '\\']).next().unwrap_or("");
    let file = name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && !extension.is_empty()
            && extension.len() <= 16
            && extension
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
    });
    let bare_file = name.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "md" | "txt"
                | "rs"
                | "py"
                | "js"
                | "ts"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "zip"
                | "7z"
                | "rar"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "pdf"
                | "html"
                | "htm"
                | "blend"
                | "docx"
                | "xlsx"
        )
    });
    absolute || (file && (value.contains('/') || value.contains('\\') || bare_file))
}

pub fn ranges(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() && found.len() < 128 {
        let Some(character) = text[cursor..].chars().next() else {
            break;
        };
        if character.is_whitespace() || matches!(character, '(' | '[' | '<') {
            cursor += character.len_utf8();
            continue;
        }
        let quote = matches!(character, '"' | '\'' | '`');
        let start = cursor + if quote { character.len_utf8() } else { 0 };
        let length = text[start..]
            .find(|c: char| {
                c == '\n'
                    || c == '\r'
                    || if quote {
                        c == character
                    } else {
                        c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`')
                    }
            })
            .unwrap_or(text.len() - start);
        let end = start + length;
        let value = text[start..end].trim_end_matches([',', ';', '!', '?', '.']);
        if !value.is_empty() && is_path(value) {
            found.push(start..start + value.len());
        }
        cursor = end;
        if cursor < text.len() {
            cursor += text[cursor..].chars().next().map_or(0, char::len_utf8);
        }
    }
    found
}

fn escaped(text: &str, output: &mut String) {
    for character in text.chars() {
        if character == '\n' {
            output.push_str("  \n");
        } else {
            if character.is_ascii_punctuation() {
                output.push('\\');
            }
            output.push(character);
        }
    }
}

pub fn projection(text: &str) -> Option<(String, Vec<String>)> {
    if text.len() > 256 * 1024 {
        return None;
    }
    let links = ranges(text);
    if links.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut targets = Vec::new();
    let mut cursor = 0;
    for range in links {
        escaped(&text[cursor..range.start], &mut output);
        output.push('[');
        escaped(&text[range.clone()], &mut output);
        output.push_str(&format!("](zed-user-path:{})", targets.len()));
        targets.push(text[range.clone()].to_owned());
        cursor = range.end;
    }
    escaped(&text[cursor..], &mut output);
    Some((output, targets))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_absolute_unicode_and_quotes() {
        let text = "Смотри Архивы\\vk-music-saver-local.zip и C:/Temp/a.png, потом \"Каталог с пробелами/мой файл.md\" /tmp/test.txt";
        let (_, targets) = projection(text).expect("links");
        assert_eq!(
            targets,
            [
                "Архивы\\vk-music-saver-local.zip",
                "C:/Temp/a.png",
                "Каталог с пробелами/мой файл.md",
                "/tmp/test.txt"
            ]
        );
    }
    #[test]
    fn preserves_literal_percent_and_does_not_enable_remote_or_script_urls() {
        assert!(projection("https://example.com/a.zip javascript:alert(1) слово").is_none());
        let (_, targets) =
            projection("./a%20b.txt ../folder/b.md file:///C:/Temp/a.zip").expect("links");
        assert_eq!(
            targets,
            ["./a%20b.txt", "../folder/b.md", "file:///C:/Temp/a.zip"]
        );
    }
    #[test]
    fn escapes_user_formatting_and_bounds_work() {
        let (display, _) = projection("# заголовок\nАрхивы\\x.zip **текст**").expect("links");
        assert!(display.starts_with("\\# заголовок  \n"));
        assert!(display.contains("\\*\\*текст\\*\\*"));
        assert!(projection(&"/a.txt ".repeat(50_000)).is_none());
        assert_eq!(ranges(&"./a.txt ".repeat(200)).len(), 128);
    }

    #[test]
    fn bare_file_names_are_relative_links() {
        let (_, targets) = projection("Открой README.md и картинка.png").expect("links");
        assert_eq!(targets, ["README.md", "картинка.png"]);
        assert!(projection("Это т.д. и обычный текст.").is_none());
    }
}

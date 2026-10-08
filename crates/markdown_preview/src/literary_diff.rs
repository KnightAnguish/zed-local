use imara_diff::{Algorithm, Diff, InternedInput};
use std::{collections::HashMap, ops::Range};

pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub before: Range<usize>,
    pub after: Range<usize>,
    pub moved_with: Option<usize>,
}

#[derive(Clone)]
pub struct Comparison {
    pub before: String,
    pub after: String,
    pub changes: Vec<Change>,
}

// Separators belong to the preceding block so that every edit is byte-exact.
fn paragraphs(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    let mut separator = false;
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if separator && !trimmed.is_empty() && fence.is_none() {
            ranges.push(start..offset);
            start = offset;
        }
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|ch| *ch == marker).count();
        if matches!(marker, '`' | '~') && count >= 3 {
            match fence {
                None => fence = Some((marker, count)),
                Some((opening, length))
                    if marker == opening
                        && count >= length
                        && trimmed[count..].trim().is_empty() =>
                {
                    fence = None
                }
                _ => {}
            }
        }
        separator = trimmed.is_empty() && fence.is_none();
        offset += line.len();
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

fn span(ranges: &[Range<usize>], range: Range<u32>, length: usize) -> Range<usize> {
    let start = ranges
        .get(range.start as usize)
        .map_or(length, |range| range.start);
    let end = if range.is_empty() {
        start
    } else {
        ranges
            .get(range.end as usize - 1)
            .map_or(length, |range| range.end)
    };
    start..end
}

pub fn compare(before: String, after: String) -> Result<Comparison, &'static str> {
    if before.len() > MAX_TEXT_BYTES || after.len() > MAX_TEXT_BYTES {
        return Err(
            "Литературное сравнение ограничено 2 МиБ на файл. Используйте обычный Git diff.",
        );
    }
    let old_blocks = paragraphs(&before);
    let new_blocks = paragraphs(&after);
    let mut input = InternedInput::default();
    input.update_before(old_blocks.iter().map(|range| &before[range.clone()]));
    input.update_after(new_blocks.iter().map(|range| &after[range.clone()]));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut changes = Vec::new();
    for hunk in diff.hunks() {
        let old_count = hunk.before.len();
        let new_count = hunk.after.len();
        // One-sided runs and aligned replacements can be reviewed paragraph by paragraph.
        if old_count == 0 || new_count == 0 || old_count == new_count {
            for index in 0..old_count.max(new_count) as u32 {
                let old = if old_count == 0 {
                    hunk.before.clone()
                } else {
                    hunk.before.start + index..hunk.before.start + index + 1
                };
                let new = if new_count == 0 {
                    hunk.after.clone()
                } else {
                    hunk.after.start + index..hunk.after.start + index + 1
                };
                changes.push(Change {
                    before: span(&old_blocks, old, before.len()),
                    after: span(&new_blocks, new, after.len()),
                    moved_with: None,
                });
            }
        } else {
            changes.push(Change {
                before: span(&old_blocks, hunk.before, before.len()),
                after: span(&new_blocks, hunk.after, after.len()),
                moved_with: None,
            });
        }
    }
    let mut old_counts = HashMap::new();
    let mut new_counts = HashMap::new();
    for range in &old_blocks {
        *old_counts.entry(&before[range.clone()]).or_insert(0) += 1;
    }
    for range in &new_blocks {
        *new_counts.entry(&after[range.clone()]).or_insert(0) += 1;
    }
    let removed: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.after.is_empty() && !change.before.is_empty())
        .map(|(index, change)| (&before[change.before.clone()], index))
        .collect();
    let pairs: Vec<_> = changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| {
            if !change.before.is_empty() || change.after.is_empty() {
                return None;
            }
            let text = &after[change.after.clone()];
            if old_counts.get(text) != Some(&1) || new_counts.get(text) != Some(&1) {
                return None;
            }
            removed.get(text).map(|removed| (*removed, index))
        })
        .collect();
    for (old, new) in pairs {
        changes[old].moved_with = Some(new);
        changes[new].moved_with = Some(old);
    }
    Ok(Comparison {
        before,
        after,
        changes,
    })
}

impl Comparison {
    pub fn replacements(&self, index: usize, accept: bool) -> Vec<(Range<usize>, String)> {
        let Some(change) = self.changes.get(index) else {
            return Vec::new();
        };
        let mut indices = vec![index];
        if let Some(other) = change.moved_with {
            indices.push(other);
        }
        indices.sort_unstable();
        let mut edits: Vec<_> = indices
            .into_iter()
            .map(|index| {
                let change = &self.changes[index];
                if accept {
                    (
                        change.before.clone(),
                        self.after[change.after.clone()].to_owned(),
                    )
                } else {
                    (
                        change.after.clone(),
                        self.before[change.before.clone()].to_owned(),
                    )
                }
            })
            .collect();
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        edits
    }

    pub fn accepted(&self, index: usize) -> String {
        let mut text = self.before.clone();
        for (range, replacement) in self.replacements(index, true).into_iter().rev() {
            text.replace_range(range, &replacement);
        }
        text
    }
}

fn words(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut previous = None;
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        let kind = if character.is_alphanumeric() || character == '_' {
            0
        } else if character.is_whitespace() {
            1
        } else {
            2
        };
        if previous.is_some_and(|previous| previous != kind || kind == 2) {
            ranges.push(start..offset);
            start = offset;
        }
        previous = Some(kind);
    }
    if start < text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

pub fn word_changes(before: &str, after: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if before.len() + after.len() > 64 * 1024 {
        return (vec![0..before.len()], vec![0..after.len()]);
    }
    let old = words(before);
    let new = words(after);
    let mut input = InternedInput::default();
    input.update_before(old.iter().map(|range| &before[range.clone()]));
    input.update_after(new.iter().map(|range| &after[range.clone()]));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            old_ranges.push(span(&old, hunk.before, before.len()));
        }
        if !hunk.after.is_empty() {
            new_ranges.push(span(&new, hunk.after, after.len()));
        }
    }
    (old_ranges, new_ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literary_roundtrip_and_independent_paragraphs() {
        for (old, new) in [
            ("Первый.\n\nВторой.\n", "Первый новый.\n\nВторой другой.\n"),
            ("", "Новый.\n\nЕщё.\n"),
            ("Удалить.\r\n\r\n", ""),
            ("A\n\nB\n\nC\n\n", "C\n\nA\n\nB\n\n"),
            ("🙂 кот\n", "🐈 кот!\n\nДобавлено"),
        ] {
            let mut baseline = old.to_owned();
            for _ in 0..20 {
                let comparison = compare(baseline.clone(), new.into()).unwrap();
                if comparison.changes.is_empty() {
                    break;
                }
                baseline = comparison.accepted(0);
            }
            assert_eq!(baseline, new);
            let mut working = new.to_owned();
            for _ in 0..20 {
                let comparison = compare(old.into(), working.clone()).unwrap();
                if comparison.changes.is_empty() {
                    break;
                }
                for (range, replacement) in comparison.replacements(0, false).into_iter().rev() {
                    working.replace_range(range, &replacement);
                }
            }
            assert_eq!(working, old);
        }
    }

    #[test]
    fn literary_moves_and_fences() {
        let comparison = compare("A\n\nB\n\nC\n\n".into(), "C\n\nA\n\nB\n\n".into()).unwrap();
        assert!(
            comparison
                .changes
                .iter()
                .all(|change| change.moved_with.is_some())
        );
        assert_eq!(paragraphs("```rust\ncode\n\nmore\n```\n\ntext").len(), 2);
        assert_eq!(paragraphs("~~~\n\ntext\n~~~\n").len(), 1);
    }

    #[test]
    fn literary_unicode_words() {
        let old = "Он увидел красный дом.";
        let new = "Он увидел синий дом!";
        let (removed, added) = word_changes(old, new);
        assert_eq!(
            removed
                .iter()
                .map(|range| &old[range.clone()])
                .collect::<Vec<_>>(),
            ["красный", "."]
        );
        assert_eq!(
            added
                .iter()
                .map(|range| &new[range.clone()])
                .collect::<Vec<_>>(),
            ["синий", "!"]
        );
        assert!(compare("x".repeat(MAX_TEXT_BYTES + 1), String::new()).is_err());
    }

    #[test]
    fn literary_repeated_paragraphs_roundtrip() {
        let texts: Vec<String> = (0..64)
            .map(|mut number| {
                let mut text = String::new();
                for _ in 0..3 {
                    text.push_str(["", "A\n\n", "B\n\n", "C\n\n"][number % 4]);
                    number /= 4;
                }
                text
            })
            .collect();
        for before in &texts {
            for after in &texts {
                let mut reviewed = before.clone();
                for _ in 0..12 {
                    let comparison = compare(reviewed.clone(), after.clone()).unwrap();
                    if comparison.changes.is_empty() {
                        break;
                    }
                    reviewed = comparison.accepted(0);
                }
                assert_eq!(&reviewed, after, "accept {before:?} → {after:?}");
                let mut working = after.clone();
                for _ in 0..12 {
                    let comparison = compare(before.clone(), working.clone()).unwrap();
                    if comparison.changes.is_empty() {
                        break;
                    }
                    for (range, text) in comparison.replacements(0, false).into_iter().rev() {
                        working.replace_range(range, &text);
                    }
                }
                assert_eq!(&working, before, "reject {before:?} → {after:?}");
            }
        }
    }
}

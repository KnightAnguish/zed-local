use std::collections::BTreeMap;

/// Guidance shown only when the `grep` tool is available.
pub const GREP_GUIDANCE: &str = "- When looking for symbols in the project, prefer the `grep` tool.
- As you learn about the structure of the project, scope searches to targeted subtrees instead of repeatedly searching the whole repository.
- If the user specifies a partial file path and you do not know the full path, use `find_path` rather than `grep` before reading or editing the file.";

/// Identifies one named block of the system prompt.
///
/// Keys are ordered, so rendering the same set of instructions always produces
/// the same prompt. Tool-specific guidance sorts after the core sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstructionKey {
    Core,
    Formatting,
    ToolUse,
    TaskExecution,
    Safety,
    Tool(&'static str),
    ProjectRules,
}

impl InstructionKey {
    pub fn heading(self) -> Option<&'static str> {
        match self {
            Self::Core => None,
            Self::Formatting => Some("Formatting Responses"),
            Self::ToolUse => Some("Tool Use"),
            Self::TaskExecution => Some("Task Execution"),
            Self::Safety => Some("Safety"),
            Self::Tool(_) => None,
            Self::ProjectRules => Some("Project Rules"),
        }
    }
}

/// A single named instruction block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    pub key: InstructionKey,
    /// Cacheable instructions are stable for the whole conversation; volatile
    /// ones (dates, counts) are rendered separately so they cannot invalidate
    /// the cached prefix.
    pub cacheable: bool,
    pub text: String,
}

/// An ordered collection of named instruction blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionSet {
    instructions: BTreeMap<InstructionKey, Instruction>,
}

impl InstructionSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces one block. Returns `true` when the rendered prompt
    /// changes, so callers can skip rebuilding requests for no-op updates.
    pub fn insert(&mut self, key: InstructionKey, cacheable: bool, text: impl Into<String>) -> bool {
        let text = text.into();
        if text.trim().is_empty() {
            return self.remove(key);
        }
        match self.instructions.get(&key) {
            Some(existing) if existing.cacheable == cacheable && existing.text == text => false,
            _ => {
                self.instructions.insert(
                    key,
                    Instruction {
                        key,
                        cacheable,
                        text,
                    },
                );
                true
            }
        }
    }

    pub fn remove(&mut self, key: InstructionKey) -> bool {
        self.instructions.remove(&key).is_some()
    }

    pub fn get(&self, key: InstructionKey) -> Option<&Instruction> {
        self.instructions.get(&key)
    }

    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Renders all blocks in key order.
    pub fn render(&self) -> String {
        render_instructions(self.instructions.values())
    }

    /// Renders only cacheable blocks, leaving volatile data for a later
    /// uncached section.
    pub fn render_cacheable(&self) -> String {
        render_instructions(
            self.instructions
                .values()
                .filter(|instruction| instruction.cacheable),
        )
    }

    /// Computes what changed relative to an earlier snapshot.
    pub fn diff(&self, previous: &InstructionSet) -> InstructionDelta {
        let mut delta = InstructionDelta::default();
        for (key, instruction) in &self.instructions {
            match previous.instructions.get(key) {
                None => delta.added.push(*key),
                Some(previous) if previous.text != instruction.text => delta.updated.push(*key),
                Some(_) => {}
            }
        }
        for key in previous.instructions.keys() {
            if !self.instructions.contains_key(key) {
                delta.removed.push(*key);
            }
        }
        delta
    }
}

/// Instructions that changed between two snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionDelta {
    pub added: Vec<InstructionKey>,
    pub updated: Vec<InstructionKey>,
    pub removed: Vec<InstructionKey>,
}

impl InstructionDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }

    /// Renders the change as a short prompt fragment.
    ///
    /// Removed instructions are named so the model stops following guidance it
    /// saw earlier in the conversation; added and updated blocks carry their
    /// new text.
    pub fn render(&self, current: &InstructionSet) -> String {
        let mut sections = Vec::new();
        for key in self.added.iter().chain(&self.updated) {
            if let Some(instruction) = current.get(*key) {
                sections.push(render_instruction(instruction));
            }
        }
        if !self.removed.is_empty() {
            let names = self
                .removed
                .iter()
                .map(|key| key_name(*key))
                .collect::<Vec<_>>()
                .join(", ");
            sections.push(format!(
                "The following instructions no longer apply: {names}."
            ));
        }
        sections.join("\n\n")
    }
}

fn render_instructions<'a>(instructions: impl Iterator<Item = &'a Instruction>) -> String {
    instructions
        .map(render_instruction)
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_instruction(instruction: &Instruction) -> String {
    match instruction.key.heading() {
        Some(heading) => format!("## {heading}\n\n{}", instruction.text.trim()),
        None => instruction.text.trim().to_string(),
    }
}

fn key_name(key: InstructionKey) -> &'static str {
    match key {
        InstructionKey::Core => "core instructions",
        InstructionKey::Formatting => "formatting",
        InstructionKey::ToolUse => "tool use",
        InstructionKey::TaskExecution => "task execution",
        InstructionKey::Safety => "safety",
        InstructionKey::Tool(name) => name,
        InstructionKey::ProjectRules => "project rules",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_deterministic_and_ordered() {
        let mut set = InstructionSet::new();
        assert!(set.insert(InstructionKey::ToolUse, true, "use tools"));
        assert!(set.insert(InstructionKey::Core, true, "you are an agent"));
        assert_eq!(
            set.render(),
            "you are an agent\n\n## Tool Use\n\nuse tools"
        );
    }

    #[test]
    fn unchanged_insert_does_not_report_a_change() {
        let mut set = InstructionSet::new();
        assert!(set.insert(InstructionKey::Core, true, "text"));
        assert!(!set.insert(InstructionKey::Core, true, "text"));
        assert!(set.insert(InstructionKey::Core, true, "other"));
        assert!(set.insert(InstructionKey::Core, true, "   "));
        assert!(set.is_empty());
    }

    #[test]
    fn cacheable_and_volatile_blocks_render_separately() {
        let mut set = InstructionSet::new();
        set.insert(InstructionKey::Core, true, "core");
        set.insert(InstructionKey::Tool("date"), false, "Today is Monday.");
        assert_eq!(set.render_cacheable(), "core");
        assert!(set.render().contains("Today is Monday."));
    }

    #[test]
    fn delta_tracks_added_updated_and_removed_keys() {
        let mut previous = InstructionSet::new();
        previous.insert(InstructionKey::Core, true, "core");
        previous.insert(InstructionKey::ToolUse, true, "old tools");

        let mut current = previous.clone();
        current.insert(InstructionKey::ToolUse, true, "new tools");
        current.insert(InstructionKey::Tool("grep"), true, "search first");
        current.remove(InstructionKey::Core);

        let delta = current.diff(&previous);
        assert_eq!(delta.added, vec![InstructionKey::Tool("grep")]);
        assert_eq!(delta.updated, vec![InstructionKey::ToolUse]);
        assert_eq!(delta.removed, vec![InstructionKey::Core]);
        let rendered = delta.render(&current);
        assert!(rendered.contains("new tools"));
        assert!(rendered.contains("search first"));
        assert!(rendered.contains("core instructions"));
    }

    #[test]
    fn empty_delta_renders_nothing() {
        let mut set = InstructionSet::new();
        set.insert(InstructionKey::Core, true, "core");
        assert!(set.diff(&set).is_empty());
        assert_eq!(set.diff(&set).render(&set), "");
    }
}

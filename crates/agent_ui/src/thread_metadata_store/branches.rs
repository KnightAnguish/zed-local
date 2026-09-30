use super::ThreadId;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Branches {
    groups: Vec<Group>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Group {
    active: ThreadId,
    variants: Vec<Variant>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Variant {
    id: ThreadId,
    parent: Option<ThreadId>,
    message: Option<String>,
    before: bool,
}

impl Branches {
    fn group(&self, id: ThreadId) -> Option<&Group> {
        self.groups
            .iter()
            .find(|group| group.variants.iter().any(|variant| variant.id == id))
    }

    pub fn contains(&self, id: ThreadId) -> bool {
        self.group(id).is_some()
    }

    pub fn variants(&self, id: ThreadId) -> Vec<ThreadId> {
        self.group(id)
            .map(|group| group.variants.iter().map(|variant| variant.id).collect())
            .unwrap_or_default()
    }

    pub fn add(
        &mut self,
        source: ThreadId,
        child: ThreadId,
        message: String,
        before: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            source != child && !self.contains(child),
            "Variant already belongs to a conversation"
        );
        if !self.contains(source) {
            self.groups.push(Group {
                active: source,
                variants: vec![Variant {
                    id: source,
                    parent: None,
                    message: None,
                    before: false,
                }],
            });
        }
        let group = self
            .groups
            .iter_mut()
            .find(|group| group.variants.iter().any(|variant| variant.id == source))
            .ok_or_else(|| anyhow::anyhow!("Missing conversation group"))?;
        group.variants.push(Variant {
            id: child,
            parent: Some(source),
            message: Some(message),
            before,
        });
        group.active = child;
        Ok(())
    }

    pub fn select(&mut self, id: ThreadId) -> bool {
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| group.variants.iter().any(|variant| variant.id == id))
        {
            if group.active != id {
                group.active = id;
                return true;
            }
        }
        false
    }

    pub fn visible(&self, id: ThreadId, exists: impl Fn(ThreadId) -> bool) -> bool {
        let Some(group) = self.group(id) else {
            return true;
        };
        let active = if exists(group.active) {
            Some(group.active)
        } else {
            group
                .variants
                .iter()
                .map(|variant| variant.id)
                .find(|id| exists(*id))
        };
        active == Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_edits_remain_one_group_and_survive_serialization() {
        let [first, second, third] = std::array::from_fn(|_| ThreadId::new());
        let mut branches = Branches::default();
        branches
            .add(first, second, "message-a".into(), true)
            .unwrap();
        branches
            .add(second, third, "message-b".into(), true)
            .unwrap();
        let mut restored: Branches =
            serde_json::from_str(&serde_json::to_string(&branches).unwrap()).unwrap();
        assert_eq!(restored.variants(first), vec![first, second, third]);
        assert!(!restored.visible(first, |_| true));
        assert!(restored.visible(third, |_| true));
        assert!(restored.select(first));
        assert!(restored.visible(first, |_| true));
        assert!(!restored.visible(third, |_| true));
    }

    #[test]
    fn missing_active_never_hides_all_surviving_variants() {
        let [first, second] = std::array::from_fn(|_| ThreadId::new());
        let mut branches = Branches::default();
        branches.add(first, second, "message".into(), true).unwrap();
        assert!(branches.visible(first, |id| id != second));
        assert!(branches.add(first, first, "bad".into(), true).is_err());
        assert!(branches.add(second, first, "bad".into(), true).is_err());
    }
}

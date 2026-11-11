use acp_thread::HistoryMessage;
use std::collections::VecDeque;

const MAX_RESIDENT_PAGES: usize = 3;

#[derive(Clone)]
pub(super) struct Page {
    pub count: usize,
    pub cursor: Option<String>,
    first: String,
    last: String,
}

impl Page {
    pub fn select(&self, messages: Vec<HistoryMessage>) -> anyhow::Result<Vec<HistoryMessage>> {
        let start = messages
            .iter()
            .position(|message| message.id.to_string() == self.first)
            .ok_or_else(|| anyhow::anyhow!("History page changed; reopen the chat"))?;
        let end = start + self.count;
        anyhow::ensure!(
            end <= messages.len() && messages[end - 1].id.to_string() == self.last,
            "History page changed; reopen the chat"
        );
        Ok(messages.into_iter().skip(start).take(self.count).collect())
    }
}

#[derive(Default)]
pub(super) struct HistoryWindow {
    pages: VecDeque<Page>,
    newer: Vec<Page>,
}

pub(super) struct Eviction {
    pub entries: usize,
    pub cursor: Option<String>,
    pages: usize,
}

impl HistoryWindow {
    pub fn prepend(&mut self, messages: &[HistoryMessage], cursor: Option<String>) {
        if let (Some(first), Some(last)) = (messages.first(), messages.last()) {
            self.pages.push_front(Page {
                count: messages.len(),
                cursor,
                first: first.id.to_string(),
                last: last.id.to_string(),
            });
        }
    }

    pub fn excess_newest(&self) -> Option<Page> {
        (self.pages.len() > MAX_RESIDENT_PAGES)
            .then(|| self.pages.back().cloned())
            .flatten()
    }

    pub fn evicted_newest(&mut self) {
        if let Some(page) = self.pages.pop_back() {
            self.newer.push(page);
        }
    }

    pub fn next_newer(&self) -> Option<Page> {
        self.newer.last().cloned()
    }

    pub fn entries(&self) -> usize {
        self.pages.iter().map(|page| page.count).sum()
    }

    pub fn restored_newer(&mut self) {
        if let Some(page) = self.newer.pop() {
            self.pages.push_back(page);
        }
    }

    pub fn excess_oldest(&self) -> Option<Page> {
        (self.pages.len() > MAX_RESIDENT_PAGES)
            .then(|| self.pages.front().cloned())
            .flatten()
    }

    pub fn evicted_oldest(&mut self) {
        self.pages.pop_front();
    }

    pub fn plan_eviction(&self, scroll_top: usize) -> Eviction {
        let available = scroll_top.saturating_sub(40);
        let mut result = Eviction {
            entries: 0,
            cursor: None,
            pages: 0,
        };
        for page in &self.pages {
            if result.entries + page.count > available {
                break;
            }
            result.entries += page.count;
            result.pages += 1;
            result.cursor = page.cursor.clone();
        }
        result
    }

    pub fn evicted(&mut self, eviction: &Eviction) {
        self.pages.drain(..eviction.pages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn messages(count: usize) -> Vec<HistoryMessage> {
        (0..count)
            .map(|index| HistoryMessage {
                id: agent_client_protocol::v1::schema::MessageId::new(index.to_string()),
                is_user: true,
                content: vec![],
            })
            .collect()
    }
    #[test]
    fn native_history_eviction_keeps_anchor_buffer_and_reload_cursor() {
        let mut window = HistoryWindow::default();
        window.prepend(&messages(40), Some("near-tail".into()));
        window.prepend(&messages(40), Some("older".into()));
        assert_eq!(window.plan_eviction(79).entries, 0);
        let eviction = window.plan_eviction(80);
        assert_eq!(eviction.entries, 40);
        assert_eq!(eviction.cursor.as_deref(), Some("older"));
        window.evicted(&eviction);
        assert_eq!(window.plan_eviction(40).entries, 0);
        window.prepend(&messages(40), Some("older".into()));
        let eviction = window.plan_eviction(120);
        assert_eq!(eviction.entries, 80);
        assert_eq!(eviction.cursor.as_deref(), Some("near-tail"));
    }

    #[test]
    fn native_history_window_bounds_resident_pages_and_restores_both_directions() {
        let mut window = HistoryWindow::default();
        for index in 0..100 {
            window.prepend(&messages(40), Some(index.to_string()));
            if window.excess_newest().is_some() {
                window.evicted_newest();
            }
            assert!(window.pages.len() <= MAX_RESIDENT_PAGES);
        }
        assert_eq!(
            window.next_newer().expect("page").cursor.as_deref(),
            Some("96")
        );
        for _ in 0..97 {
            let page = window.next_newer().expect("page");
            assert_eq!(page.select(messages(40)).expect("restored").len(), 40);
            assert!(page.select(messages(39)).is_err());
            window.restored_newer();
            assert!(window.excess_oldest().is_some());
            window.evicted_oldest();
            assert_eq!(window.pages.len(), MAX_RESIDENT_PAGES);
        }
        assert!(window.next_newer().is_none());
    }
}

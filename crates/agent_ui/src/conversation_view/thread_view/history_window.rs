use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct HistoryWindow {
    pages: VecDeque<(usize, Option<String>)>,
}

pub(super) struct Eviction {
    pub entries: usize,
    pub cursor: Option<String>,
    pages: usize,
}

impl HistoryWindow {
    pub fn prepend(&mut self, entries: usize, cursor: Option<String>) {
        if entries > 0 {
            self.pages.push_front((entries, cursor));
        }
    }

    pub fn plan_eviction(&self, scroll_top: usize) -> Eviction {
        let available = scroll_top.saturating_sub(40);
        let mut result = Eviction {
            entries: 0,
            cursor: None,
            pages: 0,
        };
        for (count, cursor) in &self.pages {
            if result.entries + count > available {
                break;
            }
            result.entries += count;
            result.pages += 1;
            result.cursor = cursor.clone();
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
    #[test]
    fn native_history_eviction_keeps_anchor_buffer_and_reload_cursor() {
        let mut window = HistoryWindow::default();
        window.prepend(40, Some("near-tail".into()));
        window.prepend(40, Some("older".into()));
        assert_eq!(window.plan_eviction(79).entries, 0);
        let eviction = window.plan_eviction(80);
        assert_eq!(eviction.entries, 40);
        assert_eq!(eviction.cursor.as_deref(), Some("older"));
        window.evicted(&eviction);
        assert_eq!(window.plan_eviction(40).entries, 0);
        window.prepend(40, Some("older".into()));
        let eviction = window.plan_eviction(120);
        assert_eq!(eviction.entries, 80);
        assert_eq!(eviction.cursor.as_deref(), Some("near-tail"));
    }
}

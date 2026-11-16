#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PromptState {
    Draft,
    Running,
    Completed,
}

#[derive(Clone)]
pub struct Viewer {
    pub title: String,
    pub text: String,
    pub scroll: usize,
    pub max_scroll: usize,
    pub prompt_state: Option<PromptState>,
    pub source_index: usize,
    pub read_only: bool,
    pub transcript: bool,
    pub prompt_rows: Vec<usize>,
    pub line_offsets: Vec<usize>,
    pub search_input: bool,
    pub query: String,
    pub matches: Vec<usize>,
    pub match_index: usize,
    search_origin: Option<usize>,
}

impl Viewer {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let line_offsets = (0..text.lines().count()).collect();
        Self {
            title: title.into(),
            text,
            scroll: 0,
            max_scroll: 0,
            prompt_state: None,
            source_index: usize::MAX,
            read_only: false,
            transcript: false,
            prompt_rows: Vec::new(),
            line_offsets,
            search_input: false,
            query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            search_origin: None,
        }
    }

    pub fn read_only(title: impl Into<String>, text: impl Into<String>) -> Self {
        let mut viewer = Self::new(title, text);
        viewer.read_only = true;
        viewer
    }

    pub fn transcript(
        title: impl Into<String>,
        text: impl Into<String>,
        prompt_rows: Vec<usize>,
    ) -> Self {
        let mut viewer = Self::new(title, text);
        viewer.transcript = true;
        viewer.read_only = true; // the transcript is for reading/searching/copying only
        viewer.prompt_rows = prompt_rows;
        viewer
    }

    pub fn with_source(
        title: impl Into<String>,
        text: impl Into<String>,
        source_index: usize,
        prompt_state: Option<PromptState>,
    ) -> Self {
        let mut viewer = Self::new(title, text);
        viewer.source_index = source_index;
        viewer.prompt_state = prompt_state;
        viewer
    }

    pub fn start_search(&mut self) {
        self.search_origin = Some(self.scroll);
        self.search_input = true;
        self.query.clear();
        self.matches.clear();
        self.match_index = 0;
    }

    fn visual_row(&self, logical_row: usize) -> usize {
        self.line_offsets
            .get(logical_row)
            .copied()
            .unwrap_or(logical_row)
    }

    fn update_matches(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = if query.is_empty() {
            Vec::new()
        } else {
            self.text
                .lines()
                .enumerate()
                .filter_map(|(row, line)| line.to_lowercase().contains(&query).then_some(row))
                .collect()
        };
        self.match_index = self
            .matches
            .iter()
            .position(|row| self.visual_row(*row) >= self.scroll)
            .unwrap_or(0);
    }

    pub fn search_push(&mut self, c: char) {
        self.query.push(c);
        self.update_matches();
    }

    pub fn search_backspace(&mut self) {
        self.query.pop();
        self.update_matches();
    }

    pub fn accept_search(&mut self) {
        self.search_input = false;
        self.search_origin = None;
        if let Some(row) = self.matches.get(self.match_index) {
            self.scroll = self.visual_row(*row).min(self.max_scroll);
        }
    }

    pub fn cancel_search(&mut self) {
        self.search_input = false;
        self.query.clear();
        self.matches.clear();
        if let Some(scroll) = self.search_origin.take() {
            self.scroll = scroll;
        }
    }

    pub fn next_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        self.match_index = if forward {
            (self.match_index + 1) % self.matches.len()
        } else {
            (self.match_index + self.matches.len() - 1) % self.matches.len()
        };
        self.scroll = self
            .visual_row(self.matches[self.match_index])
            .min(self.max_scroll);
    }

    pub fn next_prompt(&mut self, forward: bool) {
        let row = if forward {
            self.prompt_rows
                .iter()
                .find(|row| self.visual_row(**row) > self.scroll)
        } else {
            self.prompt_rows
                .iter()
                .rev()
                .find(|row| self.visual_row(**row) < self.scroll)
        };
        if let Some(row) = row {
            self.scroll = self.visual_row(*row).min(self.max_scroll);
        }
    }

    pub fn scroll_by(&mut self, delta: i32, max_scroll: usize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta as isize)
            .min(max_scroll);
    }

    pub fn home(&mut self) {
        self.scroll = 0;
    }

    pub fn end(&mut self, max_scroll: usize) {
        self.scroll = max_scroll;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_viewer_is_read_only() {
        let viewer = Viewer::transcript("Transcript", "text", vec![0]);
        assert!(viewer.read_only);
    }

    #[test]
    fn viewer_scroll_is_independent_and_clamped() {
        let mut viewer = Viewer::new("paste", "full text");
        viewer.scroll_by(-3, 10);
        assert_eq!(viewer.scroll, 0);
        viewer.scroll_by(4, 10);
        assert_eq!(viewer.scroll, 4);
        viewer.scroll_by(20, 10);
        assert_eq!(viewer.scroll, 10);
        viewer.home();
        assert_eq!(viewer.scroll, 0);
        viewer.end(10);
        assert_eq!(viewer.scroll, 10);
    }

    #[test]
    fn transcript_search_and_prompt_navigation_share_the_viewport() {
        let mut viewer = Viewer::transcript(
            "transcript",
            "❯ first\nanswer\n❯ second\nresult",
            vec![0, 2],
        );
        viewer.max_scroll = 3;
        viewer.start_search();
        for c in "❯".chars() {
            viewer.search_push(c);
        }
        viewer.accept_search();
        assert_eq!(viewer.matches, vec![0, 2]);
        assert_eq!(viewer.scroll, 0);
        viewer.next_match(true);
        assert_eq!(viewer.scroll, 2);
        viewer.next_match(true);
        assert_eq!(viewer.scroll, 0);
        viewer.next_match(false);
        assert_eq!(viewer.scroll, 2);

        viewer.scroll = 1;
        viewer.next_prompt(false);
        assert_eq!(viewer.scroll, 0);
        viewer.next_prompt(true);
        assert_eq!(viewer.scroll, 2);

        viewer.start_search();
        viewer.search_push('z');
        viewer.accept_search();
        assert!(viewer.matches.is_empty());
        assert_eq!(viewer.scroll, 2);
    }

    #[test]
    fn transcript_search_uses_visual_rows_after_wrapping() {
        let mut viewer = Viewer::transcript("transcript", "short\n❯ wrapped", vec![1]);
        viewer.line_offsets = vec![0, 3];
        viewer.max_scroll = 5;
        viewer.start_search();
        viewer.search_push('❯');
        viewer.accept_search();
        assert_eq!(viewer.scroll, 3);
        viewer.scroll = 0;
        viewer.next_prompt(true);
        assert_eq!(viewer.scroll, 3);
    }
}

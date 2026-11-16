pub struct Viewer {
    pub title: String,
    pub text: String,
    pub scroll: usize,
    pub max_scroll: usize,
}

impl Viewer {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            scroll: 0,
            max_scroll: 0,
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
}

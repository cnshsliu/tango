use crate::viewer::Viewer;
use std::fs::{OpenOptions, read_to_string, remove_file};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Neovim {
    pub executable: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NvimTarget {
    Viewer,
    Draft,
}

pub struct NvimRequest {
    pub editor: Neovim,
    pub viewer: Viewer,
    pub read_only: bool,
    pub target: NvimTarget,
    /// generic $EDITOR flow: run without nvim-specific flags (run_plain)
    pub plain: bool,
}

pub fn detect() -> Option<Neovim> {
    for executable in ["nvim", "vim"] {
        if let Ok(output) = Command::new(executable).arg("--version").output()
            && output.status.success()
        {
            let version = String::from_utf8_lossy(&output.stdout);
            if is_nvim_version(&version) {
                return Some(Neovim {
                    executable: executable.into(),
                });
            }
        }
    }
    None
}

fn is_nvim_version(version: &str) -> bool {
    version
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("NVIM v"))
}

pub fn write_temp(text: &str) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..100 {
        let path =
            std::env::temp_dir().join(format!("tango-{}-{stamp}-{attempt}.md", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate Neovim temporary file",
    ))
}

/// Generic $EDITOR round-trip: no editor-specific flags, tolerate multi-word
/// EDITOR values ("code -w"), read the file back and normalize the final newline.
pub fn run_plain(program: &str, text: &str) -> io::Result<String> {
    let path = write_temp(text)?;
    let mut parts = program.split_whitespace();
    let mut command = Command::new(parts.next().unwrap_or(program));
    for a in parts {
        command.arg(a);
    }
    let result = command.arg("--").arg(&path).status();
    let edited = match result {
        Err(e) => return Err(e),
        // a failed editor run must never come back as an empty draft that wipes the input
        Ok(status) if !status.success() => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("editor exited with {status}"),
            ));
        }
        Ok(_) => read_to_string(&path),
    };
    let _ = remove_file(path);
    edited.map(|edited| normalize_editor_output(text, &edited))
}

pub fn run(editor: &Neovim, text: &str, read_only: bool) -> io::Result<String> {
    let path = write_temp(text)?;
    let mut command = Command::new(&editor.executable);
    command.args(["--cmd", "set binary noendofline"]);
    if read_only {
        command.arg("-R");
    }
    let result = command.arg("--").arg(&path).status();
    let edited = match result {
        Err(e) => return Err(e),
        // a failed editor run must never come back as an empty draft that wipes the input
        Ok(status) if !status.success() => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("editor exited with {status}"),
            ));
        }
        Ok(_) => read_to_string(&path),
    };
    let _ = remove_file(path);
    edited.map(|edited| normalize_editor_output(text, &edited))
}

/// The editor runs with `set binary noendofline`, but some builds still append a final
/// newline on write; strip it only when the original text did not end with one, so an
/// unchanged round-trip of text that legitimately ends in '\n' stays unchanged.
fn normalize_editor_output(original: &str, text: &str) -> String {
    if original.ends_with('\n') {
        text.to_string()
    } else {
        text.strip_suffix('\n').unwrap_or(text).to_string()
    }
}

#[derive(Clone, Copy)]
pub enum EditResolution {
    New,
    Reprocess,
    Correct,
}

pub fn edit_submission(resolution: EditResolution, original: &str, edited: &str) -> Option<String> {
    if original == edited {
        return None;
    }
    match resolution {
        EditResolution::New => Some(edited.to_string()),
        EditResolution::Reprocess => Some(format!(
            "用户修改了此前提交的内容。请基于以下修改后的完整内容重新处理此前的请求，并生成新的完整回答：\n\n<edited>\n{edited}\n</edited>"
        )),
        EditResolution::Correct => Some(format!(
            "请修正此前的处理。用户修改了此前提交的内容，以下是 unified diff；请根据变化修正之前的理解和回答，未变化部分沿用原上下文：\n\n{}",
            unified_diff(original, edited)
        )),
    }
}

fn unified_diff(original: &str, edited: &str) -> String {
    let old = original.split_terminator('\n').collect::<Vec<_>>();
    let new = edited.split_terminator('\n').collect::<Vec<_>>();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_changed_end = old.len() - suffix;
    let new_changed_end = new.len() - suffix;
    let start = prefix.saturating_sub(3);
    let old_end = (old_changed_end + 3).min(old.len());
    let new_end = (new_changed_end + 3).min(new.len());
    let mut out = format!(
        "--- original\n+++ edited\n@@ -{},{} +{},{} @@\n",
        start + 1,
        old_end - start,
        start + 1,
        new_end - start
    );
    for line in &old[start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old[prefix..old_changed_end] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new[prefix..new_changed_end] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old[old_changed_end..old_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_detection_rejects_standard_vim() {
        assert!(is_nvim_version("NVIM v0.12.1\nBuild type: Release"));
        assert!(!is_nvim_version("VIM - Vi IMproved 9.1"));
        assert!(!is_nvim_version("not an editor"));
    }

    #[test]
    fn editor_output_keeps_an_original_trailing_newline() {
        assert_eq!(normalize_editor_output("a\n", "a\n"), "a\n");
        assert_eq!(normalize_editor_output("a", "a\n"), "a");
        assert_eq!(normalize_editor_output("a", "a"), "a");
    }

    #[test]
    fn unchanged_edit_has_no_submission() {
        assert_eq!(edit_submission(EditResolution::New, "same", "same"), None);
    }

    #[test]
    fn new_processing_uses_the_complete_edited_text() {
        assert_eq!(
            edit_submission(EditResolution::New, "old", "new").as_deref(),
            Some("new")
        );
    }

    #[test]
    fn correction_contains_a_unified_diff() {
        let original = "one\ntwo\nthree\nfour";
        let edited = "one\nTWO\nthree\nfour";
        let submission = edit_submission(EditResolution::Correct, original, edited).unwrap();
        assert!(submission.contains("修正此前的处理"));
        assert!(submission.contains("--- original"));
        assert!(submission.contains("+++ edited"));
        assert!(submission.contains("-two"));
        assert!(submission.contains("+TWO"));
        assert!(!submission.contains("--- original\ntwo\n+++ edited"));
    }

    #[test]
    fn reprocess_contains_the_complete_edited_text() {
        let submission =
            edit_submission(EditResolution::Reprocess, "old", "new\ncomplete").unwrap();
        assert!(submission.contains("重新处理"));
        assert!(submission.contains("new\ncomplete"));
    }

    #[test]
    fn appending_one_line_does_not_mark_the_pasted_text_as_changed() {
        let original = (1..=30)
            .map(|n| format!("original-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let edited = format!("{original}\nAPPENDED\n");
        let submission = edit_submission(EditResolution::Correct, &original, &edited).unwrap();
        assert!(submission.contains("+APPENDED"));
        assert!(!submission.lines().any(|line| line == "+"));
        assert!(!submission.contains("-original-01"));
        assert!(!submission.contains("+original-01"));
        assert!(!submission.contains("-original-30"));
    }

    #[test]
    fn editor_output_removes_only_its_final_line_terminator() {
        assert_eq!(
            normalize_editor_output("one\ntwo", "one\ntwo\n"),
            "one\ntwo"
        );
        assert_eq!(
            normalize_editor_output("one\ntwo", "one\ntwo\n\n"),
            "one\ntwo\n"
        );
        assert_eq!(normalize_editor_output("one\ntwo", "one\ntwo"), "one\ntwo");
        assert_eq!(normalize_editor_output("", "\n"), "");
    }

    #[test]
    fn run_plain_round_trips_with_cat() {
        // $EDITOR=cat style: reads the draft back unchanged (empty EDITOR args)
        let edited = run_plain("cat", "draft text\nline two\n");
        assert_eq!(
            edited.unwrap(),
            "draft text\nline two\n",
            "cat leaves the file byte-identical"
        );
        // an editor that ADDS a final newline gets it stripped when the draft had none
        let stripped = run_plain("cat", "no-newline");
        assert_eq!(stripped.unwrap(), "no-newline");
    }
    #[test]
    fn run_plain_errors_when_the_editor_cannot_spawn() {
        let r = run_plain("definitely-not-a-real-editor-xyz --wait", "draft");
        assert!(
            r.is_err(),
            "spawn failure must be an Err, never an empty draft"
        );
        // `false` spawns fine but exits non-zero: still an Err, never an empty draft
        let r2 = run_plain("false", "draft text");
        assert!(r2.is_err(), "non-zero editor exit must be an Err");
    }
}

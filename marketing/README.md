# marketing/: Tango tutorial posts for @TKNetX

One post per new feature. Each post is a short Chinese how-to thread with real screenshots and a
screen recording. The rules from the @TKNetX 发文手册 apply: show the full draft, publish only after
explicit approval, and never put `1/n` in the posted text.

```text
marketing/
  demo-ledger/        demo project: Rust personal ledger on SQLite (add / list / month / serve)
  studio/             recording pipeline
    mock-model.mjs    scripted OpenAI-compatible model (only the model's words are scripted)
    record.py         drives the real tango-tui in tmux → PNG shots + MP4
    render.py         ANSI screen → PNG in a macOS-style window
    weighted.mjs      X weighted length per post (limit 280), flags 本机 and 1/n
    scenarios/*.json  one per post: setup, scripted turns, keystroke tape
    out/              render output (ignored)
  posts/YYYY-MM-DD-<slug>/
    zh-thread.txt     draft with --- n/m --- separators and [媒体: …] lines
    media/            png + mp4 (ignored by git; re-record from the scenario)
    urls.txt          live URLs after publishing
```

## Recording a post

```sh
cd marketing/studio
/opt/homebrew/bin/python3 -m venv .venv && .venv/bin/pip install pillow   # once
.venv/bin/python record.py scenarios/bg-shell.json      # → out/bg-shell/*.png, demo.mp4
node weighted.mjs ../posts/2026-11-22-bg-shell/zh-thread.txt
```

The system Python's Pillow is blocked by macOS code signing, so use the Homebrew venv.

Everything in a recording is real except the model: the release `tango-tui`, its tools, and the
`ledger` binary. Each run uses a temp `TANGO_HOME` and a temp copy of `demo-ledger` outside this
repo, so your `~/.tango` and the repo's project hooks stay out of it. The model id shown is
`anthropic/claude-opus-5-5`, served by `mock-model.mjs` on port 7690. Rebuild `tui/` before
recording so the footage shows the current UI.

Tape ops: `type`, `key` (tmux key names), `wait` (regex on the screen), `sleep`, `shot`, `sh`.
`config` in a scenario is merged into the demo `config.json` (e.g. `nvimViewer: "internal"` so
Ctrl+O opens the built-in viewer rather than the Neovim picker).

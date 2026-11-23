#!/usr/bin/env python3
"""Record a Tango demo: real tango-tui in tmux, scripted model, frames → PNG shots + MP4.

    .venv/bin/python record.py scenarios/<name>.json [--out ../posts/<dir>/media]

Scenario keys:
  cols, rows, title       terminal size and window title
  setup                   shell commands run in the fresh demo workspace before tango starts
  config                  extra keys merged into the demo TANGO_HOME config.json
  turns                   scripted model replies (see mock-model.mjs)
  tape                    actions, each a list:
    ["type", text]            type like a person (per-character)
    ["key", "Enter"|"C-c"…]   tmux key names
    ["wait", regex, secs]     until the screen matches (default 20 s)
    ["sleep", secs]           hold (the video keeps recording)
    ["shot", name]            save the current screen as <name>.png
    ["sh", cmd]               run a shell command in the workspace (not shown)
  cover                   shot name shown for the first 0.6 s of the video, so X's default
                          thumbnail (frame 1) is a real screen (default: the last shot)
    ["mark", "start"]         the video starts here (default: when the first keystroke lands,
                              so the splash screen is not the cover frame)
Everything runs in a throwaway dir: TANGO_HOME and the workspace are temp copies.
"""
import json, os, re, shutil, subprocess, sys, tempfile, threading, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent  # the tango repo
TUI = ROOT / "tui/target/release/tango-tui"
LEDGER = HERE.parent / "demo-ledger"
SOCK = "tango-studio"
PORT = 7690
FPS = 10

sys.path.insert(0, str(HERE))
from render import Renderer  # noqa: E402


def tmux(*a, check=True):
    return subprocess.run(["tmux", "-L", SOCK, *a], capture_output=True, text=True, check=check).stdout


def screen(ansi=True):
    return tmux("capture-pane", "-p", *(["-e"] if ansi else []), "-t", "demo")


def cursor():
    x, y, on = tmux("display", "-p", "-t", "demo", "#{cursor_x} #{cursor_y} #{cursor_flag}").split()
    return (int(x), int(y)) if on == "1" else None


def workspace(sc, tmp):
    """Fresh demo home + a copy of the ledger project (built binary on PATH)."""
    home = tmp / "home"
    home.mkdir()
    (home / "models.json").write_text(json.dumps([{
        "id": "demo/scripted", "provider": "demo", "model": "scripted",
        "api": "openai-completions", "baseUrl": f"http://127.0.0.1:{PORT}/v1", "apiKey": "demo-key",
        "contextWindow": 200000, "maxTokens": 8192, "cost": {"input": 0, "output": 0},
    }]))
    (home / "config.json").write_text(json.dumps({"model": "demo/scripted", **sc.get("config", {})}))
    work = tmp / "demo-ledger"  # tmp is also $HOME, so the TUI shows ~/demo-ledger
    shutil.copytree(LEDGER, work, ignore=shutil.ignore_patterns("target", "*.db"))
    subprocess.run(["cargo", "build", "-q", "--release"], cwd=LEDGER, check=True)
    bindir = tmp / "bin"
    bindir.mkdir()
    shutil.copy(LEDGER / "target/release/ledger", bindir / "ledger")
    subprocess.run("git init -q && git add -A && git -c user.name=demo -c user.email=demo@x commit -qm init",
                   shell=True, cwd=work, check=True)
    return home, work, bindir


class Video(threading.Thread):
    """Grabs the pane about FPS times a second. Each frame keeps its wall-clock time so the
    MP4 plays at real speed even when rendering is slower than FPS; unchanged screens are
    not re-rendered, they just extend the previous frame."""

    def __init__(self, r, title, frames):
        super().__init__(daemon=True)
        self.r, self.title, self.frames = r, title, frames
        self.stop = threading.Event()
        self.log = []  # (png path, start time)
        self.last = None
        self.cut = None  # video cut-in time

    def run(self):
        while not self.stop.is_set():
            t = time.time()
            try:
                key = (screen(), cursor())
                if key != self.last:
                    out = self.frames / f"{len(self.log):05d}.png"
                    self.r.render(key[0], key[1], self.title).save(out, compress_level=1)
                    self.log.append((out, t))
                    self.last = key
            except Exception as e:  # pane gone at the end
                print("frame:", e, file=sys.stderr)
            time.sleep(max(0, 1 / FPS - (time.time() - t)))

    def encode(self, mp4, end, cover=None):
        lst = self.frames / "list.txt"
        rows = []
        if self.cut:
            k = max([i for i, (_, t) in enumerate(self.log) if t <= self.cut] or [0])
            self.log = [(self.log[k][0], self.cut)] + [x for x in self.log if x[1] > self.cut]
        if cover:
            rows.append(f"file '{cover}'\nduration 0.6")
        for i, (p, t) in enumerate(self.log):
            nxt = self.log[i + 1][1] if i + 1 < len(self.log) else end
            rows.append(f"file '{p}'\nduration {max(nxt - t, 0.04):.3f}")
        rows.append(f"file '{self.log[-1][0]}'")
        lst.write_text("\n".join(rows) + "\n")
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", str(lst),
                        "-vf", "fps=30,scale=1920:-2:flags=lanczos,format=yuv420p",
                        "-c:v", "libx264", "-preset", "slow", "-crf", "20", "-movflags", "+faststart", str(mp4)],
                       check=True)


def play(tape, work, out, video):
    for step in tape:
        op, *a = step
        if op == "type":
            if video.cut is None:
                video.cut = time.time()
            for ch in a[0]:
                tmux("send-keys", "-t", "demo", "-l", ch)
                time.sleep(0.045 if ch.isascii() else 0.09)
        elif op == "key":
            tmux("send-keys", "-t", "demo", a[0])
            time.sleep(0.15)
        elif op == "wait":
            pat, limit = re.compile(a[0]), (a[1] if len(a) > 1 else 20)
            t0 = time.time()
            while not pat.search(screen(ansi=False)):
                if time.time() - t0 > limit:
                    (out / "TIMEOUT.txt").write_text(screen(ansi=False))
                    raise SystemExit(f"timed out waiting for /{a[0]}/ — screen saved to {out}/TIMEOUT.txt")
                time.sleep(0.1)
        elif op == "sleep":
            time.sleep(a[0])
        elif op == "shot":
            time.sleep(0.3)  # let the frame settle
            video.r.render(screen(), cursor(), video.title).save(out / f"{a[0]}.png")
            (out / f"{a[0]}.txt").write_text(screen(ansi=False))
            print("shot", a[0])
        elif op == "mark":
            video.cut = time.time()
        elif op == "sh":
            subprocess.run(a[0], shell=True, cwd=work, check=True)
        else:
            raise SystemExit(f"unknown tape op {op}")


def main():
    args = sys.argv[1:]
    if not args:
        raise SystemExit(__doc__)
    sc_path = Path(args[0]).resolve()
    sc = json.loads(sc_path.read_text())
    out = Path(args[args.index("--out") + 1]).resolve() if "--out" in args else HERE / "out" / sc_path.stem
    out.mkdir(parents=True, exist_ok=True)
    cols, rows = sc.get("cols", 100), sc.get("rows", 30)
    tmp = Path(tempfile.mkdtemp(prefix="tango-studio-")).resolve()  # /var → /private/var, so ~ matches
    home, work, bindir = workspace(sc, tmp)
    for cmd in sc.get("setup", []):
        subprocess.run(cmd, shell=True, cwd=work, check=True, env={**os.environ, "PATH": f"{bindir}:{os.environ['PATH']}"})
    turns = tmp / "turns.json"
    turns.write_text(json.dumps({"turns": sc["turns"]}, ensure_ascii=False))
    mock = subprocess.Popen(["node", str(HERE / "mock-model.mjs"), str(turns), str(PORT)],
                            env={**os.environ, "MOCK_LOG": str(out / "mock.log")}, stdout=subprocess.DEVNULL)
    time.sleep(0.6)
    env = (f"cd {work} && env HOME={tmp} TANGO_HOME={home} CARGO_HOME={os.environ.get('CARGO_HOME', Path.home() / '.cargo')} RUSTUP_HOME={os.environ.get('RUSTUP_HOME', Path.home() / '.rustup')} PATH={bindir}:$PATH COLORTERM=truecolor "
           f"TERM=xterm-256color LANG=en_US.UTF-8 {TUI} {sc.get('flags', '')}")
    tmux("kill-server", check=False)
    tmux("-f", "/dev/null", "new-session", "-d", "-s", "demo", "-x", str(cols), "-y", str(rows), env)
    tmux("set", "-g", "status", "off")
    frames = out / "frames"
    shutil.rmtree(frames, ignore_errors=True)
    frames.mkdir()
    video = Video(Renderer(cols, rows), sc.get("title", "tango — ~/demo-ledger"), frames)
    video.start()
    try:
        play(sc["tape"], work, out, video)
    finally:
        video.stop.set()
        video.join()
        end = time.time()
        tmux("kill-server", check=False)
        mock.terminate()
        time.sleep(1)
        # anything a demo started from the temp bin dir (e.g. a background server) goes too
        subprocess.run(["pkill", "-f", str(bindir)], check=False)
    if video.log:
        shots = [st[1] for st in sc["tape"] if st[0] == "shot"]
        cover = sc.get("cover") or (shots[-1] if shots else None)
        video.encode(out / "demo.mp4", end, out / f"{cover}.png" if cover else None)
        print("video", out / "demo.mp4", f"{end - video.log[0][1]:.1f}s", len(video.log), "frames")
    shutil.rmtree(frames)
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()

# Grok CLI 动画效果研究报告（只研究，未改代码）

日期：2026-11-21。对象：superagent-ai/grok-cli（3.5k stars，社区版，非 xAI 官方；
没有找到 xAI 官方 CLI）。技术栈：TypeScript + Bun，UI 用 OpenTUI
（@opentui/react，React 风格 TUI 框架，Zig 原生内核，github.com/anomalyco/opentui）。

## 0. 字体大小的结论

终端 TUI 控制不了字体和字号。字体属于终端模拟器（iTerm2/Terminal.app/WezTerm…），
程序只能在等宽字符网格里写字符 + 颜色 + 粗斜体/反色。Grok CLI 一样受此限制。
"看起来字号有变化"的产品（Warp 等）是 GPU 渲染的终端本体，不是 TUI 程序做的。

## 1. grok-cli 当前版本（v1.1.7）实际有的动画

审了 src/ui/app.tsx（5.8k 行）全量，动画只有这几类，全部是
"setInterval 改 state → 重渲染"：

| 效果 | 位置 | 实现 |
| --- | --- | --- |
| 启动 LOGO 星星闪烁 | HeroLogo, app.tsx:196 | 每 900ms tick，每颗星的 fg 颜色从 5 阶灰色调色板里按 `(i*7 + tick*3 + i*tick) % 5` 伪随机换 |
| 加载 spinner | app.tsx:100,4688 | `⬒ ⬔ ⬓ ⬕` 四帧字符，120ms 一帧 |
| 发送中的三格加载条 | PromptLoadingBoxes, app.tsx:4024 | 3 个格子，活动格 `■` 全亮色，拖尾格 `⬝` 用 72% 透明度，其余 22%，左右往返弹跳，120ms 一帧。透明度=hex 颜色向背景色插值 → 24bit truecolor |
| 左侧竖条 `┃` | app.tsx:256,4251; plan.tsx:106 | 用户气泡/计划面板/付款弹窗左边一列 `┃`，颜色按模式（mode）取。**静态色，不滚动** |
| 计时器 | 多处 | 1s interval + Date.now() |

旧版（2025 年，React Ink 时代）曾有：cfonts 打印 magenta→cyan 渐变 GROK ASCII
LOGO（`transitionGradient: true`，一次性打印不是持续动画）、`/ - \ |` spinner、
每 2s 随机换 "Thinking.../Computing..." 动词。

**结论：现在这版 grok-cli 没有"颜色滚动的左侧亮条"。** 你记忆里那个效果，
要么是它的静态彩色 `┃` 条 + spinner 的组合印象，要么是 OpenTUI 官方 demo /
别的工具（OpenTUI demo 里有彩虹字、CRT 扫描条、火焰）。

## 2. 真正"动画多"的是它脚下的框架 OpenTUI

grok-cli 没用到的能力，OpenTUI 都内置了（这大概是你印象里"效果非常多"的来源）：

- **渲染循环**：默认 targetFps=30（可到 60），每帧带 deltaTime。
- **Timeline 动画引擎**（packages/core/src/animation/Timeline.ts）：GSAP 风格，
  duration/ease/loop/alternate/loopDelay，约 20 个缓动函数（linear/inQuad/outExpo…）。
  React 侧 `useAnimationTick(fps, cb)` 在共享动画时钟上打点，state 同步提交。
- **逐单元格后处理特效**（packages/core/src/post/effects.ts，每帧作用于整个
  cell buffer，每个单元格独立 RGB）：
  - `RainbowTextEffect`：白字按 HSV 色相随时间循环变色（speed/saturation/repeats）——这就是"颜色滚动"；
  - `CRTRollingBarEffect`：一条水平亮带自上而下扫屏；
  - `FlamesEffect`：Perlin 噪声驱动的火焰渐变；
  - `CloudsEffect` / `VignetteEffect` / `DistortionEffect`（故障风：整行位移、字符翻转、颜色错乱）。

## 3. "thinking 左侧滚动亮条"的通用实现配方

不需要 OpenTUI，我们 ratatui 也能做，原理就是上面 Rainbow/CRT 的最小版：

```
每帧（80–120ms 一次 tick，只在有 thinking/流式块显示时开）：
  对条子的第 r 行（共 n 行）：
    phase = (t/period − r) mod 1            // 相位随时间推进、随行号偏移 → 滚动
    color = lerp(渐变LUT, phase)            // 如 暗→亮青→暗
    该单元格 fg 或 bg = color（24bit SGR）
```

- 滚动感来自"相位 = 时间 − 行号"：同一条纹带沿竖条向下/向上流动。
- ratatui 自带 diff，实际只重写条子那一列的转义序列，CPU 可忽略。
- 需要的基建只有一样：**比现在 1s 心跳更快的重绘时钟**（仅 busy 时开到 ~10fps）。
- 兼容性：`$COLORTERM=truecolor` 才用 24bit；否则退 256 色立方或静态色；
  另外要给一个 reduced-motion/关动画的开关。

## 4. 和我们 tango TUI 的对照

我们已有：tango 舞者 8 帧（1s/帧）、busy 行 spinner、跳底 pill。缺的只是
"定时相位 + 更快 tick + 逐格颜色"这一层，属于小改动。

## 来源

- https://github.com/superagent-ai/grok-cli （src/ui/app.tsx, src/ui/plan.tsx）
- https://github.com/anomalyco/opentui （packages/core/src/post/effects.ts,
  packages/core/src/animation/Timeline.ts, packages/react/src/hooks/use-animation-tick.ts）

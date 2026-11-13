import type { Tool, ToolSchema } from '../types.js';

/** Built-in and user tools register here with no distinction. */
export class ToolRegistry {
  private tools = new Map<string, Tool<any>>();

  register(tool: Tool<any>): void {
    this.tools.set(tool.name, tool);
  }

  get(name: string): Tool<any> | undefined {
    return this.tools.get(name);
  }

  schemas(): ToolSchema[] {
    return [...this.tools.values()].map(({ name, description, input }) => ({ name, description, input }));
  }

  names(): string[] {
    return [...this.tools.keys()];
  }
}

export function text(s: string) {
  return { content: [{ type: 'text' as const, text: s }] };
}
export function error(s: string) {
  return { content: [{ type: 'text' as const, text: s }], isError: true };
}

const MAX_OUTPUT = 50_000;
export function truncate(s: string, max = MAX_OUTPUT): string {
  if (s.length <= max) return s;
  return s.slice(0, max) + `\n…[truncated ${s.length - max} chars]`;
}

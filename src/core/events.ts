import type { EventName, Handler, TangoEvents, Veto } from './types.js';

/** Minimal typed event bus. Handlers run in registration order; first veto wins. */
export class EventBus {
  private handlers = new Map<EventName, Handler<any>[]>();

  on<K extends EventName>(name: K, handler: Handler<K>): () => void {
    const list = this.handlers.get(name) ?? [];
    list.push(handler);
    this.handlers.set(name, list);
    return () => {
      const l = this.handlers.get(name);
      if (l) this.handlers.set(name, l.filter((h) => h !== handler));
    };
  }

  async emit<K extends EventName>(name: K, payload: TangoEvents[K]): Promise<Veto> {
    for (const h of this.handlers.get(name) ?? []) {
      const r = await h(payload);
      if (r && r.block) return r;
    }
  }
}

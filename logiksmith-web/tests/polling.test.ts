import { afterEach, describe, expect, it, vi } from 'vitest';
import { DashboardClient, decodeChanges, decodeMeta } from '../src/lib/api';

function snapshot(revision: number) {
  return {
    revision,
    connection: { state: 'connected' },
    config: { input: { address: '1/2/3', dpt: '1.001' }, output: { address: '1/2/4', dpt: '1.001' } },
    values: { input: { observed: false }, output: { observed: false, requested: null } },
    timer: { state: 'idle', remaining_ms: null },
    telegrams: [],
    logs: []
  };
}

function meta(revision = '1') {
  return { host_kind: 'embedded', revision, capabilities: { schedules: false, external_inputs: false, http_inputs: false, webhook_inputs: false }, programming_mode: false, runtime: { status: 'ready' }, storage: { status: 'ready' } };
}

function response(value: unknown, status = 200): Response {
  return new Response(value === undefined ? null : JSON.stringify(value), { status, headers: { 'content-type': 'application/json' } });
}

function handlers() {
  return { onSnapshot: vi.fn(), onEvent: vi.fn(), onStreamOpen: vi.fn(), onStreamLost: vi.fn(), onError: vi.fn() };
}

afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); });

describe('embedded metadata and revision polling', () => {
  it('binds default browser timers to the global receiver', async () => {
    const h = handlers();
    const fetchImpl: typeof fetch = async (input) => {
      const url = String(input);
      if (url === '/api/meta') return response({ host_kind: 'desktop', revision: '1', capabilities: {}, programming_mode: true, runtime: { status: 'ready' }, storage: { status: 'ready' } });
      if (url === '/api/snapshot') return response(snapshot(1));
      throw new Error(`unexpected ${url}`);
    };
    vi.stubGlobal('setTimeout', function (this: unknown, callback: TimerHandler, delay?: number) {
      expect(this).toBe(globalThis);
      return 1 as unknown as ReturnType<typeof setTimeout>;
    });
    const client = new DashboardClient({ fetchImpl, handlers: h });
    await client.start();
    expect(h.onError).not.toHaveBeenCalled();
    client.stop();
  });

  it('decodes embedded capabilities and mutation lock state', () => {
    expect(decodeMeta(meta())).toMatchObject({ host: 'embedded', revision: '1', programmingMode: false, mutationLocked: true, capabilities: { schedules: false, externalInputs: false } });
    expect(() => decodeMeta({ ...meta(), revision: 1 })).toThrow(/meta\.revision/);
    expect(() => decodeChanges({ revision: 2, changed_blocks: [] })).toThrow(/changes\.revision/);
  });

  it('polls unchanged responses without emitting a dashboard update', async () => {
    vi.useFakeTimers();
    const calls: string[] = [];
    const fetchImpl: typeof fetch = async (input) => {
      const url = String(input); calls.push(url);
      if (url === '/api/meta') return response(meta());
      if (url === '/api/snapshot') return response(snapshot(1));
      if (url.includes('/api/changes')) return new Response(null, { status: 204 });
      throw new Error(`unexpected ${url}`);
    };
    const h = handlers();
    const client = new DashboardClient({ fetchImpl, handlers: h });
    await client.start();
    expect(calls).toEqual(['/api/meta', '/api/blocks', '/api/snapshot']);
    // Embedded initial compact loading falls back to the desktop snapshot in
    // this fixture, then waits the bounded visible interval before polling.
    await vi.advanceTimersByTimeAsync(999);
    expect(calls.filter((url) => url.includes('/api/changes'))).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls.filter((url) => url.includes('/api/changes'))).toHaveLength(1);
    expect(h.onEvent).not.toHaveBeenCalled();
    client.stop();
  });

  it('emits a coalesced update for a changed revision', async () => {
    vi.useFakeTimers();
    let changeCount = 0;
    const fetchImpl: typeof fetch = async (input) => {
      const url = String(input);
      if (url === '/api/meta') return response(meta());
      if (url === '/api/snapshot') return response(snapshot(1));
      if (url.includes('/api/changes')) {
        changeCount += 1;
        return response({ revision: String(changeCount + 1), changed_blocks: [], executions: [], telegrams_changed: false, logs_changed: false });
      }
      throw new Error(`unexpected ${url}`);
    };
    const h = handlers();
    const client = new DashboardClient({ fetchImpl, handlers: h });
    await client.start();
    await client.pollNow();
    expect(h.onEvent).toHaveBeenCalledWith(expect.objectContaining({ source: 'poll', revision: '2' }));
    client.stop();
  });

  it('uses the five-second interval while the tab is hidden', async () => {
    vi.useFakeTimers();
    const calls: string[] = [];
    const fetchImpl: typeof fetch = async (input) => {
      const url = String(input); calls.push(url);
      if (url === '/api/meta') return response(meta());
      if (url === '/api/blocks') throw new Error('compact fixture unavailable');
      if (url === '/api/snapshot') return response(snapshot(1));
      if (url.includes('/api/changes')) return new Response(null, { status: 204 });
      throw new Error(`unexpected ${url}`);
    };
    const client = new DashboardClient({ fetchImpl, handlers: handlers(), visibility: () => false });
    await client.start();
    await vi.advanceTimersByTimeAsync(4_999);
    expect(calls.filter((url) => url.includes('/api/changes'))).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls.filter((url) => url.includes('/api/changes'))).toHaveLength(1);
    client.stop();
  });

  it('backs off a bounded HTTP failure and recovers with a fresh snapshot', async () => {
    vi.useFakeTimers();
    let changes = 0;
    const fetchImpl: typeof fetch = async (input) => {
      const url = String(input);
      if (url === '/api/meta') return response(meta());
      if (url === '/api/snapshot') return response(snapshot(changes > 0 ? 3 : 1));
      if (url.includes('/api/changes')) { changes += 1; return changes === 1 ? response({ error: 'busy' }, 503) : new Response(null, { status: 204 }); }
      throw new Error(`unexpected ${url}`);
    };
    const h = handlers();
    const client = new DashboardClient({ fetchImpl, handlers: h, maxBackoffMs: 4_000 });
    await client.start();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(h.onStreamLost).toHaveBeenCalled();
    // The failed request doubles the next delay to two seconds.
    await vi.advanceTimersByTimeAsync(1_999);
    expect(h.onSnapshot).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.onSnapshot).toHaveBeenCalledTimes(2);
    client.stop();
  });
});

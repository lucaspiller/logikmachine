import type { DashboardEvent, DisplayBlock, DisplayChanges, DisplayHostMeta, DisplayLog, DisplaySnapshot, DisplayTelegram } from './state';
import type { RevisionToken } from './revision';
import { ApiDecodeError, decodeEvent, decodeSnapshot, loadBlockProjection, loadBlockProjections, loadChanges, loadLogPage, loadMeta, loadSnapshot, loadTelegramPage } from './api';
import { encodeRevisionToken } from './revision';

type FetchLike = typeof fetch;
type EventSourceLike = { onopen: (() => void) | null; onerror: (() => void) | null; addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void; close(): void; };
type EventSourceConstructor = new (url: string) => EventSourceLike;

export interface DashboardClientHandlers { onSnapshot: (snapshot: DisplaySnapshot) => void; onEvent: (event: DashboardEvent) => void; onStreamOpen: () => void; onStreamLost: (error?: string) => void; onError: (error: Error) => void; onMeta?: (meta: DisplayHostMeta) => void; }
export interface DashboardClientOptions {
  handlers: DashboardClientHandlers;
  fetchImpl?: FetchLike;
  /** Passing an EventSource is an explicit desktop compatibility opt-in. */
  eventSource?: EventSourceConstructor;
  reconnectDelayMs?: number;
  visiblePollMs?: number;
  hiddenPollMs?: number;
  maxBackoffMs?: number;
  visibility?: () => boolean;
  setTimeoutImpl?: typeof setTimeout;
  clearTimeoutImpl?: typeof clearTimeout;
}
function revisionCompare(left: RevisionToken, right: RevisionToken): number { const a = BigInt(String(left)); const b = BigInt(String(right)); return a < b ? -1 : a > b ? 1 : 0; }
function mergeSnapshot(snapshot: DisplaySnapshot, changes: DisplayChanges, blocks: DisplayBlock[], telegrams?: DisplayTelegram[], logs?: DisplayLog[]): DisplaySnapshot {
  const byId = new Map(blocks.map((block) => [block.id, block]));
  const nextBlocks = snapshot.blocks.map((block) => byId.get(block.id) ?? block);
  const first = nextBlocks[0];
  return { ...snapshot, revision: changes.revision, blocks: nextBlocks, pendingTimers: nextBlocks.flatMap((block) => block.pendingTimers), executions: nextBlocks.flatMap((block) => block.executions).sort((a, b) => b.executionId - a.executionId), state: first?.state ?? snapshot.state, values: { input: { observed: first?.inputs[0]?.observed ?? snapshot.values.input.observed }, output: { observed: first?.outputs[0]?.observed ?? snapshot.values.output.observed, requested: first?.outputs[0]?.requested ?? snapshot.values.output.requested } }, telegrams: telegrams ?? snapshot.telegrams, logs: logs ?? snapshot.logs, automation: first ? { inputs: first.inputs, outputs: first.outputs, bindings: first.bindings, signalBindings: first.signalBindings, source: first.source } : snapshot.automation };
}

/** Polls a bounded revision cursor; an EventSource is retained only as an explicit legacy seam. */
export class DashboardClient {
  private readonly fetchImpl: FetchLike;
  private readonly EventSourceImpl: EventSourceConstructor | undefined;
  private readonly handlers: DashboardClientHandlers;
  private readonly reconnectDelayMs: number;
  private readonly visiblePollMs: number;
  private readonly hiddenPollMs: number;
  private readonly maxBackoffMs: number;
  private readonly visibility: () => boolean;
  private readonly setTimeoutImpl: typeof setTimeout;
  private readonly clearTimeoutImpl: typeof clearTimeout;
  private source: EventSourceLike | null = null;
  private pollTimer: ReturnType<typeof setTimeout> | null = null;
  private legacyReconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private revision: RevisionToken = 0;
  private running = false;
  private polling = false;
  private snapshotLoaded = false;
  private needsSnapshot = true;
  private backoffMs = 0;
  private snapshot: DisplaySnapshot | null = null;
  private meta: DisplayHostMeta | null = null;
  private selectedBlockId: string | null = null;
  private readonly visibilityListener = (): void => { if (!this.running || this.polling || this.EventSourceImpl) return; if (this.pollTimer !== null) this.clearTimeoutImpl(this.pollTimer); this.pollTimer = null; this.schedulePoll(); };
  constructor(options: DashboardClientOptions) { this.fetchImpl = options.fetchImpl ?? fetch; this.EventSourceImpl = options.eventSource; this.handlers = options.handlers; this.reconnectDelayMs = options.reconnectDelayMs ?? 1_000; this.visiblePollMs = options.visiblePollMs ?? 1_000; this.hiddenPollMs = options.hiddenPollMs ?? 5_000; this.maxBackoffMs = options.maxBackoffMs ?? 30_000; this.visibility = options.visibility ?? (() => typeof document === 'undefined' || document.visibilityState === 'visible'); this.setTimeoutImpl = options.setTimeoutImpl ?? setTimeout; this.clearTimeoutImpl = options.clearTimeoutImpl ?? clearTimeout; }
  async start(): Promise<void> {
    this.running = true;
    if (!this.EventSourceImpl && typeof document !== 'undefined') document.addEventListener('visibilitychange', this.visibilityListener);
    try {
      // Older desktop hosts do not expose /api/meta; their legacy snapshot is
      // still useful, while malformed M15 metadata is reported to the UI.
      if (!this.EventSourceImpl) { try { this.meta = await loadMeta(this.fetchImpl); this.handlers.onMeta?.(this.meta); } catch (error) { if (error instanceof ApiDecodeError) this.handleError(error); this.meta = null; } }
      const snapshot = this.meta?.host === 'embedded' ? await this.loadInitialEmbedded().catch((error) => { if (error instanceof ApiDecodeError) throw error; return loadSnapshot(this.fetchImpl); }) : await loadSnapshot(this.fetchImpl);
      this.acceptSnapshot(snapshot); this.handlers.onStreamOpen(); if (this.EventSourceImpl) this.connectEventSource(); else this.schedulePoll();
    } catch (error) { this.handleError(error); this.handlers.onStreamLost(this.message(error)); this.schedulePoll(this.reconnectDelayMs); }
  }
  stop(): void { this.running = false; if (typeof document !== 'undefined') document.removeEventListener('visibilitychange', this.visibilityListener); if (this.pollTimer !== null) this.clearTimeoutImpl(this.pollTimer); if (this.legacyReconnectTimer !== null) this.clearTimeoutImpl(this.legacyReconnectTimer); this.pollTimer = null; this.legacyReconnectTimer = null; this.source?.close(); this.source = null; }
  /** Useful for deterministic fixture tests and manual recovery controls. */
  async pollNow(): Promise<void> { await this.poll(); }
  /** Keep compact refreshes focused on the block the workbench displays. */
  selectBlock(blockId: string | null): void { this.selectedBlockId = blockId; }
  private acceptSnapshot(snapshot: DisplaySnapshot): void { this.snapshot = this.meta ? { ...snapshot, meta: this.meta } : snapshot; this.revision = snapshot.revision; this.selectedBlockId = this.selectedBlockId ?? snapshot.blocks[0]?.id ?? null; this.snapshotLoaded = true; this.needsSnapshot = false; this.backoffMs = 0; this.handlers.onSnapshot(this.snapshot); }
  private async loadInitialEmbedded(): Promise<DisplaySnapshot> { const { revision, blocks } = await loadBlockProjections(this.fetchImpl); const first = blocks[0]; const source = { revision, connection: { state: 'connected' }, config: { input: first?.inputs[0] ?? { address: '', dpt: '1.001' }, output: first?.outputs[0] ?? { address: '', dpt: '1.001' } }, values: { input: { observed: first?.inputs[0]?.observed ?? null }, output: { observed: first?.outputs[0]?.observed ?? null, requested: first?.outputs[0]?.requested ?? null } }, telegrams: [], logs: [], blocks }; return { ...decodeSnapshot(source), meta: this.meta ?? undefined }; }
  private schedulePoll(delay?: number): void { if (!this.running || this.pollTimer !== null) return; const base = delay ?? (this.backoffMs || (this.visibility() ? this.visiblePollMs : this.hiddenPollMs)); this.pollTimer = this.setTimeoutImpl(() => { this.pollTimer = null; void this.poll(); }, Math.max(0, base)); }
  private async poll(): Promise<void> {
    if (!this.running || this.polling) return; this.polling = true;
    try { if (this.needsSnapshot || !this.snapshotLoaded) this.acceptSnapshot(await loadSnapshot(this.fetchImpl)); else { const { changes } = await loadChanges(this.revision, this.fetchImpl); if (changes) { const comparison = revisionCompare(changes.revision, this.revision); if (comparison > 0) await this.applyChanges(changes); else if (comparison < 0) { this.needsSnapshot = true; this.acceptSnapshot(await loadSnapshot(this.fetchImpl)); } } } this.backoffMs = 0; this.handlers.onStreamOpen(); }
    catch (error) { this.handleError(error); this.handlers.onStreamLost(this.message(error)); this.needsSnapshot = this.isRestart(error); const base = this.visibility() ? this.visiblePollMs : this.hiddenPollMs; this.backoffMs = Math.min(this.maxBackoffMs, this.backoffMs ? this.backoffMs * 2 : base * 2); }
    finally { this.polling = false; this.schedulePoll(this.backoffMs || undefined); }
  }
  private async applyChanges(changes: DisplayChanges): Promise<void> {
    if (!this.snapshot) return;
    if (changes.snapshot) { this.snapshot = this.meta ? { ...changes.snapshot, meta: this.meta } : changes.snapshot; this.revision = changes.revision; this.handlers.onEvent({ kind: 'update', revision: changes.revision, snapshot: this.snapshot, source: 'poll' }); return; }
    const selected = this.selectedBlockId ?? this.snapshot.blocks[0]?.id;
    const blocks = selected && (changes.changedBlocks.includes(selected) || changes.executions.length > 0) ? [await loadBlockProjection(selected, this.fetchImpl)] : [];
    const [telegrams, logs] = await Promise.all([changes.telegramsChanged ? loadTelegramPage(this.fetchImpl, changes.cursors.telegrams) : Promise.resolve(undefined), changes.logsChanged ? loadLogPage(this.fetchImpl, changes.cursors.logs) : Promise.resolve(undefined)]);
    this.snapshot = mergeSnapshot(this.snapshot, changes, blocks, telegrams, logs); this.revision = changes.revision; this.handlers.onEvent({ kind: 'update', revision: changes.revision, snapshot: this.snapshot, source: 'poll' });
  }
  private scheduleLegacyReconnect(): void { if (!this.running || this.legacyReconnectTimer !== null) return; this.legacyReconnectTimer = this.setTimeoutImpl(() => { this.legacyReconnectTimer = null; this.connectEventSource(); }, this.reconnectDelayMs); }
  private connectEventSource(): void { if (!this.running || !this.EventSourceImpl) return; this.source?.close(); const source = new this.EventSourceImpl(`/api/events?since=${encodeURIComponent(encodeRevisionToken(this.revision))}`); this.source = source; source.onopen = () => this.handlers.onStreamOpen(); source.onerror = () => { if (this.source !== source) return; source.close(); this.source = null; this.handlers.onStreamLost('The browser event stream disconnected.'); this.scheduleLegacyReconnect(); }; source.addEventListener('update', (event) => this.handleEvent('update', event)); source.addEventListener('resync', (event) => this.handleEvent('resync', event)); }
  private handleEvent(name: string, event: MessageEvent<string>): void { try { const decoded = decodeEvent(event.data, name, event.lastEventId); if (decoded.kind === 'resync') { this.needsSnapshot = true; void this.poll(); return; } if (revisionCompare(decoded.revision, this.revision) <= 0) return; this.revision = decoded.revision; this.handlers.onEvent(decoded); } catch (error) { this.handleError(error); this.source?.close(); this.source = null; this.handlers.onStreamLost('The browser event stream sent malformed data.'); this.scheduleLegacyReconnect(); } }
  private handleError(error: unknown): void { this.handlers.onError(error instanceof Error ? error : new Error(String(error))); }
  private message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
  private isRestart(error: unknown): boolean { return error instanceof Error && /Changes request failed \((404|409|410|503)\)/.test(error.message); }
}

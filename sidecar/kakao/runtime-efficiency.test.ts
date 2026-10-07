import { afterEach, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { antiEntropyInterval, startSidecar, type Listener } from './runtime';
import type { Client, Config, Message } from './types';
const cleanups: Array<() => void> = [];
afterEach(() => { for (const cleanup of cleanups.splice(0).reverse()) cleanup(); });
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
async function until(condition: () => boolean) {
  const deadline = Date.now() + 3000;
  while (!condition()) { if (Date.now() >= deadline) throw new Error('test_condition_timeout'); await sleep(5); }
}
function fixture() {
  const dir = mkdtempSync(join(tmpdir(), 'loco-efficient-'));
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  const config: Config = { socket_path: join(dir, 'loco.sock'), hub_socket: join(dir, 'hub.sock'), state_dir: dir,
    expected_user_id: '42', credential_service: 'test', credential_account: 'test', allowed_chat_ids: ['100', '101'],
    mode: 'shadow', attachment_roots: [] };
  const pages: string[] = [];
  let metadata = 0, connected = true, unlocked = 0;
  let session: Parameters<Client['onSessionEvent']>[0] = () => {};
  let message: (value: Message) => void = () => {};
  const client: Client = {
    getCredentials: () => ({ userId: '42' }), isConnected: () => connected,
    acquireSession: async () => { if (!connected) { connected = true; session({ type: 'connected' }); } },
    close: () => { connected = false; },
    getChats: async () => { metadata++; return config.allowed_chat_ids.map(chat_id => ({ chat_id, title: 'test', display_name: 'test' })); },
    getLatestLogId: async () => '10',
    getMessagePage: async chat => { pages.push(chat); return { messages: [], complete: true, next_cursor: null }; },
    sendMessage: async () => { throw new Error('unexpected_write'); },
    sendAttachment: async () => { throw new Error('unexpected_write'); },
    onSessionEvent: handler => { session = handler; return () => {}; },
  };
  const listener: Listener = { start: async () => {}, stop: () => {}, on: (event: string, handler: any) => {
    if (event === 'message') message = handler;
  } };
  const dependencies = { credentials: async () => ({ oauthToken: 'fake', userId: '42', deviceUuid: 'test', deviceType: 'tablet' as const }),
    createClient: async () => client, createListener: () => listener, lock: () => () => { unlocked++; } };
  return { config, client, dependencies, pages, metadata: () => metadata, unlocked: () => unlocked,
    push: (chat = '100') => message({ chat_id: chat, log_id: '10', author_id: '43', message: 'test', sent_at: 1 }),
    disconnect: () => { connected = false; session({ type: 'disconnected' }); } };
}
async function start(f: ReturnType<typeof fixture>) {
  const app = await startSidecar(f.config, f.dependencies); cleanups.push(app.close);
  expect(app.status().status).toBe('ready'); return app;
}
describe('push-driven room synchronization', () => {
  test('initial metadata and catch-up run once; idle generates no short-interval traffic', async () => {
    const f = fixture(); await start(f);
    expect(f.metadata()).toBe(1); expect(f.pages).toEqual(['100', '101']); await sleep(150);
    expect(f.metadata()).toBe(1); expect(f.pages).toEqual(['100', '101']);
  });
  test('bursts for room A sync only A and never refresh all room metadata', async () => {
    const f = fixture(); await start(f); f.pages.length = 0;
    for (let i = 0; i < 20; i++) f.push('100');
    await until(() => f.pages.length === 1); await sleep(80);
    expect(f.pages).toEqual(['100']); expect(f.metadata()).toBe(1);
    f.push('999'); await sleep(80); expect(f.pages).toEqual(['100']);
  });
  test('push during in-flight catch-up remains scheduled for its own room', async () => {
    const f = fixture(); await start(f); f.pages.length = 0;
    let release!: () => void; const pending = new Promise<void>(resolve => { release = resolve; });
    f.client.getMessagePage = async chat => { f.pages.push(chat); if (chat === '100') await pending;
      return { messages: [], complete: true, next_cursor: null }; };
    f.push('100'); await until(() => f.pages.includes('100'));
    f.push('101'); await sleep(80); release(); await until(() => f.pages.includes('101'));
    expect(f.pages).toEqual(['100', '101']); expect(f.metadata()).toBe(1);
  });
  test('reconnect refreshes metadata and catches up every approved room', async () => {
    const f = fixture(); const app = await start(f); f.pages.length = 0; f.disconnect();
    await until(() => f.pages.length >= 2 && app.status().status === 'ready');
    expect(f.pages).toEqual(['100', '101']); expect(f.metadata()).toBe(2);
  });
  test('anti-entropy defaults to 30 seconds and rejects fast or invalid polling settings', () => {
    expect(antiEntropyInterval('30000')).toBe(30000); expect(antiEntropyInterval('60000')).toBe(60000);
    for (const value of ['0', '5000', '-1', 'NaN', '3600001', '30000.5']) expect(() => antiEntropyInterval(value)).toThrow();
  });
  test('startup SIGTERM releases account lock and never creates a client after credentials resolve', async () => {
    const f = fixture(); let resolveCredentials!: (value: Awaited<ReturnType<typeof f.dependencies.credentials>>) => void; let clients = 0;
    const pending = startSidecar(f.config, { ...f.dependencies,
      credentials: () => new Promise(resolve => { resolveCredentials = resolve; }),
      createClient: async () => { clients++; return f.client; } });
    const stopped = pending.then(() => 'unexpected_success', error => error.message);
    for (const handler of process.listeners('SIGTERM')) handler('SIGTERM');
    resolveCredentials(await f.dependencies.credentials()); expect(await stopped).toBe('sidecar_stopped');
    expect(f.unlocked()).toBe(1); expect(clients).toBe(0);
  });
});

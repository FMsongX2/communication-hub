import { mkdirSync, lstatSync } from 'node:fs';
import { join } from 'node:path';
import { KakaoTalkClient } from '@communication-hub/agent-messenger-kakao/client';
import { KakaoTalkListener } from '@communication-hub/agent-messenger-kakao/listener';
import { Config, ConfigSchema, Client, Message, positiveId } from './types';
import { readCredentials, type KakaoCredentials } from './auth';
import { accountLock } from './lock';
import { Store } from './store';
import { Sender } from './send';
import { Receiver } from './receive';
import { hubRequest, hubCall, serve } from './ipc';

export interface Listener {
  start(): Promise<void>;
  stop(): void;
  on(event: 'message', handler: (message: Message) => void): unknown;
  on(event: 'error', handler: (error: Error) => void): unknown;
}
export interface Dependencies {
  credentials?: (service: string, account: string) => Promise<KakaoCredentials>;
  createClient?: (credentials: KakaoCredentials, syncDir: string) => Promise<Client>;
  createListener?: (client: Client) => Listener;
  lock?: (userId: string) => () => void;
}
export function antiEntropyInterval(value = process.env.KAKAO_LOCO_ANTI_ENTROPY_MS): number {
  const interval = Number(value ?? 30000);
  if (!Number.isSafeInteger(interval) || interval < 30000 || interval > 3600000) {
    throw new Error('anti_entropy_interval_must_be_30000_to_3600000_ms');
  }
  return interval;
}

export async function startSidecar(input: Config, deps: Dependencies = {}) {
  process.umask(0o077);
  const config = ConfigSchema.parse(input);
  const antiEntropyMs = antiEntropyInterval();
  const approved = new Set(config.allowed_chat_ids);
  const dirty = new Set<string>();
  const unlock = (deps.lock ?? accountLock)(config.expected_user_id);
  let closed = false, reason = 'connecting', terminal = false, ready = false;
  let loopRunning = false, backoff = 1000, fullRequested = true, metadataRequested = true;
  let nextFullAt = Date.now() + antiEntropyMs, timerAt = Infinity;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let client: Client | undefined, listener: Listener | undefined, store: Store | undefined;
  let server: Awaited<ReturnType<typeof serve>> | undefined;
  const close = () => {
    if (closed) return;
    closed = true;
    process.off('SIGINT', close);
    process.off('SIGTERM', close);
    if (timer) clearTimeout(timer);
    listener?.stop();
    client?.close();
    server?.close();
    store?.close();
    unlock();
  };
  // Install before credential/session awaits so an interrupted first launch releases its lock.
  process.once('SIGINT', close);
  process.once('SIGTERM', close);
  try {
    mkdirSync(config.state_dir, { recursive: true, mode: 0o700 });
    const stateStat = lstatSync(config.state_dir);
    if (stateStat.isSymbolicLink() || stateStat.uid !== process.getuid?.() || (stateStat.mode & 0o077) !== 0) {
      throw new Error('state_directory_not_private');
    }
    const credentials = await (deps.credentials ?? readCredentials)(config.credential_service, config.credential_account);
    if (closed) throw new Error('sidecar_stopped');
    if (credentials.userId !== config.expected_user_id) throw new Error('credential_account_mismatch');
    const sdkDir = join(config.state_dir, 'sdk-sync');
    mkdirSync(sdkDir, { recursive: true, mode: 0o700 });
    const sdkStat = lstatSync(sdkDir);
    if (sdkStat.isSymbolicLink() || sdkStat.uid !== process.getuid?.() || (sdkStat.mode & 0o077) !== 0) {
      throw new Error('sdk_directory_not_private');
    }
    client = deps.createClient ? await deps.createClient(credentials, sdkDir) : await new KakaoTalkClient(sdkDir).login(credentials);
    if (closed) { client.close(); throw new Error('sidecar_stopped'); }
    store = new Store(config.state_dir, config.expected_user_id);
    const receiver = new Receiver(config, client, store, event => hubRequest(config.hub_socket, event));
    const sender = new Sender(config, client, store, () => ready && !terminal && !closed, async params => {
      const ack = await hubCall(config.hub_socket, { method: 'authorize_kakao_loco', params });
      return ack?.ok === true && ack.result?.status === 'authorized';
    });
    const status = () => ({ status: ready ? 'ready' : 'held', reason: ready ? undefined : reason,
      user_id: config.expected_user_id, connected: client!.isConnected(), mode: config.mode,
      receive_issues: Object.fromEntries(receiver.issues) });
    server = await serve(config.socket_path, async (method, params) => {
      if (method === 'status') return status();
      if (method === 'send') return sender.send(params);
      if (method === 'list_rooms') {
        if (terminal || !client!.isConnected()) return status();
        const rooms = await client!.getChats({ all: true, resolveTitles: true });
        for (const room of rooms) receiver.setTitle(room.chat_id, room.title ?? room.display_name ?? room.chat_id);
        return { status: 'ready', user_id: config.expected_user_id, rooms };
      }
      throw new Error('unknown_method');
    });
    if (closed) { server.close(); throw new Error('sidecar_stopped'); }
    const isAuthError = (error: any) => ['invalid_access_token', 'login_rejected', 'not_authenticated', 'client_closed'].includes(error?.code)
      || [-950, -951, -952].includes(error?.serverStatus);
    const schedule = (delay: number) => {
      if (closed || terminal) return;
      const due = Date.now() + Math.max(0, delay);
      // A stream of pushes must not continually postpone already scheduled work.
      if (timer && timerAt <= due) return;
      if (timer) clearTimeout(timer);
      timerAt = due;
      timer = setTimeout(() => {
        timer = undefined;
        timerAt = Infinity;
        void reconcile();
      }, Math.max(0, due - Date.now()));
    };
    const reconcile = async () => {
      if (loopRunning || terminal || closed) return;
      loopRunning = true;
      // Remove only this pass's work. Pushes received across awaits stay dirty for the next pass.
      const batch = new Set(dirty);
      dirty.clear();
      let refreshMetadata = false;
      try {
        await listener!.start();
        if (terminal || closed) return;
        await client!.acquireSession();
        if (terminal || closed) return;
        refreshMetadata = metadataRequested;
        metadataRequested = false;
        const full = fullRequested || Date.now() >= nextFullAt;
        fullRequested = false;
        if (refreshMetadata) {
          const rooms = await client!.getChats({ all: true, resolveTitles: false });
          if (terminal || closed) return;
          for (const room of rooms) receiver.setTitle(room.chat_id, room.title ?? room.display_name ?? room.chat_id);
        }
        if (full) {
          await receiver.syncAll();
          nextFullAt = Date.now() + antiEntropyMs;
        } else {
          for (const chat of batch) {
            if (terminal || closed) return;
            await receiver.sync(chat);
          }
        }
        if (terminal || closed) return;
        ready = true;
        reason = '';
        backoff = 1000;
      } catch (error) {
        ready = false;
        for (const chat of batch) dirty.add(chat);
        // Partial catch-up remains retryable without discarding the failed room.
        fullRequested = true;
        metadataRequested ||= refreshMetadata;
        if (terminal) { /* Preserve KICKOUT and stop; no automatic takeover. */ }
        else if (isAuthError(error)) { terminal = true; reason = 'auth_required'; client!.close(); }
        else { reason = [...receiver.issues.values()][0] ?? 'reconnecting'; backoff = Math.min(backoff * 2, 30000); }
      } finally {
        loopRunning = false;
        schedule(ready ? (dirty.size || fullRequested || metadataRequested ? 0 : nextFullAt - Date.now()) : backoff);
      }
    };
    client.onSessionEvent(event => {
      if (event.type === 'kicked') {
        terminal = true; ready = false; reason = 'kicked';
        if (timer) clearTimeout(timer);
        client!.close();
      } else if (event.type === 'disconnected') {
        ready = false; reason = 'reconnecting'; fullRequested = true; metadataRequested = true;
        schedule(backoff);
      } else if (event.type === 'connected') {
        fullRequested = true; metadataRequested = true;
        schedule(0);
      }
    });
    listener = deps.createListener ? deps.createListener(client) : new KakaoTalkListener(client as KakaoTalkClient);
    listener.on('message', message => {
      if (!approved.has(message.chat_id) || !positiveId(message.log_id)) return;
      receiver.notify(message);
      dirty.add(message.chat_id);
      schedule(50);
    });
    listener.on('error', error => {
      ready = false;
      if (isAuthError(error)) { terminal = true; reason = 'auth_required'; }
      else if (!terminal) { reason = 'listener_error'; schedule(backoff); }
    });
    await reconcile();
    if (closed) throw new Error('sidecar_stopped');
    return { close, status, server };
  } catch (error) { close(); throw error; }
}

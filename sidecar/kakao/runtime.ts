import { z } from 'zod';
import { mkdirSync, lstatSync } from 'node:fs';
import { join } from 'node:path';
import { KakaoTalkClient } from '@communication-hub/agent-messenger-kakao/client';
import { KakaoTalkListener } from '@communication-hub/agent-messenger-kakao/listener';
import { Config, ConfigSchema, Client, Message, positiveId, MAX_FRAME } from './types';
import { readCredentials, type KakaoCredentials } from './auth';
import { accountLock } from './lock';
import { Store } from './store';
import { Sender } from './send';
import { Receiver } from './receive';
import { RoomPolicy } from './policy';
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
  const initialStaticIds=new Set(config.allowed_chat_ids);
  if(config.dynamic_room_policy)config.allowed_chat_ids=[];
  const approved = new Set(config.allowed_chat_ids);
  const dirty = new Set<string>();
  const unlock = (deps.lock ?? accountLock)(config.expected_user_id);
  let closed = false, reason = 'connecting', terminal = false, ready = false;
  let loopRunning = false, backoff = 1000, fullRequested = true, metadataRequested = true;
  let nextFullAt = Date.now() + antiEntropyMs, timerAt = Infinity;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let policyTimer: ReturnType<typeof setInterval> | undefined;
  let policy: RoomPolicy | undefined;
  let wake=()=>{};
  let bootstrapping=true;
  let client: Client | undefined, listener: Listener | undefined, store: Store | undefined;
  let server: Awaited<ReturnType<typeof serve>> | undefined;
  const close = () => {
    if (closed) return;
    closed = true;
    process.off('SIGINT', close);
    process.off('SIGTERM', close);
    if (timer) clearTimeout(timer);
    if(policyTimer)clearInterval(policyTimer);
    policy?.close();
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
    if(config.dynamic_room_policy) {
      policy=new RoomPolicy(config,receiver,approved,(added,healthy)=>{
        for(const chat of dirty)if(!approved.has(chat))dirty.delete(chat);
        if(!healthy){ready=false;reason='policy_unavailable';}
        for(const chat of added)dirty.add(chat);
        if(added.length||!ready)wake();
      },initialStaticIds);
    }
    const sender = new Sender(config, client, store, () => ready && !terminal && !closed, async params => {
      const ack = await hubCall(config.hub_socket, { method: 'authorize_kakao_loco', params });
      return ack?.ok === true && ack.result?.status === 'authorized';
    },chat=>!config.dynamic_room_policy?receiver.scope(chat):policy?.healthy&&receiver.roomReady(chat)?receiver.scope(chat):null,chat=>receiver.approvalEpoch(chat));
    const status = () => ({ status: ready ? 'ready' : 'held', reason: ready ? undefined : reason,
      user_id: config.expected_user_id, connected: client!.isConnected(), mode: config.mode,
      receive_issues: Object.fromEntries(receiver.issues),
      ...(policy?{policy_revision:policy.revision,policy_status:policy.healthy?'ready':'unavailable',rooms:[...approved].map(chat_id=>({chat_id,status:receiver.roomReady(chat_id)?'ready':receiver.issues.has(chat_id)?'held':'sync_pending',reason:receiver.issues.get(chat_id)}))}:{}) });
    server = await serve(config.socket_path, async (method, params) => {
      if (method === 'status') return status();
      if(method==='catalog_diagnostics') {
        z.object({}).strict().parse(params);
        if(terminal||closed||client!.getCredentials().userId!==config.expected_user_id)throw new Error('transport_not_ready');
        const diagnostics=client!.getCatalogDiagnostics?.();
        if(!diagnostics)throw new Error('catalog_diagnostics_unavailable');
        const result={status:'ready',user_id:config.expected_user_id,diagnostics};
        if(Buffer.byteLength(JSON.stringify(result))>MAX_FRAME-256)throw new Error('catalog_diagnostics_frame_limit');
        return result;
      }
      if (method === 'send') return sender.send(params);
      if(method==='refresh_policy') {
        z.object({}).strict().parse(params);
        if(!policy)throw new Error('dynamic_policy_disabled');
        await policy.refresh();
        return {status:'ready',user_id:config.expected_user_id,revision:policy.revision,rooms:status().rooms};
      }
      if (method === 'room_info' || method === 'message_page') {
        const id = z.string().refine(positiveId);
        const roomSchema = z.object({chat_id:id}).strict();
        const pageSchema = z.object({chat_id:id,from:z.string().refine(v=>v==='0'||positiveId(v)),
          count:z.number().int().min(1).max(20)}).strict();
        const request = method === 'room_info' ? roomSchema.parse(params) : pageSchema.parse(params);
        if(policy)await policy.refresh();
        const scope=receiver.scope(request.chat_id);
        const assertApproved=()=>{if(!scope||receiver.scope(request.chat_id)!==scope||!approved.has(request.chat_id)||policy&&!policy.healthy)throw new Error('room_not_approved');};
        assertApproved();
        if (terminal || closed || !client!.isConnected()) throw new Error('transport_not_ready');
        if (client!.getCredentials().userId !== config.expected_user_id) throw new Error('wrong_account');
        if (method === 'room_info') {
          if (!client!.getChat || !client!.getMemberSnapshot) throw new Error('diagnostics_unavailable');
          let chat, members;
          try {
            chat = await client!.getChat(request.chat_id);
            assertApproved();
            members = await client!.getMemberSnapshot(request.chat_id);
          } catch (error:any) {
            const codes = ['get_chat_failed','get_member_snapshot_failed','invalid_access_token','login_rejected','not_authenticated','client_closed'];
            return {status:'held',reason:'room_info_failed',error_code:codes.includes(error?.code)?error.code:'sdk_read_failed'};
          }
          assertApproved();
          const memberIds = members.members.map(member=>member.user_id);
          if (chat.chat_id !== request.chat_id || members.chat_id !== request.chat_id || members.complete !== true
            || chat.active_members !== members.active_members || memberIds.length !== members.active_members
            || !memberIds.every(positiveId) || new Set(memberIds).size !== memberIds.length) throw new Error('invalid_member_snapshot');
          return {status:'ready',chat_id:request.chat_id,type:chat.type,active_members:members.active_members,
            member_ids:memberIds,self_account:config.expected_user_id,complete:true,consistency_basis:members.consistency_basis};
        }
        const pageRequest = pageSchema.parse(params);
        const page = await client!.getMessagePage(pageRequest.chat_id,{from:pageRequest.from,count:pageRequest.count});
        assertApproved();
        if (page.messages.length > pageRequest.count) throw new Error('invalid_message_page');
        const messages = page.messages.map(({log_id,author_id,message,sent_at,type,attachment})=>({log_id,author_id,message,sent_at,type,attachment}));
        const result = {status:'ready',chat_id:pageRequest.chat_id,messages,next_cursor:page.next_cursor,complete:page.complete};
        if (Buffer.byteLength(JSON.stringify(result)) > MAX_FRAME - 256) throw new Error('diagnostic_frame_limit');
        return result;
      }
      if (method === 'list_rooms' || method === 'list_chats' || method === 'resolve_chat') {
        const request=method==='resolve_chat'?z.object({chat_id:z.string().refine(positiveId)}).strict().parse(params):z.object({}).strict().parse(params);
        if (terminal || closed || !client!.isConnected()) throw new Error('transport_not_ready');
        if(client!.getCredentials().userId!==config.expected_user_id)throw new Error('wrong_account');
        const catalog = await client!.getChats({ all: true, resolveTitles: true });
        if(terminal||closed||client!.getCredentials().userId!==config.expected_user_id)throw new Error('transport_not_ready');
        const ids=new Set<string>();
        const rooms=catalog.map(room=>{
          if(!positiveId(room.chat_id)||ids.has(room.chat_id)||typeof room.type!=='string'&&typeof room.type!=='number'||!Number.isSafeInteger(room.active_members)||room.active_members!<0)throw new Error('invalid_catalog');
          ids.add(room.chat_id);
          const name=room.title??room.display_name??room.chat_id;
          receiver.setTitle(room.chat_id,name);
          return {chat_id:room.chat_id,name,type:room.type,member_count:room.active_members};
        });
        if(method==='resolve_chat') {
          const room=rooms.find(room=>room.chat_id===(request as {chat_id:string}).chat_id);
          if(!room)throw new Error('chat_not_found');
          return {status:'ready',user_id:config.expected_user_id,...room};
        }
        const result={status:'ready',user_id:config.expected_user_id,rooms};
        if(Buffer.byteLength(JSON.stringify(result))>MAX_FRAME-256)throw new Error('catalog_frame_limit');
        return result;
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
        if(policy&&!policy.healthy)throw new Error('policy_unavailable');
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
          if(policy) {
            for(const chat of [...approved]) {
              if(!policy.healthy||terminal||closed)return;
              try {await receiver.sync(chat);}catch(error){if(isAuthError(error))throw error;}
            }
          } else await receiver.syncAll();
          nextFullAt = Date.now() + antiEntropyMs;
        } else {
          for (const chat of batch) {
            if (terminal || closed) return;
            if(!approved.has(chat)||policy&&!policy.healthy)continue;
            try{await receiver.sync(chat);}catch(error){if(!policy||isAuthError(error))throw error;}
          }
        }
        if (terminal || closed) return;
        if(policy&&!policy.healthy)throw new Error('policy_unavailable');
        ready = true;
        reason = '';
        backoff = 1000;
      } catch (error) {
        ready = false;
        for (const chat of batch) if(approved.has(chat))dirty.add(chat);
        // Partial catch-up remains retryable without discarding the failed room.
        fullRequested = true;
        metadataRequested ||= refreshMetadata;
        if (terminal) { /* Preserve KICKOUT and stop; no automatic takeover. */ }
        else if (isAuthError(error)) { terminal = true; reason = 'auth_required'; client!.close(); }
        else { reason = policy&&!policy.healthy?'policy_unavailable':[...receiver.issues.values()][0] ?? 'reconnecting'; backoff = Math.min(backoff * 2, 30000); }
      } finally {
        loopRunning = false;
        schedule(ready ? (dirty.size || fullRequested || metadataRequested ? 0 : nextFullAt - Date.now()) : backoff);
      }
    };
    wake=()=>{if(!bootstrapping)schedule(0);};
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
    if(policy){try{await policy.refresh();}catch{}
      let pulling=false;
      policyTimer=setInterval(()=>{if(pulling)return;pulling=true;void policy!.refresh().catch(()=>{}).finally(()=>{pulling=false;});},5000);}
    await reconcile();
    bootstrapping=false;
    if (closed) throw new Error('sidecar_stopped');
    return { close, status, server };
  } catch (error) { close(); throw error; }
}

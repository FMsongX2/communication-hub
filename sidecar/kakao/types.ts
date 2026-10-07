import { z } from 'zod';
import { isAbsolute } from 'node:path';
export const positiveId = (v: unknown): v is string => typeof v === 'string' && /^[1-9][0-9]{0,18}$/.test(v) && BigInt(v) <= 9223372036854775807n;
const id = z.string().refine(positiveId);
const path = z.string().min(1).refine(isAbsolute);
export const ConfigSchema = z.object({
  socket_path: path, hub_socket: path, state_dir: path, expected_user_id: id,
  credential_service: z.string().min(1), credential_account: z.string().min(1),
  allowed_chat_ids: z.array(id).max(100).refine(a => new Set(a).size === a.length),
  mode: z.enum(['shadow', 'active']), attachment_roots: z.array(path).max(32),
}).strict();
export type Config = z.infer<typeof ConfigSchema>;
export const SendSchema = z.object({
  delivery_id: z.string().min(1).max(256), expected_user_id: id, chat_id: id,
  reply: z.string().max(16000), attachment_path: z.preprocess(v=>v===null?undefined:v,path.optional()),
  attachment_sha256: z.preprocess(v=>v===null?undefined:v,z.string().regex(/^[a-f0-9]{64}$/).optional()),
  expires_at: z.number().finite().positive(),
}).strict().refine(p => !!p.reply || !!p.attachment_path)
  .refine(p => !!p.attachment_path === !!p.attachment_sha256);
export type SendParams = z.infer<typeof SendSchema>;
export type Receipt = {
  status: 'sent_verified' | 'held' | 'partial_file_held' | 'sending_uncertain'; reason?: string;
  transport: 'loco'; user_id: string; chat_id: string; text_sent: boolean; attachment_sent: boolean;
  input_started: boolean; side_effects_started: boolean; text_log_id?: string; attachment_log_id?: string;
};
export type Message = {chat_id: string; log_id: string; author_id: number | string; message: string; sent_at: number; type?:number; attachment?:Record<string,unknown>|null};
export type HubEvent = {mode:'shadow'|'active';user_id:string;chat_id:string;log_id:string;author_id:string;body:string;sent_at:number;title:string};
export type SDKReceipt = {success:boolean;status_code:number;chat_id:string;log_id:string;sent_at:number};
export interface Client {
  getCredentials(): {userId:string}; isConnected():boolean; acquireSession():Promise<unknown>; close():void;
  getChats(options?:{all?:boolean;resolveTitles?:boolean}):Promise<Array<{chat_id:string;title:string|null;display_name:string|null}>>;
  getChat?(chatId:string):Promise<{chat_id:string;type:string|number;active_members:number}>;
  getMemberSnapshot?(chatId:string):Promise<{chat_id:string;active_members:number;members:Array<{user_id:string}>;complete:true;consistency_basis:string}>;
  getLatestLogId(chatId:string):Promise<string>;
  getMessagePage(chatId:string,options:{count:number;from:string}):Promise<{messages:Array<Omit<Message,'chat_id'>>;next_cursor:string|null;complete:boolean}>;
  sendMessage(chatId:string,text:string):Promise<SDKReceipt>;
  sendAttachment(chatId:string,data:Uint8Array,filename:string,mimeType?:string):Promise<SDKReceipt>;
  onSessionEvent(handler:(event:{type:'connected'|'disconnected'|'kicked';reason?:string})=>void):()=>void;
}
export const MAX_FRAME = 128 * 1024;
export const MAX_ATTACHMENT = 20 * 1024 * 1024;

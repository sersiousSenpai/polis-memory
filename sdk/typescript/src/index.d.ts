export interface Scope { principal?: string; project?: string; agent?: string; run?: string; org?: string; includeShared?: boolean }
export interface IngestItem { body: string; ts?: number | null; role?: 'user' | 'assistant' | 'agent' | 'system'; session?: string; run?: string; project?: string }
import type {IngestReceipt, ContextBlock, AnswerPack, Claim, ClaimWrite, EvidenceRecord, RetrievalTrace, WriteReceipt, EvidenceFilter, ForgetReceipt} from "./generated.js";
export type {IngestReceipt, ContextBlock, AnswerPack, Claim, ClaimWrite, EvidenceRecord, RetrievalTrace, WriteReceipt, EvidenceFilter, ForgetReceipt} from "./generated.js";
export type ForgetTargetKind = 'ledger_event' | 'prompt' | 'browse_event' | 'note' | 'user_note';
export interface RequestOptions { traceId?: string; signal?: AbortSignal }
export interface ClientOptions { baseUrl?: string; token?: string; scope?: Scope; timeoutMs?: number; fetch?: typeof globalThis.fetch }
export class PolisError extends Error { status?: number; traceId?: string }
export class AuthenticationError extends PolisError {}
export class RejectedError extends PolisError {}
export class NotFoundError extends PolisError {}
export class UnavailableError extends PolisError {}
export class TimeoutError extends PolisError {}
export class Client {
  constructor(options?: ClientOptions);
  ingest(items: IngestItem[], options: RequestOptions & {idempotencyKey: string}): Promise<IngestReceipt>;
  context(q: string, options?: RequestOptions & {maxTokens?: number; roles?: string[]; filter?: EvidenceFilter}): Promise<ContextBlock>;
  search(q: string, options?: RequestOptions & {limit?: number; maxTokens?: number; candidateLimit?: number; cursor?: string; filter?: EvidenceFilter}): Promise<AnswerPack>;
  decide(sourceSeq: number, options?: RequestOptions & {kind?: string}): Promise<WriteReceipt>;
  forget(targetId: number, options: RequestOptions & {confirm: 'forget'; targetKind?: ForgetTargetKind}): Promise<ForgetReceipt>;
  writeClaim(claim: Omit<ClaimWrite, 'scope'>, options?: RequestOptions): Promise<Claim>;
  claims(options?: RequestOptions & {subject?: string; validAt?: number; knownAt?: number}): Promise<Claim[]>;
  evidence(seq: number, options?: RequestOptions & {chainId?: string}): Promise<EvidenceRecord>;
  traces(options?: RequestOptions & {id?: string; limit?: number}): Promise<RetrievalTrace[]>;
  health(options?: RequestOptions): Promise<Record<string, unknown>>;
}

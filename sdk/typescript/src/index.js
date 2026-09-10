/** A dependency-free fetch client; all operations are asynchronous. */
export class PolisError extends Error {
  constructor(message, status, traceId) { super(message); this.name = new.target.name; this.status = status; this.traceId = traceId; }
}
export class AuthenticationError extends PolisError {}
export class RejectedError extends PolisError {}
export class NotFoundError extends PolisError {}
export class UnavailableError extends PolisError {}
export class TimeoutError extends PolisError {}

export class Client {
  constructor({baseUrl = 'http://127.0.0.1:7677', token, scope = {}, timeoutMs = 10000, fetch: transport = globalThis.fetch} = {}) {
    const url = new URL(baseUrl);
    if (!['http:', 'https:'].includes(url.protocol)) throw new TypeError('baseUrl must use HTTP(S)');
    if (!(timeoutMs > 0)) throw new TypeError('timeoutMs must be positive');
    this.baseUrl = baseUrl.replace(/\/$/, ''); this.token = token; this.scope = {...scope};
    this.timeoutMs = timeoutMs; this.transport = transport;
  }
  async request(method, path, {query, body, traceId = globalThis.crypto.randomUUID(), signal} = {}) {
    const url = new URL(this.baseUrl + path);
    for (const [key, value] of Object.entries(query || {})) if (value != null) url.searchParams.set(key, String(value));
    const controller = new AbortController();
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; controller.abort(); }, this.timeoutMs);
    const cancel = () => controller.abort(signal.reason);
    if (signal?.aborted) cancel(); else signal?.addEventListener('abort', cancel, {once: true});
    const headers = {'accept': 'application/json', 'x-polis-trace-id': traceId};
    if (this.token) headers.authorization = `Bearer ${this.token}`;
    if (body !== undefined) headers['content-type'] = 'application/json';
    try {
      const res = await this.transport(url, {method, headers, body: body === undefined ? undefined : JSON.stringify(body), signal: controller.signal});
      const text = await res.text();
      let data;
      try { data = JSON.parse(text); } catch { data = {error: text}; }
      if (!res.ok) {
        const Kind = ({400: RejectedError, 401: AuthenticationError, 403: AuthenticationError, 404: NotFoundError, 503: UnavailableError})[res.status] || PolisError;
        throw new Kind(data.error || `HTTP ${res.status}`, res.status, traceId);
      }
      return data;
    } catch (error) {
      if (timedOut) throw new TimeoutError('Polis request timed out', undefined, traceId);
      if (signal?.aborted || error instanceof PolisError) throw error;
      throw new UnavailableError(String(error), undefined, traceId);
    } finally { clearTimeout(timer); signal?.removeEventListener('abort', cancel); }
  }
  scopeQuery() { const {includeShared = false, ...rest} = this.scope; return {...rest, include_shared: includeShared}; }
  ingest(items, {idempotencyKey, ...options}) {
    if (!idempotencyKey?.trim()) throw new TypeError('idempotencyKey is required');
    if (this.scope.run != null && this.scope.run !== idempotencyKey) throw new TypeError('scope.run must match idempotencyKey when supplied');
    if (items.some(it => it.run != null && it.run !== idempotencyKey)) throw new TypeError('item run conflicts with idempotencyKey');
    return this.request('POST', '/v1/memory/events', {...options, body: {items: items.map(it => ({...it, run: idempotencyKey})), scope: this.scope}});
  }
  filterQuery({roles, validAt, knownAt, ...rest} = {}) { return {...rest, roles: roles?.join(','), valid_at: validAt, known_at: knownAt}; }
  context(q, {maxTokens = 2000, roles, filter, traceId = globalThis.crypto.randomUUID(), ...options} = {}) {
    return this.request('GET', '/v1/memory/context', {...options, traceId, query: {...this.scopeQuery(), ...this.filterQuery(filter), q, max_tokens: maxTokens, ...(roles ? {roles: roles.join(',')} : {}), trace_id: traceId}});
  }
  search(q, {limit = 20, filter, maxTokens, candidateLimit, cursor, traceId = globalThis.crypto.randomUUID(), ...options} = {}) {
    return this.request('GET', '/v1/memory/answer-pack', {...options, traceId, query: {...this.scopeQuery(), ...this.filterQuery(filter), q, limit, max_tokens: maxTokens, candidate_limit: candidateLimit, cursor, trace_id: traceId}});
  }
  decide(sourceSeq, {kind = 'decision', ...options} = {}) {
    return this.request('POST', '/v1/memory/decisions', {...options, body: {sourceSeq, kind, scope: this.scope}});
  }
  forget(targetId, {confirm, targetKind = 'ledger_event', ...options} = {}) {
    if (confirm !== 'forget') throw new TypeError('confirm must be exactly "forget"');
    if (!['ledger_event', 'prompt', 'browse_event', 'note', 'user_note'].includes(targetKind)) throw new TypeError('unsupported forgetting target kind');
    return this.request('POST', '/v1/memory/forget', {...options, body: {targetKind, targetId: String(targetId), confirm, scope: this.scope}});
  }
  writeClaim(claim, options = {}) { return this.request('POST', '/v1/memory/claims', {...options, body: {...claim, scope: this.scope}}); }
  async claims({subject, validAt, knownAt, ...options} = {}) {
    return (await this.request('GET', '/v1/memory/claims', {...options, query: {...this.scopeQuery(), subject, valid_at: validAt, known_at: knownAt}})).claims;
  }
  evidence(seq, {chainId, ...options} = {}) {
    return this.request('GET', `/v1/memory/evidence/${encodeURIComponent(seq)}`, {...options, query: {...this.scopeQuery(), chain_id: chainId}});
  }
  async traces({id, limit = 20, ...options} = {}) {
    return (await this.request('GET', '/v1/memory/traces', {...options, query: {...this.scopeQuery(), id, limit}})).traces;
  }
  health(options) { return this.request('GET', '/v1/memory/health', options); }
}

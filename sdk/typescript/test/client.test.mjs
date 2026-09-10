import assert from 'node:assert/strict';
import {test} from 'node:test';
import {Client, AuthenticationError, TimeoutError} from '../src/index.js';

test('scope, trace correlation and natural ingestion idempotency', async () => {
  const calls = [];
  const client = new Client({token: 't', scope: {principal: 'p', agent: 'a'}, fetch: async (url, options) => {
    calls.push({url, options}); return new Response(JSON.stringify({recorded: [1], skipped: 0, text: 'SQLite [#1]'}));
  }});
  await client.ingest([{body: 'SQLite', role: 'assistant'}], {idempotencyKey: 'decision-1', traceId: 'trace-1'});
  assert.equal(JSON.parse(calls[0].options.body).items[0].run, 'decision-1');
  assert.equal(calls[0].options.headers['x-polis-trace-id'], 'trace-1');
  await client.context('database', {traceId: 'trace-2', filter: {roles: ['assistant'], validAt: 1000}});
  assert.equal(calls[1].url.searchParams.get('principal'), 'p');
  assert.equal(calls[1].url.searchParams.get('valid_at'), '1000');
  assert.equal(calls[1].url.searchParams.get('roles'), 'assistant');
  assert.equal(calls[1].url.searchParams.get('trace_id'), 'trace-2');
  assert.equal(calls[1].url.searchParams.get('include_shared'), 'false');
  assert.throws(() => client.ingest([{body: 'x', run: 'other'}], {idempotencyKey: 'stable'}));
});

test('typed auth error includes trace', async () => {
  const client = new Client({fetch: async () => new Response('{"error":"token required"}', {status: 401})});
  await assert.rejects(client.context('q', {traceId: 't'}), error => error instanceof AuthenticationError && error.traceId === 't');
});

test('timeout aborts in-flight transport', async () => {
  const client = new Client({timeoutMs: 10, fetch: async (_, {signal}) => new Promise((_, reject) => {
    signal.addEventListener('abort', () => reject(new Error('aborted')));
  })});
  await assert.rejects(client.context('q'), TimeoutError);
});

test('caller cancellation reaches transport', async () => {
  const abort = new AbortController();
  const client = new Client({fetch: async (_, {signal}) => new Promise((_, reject) => {
    signal.addEventListener('abort', () => reject(new Error('caller aborted')));
  })});
  const work = client.context('q', {signal: abort.signal});
  abort.abort();
  await assert.rejects(work, /caller aborted/);
});

test('forget requires explicit confirmation and carries the cited source and scope',async()=>{
  const calls=[];const client=new Client({scope:{project:'/private'},fetch:async(url,options)=>{calls.push({url,options});return new Response('{"forgotten":true,"seq":10}');}});
  assert.throws(()=>client.forget(9),/confirm/);assert.equal(calls.length,0);
  await client.forget(9,{confirm:'forget'});
  assert.deepEqual(JSON.parse(calls[0].options.body),{targetKind:'ledger_event',targetId:'9',confirm:'forget',scope:{project:'/private'}});
});


test('forget validates confirmation before dispatch and supports scoped note row IDs', async () => {
  const calls = [];
  const scope = {principal: 'p', project: '/notes', agent: 'annotator', org: 'org'};
  const client = new Client({scope, fetch: async (url, options) => {
    calls.push({url, options}); return new Response(JSON.stringify({forgotten: true, seq: 11}));
  }});
  assert.throws(() => client.forget(17), /confirm/);
  assert.throws(() => client.forget(17, {confirm: 'yes'}), /confirm/);
  assert.throws(() => client.forget(17, {confirm: 'forget', targetKind: 'unsupported'}), /target kind/);
  assert.equal(calls.length, 0);
  assert.equal((await client.forget(17, {confirm: 'forget', targetKind: 'note'})).forgotten, true);
  await client.forget(18, {confirm: 'forget', targetKind: 'user_note', traceId: 'note-18'});
  assert.equal(calls.length, 2);
  assert.equal(calls[0].url.pathname, '/v1/memory/forget');
  assert.deepEqual(JSON.parse(calls[0].options.body), {targetKind: 'note', targetId: '17', confirm: 'forget', scope});
  assert.deepEqual(JSON.parse(calls[1].options.body), {targetKind: 'user_note', targetId: '18', confirm: 'forget', scope});
  assert.equal(calls[1].options.headers['x-polis-trace-id'], 'note-18');
});

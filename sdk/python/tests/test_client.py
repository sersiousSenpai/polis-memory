import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sys
import threading
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from polis_memory import Client, AsyncClient, Scope, RejectedError, AuthenticationError


class ClientContract(unittest.TestCase):
    def setUp(self):
        self.requests = []
        requests = self.requests
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self): self.respond(None)
            def do_POST(self): self.respond(json.loads(self.rfile.read(int(self.headers['content-length']))))
            def respond(self, body):
                requests.append((self.path, body, {k.lower(): v for k, v in self.headers.items()}))
                status = 401 if self.headers.get('authorization') != 'Bearer test' else 200
                self.send_response(status)
                self.send_header('content-type', 'application/json')
                self.end_headers()
                reply = {'error': 'token required'} if status == 401 else ({'forgotten': True, 'seq': 11} if self.path == '/v1/memory/forget' else {'text': 'Use SQLite [#1]', 'terms': ['SQLite'], 'recorded': [1], 'skipped': 0})
                self.wfile.write(json.dumps(reply).encode())
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()

    def test_scope_trace_and_stable_ingestion_key(self):
        client = Client(self.url, token='test', scope=Scope(principal='p', agent='agent-a', project='/project'))
        client.ingest([{'body': 'Choose SQLite', 'role': 'assistant'}], idempotency_key='decision-1', trace_id='trace-1')
        path, body, headers = self.requests[-1]
        self.assertEqual(path, '/v1/memory/events')
        self.assertEqual(body['items'][0]['run'], 'decision-1')
        self.assertEqual(body['scope']['agent'], 'agent-a')
        self.assertEqual(headers['x-polis-trace-id'], 'trace-1')
        client.context('SQLite', filter={'roles': ['assistant'], 'validAt': 1000, 'knownAt': 2000}, trace_id='trace-2')
        self.assertIn('principal=p', self.requests[-1][0])
        self.assertIn('valid_at=1000', self.requests[-1][0])
        self.assertIn('known_at=2000', self.requests[-1][0])
        self.assertIn('roles=assistant', self.requests[-1][0])
        self.assertIn('trace_id=trace-2', self.requests[-1][0])
        self.assertIn('include_shared=false', self.requests[-1][0])

    def test_second_agent_and_async_read(self):
        async def scenario():
            writer = AsyncClient(self.url, token='test', scope=Scope(principal='p', agent='agent-a'))
            reader = AsyncClient(self.url, token='test', scope=Scope(principal='p'))
            await writer.ingest([{'body': 'SQLite chosen', 'role': 'user'}], idempotency_key='decision-1')
            return await reader.context('database')
        self.assertIn('#1', asyncio.run(scenario())['text'])

    def test_typed_authentication_failure_and_idempotency_validation(self):
        with self.assertRaises(AuthenticationError) as caught:
            Client(self.url).context('q', trace_id='t')
        self.assertEqual(caught.exception.trace_id, 't')
        self.assertEqual(caught.exception.status, 401)
        with self.assertRaises(ValueError):
            Client(self.url).ingest([{'body': 'q', 'run': 'different'}], idempotency_key='stable')

    def test_forget_requires_confirmation_and_sends_citation_scope(self):
        client=Client(self.url,token='test',scope=Scope(project='/private'))
        with self.assertRaises(ValueError): client.forget(9,confirm='no')
        self.assertEqual(self.requests,[])
        client.forget(9,confirm='forget',trace_id='erase-9')
        path,body,headers=self.requests[-1]
        self.assertEqual(path,'/v1/memory/forget')
        self.assertEqual(body['targetKind'],'ledger_event')
        self.assertEqual(body['targetId'],'9')
        self.assertEqual(body['scope']['project'],'/private')
        self.assertEqual(headers['x-polis-trace-id'],'erase-9')

    def test_forgetting_requires_confirmation_before_any_request(self):
        client = Client(self.url, token='test')
        with self.assertRaises(TypeError):
            client.forget(9)
        with self.assertRaises(ValueError):
            client.forget(9, confirm='yes')
        with self.assertRaises(ValueError):
            client.forget(9, confirm='forget', target_kind='unsupported')
        self.assertEqual(self.requests, [])

    def test_direct_note_target_sync_and_async_propagate_scope(self):
        scope = Scope(principal='p', project='/notes', agent='annotator', org='org')
        client = Client(self.url, token='test', scope=scope)
        self.assertTrue(client.forget(17, target_kind='note', confirm='forget')['forgotten'])
        path, body, _ = self.requests[-1]
        self.assertEqual(path, '/v1/memory/forget')
        self.assertEqual(body, {'targetKind': 'note', 'targetId': '17', 'confirm': 'forget', 'scope': scope.wire()})
        async def erase_alias():
            client = AsyncClient(self.url, token='test', scope=scope)
            with self.assertRaises(ValueError):
                await client.forget(18, target_kind='user_note', confirm='yes')
            return await client.forget(18, target_kind='user_note', confirm='forget', trace_id='note-18')
        self.assertTrue(asyncio.run(erase_alias())['forgotten'])
        self.assertEqual(len(self.requests), 2)
        _, body, headers = self.requests[-1]
        self.assertEqual(body, {'targetKind': 'user_note', 'targetId': '18', 'confirm': 'forget', 'scope': scope.wire()})
        self.assertEqual(headers['x-polis-trace-id'], 'note-18')

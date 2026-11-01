#!/usr/bin/env python3
"""Opt-in installed-CLI acceptance with temporary data and a local Anthropic peer."""
import http.server
import json
import os
import pathlib
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid


class Model(http.server.BaseHTTPRequestHandler):
    images_seen = False

    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get('content-length', '0'))))
        if 'count_tokens' in self.path:
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(b'{"input_tokens":1}')
            return
        parts = [part for message in body.get('messages', []) for part in message.get('content', []) if isinstance(part, dict)]
        Model.images_seen |= any(part.get('type') == 'image' for part in parts)
        tool_result = any(part.get('type') == 'tool_result' for part in parts)
        write = any('write watcher fixture' in part.get('text', '') for part in parts) and not tool_result
        content = {'type': 'tool_use', 'id': 'tool_fixture', 'name': 'Write', 'input': {'file_path': Model.file_path, 'content': 'Claude watcher fixture\n'}} if write else {'type': 'text', 'text': 'Claude harness fixture reply'}
        message = {'id': 'msg_' + str(uuid.uuid4()), 'type': 'message', 'role': 'assistant', 'model': body.get('model'), 'content': [], 'stop_reason': None, 'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 0}}
        frames = [('message_start', {'type': 'message_start', 'message': message})]
        initial = {**content, 'input': {}} if write else {**content, 'text': ''}
        frames.append(('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': initial}))
        delta = {'type': 'input_json_delta', 'partial_json': json.dumps(content['input'])} if write else {'type': 'text_delta', 'text': content['text']}
        frames.extend([('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}), ('content_block_stop', {'type': 'content_block_stop', 'index': 0}), ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if write else 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 1}}), ('message_stop', {'type': 'message_stop'})])
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        time.sleep(1)
        for event, frame in frames:
            self.wfile.write(('event: ' + event + '\ndata: ' + json.dumps(frame) + '\n\n').encode())
            self.wfile.flush()



class EventObserver:
    def __init__(self, port, scope):
        self.condition = threading.Condition()
        self.pages = {}
        self.failure = None
        subscriptions = [{'id': 'harness', 'plugin': 'harness', 'args': scope}, {'id': 'files', 'plugin': 'projects', 'args': scope}]
        request = urllib.request.Request(f'http://127.0.0.1:{port}/v1/events', data=json.dumps({'version': 1, 'subscriptions': subscriptions}).encode(), headers={'Authorization': 'Bearer fixture-token-0123456789abcdef', 'Content-Type': 'application/json'})
        self.response = urllib.request.urlopen(request, timeout=30)
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            for line in self.response:
                if not line.startswith(b'data: '):
                    continue
                frame = json.loads(line[6:])
                with self.condition:
                    if 'error' in frame:
                        raise RuntimeError(frame['error']['code'])
                    prior = self.pages.get(frame['id'], {})
                    page = frame['data']
                    if frame['id'] == 'harness':
                        page['events'] = (prior.get('events', []) + page['events'])[-1024:]
                    self.pages[frame['id']] = page
                    self.condition.notify_all()
        except Exception as error:
            with self.condition:
                self.failure = error
                self.condition.notify_all()

    def wait(self, channel, predicate=lambda page: True, timeout=30):
        with self.condition:
            ready = self.condition.wait_for(lambda: self.failure or channel in self.pages and predicate(self.pages[channel]), timeout)
            if self.failure:
                raise self.failure
            assert ready, f'No matching {channel} event received'
            return self.pages[channel]


def main():
    command = os.environ.get('CELLS_CLAUDE_COMMAND') or shutil.which('claude')
    assert command, 'Install Claude Code or set CELLS_CLAUDE_COMMAND'
    binary = pathlib.Path(__file__).resolve().parents[4] / 'target/debug/cells'
    assert binary.is_file(), 'Run cargo build first'
    with tempfile.TemporaryDirectory(prefix='hs-real-claude-') as temporary:
        root = pathlib.Path(temporary)
        for name in ['export/project', 'native', 'catalog']:
            (root / name).mkdir(parents=True)
        model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
        threading.Thread(target=model.serve_forever, daemon=True).start()
        with socket.socket() as free:
            free.bind(('127.0.0.1', 0))
            port = free.getsockname()[1]
        config = root / 'cells.toml'
        config.write_text(f'listen="127.0.0.1:{port}"\nexecution=true\nserver_id="real-claude-test"\ntoken="fixture-token-0123456789abcdef"\n[projects]\nroot={json.dumps(str(root/"catalog"))}\nnetwork=true\n[[exports]]\nalias="root"\npath={json.dumps(str(root/"export"))}\naccess="rw"\n[[harnesses]]\nid="claude"\nkind="claude"\ncommand={json.dumps(command)}\nhome={json.dumps(str(root/"native"))}\nprojects=true\n')
        env = {'PATH': os.environ['PATH'], 'HOME': str(root), 'CLAUDE_CONFIG_DIR': str(root / 'native'), 'ANTHROPIC_API_KEY': 'local-fixture-key', 'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{model.server_port}', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1'}
        gateway = subprocess.Popen([str(binary), str(config)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        external = None
        sequence = 0

        def call(name, args):
            nonlocal sequence
            sequence += 1
            data = {'jsonrpc': '2.0', 'id': sequence, 'method': 'tools/call', 'params': {'name': name, 'arguments': args, '_meta': {'io.modelcontextprotocol/protocolVersion': '2026-07-28', 'io.modelcontextprotocol/clientInfo': {'name': 'real-claude-test', 'version': '1'}, 'io.modelcontextprotocol/clientCapabilities': {}}}}
            request = urllib.request.Request(f'http://127.0.0.1:{port}/mcp', data=json.dumps(data).encode(), headers={'Authorization': 'Bearer fixture-token-0123456789abcdef', 'Content-Type': 'application/json', 'mcp-protocol-version': '2026-07-28', 'mcp-method': 'tools/call', 'mcp-name': name})
            result = json.load(urllib.request.urlopen(request, timeout=30))['result']
            assert not result.get('isError'), result.get('structuredContent')
            return result['structuredContent']

        try:
            for _ in range(100):
                try:
                    epoch = call('harness_profiles', {})['epoch']
                    break
                except (OSError, KeyError):
                    time.sleep(.1)
            project = call('project_register', {'name': 'Fixture', 'alias': 'root', 'path': 'project', 'access': 'rw'})['project']
            scope = {'profileId': 'claude', 'projectId': project['id'], 'revision': project['revision']}
            Model.file_path = f'/projects/{project["id"]}/fixture.md'

            def mutate(name, extra):
                receipt = call(name, {**scope, 'epoch': epoch, 'requestId': str(uuid.uuid4()), **extra})
                assert receipt['outcome'] == 'committed', receipt
                return receipt['result']

            created = mutate('harness_create', {'workspaceId': project['id']})['session']
            session_id = created['id']
            observer = EventObserver(port, scope)
            watcher = observer.wait('files')
            turn = mutate('harness_turn', {'sessionId': session_id, 'prompt': 'write watcher fixture'})
            events = observer.wait('harness', lambda page: any(request['params']['threadId'] == session_id for request in page['requests']))
            approval = next(request for request in events['requests'] if request['params']['threadId'] == session_id)
            assert approval, 'Native Write approval was not received'
            queried = mutate('harness_command', {'sessionId': session_id, 'command': 'session:status'})
            state = json.loads(queried['output'])
            assert state['scope'] == 'controlled' and state['activeTurnId'] == turn['turnId']
            mutate('harness_respond', {'nativeRequestId': approval['id'], 'response': {'decision': 'accept'}})
            for _ in range(200):
                ended = call('harness_session_info', {**scope, 'sessionId': session_id})['session']
                if ended['status'] == 'idle':
                    break
                time.sleep(.1)
            assert ended['status'] == 'idle' and ended['owned'] is True
            context = json.loads(mutate('harness_command', {'sessionId': session_id, 'command': 'session:context'})['output'])
            assert isinstance(context['totalTokens'], (float, int))
            mcp = json.loads(mutate('harness_command', {'sessionId': session_id, 'command': 'session:mcp'})['output'])
            assert isinstance(mcp, list)
            assert (root / 'export/project/fixture.md').read_text() == 'Claude watcher fixture\n'
            observer.wait('files', lambda page: page['version'] != watcher['version'])
            for _ in range(100):
                history = call('harness_session_read', {**scope, 'sessionId': session_id, 'toolDetail': 'summary'})
                if 'Claude harness fixture reply' in json.dumps(history):
                    break
                time.sleep(.1)
            assert 'Claude harness fixture reply' in json.dumps(history), history
            mutate('harness_turn', {'sessionId': session_id, 'prompt': 'Inspect the fixture attachment', 'attachments': [{'kind': 'image', 'name': 'fixture.png', 'mimeType': 'image/png', 'content': 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aGZkAAAAASUVORK5CYII='}]})
            for _ in range(200):
                if call('harness_session_info', {**scope, 'sessionId': session_id})['session']['status'] == 'idle':
                    break
                time.sleep(.1)
            assert Model.images_seen, 'Native Claude model request did not include the PNG'
            # A distinct CLI has readable history, but this gateway never adopts it.
            external = subprocess.Popen([command, '-p', '--output-format', 'json', '--setting-sources', '', 'Reply with the fixture text'], cwd=root/'export/project', env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            assert external.wait(timeout=60) == 0
            sessions = call('harness_sessions', scope)['sessions']
            independent = next(session for session in sessions if session['id'] != session_id)
            assert independent['status'] == 'notLoaded' and independent['owned'] is False
            resumed = mutate('harness_resume', {'sessionId': independent['id']})['session']
            assert resumed['owned'] is True
            mutate('harness_turn', {'sessionId': independent['id'], 'prompt': 'Continue the fixture conversation'})
            for _ in range(200):
                resumed_end = call('harness_session_info', {**scope, 'sessionId': independent['id']})['session']
                if resumed_end['status'] == 'idle':
                    break
                time.sleep(.1)
            assert resumed_end['status'] == 'idle'
            version = observer.wait('files')['version']
            (root/'export/project/external.md').write_text('Independent directory write')
            observer.wait('files', lambda page: page['version'] != version)
            print(json.dumps({'native_create': True, 'approval_roundtrip': True, 'native_write': True, 'controlled_idle_owned': True, 'native_history': True, 'external_cli_owned': independent['owned'], 'external_cli_status': independent['status'], 'directory_watch': True, 'external_resume': True, 'native_image': True}))
        finally:
            if external and external.poll() is None:
                external.kill()
                external.wait()
            gateway.terminate()
            try:
                gateway.wait(timeout=10)
            except subprocess.TimeoutExpired:
                gateway.kill()
                gateway.wait()
            model.shutdown()


if __name__ == '__main__':
    main()

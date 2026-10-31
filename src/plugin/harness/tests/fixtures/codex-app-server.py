#!/usr/bin/env python3
"""Deterministic app-server peer: no model access or personal Codex state."""
import json
import copy
import os
import sys
import time

threads = {}
turn_count = 0
awaiting_turn = None

def emit(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    params = request.get('params', {})
    if method == 'initialized':
        continue
    if method is None:
        if awaiting_turn is not None:
            emit(awaiting_turn)
            awaiting_turn = None
        emit({'method': 'serverRequest/resolved', 'params': {'threadId': 'session-1', 'requestId': request['id']}})
        continue
    result = {}
    if method == 'model/list':
        result = {'data': [{'id': 'fixture-model', 'model': 'fixture-model', 'displayName': 'Fixture model', 'isDefault': True, 'defaultReasoningEffort': 'medium', 'supportedReasoningEfforts': [{'reasoningEffort': 'medium'}, {'reasoningEffort': 'high'}]}], 'nextCursor': None}
    elif method == 'skills/list':
        result = {'data':[{'cwd':params['cwds'][0],'skills':[{'name':'fixture-skill','description':'Fixture skill','path':'/harness/skills/fixture/SKILL.md','enabled':True}],'errors':[]}]}
    elif method == 'mcpServerStatus/list':
        result = {'data':[{'name':'fixture','authStatus':'OAuth','tools':{}}],'nextCursor':None}
    elif method == 'thread/start':
        if params.get('sandbox') != ('danger-full-access' if os.environ.get('PI_AGENT_PROJECT') else 'workspace-write'):
            emit({'id': request['id'], 'error': {'code': -32602, 'message': 'sandbox'}})
            continue
        thread_id = 'session-' + str(len(threads) + 1)
        thread = {'id': thread_id, 'cwd': params['cwd'], 'name': '<script>history</script>', 'createdAt': 1791458500, 'updatedAt': 1791458565, 'status': {'type': 'idle'}, 'turns': []}
        threads[thread_id] = thread
        result = {'thread': thread}
    elif method == 'thread/list':
        result = {'data': [thread for thread in threads.values() if bool(thread.get('archived')) == bool(params.get('archived')) and (not params.get('ancestorThreadId') or thread.get('spawnedFromId') == params['ancestorThreadId'])], 'nextCursor': None}
    elif method in ('thread/read', 'thread/resume'):
        thread = threads.get(params['threadId'])
        if thread is None:
            thread = {'id': params['threadId'], 'cwd': '/unauthorized', 'status': {'type': 'idle'}, 'turns': []}
        result = {'thread': copy.deepcopy(thread)}
        if method == 'thread/read' and params.get('includeTurns') is False:
            result['thread']['turns'] = []
    elif method == 'thread/fork':
        original = threads[params['threadId']]
        thread_id = 'session-' + str(len(threads) + 1)
        thread = copy.deepcopy(original)
        thread.update(id=thread_id, cwd=params['cwd'], forkedFromId=original['id'])
        if params.get('lastTurnId') and original.get('name') != 'fixture:ignore-checkpoint':
            index = next(i for i, turn in enumerate(thread['turns']) if turn['id'] == params['lastTurnId'])
            thread['turns'] = thread['turns'][:index + 1]
        threads[thread_id] = thread
        result = {'thread': thread}
    elif method == 'thread/turns/list':
        assert params.get('itemsView') == 'notLoaded'
        turns = list(reversed(threads[params['threadId']]['turns']))
        offset = int(params.get('cursor') or 0)
        size = params.get('limit') or 100
        result = {'data': turns[offset:offset + size], 'nextCursor': str(offset + size) if offset + size < len(turns) else None}
    elif method == 'thread/name/set':
        threads[params['threadId']]['name'] = params['name']
        if params['name'] == 'fixture:foreign-child':
            threads['foreign-child'] = {'id': 'foreign-child', 'cwd': '/unauthorized', 'status': {'type': 'notLoaded'}, 'archived': True, 'spawnedFromId': params['threadId']}
        emit({'method': 'thread/name/updated', 'params': {'threadId': params['threadId'], 'threadName': params['name']}})
    elif method in ('thread/archive', 'thread/unarchive'):
        thread = threads[params['threadId']]
        thread['archived'] = method == 'thread/archive'
        thread['status'] = {'type': 'notLoaded'} if thread['archived'] else {'type': 'idle'}
        result = {} if thread['archived'] else {'thread': thread}
        emit({'method': 'thread/archived' if thread['archived'] else 'thread/unarchived', 'params': {'threadId': thread['id']}})
    elif method == 'thread/delete':
        deleted = [params['threadId']] + [t['id'] for t in threads.values() if t.get('spawnedFromId') == params['threadId']]
        for thread_id in deleted:
            threads.pop(thread_id, None)
            emit({'method': 'thread/deleted', 'params': {'threadId': thread_id}})
    elif method == 'thread/compact/start':
        threads[params['threadId']]['compacted'] = True
        emit({'method':'turn/started','params':{'threadId':params['threadId'],'turn':{'id':'compact-turn','status':'inProgress'}}})
        emit({'method':'turn/completed','params':{'threadId':params['threadId'],'turn':{'id':'compact-turn','status':'completed'}}})
    elif method == 'review/start':
        assert params['delivery'] == 'inline'
        result = {'reviewThreadId':params['threadId'],'turn':{'id':'review-turn'}}
    elif method == 'turn/start':
        prompt = params['input'][0].get('text', '')
        if prompt.startswith('fixture:checkpoint-summary-'):
            expected = prompt.removeprefix('fixture:checkpoint-summary-')
            assert ('summary' not in params) if expected == 'unset' else params['summary'] == expected
            assert not any(key in params for key in ('model', 'effort', 'serviceTier'))
        if params['input'][0].get('text') == 'check-model-selection':
            assert params['model'] == 'fixture-model' and params['effort'] == 'high'
        turn_count += 1
        turn_id = 'turn-' + str(turn_count)
        result = {'turn': {'id': turn_id}}
        thread_id = params['threadId']
        threads[thread_id]['status'] = {'type': 'active'}
        if params['input'][0].get('text', '').startswith('fixture:checkpoint-'):
            status = 'inProgress' if params['input'][0]['text'] == 'fixture:checkpoint-open-turn' else 'completed'
            threads[thread_id]['turns'].append({'id': turn_id, 'status': status, 'items': []})
            threads[thread_id]['status'] = {'type': 'idle'}
            emit({'id': request['id'], 'result': result})
            if status == 'completed':
                emit({'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': turn_id, 'status': status}}})
            continue
        if params['input'][0].get('text') == 'fixture:long-unflushed-response':
            home = os.environ.get('CODEX_HOME', threads[thread_id]['cwd'])
            directory = os.path.join(home, 'sessions')
            os.makedirs(directory, exist_ok=True)
            path = os.path.join(directory, 'fixture-live.jsonl')
            with open(path, 'w') as history:
                history.write(json.dumps({'type': 'response_item', 'payload': {'id': 'old', 'type': 'message', 'role': 'assistant', 'content': [{'text': 'Persisted earlier answer'}]}}) + '\n')
            threads[thread_id]['path'] = path
            text = 'Long response segment. ' * 8192
            threads[thread_id]['turns'] = [{'id': turn_id, 'status': 'inProgress', 'items': [{'id': 'live-answer', 'type': 'agentMessage', 'text': text}]}]
            emit({'id': request['id'], 'result': result})
            emit({'method': 'turn/started', 'params': {'threadId': thread_id, 'turn': {'id': turn_id}}})
            for offset in range(0, len(text), 128):
                emit({'method': 'item/agentMessage/delta', 'params': {'threadId': thread_id, 'turnId': turn_id, 'itemId': 'live-answer', 'delta': text[offset:offset + 128]}})
            continue
        if params['input'][0].get('text') == 'complete-before-start-reply':
            threads[thread_id]['status'] = {'type': 'idle'}
            emit({'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': turn_id, 'status': 'completed'}}})
            emit({'id': request['id'], 'result': result})
            continue
        if params['input'][0].get('text') == 'approval-before-start-reply':
            awaiting_turn = {'id': request['id'], 'result': result}
            emit({'id': 'approval-early', 'method': 'item/commandExecution/requestApproval', 'params': {'threadId': thread_id, 'turnId': turn_id, 'command': 'echo test'}})
            continue
        emit({'id': request['id'], 'result': result})
        emit({'method': 'turn/started', 'params': {'threadId': thread_id, 'turn': {'id': turn_id}}})
        emit({'method': 'item/agentMessage/delta', 'params': {'threadId': thread_id, 'turnId': turn_id, 'delta': '<script>output</script>'}})
        emit({'id': 'approval-1', 'method': 'item/commandExecution/requestApproval', 'params': {'threadId': thread_id, 'turnId': turn_id, 'command': 'echo test'}})
        continue
    elif method == 'turn/interrupt':
        threads[params['threadId']]['status'] = {'type': 'idle'}
        emit({'method': 'turn/completed', 'params': {'threadId': params['threadId'], 'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
    elif method == 'fixture/delay':
        time.sleep(0.1)
    emit({'id': request['id'], 'result': result})

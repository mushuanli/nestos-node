#!/usr/bin/python3
"""Native-shaped SDK peer; persists only fixture transcripts under CLAUDE_CONFIG_DIR."""
import sys, json, os, pathlib, uuid
args=sys.argv[1:]
session=args[args.index('--resume' if '--resume' in args else '--session-id')+1]
if session.endswith('.jsonl'): session=pathlib.Path(session).stem
cwd=os.getcwd()
directory=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'projects'/''.join(c if c.isascii() and c.isalnum() else '-' for c in cwd)
directory.mkdir(parents=True,exist_ok=True)
pending=None
deferred_user=None
model=None
effort=None
metrics_mode=False

def emit(value):
 print(json.dumps(value),flush=True)
def append(value):
 value.update({'cwd':cwd,'sessionId':session,'timestamp':'2026-10-09T03:00:00Z'})
 with (directory/(session+'.jsonl')).open('a') as output:output.write(json.dumps(value)+'\n')
def finish(error=False):
 global pending
 emit({'type':'result','session_id':session,'is_error':error,'subtype':'error_during_execution' if error else 'success', **({'total_cost_usd':0.123} if metrics_mode else {})})
 pending=None
for line in sys.stdin:
 value=json.loads(line)
 if value['type']=='control_request':
  request=value['request']
  if request['subtype']=='set_model':model=request['model']
  if request['subtype']=='apply_flag_settings':effort=request['settings']['effortLevel']
  emit({'type':'control_response','response':{'subtype':'success','request_id':value['request_id'],'response':{'models':[{'value':'fixture-model','displayName':'Fixture model','supportsEffort':True,'supportedEffortLevels':['low','high']}], 'commands':[{'name':'compact','description':'Compact native history','argumentHint':'[focus]','aliases':['condense']}, {'name':'team:verify','description':'Verify this project','argumentHint':'[scope]','aliases':['verify']}]} if request['subtype']=='initialize' else {'totalTokens':1234,'maxTokens':200000} if request['subtype']=='get_context_usage' else {}}})
  if request['subtype']=='interrupt':
   if deferred_user is not None:
    emit(deferred_user);deferred_user=None
   finish(True)
 elif value['type']=='user':
  assert isinstance(value['message']['content'],list)
  text='\n'.join(p.get('text','') for p in value['message']['content'])
  if value.get('priority')=='next':
   assert '_steerTurnId' not in value
   if text=='steer-after-result':finish()
   append({**value,'type':'user'});emit(value)
   if text=='steer-after-result':finish()
   continue
  metrics_mode=text=='metrics-report'
  if metrics_mode:emit({'type':'rate_limit_event','rate_limit_info':{'status':'allowed_warning','utilization':0.8,'rateLimitType':'five_hour','resetsAt':1800000000}})
  if text.split()[0] in ['/compact','/condense']:
   emit({'type':'system','subtype':'compact_boundary','compact_metadata':{'trigger':'manual','pre_tokens':32}});finish();continue
  if text=='native-state-events':
   emit({'type':'system','subtype':'session_state_changed','state':'waiting_for_input'})
   emit({'type':'system','subtype':'local_command','content':'Native state output'})
   finish();continue
  append({**value,'type':'user'})
  if text=='late-admission':
   deferred_user=value;continue
  emit(value)
  if text=='check-model-selection':assert model=='fixture-model' and effort=='high'
  if 'question' in text:
   pending='question-request'
   emit({'type':'control_request','request_id':pending,'request':{'subtype':'can_use_tool','tool_name':'AskUserQuestion','input':{'questions':[{'question':'Which option?','options':[{'label':'A'},{'label':'B'}]}]}}})
  elif 'approval' in text:
   pending='approval-request'
   emit({'type':'control_request','request_id':pending,'request':{'subtype':'can_use_tool','tool_name':'Bash','input':{'command':'printf fixture'}}})
  elif 'wait' not in text:
   if 'tool-roundtrip' in text:
    tool={'type':'assistant','uuid':str(uuid.uuid4()),'message':{'id':'tool-message','content':[{'type':'tool_use','id':'fixture-tool','name':'Bash','input':{'command':'printf fixture'}}]}}
    append(tool);emit(tool)
    result={'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'fixture-tool','content':'private fixture tool output'}]}}
    append(result);emit(result)
   message={'type':'assistant','uuid':str(uuid.uuid4()),'parentUuid':value['uuid'],'message':{'id':'message-'+value['uuid'],'role':'assistant','content':[{'type':'text','text':'Fixture reply'}]}}
   append(message);emit(message);finish()
 elif value['type']=='control_response':
  assert value['response']['request_id']==pending
  assert value['response']['response']['behavior'] in ['allow','deny']
  finish()

#!/usr/bin/env python3
"""Architecture fixtures only: no app router, network, credentials or inference."""
import argparse
import codecs
import hashlib
import json
import sqlite3
from decimal import Decimal
from pathlib import Path

ROOT=Path(__file__).resolve().parents[2]


def decode_sse(chunks, limit=131072):
    decoder=codecs.getincrementaldecoder('utf-8')()
    pending=''; events=[]
    for chunk in chunks:
        pending+=decoder.decode(chunk)
        if len(pending.encode())>limit:
            raise ValueError('frame limit')
        while '\n\n' in pending:
            frame,pending=pending.split('\n\n',1)
            data='\n'.join(line[5:].lstrip(' ') for line in frame.split('\n') if line.startswith('data:'))
            if data:
                events.append('[DONE]' if data=='[DONE]' else json.loads(data))
    pending+=decoder.decode(b'',final=True)
    return events,pending


def terminal(profile, events):
    objects=[e for e in events if isinstance(e,dict)]
    if any(e.get('type') in ('error','response.failed') or 'error' in e for e in objects):
        return 'Failed'
    if profile=='responses':
        return 'Completed' if any(e.get('type')=='response.completed' and e.get('response',{}).get('status')=='completed' for e in objects) else 'Incomplete'
    if profile=='messages':
        stopped=any(e.get('type')=='message_stop' for e in objects)
        reasons=[e.get('delta',{}).get('stop_reason') for e in objects if e.get('type')=='message_delta']
    else:
        stopped='[DONE]' in events
        reasons=[c.get('finish_reason') for e in objects for c in e.get('choices',[]) if c.get('finish_reason')]
    if not stopped or not reasons:
        return 'Incomplete'
    if reasons[-1] in ('tool_use','tool_calls'):
        return 'ToolCallsReady'
    return 'Completed' if reasons[-1] in ('end_turn','stop','stop_sequence') else 'Incomplete'


def ordered_native_results(profile, calls, results):
    ids=[c['id'] for c in calls]
    if len(set(ids))!=len(ids) or set(results)!=set(ids):
        raise ValueError('call/result mismatch')
    ordered=[(c['id'],results[c['id']]) for c in sorted(calls,key=lambda c:c['ordinal'])]
    if profile=='responses':
        return [{'type':'function_call_output','call_id':i,'output':v} for i,v in ordered]
    if profile=='messages':
        return {'role':'user','content':[{'type':'tool_result','tool_use_id':i,'content':v} for i,v in ordered]}
    return [{'role':'tool','tool_call_id':i,'content':v} for i,v in ordered]


def classify(local=False,reported=None,configured=None,official=None,subscription=False):
    if local:
        return 'LocalFree',Decimal(0)
    if reported is not None:
        return 'ReportedCharge',Decimal(reported)
    if configured is not None:
        return 'ConfiguredEstimate',Decimal(configured)
    if official is not None:
        return ('SubscriptionApiEquivalent' if subscription else 'OfficialEstimate'),Decimal(official)
    return 'Untracked',None


def run(out):
    checks=[]
    def check(name, ok):
        if not ok:raise AssertionError(name)
        checks.append(name)
    def rejects(name, fn, error=ValueError):
        try:fn()
        except error:check(name,True)
        else:check(name,False)
    wire=b': heartbeat\n\nevent: response.output_text.delta\ndata: '+json.dumps({'type':'response.output_text.delta','delta':'caf\u00e9'},ensure_ascii=False).encode()+b'\n\nevent: response.completed\ndata: {"type":"response.completed",\ndata: "response":{"status":"completed"}}\n\n'
    expected=None
    for size in [1,3,7,16,1024]:
        decoded,tail=decode_sse([wire[i:i+size] for i in range(0,len(wire),size)])
        if expected is None:expected=decoded
        check('fragmented_utf8_sse_'+str(size),decoded==expected and not tail and decoded[0]['delta']=='caf\u00e9')
    check('responses_terminal_required',terminal('responses',expected)=='Completed' and terminal('responses',expected[:-1])=='Incomplete')
    check('error_after_http_success_not_completion',terminal('responses',expected+[{'type':'error'}])=='Failed')
    check('bare_done_not_chat_success',terminal('chat',['[DONE]'])=='Incomplete')
    check('chat_finish_and_done_required',terminal('chat',[{'choices':[{'finish_reason':'stop'}]},'[DONE]'])=='Completed')
    check('messages_tool_turn_closes_explicitly',terminal('messages',[{'type':'message_delta','delta':{'stop_reason':'tool_use'}},{'type':'message_stop'}])=='ToolCallsReady')
    check('output_length_stop_not_complete_answer',terminal('chat',[{'choices':[{'finish_reason':'length'}]},'[DONE]'])=='Incomplete')
    rejects('malformed_json_rejected',lambda:decode_sse([b'data: {bad}\n\n']),json.JSONDecodeError)
    rejects('bounded_stream_frame',lambda:decode_sse([b'data: '+b'x'*100],limit=32))
    calls=[{'id':'A','ordinal':0},{'id':'B','ordinal':1}]; results={'B':'ApprovalRequired','A':'read complete'}
    for profile,key in [('responses','call_id'),('messages','tool_use_id'),('chat','tool_call_id')]:
        packet=ordered_native_results(profile,calls,results)
        rows=packet['content'] if profile=='messages' else packet
        check('native_order_and_pairing_'+profile,[r[key] for r in rows]==['A','B'])
    rejects('duplicate_native_call_id_rejected',lambda:ordered_native_results('responses',[calls[0],calls[0]],results))
    rejects('missing_result_not_fabricated',lambda:ordered_native_results('messages',calls,{'A':'done'}))
    prior={'instructions':'stable role','tools':[{'name':'read','schema':{'type':'object'}}],'native_history':[{'id':'call-A','opaque':'provider-only'}]}
    prefix=json.dumps(prior,separators=(',',':')).encode()
    next_turn={'prefix':prior,'dynamic':{'screen':'repository','progress':'running'}}
    check('dynamic_append_does_not_rewrite_prefix',json.dumps(next_turn['prefix'],separators=(',',':')).encode()==prefix)
    cumulative={'input_tokens':100,'output_tokens':2};cumulative.update({'output_tokens':7})
    check('cumulative_usage_replaces_not_sums',cumulative['output_tokens']==7)
    inclusive={'input_total':100,'cached_subset':60}
    disjoint={'uncached':40,'cache_read':60,'cache_write':5}
    check('inclusive_and_disjoint_input_semantics',inclusive['input_total']==100 and sum(disjoint.values())==105)
    check('reported_zero_preserved',classify(reported='0',configured='8')==('ReportedCharge',Decimal(0)))
    check('local_is_explicit_free',classify(local=True)==('LocalFree',Decimal(0)))
    check('unpriced_custom_is_untracked_not_zero',classify()==('Untracked',None))
    check('subscription_uses_api_equivalent_estimate',classify(official='2.50',subscription=True)==('SubscriptionApiEquivalent',Decimal('2.50')))
    check('actual_charge_supersedes_estimate',classify(reported='1.25',official='2.50')==('ReportedCharge',Decimal('1.25')))
    db=sqlite3.connect(':memory:',isolation_level=None);db.executescript((ROOT/'docs/architecture/persistence-schema.sql').read_text())
    db.executescript("""
    INSERT INTO projects VALUES('p','Fixture','Active',1,1,1);
    INSERT INTO work_items VALUES('w','p','feature',NULL,NULL,NULL,'Work',1,0,1,1,1,'{}');
    INSERT INTO provider_connections VALUES('conn','OpenAI','Subscription','CodexResponses',1,NULL,1,'Connected','{}');
    INSERT INTO provider_models VALUES('model','conn','fixture-model',1,1,'{}');
    INSERT INTO price_snapshots VALUES('price1','OpenAI','Official','https://example.invalid/pricing','old','fixture',1,1,'{}');
    INSERT INTO price_snapshots VALUES('price2','OpenAI','Official','https://example.invalid/pricing','new','fixture',2,2,'{}');
    INSERT INTO published_prices VALUES('OpenAI','price1',1);
    INSERT INTO inference_requests VALUES('request','embedding','p','w',NULL,NULL,NULL,'Admitted',1,'{}');
    INSERT INTO provider_attempts VALUES('attempt','request',NULL,NULL,1,NULL,'model','price1','Admitted','{}');
    INSERT INTO attempt_costs VALUES('attempt','SubscriptionApiEquivalent',2500000,'USD','price1','Estimated',1);
    INSERT INTO price_overrides VALUES('model',1,'{"input":1}');
    """)
    check('non_conversational_request_keeps_work_attribution',db.execute("SELECT root_id,session_id FROM inference_requests WHERE id='request'").fetchone()==('w',None))
    rejects('one_attempt_number_per_logical_request',lambda:db.execute("INSERT INTO provider_attempts VALUES('duplicate','request',NULL,NULL,1,NULL,'model','price1','Admitted','{}')"),sqlite3.IntegrityError)
    db.execute("UPDATE published_prices SET snapshot_id='price2',revision=2 WHERE provider_key='OpenAI' AND revision=1")
    check('price_refresh_does_not_reprice_historical_attempt',db.execute("SELECT price_snapshot_id FROM provider_attempts WHERE id='attempt'").fetchone()[0]=='price1' and db.execute("SELECT amount_units FROM attempt_costs").fetchone()[0]==2500000)
    check('price_override_survives_refresh',db.execute('SELECT revision FROM price_overrides').fetchone()[0]==1)
    check('stale_price_publisher_loses',db.execute("UPDATE published_prices SET snapshot_id='price1',revision=2 WHERE provider_key='OpenAI' AND revision=1").rowcount==0)
    rejects('price_snapshot_provider_identity_guarded',lambda:db.execute("INSERT INTO published_prices VALUES('Anthropic','price1',1)"),sqlite3.IntegrityError)
    check('router_schema_integrity',db.execute('PRAGMA foreign_key_check').fetchall()==[] and db.execute('PRAGMA integrity_check').fetchone()[0]=='ok')
    db.close()
    out.mkdir(parents=True,exist_ok=True)
    report={'status':'Passed architecture fixture/reference mechanics; not production adapters','date':'2026-11-01','checks':checks,'schemaSha256':hashlib.sha256((ROOT/'docs/architecture/persistence-schema.sql').read_bytes()).hexdigest(),'sqliteVersion':sqlite3.sqlite_version,'network':False,'inference':False,'limitations':['Small reference decoder/assembler and policy examples, not actual Rust transport code','Synthetic usage and tariff numbers, no live prices or billing proof','No live auth, generation, cache hit or subscription entitlement test','No production retry/cancellation/secret-store or performance acceptance']}
    (out/'contract-probe.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'checks':len(checks),'status':report['status']}))

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--output-dir',type=Path,required=True)
    run(parser.parse_args().output_dir.resolve())

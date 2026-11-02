"""Owned native framed helper round trips; all grant/data/effect values are scripted fixtures."""
from copy import deepcopy as cp
import argparse
import json
import os
from pathlib import Path
import selectors
import struct
import subprocess
import time
ROOT=Path(__file__).resolve().parents[2]
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);args=p.parse_args();args.output.mkdir(parents=True,exist_ok=True)
FIX=json.loads((ROOT/'docs/architecture/contracts/helper-fixture.json').read_text());BINARY=str(args.binary.resolve());CHECKS=[];MESSAGES=[]
def check(name,value):
    if not value:raise AssertionError(name)
    CHECKS.append(name)
def frame(value):
    b=json.dumps(value,separators=(',',':')).encode();return struct.pack('<I',len(b))+b
class Helper:
    def __init__(self):self.p=subprocess.Popen([BINARY,'package-helper'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE);self.sel=selectors.DefaultSelector();self.sel.register(self.p.stdout,selectors.EVENT_READ);self.buffer=bytearray()
    def __enter__(self):return self
    def __exit__(self,*_):
        if self.p.poll() is None:self.p.kill()
        self.p.wait(timeout=3);self.sel.close()
    def send(self,v,fragment=False):
        b=frame(v)
        if fragment:
            for byte in b:self.p.stdin.write(bytes([byte]));self.p.stdin.flush()
        else:self.p.stdin.write(b);self.p.stdin.flush()
    def exact(self,n):
        deadline=time.monotonic()+3
        while len(self.buffer)<n:
            remaining=deadline-time.monotonic()
            if remaining<=0 or not self.sel.select(remaining):raise TimeoutError('native helper response timed out')
            b=os.read(self.p.stdout.fileno(),65536)
            if not b:raise EOFError(self.p.stderr.read().decode())
            self.buffer.extend(b)
        result=bytes(self.buffer[:n]);del self.buffer[:n];return result
    def receive(self):
        n=struct.unpack('<I',self.exact(4))[0];assert 0<n<=1048576
        v=json.loads(self.exact(n));entry='package-request' if v.get('method')=='broker.call' else 'package-notification' if 'method' in v else 'package-response'
        MESSAGES.append({'id':'actual-frame-'+str(len(MESSAGES)),'entry':entry,'raw':json.dumps(v,separators=(',',':')),'typedValid':True,'schemaValid':True});return v
    def start(self,active=True):
        self.send(FIX['initialize'],fragment=True);v=self.receive();check('native_handshake_and_fragmented_header_'+str(len(CHECKS)),v['result']['selected']=={'major':1,'minor':0} and v['result']['manifestDigest']==FIX['initialize']['params']['expectedManifestDigest'])
        if active:self.send(FIX['activate']);assert self.receive()['result']['kind']=='Activated'
    def shutdown(self,request_id='session-a:h:99'):
        v=cp(FIX['shutdown']);v['id']=request_id;self.send(v);assert self.receive()['result']['kind']=='ShutdownAcknowledged';self.p.stdin.close();self.p.wait(timeout=3);assert self.p.returncode==0

def reply(callback):return {'jsonrpc':'2.0','id':callback['id'],'result':{'kind':'BrokerCompleted','artifact':FIX['artifact']}}
def valid_callback(v,revoked=False):
    p=v['params'];return not revoked and p['processSessionId']=='session-a' and p['invocationId']=='invocation-a' and p['invocationToken']=='token-a' and p['call']['kind']=='ArtifactRead' and p['call']['artifact']==FIX['artifact']
with Helper() as h:
    h.start();h.send(FIX['invoke']);callback=h.receive();check('callback_uses_same_invocation_and_scoped_artifact',valid_callback(callback));h.send(reply(callback));result=h.receive();check('one_terminal_result_after_callback',result['id']==FIX['invoke']['id'] and result['result']['outcome']['kind']=='Completed');h.shutdown();check('owned_helper_exits_after_shutdown',h.p.poll()==0)
with Helper() as h:
    h.start(False);h.send(FIX['invoke']);check('startup_has_no_activation_authority','error' in h.receive());h.shutdown()
with Helper() as h:
    h.start();h.send(FIX['invoke']);callback=h.receive();h.send(FIX['cancel']);ack=h.receive();check('cancel_ack_is_separate_from_invocation_outcome',ack['id']==FIX['cancel']['id'] and ack['result']['kind']=='CancelAcknowledged');check('scripted_host_revokes_callback_token',not valid_callback(callback,revoked=True));h.send(reply(callback));terminal=h.receive();check('late_broker_result_after_cancel_retains_uncertainty',terminal['id']==FIX['invoke']['id'] and terminal['result']['outcome']['kind']=='OutcomeUnknown');h.shutdown()
with Helper() as h:
    h.start();v=cp(FIX['invoke']);v['params']['registryRevision']='2';h.send(v);check('stale_registry_generation_rejected','error' in h.receive());h.shutdown()
with Helper() as h:
    h.start();h.send(FIX['invoke']);callback=h.receive();v=cp(FIX['activate']);v['id']='session-a:h:6';v['params']['registryRevision']='2';h.send(v);check('activation_waits_for_inflight_invocation','error' in h.receive());h.send(reply(callback));assert h.receive()['result']['outcome']['kind']=='Completed';v['id']='session-a:h:7';h.send(v);check('activation_can_advance_after_quiescence',h.receive()['result']['registryRevision']=='2');old=cp(FIX['invoke']);old['id']='session-a:h:8';h.send(old);check('old_generation_stays_fenced_after_activation','error' in h.receive());h.shutdown()
with Helper() as h:
    h.start();h.send(FIX['invoke']);a=h.receive();breq=cp(FIX['invoke']);breq['id']='session-a:h:6';breq['params'].update({'invocationId':'invocation-b','invocationToken':'token-b','operationId':'op-b'});h.send(breq);b=h.receive();credit_a={'jsonrpc':'2.0','method':'broker.streamCredit','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','streamId':'shared-name','sequence':'1','grantBytes':262144}};credit_b=cp(credit_a);credit_b['params'].update({'invocationId':'invocation-b','invocationToken':'token-b'});h.send(credit_a);ca=h.receive();h.send(credit_b);cb=h.receive();check('credit_windows_are_isolated_by_invocation',ca['params']['invocationId']=='invocation-a' and cb['params']['invocationId']=='invocation-b');h.send(reply(b));br=h.receive();h.send(reply(a));ar=h.receive();check('independent_invocations_finish_out_of_order',br['result']['invocationId']=='invocation-b' and ar['result']['invocationId']=='invocation-a');h.shutdown()
with Helper() as h:
    h.start();h.send(FIX['invoke']);callback=h.receive();h.send(reply(callback));assert h.receive()['result']['kind']=='InvocationFinished';h.send(reply(callback));h.p.stdin.close();h.p.wait(timeout=3);check('duplicate_terminal_callback_closes_session',h.p.returncode!=0)
with Helper() as h:
    h.start();h.send({'jsonrpc':'2.0','id':'session-a:p:unknown','result':{'kind':'BrokerCompleted','artifact':FIX['artifact']}});h.p.stdin.close();h.p.wait(timeout=3);check('unknown_terminal_callback_closes_session',h.p.returncode!=0)
with Helper() as h:
    h.start();wrong=cp(FIX['cancel']);wrong['id']='foreign-session:h:1';h.send(wrong);h.p.stdin.close();h.p.wait(timeout=3);check('wrong_request_nonce_closes_session',h.p.returncode!=0)
credit={'jsonrpc':'2.0','method':'broker.streamCredit','params':{'processSessionId':'session-a','invocationId':'invocation-a','invocationToken':'token-a','streamId':'stream-a','sequence':'1','grantBytes':262144}}
with Helper() as h:
    h.start();h.send(FIX['invoke']);h.receive();h.send(credit);check('credit_notification_does_not_finish_invocation',h.receive()['method']=='package.progress');h.send(credit);h.p.stdin.close();h.p.wait(timeout=3);check('duplicate_credit_sequence_rejected',h.p.returncode!=0)
with Helper() as h:
    h.start();h.send(FIX['invoke']);h.receive();h.send(credit);h.receive();v=cp(credit);v['params'].update({'sequence':'2','grantBytes':1});h.send(v);h.p.stdin.close();h.p.wait(timeout=3);check('cumulative_credit_window_cannot_overflow',h.p.returncode!=0)
with Helper() as h:
    h.start();h.send(FIX['invoke']);h.receive();h.send(FIX['cancel']);h.receive();h.send(credit);h.p.stdin.close();h.p.wait(timeout=3);check('cancelled_invocation_cannot_get_stream_credit',h.p.returncode!=0)
# Malformed framing tests close their owned helper without allocating a declared huge body.
valid=frame(FIX['initialize']);duplicate=json.dumps(FIX['initialize'],separators=(',',':')).replace('"jsonrpc":"2.0"','"jsonrpc":"2.0","jsonrpc":"2.0"').encode()
malformed={'zero-length':b'\0'*4,'oversized':struct.pack('<I',1048577),'wrong-endian':struct.pack('>I',len(valid)-4),'truncated-header':b'\x10\0','truncated-body':struct.pack('<I',100)+b'{}','invalid-utf8':struct.pack('<I',1)+b'\xff','batch-not-object':struct.pack('<I',2)+b'[]','duplicate-json-member':struct.pack('<I',len(duplicate))+duplicate}
for name,raw in malformed.items():
    run=subprocess.run([BINARY,'package-helper'],input=raw,capture_output=True,timeout=3);check('framing-'+name,run.returncode!=0 and not run.stdout)
# Validate all actually exchanged frames through the same compiled DTOs.
actual=args.output/'actual-frame-fixtures.json';actual.write_text(json.dumps(MESSAGES,indent=2)+'\n')
run=subprocess.run([BINARY,'check',str(actual)],capture_output=True,text=True,check=True);native=json.loads(run.stdout);assert native['allPassed']
report={'date':'2026-11-01','status':'Passed owned native helper framing/RPC experiment; host grants/effects are scripted, not production authority','checks':CHECKS,'passed':len(CHECKS),'actualFramesTypedChecked':len(MESSAGES),'allOwnedProcessesStopped':True,'noInference':True,'noExternalEffects':True,'limitations':['No native sandbox/security proof, production host/SDK or real callback effect ran.','Stream credits are checked without sustained bulk-transfer throughput or consumption qualification.','Cancellation uncertainty is scripted; no remote effect reconciler or full process tree is implemented.']}
(args.output/'package-process-results.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps({'passed':len(CHECKS),'actualFrames':len(MESSAGES),'ownedProcessesStopped':True}))

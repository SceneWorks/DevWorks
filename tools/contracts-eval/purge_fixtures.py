"""DW83 wire boundaries: purged content never becomes an unknown/uncommitted command."""
import copy

def append_purge_cases(case, sample, request):
    outcome={'commandId':'committed-command','committedSequence':'17','acceptedAtMs':'100','requestDigest':'a'*64,'originalDecision':'Applied','purgeId':'reviewed-purge'}
    reply={'kind':'ReceiptPurged','protocol':'1.0-draft.1','requestKey':'original-key','outcome':outcome}
    for decision in ['Applied','NoChange','Accepted','Refused']:
        value=copy.deepcopy(reply);value['outcome']['originalDecision']=decision
        if decision=='Accepted':value['outcome']['operationId']='original-operation'
        if decision=='Refused':value['outcome']['originalFaultCode']='PermissionDenied'
        case('dw83-purged-command-'+decision,'domain-command-reply.r6',value)
    old=copy.deepcopy(reply)
    case('dw83-purged-command-before-r6','domain-command-reply.r5',old,False,False)
    bad=copy.deepcopy(reply);bad['outcome']['originalDecision']='Accepted'
    case('dw83-purged-accepted-needs-original-operation','domain-command-reply.r6',bad,False,True)
    bad=copy.deepcopy(reply);bad['outcome']['originalFaultCode']='PermissionDenied'
    case('dw83-purged-applied-cannot-claim-original-refusal','domain-command-reply.r6',bad,False,True)
    observed={'kind':'history.command_receipt','value':{'requestKey':'original-key','commandId':'committed-command','committedSequence':'17','state':'Recorded','purged':copy.deepcopy(outcome)}}
    case('dw83-purged-content-observation','operation-result.r6',observed)
    case('dw83-purged-observation-before-r6','operation-result.r5',observed,False,False)
    bad=copy.deepcopy(observed);bad['value']['state']='NotFound'
    case('dw83-purged-receipt-is-not-missing','operation-result.r6',bad,False,True)
    bad=copy.deepcopy(observed);bad['value']['commandId']='another-command'
    case('dw83-purged-receipt-keeps-original-identity','operation-result.r6',bad,False,True)
    bad=copy.deepcopy(observed);bad['value']['replySchema']={'schemaId':'fixture/object','revision':'1','sha256':'b'*64}
    case('dw83-purged-receipt-cannot-claim-private-reply','operation-result.r6',bad,False,True)
    case('dw83-current-history-receipt-admission','query-request.r3',{'protocol':'1.0-draft.1','schemaRevision':6,'queryId':'purged-lookup','query':{'kind':'history.command_receipt','requestKey':'original-key'}})
    confirm=request('projects.purge','Command')
    case('dw83-native-confirmation-shape','domain-request.r6',confirm)
    bad=copy.deepcopy(confirm);bad['body']['confirm']=False
    case('dw83-purge-needs-explicit-confirmation','domain-request.r6',bad,False,True)
    bad=copy.deepcopy(confirm);bad['body']['actorKind']='Human'
    case('dw83-purge-cannot-inject-human-authority','domain-request.r6',bad,False,False)
    bad=copy.deepcopy(confirm);bad['mode']='Query'
    case('dw83-purge-cannot-use-query-mode','domain-request.r6',bad,False,False)
    bad=copy.deepcopy(confirm);bad['schemaRevision']=5
    case('dw83-purge-before-r6','domain-request.r5',bad,False,False)
    complete=sample('ProjectPurgeManifest');complete.update(phase='Complete',operationId='purge-operation',pendingEffects='0')
    case('dw83-complete-purge','operation-result.r6',{'kind':'projects.purge_status','value':complete})
    bad=copy.deepcopy(complete);bad['pendingEffects']='1'
    case('dw83-no-false-physical-completion','operation-result.r6',{'kind':'projects.purge_status','value':bad},False,True)
    bad=copy.deepcopy(complete);bad['errorCode']='unlink_failed'
    case('dw83-complete-purge-cannot-hide-failure','operation-result.r6',{'kind':'projects.purge_status','value':bad},False,True)

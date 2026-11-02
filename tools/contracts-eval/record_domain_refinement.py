"""Bind the final planning artifact/evidence state without claiming production acceptance."""
from pathlib import Path
import argparse,hashlib,json,subprocess
ROOT=Path(__file__).resolve().parents[2];D=ROOT/'docs/architecture';E=D/'validation/domain-contracts'
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);a=p.parse_args()
read=lambda x:json.loads(x.read_text())
model=read(E/'schema-model-results.json');audit=read(E/'source-binding-results.json');reader=read(E/'reader-checks.json');plan=read(D/'implementation-plan.json');structure=read(D/'architecture-validation.json')
for report,key,base in [(model,'sourceSha256',ROOT),(audit,'inputSha256',ROOT),(plan,'sourceSha256',D)]:
 for name,digest in report[key].items():assert hashlib.sha256((base/name).read_bytes()).hexdigest()==digest,name
assert model['allPassed'] and audit['allPassed'] and not reader['pageErrors']
assert model['catalogOperations']==model['inputCandidates']==model['qualifiedResultTags']==347 and model['inputsPending']==0
assert plan['counts']['inputSchemasPending']==0 and plan['counts']['requirementsByStatus']=={'Confirmed':208,'Proposed':137}
assert all(op['inputSchema'] and op['resultSchema'] and op['requestModes'] for op in plan['operations'])
assert structure['requirements']==345 and structure['diagrams']==43
files=[*sorted((ROOT/'tools/contracts-eval').glob('*.py')),ROOT/'tools/contracts-eval/domain-spec.json',ROOT/'tools/contracts-eval/README.md',ROOT/'tools/contracts-eval/Cargo.toml',ROOT/'tools/contracts-eval/Cargo.lock',*sorted((ROOT/'tools/contracts-eval/src').glob('*.rs')),
 ROOT/'tools/planning/build_architecture.cjs',ROOT/'tools/planning/build_implementation_plan.py',ROOT/'tools/planning/verify_domain_planning_readers.cjs',
 ROOT/'docs/planning/requirements-interview.json',ROOT/'docs/planning/requirements-content.json',ROOT/'README.md',
 *[D/n for n in ['domain-operation-contracts.md','domain-operation-contracts.html','domain-operation-reference.md','domain-operation-reference.html','shared-operation-contracts.md','shared-operation-contracts.html','devworks-architecture.md','devworks-architecture.html','architecture-handoff.md','README.md','implementation-plan.json','implementation-plan.md','implementation-plan.html','implementation-slices.json','contracts/manifest.json','contracts/family-coverage.json']],
 *sorted(E.glob('*-results.json')),E/'reader-checks.json',E/'README.md',
 *[D/'architecture-diagrams'/(stem+suffix) for stem in ['42-domain-operation-admission','43-structured-decision-contract'] for suffix in ['.mmd','.svg']]]
report={'date':'2026-11-02','status':'Operation wire-candidate coverage complete; production/authority/SDK/provider/target and retained semantic acceptance unqualified',
 'branch':subprocess.check_output(['git','branch','--show-current'],text=True).strip(),
 'requirements':345,'requirementStatus':plan['counts']['requirementsByStatus'],'requirementsInputSha256':structure['requirementsInputSha256'],
 'operationFamilies':47,'inputCandidates':347,'unboundInputs':0,'qualifiedResultTags':347,'schemaFiles':model['schemaFiles'],'newModeVariants':model['newInputModeVariants'],'hybridOperations':model['hybridOperations'],
 'newMessageFixtures':model['newMessageFixtures'],'newNegativeFixtures':model['newNegativeFixtures'],'combinedMessageFixtures':model['allMessageFixtures'],'nativeScenarios':model['scenarios'],'nativeScenarioSteps':model['scenarioSteps'],
 'sourceFilesVerified':audit['sourceFilesVerified'],'retainedSourceEntries':audit['retainedEntries'],'excludedSourceEntries':audit['excludedEntries'],
 'localSlices':62,'acceptanceGroups':33,'unexecutedWorkloadSpecifications':12,'decisionCount':71,'diagramCount':43,
 'documentReadersChecked':len(reader['readers']),'readerWidths':[1440,800],'visibleTabsOpened':0,
 'visualReview':['42 and43 standalone diagrams','42 inline at800','domain normative reader at1440','field reference at800; final layout checks at1440/800'],
 'noInference':True,'newProductionImplementation':False,'newNativePlatformQualification':False,'newHelperExecution':False,
 'nativeBinarySha256':hashlib.sha256(a.binary.read_bytes()).hexdigest(),
 'nativeToolVersions':{cmd:subprocess.check_output([cmd,'--version'],text=True).strip() for cmd in ['cargo','rustc']},
 'sourceAndEvidenceSha256':{str(f.relative_to(ROOT)):hashlib.sha256(f.read_bytes()).hexdigest() for f in files},
 'confidence':{'wireCoverageAndRecordedPureChecks':'High','productionConformance':'Medium pending actual integration/qualification'},
 'remainingGates':['Actual host principal/scope/grants/receipt/dispatch enforcement and race tests',
 'Current trusted descriptor schemas, compatibility/upcasters, SDK and actual provider transports',
 'Real domain repositories, filesystem/process/credential/desktop/container/platform enforcement',
 'Retained source argument/result/authority/recovery semantics and drift reconciliation against native services',
 'All named OS/CPU builds/installers, actual production workloads and separately authorized later inference E2E'],
 'allPlanningChecksPassed':True}
(E/'refinement-review.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({k:report[k] for k in ['inputCandidates','unboundInputs','qualifiedResultTags','schemaFiles','combinedMessageFixtures','newNegativeFixtures','documentReadersChecked','allPlanningChecksPassed']}))

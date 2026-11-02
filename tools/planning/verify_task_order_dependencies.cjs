/* Local prototype fixtures; no production service or inference. */
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path');
(async()=>{
 const root=process.cwd(),out=path.join(root,'docs/architecture/validation/task-order-dependencies');
 await fs.mkdir(out,{recursive:true});const browser=await chromium.launch({headless:true}),checks=[],errors=[];
 try{
  const page=await browser.newPage({viewport:{width:1440,height:1100}});page.on('pageerror',e=>errors.push(e.message));
  await page.goto('file://'+path.join(root,'docs/planning/harness-workbench.html')+'?page=feature&project=sw&feature=SW-F42&screenshot=1');
  const fixture=await page.evaluate(()=>{
   const f=data.features.find(x=>x.id==='SW-F42');f.approved=true;
   const other={...f,id:'FIX-F2',title:'Second feature'};data.features.push(other);
   const base=data.cards.find(t=>t.feature===f.id),ready=workflow.find(n=>data.wb.workflows.sw.find(s=>s.name===n)?.meaning!=='Complete');
   const a={...base,id:'FIX-A',feature:f.id,title:'Prerequisite',priority:priorities.at(-1),status:ready,deps:[],version:1};
   const b={...a,id:'FIX-B',feature:other.id,title:'Cross-feature dependent'};
   data.cards.push(a,b);data.wb.issues.push({id:'FIX-I',project:'sw',title:'Standalone dependent',description:'Fixture',priority:priorities[0],status:ready,repo:a.repo,run:'Ready',coordinator:'Unassigned',subagents:10,deps:[],version:1,messages:[]});
   editCard(a.id);return {priority:priorities[0],otherPriority:priorities[1]};
  });
  await page.locator('#dialog-form select[name="priority"]').selectOption(fixture.priority);
  await page.locator('#dialog-form').evaluate(f=>f.requestSubmit());
  await page.evaluate(priority=>{if(data.cards.find(x=>x.id==='FIX-A').priority!==priority)throw Error('Human priority edit failed');},fixture.priority);
  checks.push('human_task_editor_updates_priority');
  await page.evaluate(priority=>{const a=data.cards.find(x=>x.id==='FIX-A');dispatch('tasks.edit',{id:a.id,version:a.version,patch:{priority}},'Feature coordinator');if(a.priority!==priority)throw Error('Agent priority edit failed');},fixture.otherPriority);
  checks.push('agent_uses_same_task_mutation_boundary');
  await page.evaluate(()=>editCard('FIX-B'));
  await page.locator('input[name="deps"][value="FIX-A"]').check();
  await page.locator('#dialog-form').evaluate(f=>f.requestSubmit());
  await page.evaluate(()=>{const a=data.cards.find(x=>x.id==='FIX-A'),b=data.cards.find(x=>x.id==='FIX-B');if(!wbTaskBlockers(b).includes(a.id)||!wbTaskDependents(a).some(t=>t.id===b.id))throw Error('Cross-feature link or reciprocal view missing');wbEditIssue('FIX-I');});
  checks.push('human_can_add_cross_feature_blocking_link');
  await page.locator('input[name="deps"][value="FIX-B"]').check();
  await page.locator('#dialog-form').evaluate(f=>f.requestSubmit());
  await page.evaluate(()=>{const i=data.wb.issues.find(x=>x.id==='FIX-I');if(!wbTaskBlockers(i).includes('FIX-B'))throw Error('Standalone issue did not get blocker');editCard('FIX-B');});
  checks.push('standalone_issue_dependency_uses_same_project_graph');
  const shots=path.join(root,'.planning-build/task-order-dependencies');await fs.mkdir(shots,{recursive:true});
  await page.getByText('Blocked by',{exact:true}).first().scrollIntoViewIfNeeded();
  await page.screenshot({path:path.join(shots,'task-dependencies.png')});
  await page.getByRole('dialog').getByLabel('Close dialog').click();
  await page.evaluate(()=>{
   const a=data.cards.find(x=>x.id==='FIX-A'),b=data.cards.find(x=>x.id==='FIX-B'),issue=data.wb.issues.find(x=>x.id==='FIX-I');
   data.wb.issues.push({...issue,id:'FIX-Q',project:'photo',deps:[]});
   for(const [deps,reason] of [[['FIX-Q'],'same project'],[['FIX-I'],'cycle'],[['FIX-A'],'itself'],[['FIX-B','FIX-B'],'Duplicate'],[['missing'],'same project']]){
    let denied=false;try{dispatch('tasks.edit',{id:a.id,version:a.version,patch:{deps}},'Feature coordinator');}catch(e){denied=e.message.includes(reason);}
    if(!denied||a.deps.length)throw Error('Invalid dependency was committed: '+reason);
   }
   let stale=false;try{dispatch('tasks.edit',{id:b.id,version:b.version-1,patch:{priority:priorities[0]}});}catch(e){stale=e.message.includes('changed');}if(!stale)throw Error('Stale task edit accepted');
   let startDenied=false;try{wbCommand('issues.run',{id:issue.id});}catch(e){startDenied=e.message.includes('blocked');}if(!startDenied||issue.run!=='Ready')throw Error('Blocked issue started');
   dispatch('tasks.edit',{id:b.id,version:b.version,patch:{priority:priorities[0]}},'You');
   if(wbEligibleTaskPreview('sw','FIX-F2').some(t=>t.id===b.id))throw Error('Priority bypassed blocker');
   a.status=data.wb.workflows.sw.find(s=>s.meaning==='Complete').name;
   if(!wbTaskBlockers(b).length)throw Error('Manual Done incorrectly proved acceptance');
   a.acceptanceVerified=true;if(wbTaskBlockers(b).length)throw Error('Verified completion did not unblock');
   issue.run='Paused';dispatch('tasks.edit',{id:issue.id,version:issue.version,patch:{deps:[]}},'You');
   if(issue.run!=='Paused'||wbEligibleTaskPreview('sw').some(t=>t.id===issue.id))throw Error('Removing dependency cleared pause');
   const revision=wbTaskQueueRevision('sw','SW-F42');dispatch('tasks.prioritize',{projectId:'sw',featureId:'SW-F42',expectedQueueRevision:revision,priority:priorities[0],orderedTaskIds:['FIX-A']},'Feature coordinator');
   let conflict=false;try{dispatch('tasks.prioritize',{projectId:'sw',featureId:'SW-F42',expectedQueueRevision:revision,priority:priorities[0],orderedTaskIds:['FIX-A']});}catch(e){conflict=e.message.includes('queue changed');}if(!conflict)throw Error('Stale reorder accepted');
   const root=data.features.find(f=>f.id==='SW-F42');root.priority=priorities[0];const projectRevision=wbTaskQueueRevision('sw');dispatch('tasks.prioritize',{projectId:'sw',expectedQueueRevision:projectRevision,priority:priorities[0],orderedTaskIds:[issue.id]});if(issue.manualOrder!==0||root.manualOrder<=0||issue.run!=='Paused')throw Error('Standalone ordering diverged from the mixed project queue');
   if(!data.features.find(f=>f.id==='SW-F42').approved)throw Error('Coordination invalidated approved plan');
  });
  checks.push('stale_task_edit_and_cross_project_link_refused','blocked_issue_cannot_start','self_duplicate_cycle_and_missing_targets_refused_atomically','blocked_priority_cannot_bypass_prerequisite','manual_done_is_distinct_from_verified_completion','dependency_removal_preserves_manual_pause','bulk_order_checks_queue_revision','standalone_order_uses_mixed_feature_issue_queue','routine_coordination_preserves_approved_plan');
  if(errors.length)throw Error(errors.join('\n'));
  const report={date:'2026-11-02',scope:'Mock UI/shared sample operations only; no native scheduling, authority or real evidence qualification',checks,passed:true,pageErrors:errors,visibleTabsOpened:0};
  await fs.writeFile(path.join(out,'prototype-checks.json'),JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report));
 }finally{await browser.close();}
})().catch(e=>{console.error(e.stack);process.exitCode=1;});

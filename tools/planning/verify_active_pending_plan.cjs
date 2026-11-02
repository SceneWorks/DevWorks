/* Mock UI only: no native application, inference or external effects. */
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path');
(async()=>{
 const root=process.cwd(),out=path.join(root,'docs/architecture/validation/active-pending-plan');
 await fs.mkdir(out,{recursive:true});
 const browser=await chromium.launch({headless:true}),checks=[],errors=[];
 try{
  const page=await browser.newPage({viewport:{width:1440,height:1100}});
  page.on('pageerror',e=>errors.push(e.message));
  await page.goto('file://'+path.join(root,'docs/planning/harness-workbench.html')+'?page=feature&project=sw&feature=SW-F42&screenshot=1');
  const before=await page.evaluate(()=>{
   const f=data.features.find(x=>x.id==='SW-F42');f.approved=true;f.run='Working';
   const saved={requirements:f.requirements,acceptance:f.acceptance,repos:[...f.repos]};
   dispatch('feature.update',{id:f.id,version:f.version,patch:{requirements:'Pending replacement requirements',acceptance:'Pending replacement acceptance',repos:[...f.repos]}});
   render();
   if(f.requirements!==saved.requirements||!f.approved||f.run!=='Working'||!f.pendingPlan)throw Error('Draft replaced or stopped the active plan');
   return saved;
  });checks.push('saving_revision_preserves_approved_plan_and_running_work');
  await page.locator('[data-wb="approve-plan"][data-id="SW-F42"]').first().click();
  await page.getByText('Approved plan remains active',{exact:true}).waitFor();
  const screenshots=path.join(root,'.planning-build/active-pending-plan');await fs.mkdir(screenshots,{recursive:true});
  await page.screenshot({path:path.join(screenshots,'pending-review.png')});
  await page.locator('[data-wb="plan-reject"]').click();
  await page.evaluate(before=>{
   const f=data.features.find(x=>x.id==='SW-F42');
   if(f.requirements!==before.requirements||f.acceptance!==before.acceptance||!f.approved||f.run!=='Working'||f.pendingPlan.state!=='Rejected')throw Error('Rejection changed active authority');
  },before);checks.push('rejection_preserves_original_approval_and_execution');
  await page.evaluate(()=>{
   const f=data.features.find(x=>x.id==='SW-F42');
   const stale={id:f.id,edit:String(f.version),pending:String(f.pendingPlan.revision)};
   dispatch('feature.update',{id:f.id,version:f.version,patch:{requirements:'Latest pending requirements'}});
   let rejected=false;try{wbDecideFeaturePlan(stale,true);}catch(e){rejected=e.message.includes('plan changed');}
   if(!rejected||f.requirements==='Latest pending requirements')throw Error('Stale approval applied');
   render();
  });checks.push('editing_pending_revision_rejects_stale_approval');
  await page.locator('[data-wb="approve-plan"][data-id="SW-F42"]').first().click();
  await page.evaluate(()=>{const f=data.features.find(x=>x.id==='SW-F42');dispatch('feature.update',{id:f.id,version:f.version,patch:{title:'Updated feature title'}});});
  checks.push('unrelated_metadata_does_not_invalidate_reviewed_pending_content');
  await page.locator('[data-wb="plan-confirm"]').click();
  await page.evaluate(()=>{
   const f=data.features.find(x=>x.id==='SW-F42');
   if(f.requirements!=='Latest pending requirements'||!f.approved||f.pendingPlan||f.run!=='Working')throw Error('Exact pending revision not promoted');
   f.run='Paused';dispatch('feature.update',{id:f.id,version:f.version,patch:{requirements:'Approved while paused'}});render();
  });checks.push('approval_promotes_exact_draft_and_clears_pending_slot');
  await page.locator('[data-wb="approve-plan"][data-id="SW-F42"]').first().click();
  await page.locator('[data-wb="plan-confirm"]').click();
  await page.evaluate(()=>{
   const f=data.features.find(x=>x.id==='SW-F42');if(f.run!=='Paused')throw Error('Approval cleared manual pause');
  });checks.push('approval_does_not_continue_paused_work');
  if(errors.length)throw Error(errors.join('\n'));
  const report={date:'2026-11-02',scope:'Interactive prototype state only; not native approval, storage or dispatch enforcement',checks,passed:true,pageErrors:errors,visibleTabsOpened:0};
  await fs.writeFile(path.join(out,'prototype-checks.json'),JSON.stringify(report,null,2)+'\n');
  console.log(JSON.stringify(report));
 }finally{await browser.close();}
})().catch(e=>{console.error(e.stack);process.exitCode=1;});

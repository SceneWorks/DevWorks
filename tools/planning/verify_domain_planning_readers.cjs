/* Authoring-only reader QA. Run at repository root with Playwright installed. */
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path');
(async()=>{
 const root=process.cwd(),dir=path.join(root,'docs/architecture'),out=path.join(root,'.planning-build/mcp-server-amendment/readers');
 await fs.mkdir(out,{recursive:true});
 const report={date:'2026-11-02',status:'Document QA only; separate from DTO/schema/native acceptance',visibleTabsOpened:0,readers:[]};
 const browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
  for(const [stem,anchor] of [
   ['devworks-architecture','devworks-mcp-server-for-other-harnesses'],
   ['mcp-server-contracts','principals-and-authority'],
   ['architecture-feasibility-review','disposition-of-the-four-questions'],
   ['architecture-handoff','architecture-closure-checklist'],
   ['native-backend-feasibility','native-ui-state-and-projection-ownership'],
   ['domain-operation-contracts','selected-operation-and-request-mode'],
   ['domain-operation-reference','router'],
   ['shared-operation-contracts','complete-domain-schema-pass-2-november-2026'],
   ['implementation-plan','dw-022'],
   ['provider-session-package-contracts','router-selection-correction'],
   ['data-retrieval-delivery-contracts','artifact-mutation-and-deletion'],
   ['devworks-storage-evaluation','dataset-and-execution'],
   ['devworks-graphqlite-evaluation','what-ran']]){
   await page.goto('file://'+path.join(dir,stem+'.html'));
   await page.locator('img').evaluateAll(xs=>Promise.all(xs.map(x=>x.decode())));
   if(await page.locator('#'+anchor).count()!==1)throw Error('Missing anchor '+stem+' #'+anchor);
   const record={stem,imageCount:await page.locator('img').count(),layouts:[]};
   if(stem==='devworks-architecture'&&record.imageCount!==44)throw Error('Main image count');
   for(const width of [1440,800]){
    await page.setViewportSize({width,height:1000});
    const state=await page.evaluate(()=>({overflow:document.documentElement.scrollWidth>innerWidth+1,brokenImages:[...document.images].filter(x=>!x.complete||x.naturalWidth===0).length}));
    if(state.overflow||state.brokenImages)throw Error(stem+' '+width+' '+JSON.stringify(state));
    await page.locator('#'+anchor).scrollIntoViewIfNeeded();await page.screenshot({path:path.join(out,stem+'-'+width+'.png')});
    if(stem==='domain-operation-contracts'){
     const style=await page.addStyleTag({content:'.top{visibility:hidden!important}'});
     for(const id of ['42-domain-operation-admission','43-structured-decision-contract'])await page.locator('img[src$="'+id+'.svg"]').screenshot({path:path.join(out,id+'-inline-'+width+'.png')});
     await style.evaluate(el=>el.remove());
    }
    record.layouts.push({width,...state});
   }
   const hrefs=await page.locator('a[href]').evaluateAll(xs=>xs.map(x=>x.getAttribute('href')));let checked=0;
   for(const href of new Set(hrefs)){
    if(/^(https?:|data:|mailto:)/.test(href))continue;
    const [file,fragment]=href.split('#'),resolved=file?path.resolve(dir,decodeURIComponent(file)):path.join(dir,stem+'.html');
    await fs.access(resolved);checked++;
    if(fragment&&resolved.endsWith('.html')){
     const html=await fs.readFile(resolved,'utf8');const count=await page.evaluate(({html,id})=>new DOMParser().parseFromString(html,'text/html').querySelectorAll('[id="'+CSS.escape(id)+'"]').length,{html,id:decodeURIComponent(fragment)});
     if(count!==1)throw Error('Invalid local fragment '+stem+' '+href+' '+count);
    }
   }
   record.localLinksChecked=checked;
   if(stem==='devworks-architecture'){
    const decisions=await page.locator('#architecture-decisions-and-validation-gates + p + table tbody tr').count();if(decisions!==81)throw Error('Decision rows '+decisions);record.decisionRows=decisions;
    await page.locator('#doc-search').fill('complete domain operation schemas');if(!await page.locator('#complete-domain-operation-schemas').isVisible())throw Error('Section filter');await page.locator('#doc-search').fill('');
    await page.locator('#req-search').fill('ROUTE-18');if(!await page.locator('.trace tbody tr:visible').filter({hasText:'ROUTE-18'}).count())throw Error('Requirement filter');await page.locator('#req-search').fill('');await page.locator('#req-status').selectOption('Confirmed');if(await page.locator('.trace tbody tr:visible').count()!==230)throw Error('Confirmed filter');record.filtersVerified=true;
   }
   if(stem==='domain-operation-reference'){
    const catalog=JSON.parse(await fs.readFile(path.join(dir,'devworks-capability-catalog.json'),'utf8'));
    const visible=await page.locator('td:first-child code').allTextContents();
    for(const f of catalog.families)for(const op of f.operations){const name=f.family+'.'+op;if(visible.filter(v=>v===name).length!==1)throw Error('Operation field reference '+name)}record.qualifiedOperationRows=catalog.families.reduce((n,f)=>n+f.operations.length,0);
   }
   if(stem==='implementation-plan'){const slices=JSON.parse(await fs.readFile(path.join(dir,'implementation-slices.json'),'utf8')).slices;for(const s of slices)if(await page.locator('#'+s.id.toLowerCase()).count()!==1)throw Error('Slice '+s.id);if(await page.locator('#dw-038').count())throw Error('Retired slice exposed');record.sliceAnchors=slices.length;}
   report.readers.push(record);
  }
  if(errors.length)throw Error(JSON.stringify(errors));report.pageErrors=errors;
  await fs.writeFile(path.join(dir,'validation/mcp-server-amendment/reader-checks.json'),JSON.stringify(report,null,2)+'\n');console.log(JSON.stringify(report));
 }finally{await browser.close();}
})().catch(e=>{console.error(e.stack);process.exitCode=1});

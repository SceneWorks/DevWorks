const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path');
(async()=>{
 const root=process.cwd(),dir=path.join(root,'docs/architecture'),out=path.join(root,'.planning-build/native-boundaries');
 const report={visibleTabsOpened:0,status:'Document/reader verification only; separate from 39 native experiment assertions',readers:[]};
 const browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  for(const [stem,heading,imageCount] of [
   ['devworks-architecture','DevWorks architecture review draft',41],
   ['native-backend-feasibility','Native backend feasibility and remaining qualification',1],
   ['implementation-plan','Local implementation planning draft',1]]){
   await page.goto('file://'+path.join(dir,stem+'.html'));
   if(await page.getByRole('heading',{name:heading,exact:true}).count()!==1)throw Error('Heading '+stem);
   await page.locator('img').evaluateAll(xs=>Promise.all(xs.map(x=>x.decode())));
   const record={stem,layouts:[],imageCount:await page.locator('img').count()};
   if(record.imageCount!==imageCount)throw Error('Image count '+stem+' '+record.imageCount);
   for(const width of [1440,800]){
    await page.setViewportSize({width,height:1000});
    const state=await page.evaluate(()=>({overflow:document.documentElement.scrollWidth>innerWidth+1,brokenImages:[...document.images].filter(x=>!x.complete||x.naturalWidth===0).length}));
    if(state.overflow||state.brokenImages)throw Error(stem+' '+width+' '+JSON.stringify(state));
    record.layouts.push({width,...state});
    const anchor=stem==='devworks-architecture'?'native-backend-feasibility-and-planning-coverage':stem==='implementation-plan'?'dw-022':'registered-root-read-feasibility-and-authority-linearization';
    await page.locator('#'+anchor).scrollIntoViewIfNeeded();
    await page.screenshot({path:path.join(out,stem+'-'+width+'.png')});
    if(stem==='devworks-architecture'){
     const style=await page.addStyleTag({content:'.top{visibility:hidden!important}'});
     for(const id of ['40-native-backend-boundaries','41-implementation-coverage'])await page.locator('img[src$="'+id+'.svg"]').screenshot({path:path.join(out,id+'-inline-'+width+'.png')});
     await style.evaluate(el=>el.remove());
    }
   }
   const hrefs=await page.locator('a[href]').evaluateAll(xs=>xs.map(x=>x.getAttribute('href')));
   let checked=0;
   for(const href of new Set(hrefs)){
    if(/^(https?:|data:|mailto:)/.test(href))continue;
    const [file,fragment]=href.split('#');
    const resolved=file?path.resolve(dir,decodeURIComponent(file)):path.join(dir,stem+'.html');
    await fs.access(resolved);checked++;
    if(fragment&&resolved.endsWith('.html')){
     const html=await fs.readFile(resolved,'utf8');
     const count=await page.evaluate(({html,id})=>new DOMParser().parseFromString(html,'text/html').querySelectorAll('[id="'+CSS.escape(id)+'"]').length,{html,id:decodeURIComponent(fragment)});
     if(count!==1)throw Error('Invalid fragment '+stem+' '+href+' count '+count);
    }
   }
   record.localLinksChecked=checked;
   if(stem==='devworks-architecture'){
    const count=await page.locator('#architecture-decisions-and-validation-gates + p + table tbody tr').count();
    if(count!==75)throw Error('Decision rows '+count);record.decisionRows=count;
    await page.locator('#doc-search').fill('native backend feasibility and planning coverage');
    if(!await page.locator('#native-backend-feasibility-and-planning-coverage').isVisible())throw Error('Section search');
    await page.locator('#doc-search').fill('');
    await page.locator('#req-search').fill('ROUTE-18');
    if(!await page.locator('.trace tbody tr:visible').filter({hasText:'ROUTE-18'}).count())throw Error('Requirement search');
    await page.locator('#req-search').fill('');await page.locator('#req-status').selectOption('Confirmed');
    if(await page.locator('.trace tbody tr:visible').count()!==208)throw Error('Status count');
    record.sectionSearch=true;record.requirementSearch=true;record.confirmedStatusFilter=208;
   }
   if(stem==='implementation-plan'){
    const slices=JSON.parse(await fs.readFile(path.join(dir,'implementation-slices.json'),'utf8')).slices;for(const s of slices)if(await page.locator('#'+s.id.toLowerCase()).count()!==1)throw Error('Missing slice '+s.id);
    record.sliceAnchors=slices.length;
   }
   report.readers.push(record);
  }
  if(errors.length)throw Error(JSON.stringify(errors));report.pageErrors=errors;
  await fs.writeFile(path.join(dir,'validation/native-boundaries/reader-checks.json'),JSON.stringify(report,null,2)+'\n');
  console.log(JSON.stringify(report));
 }finally{await browser.close();}
})().catch(e=>{console.error(e.stack);process.exitCode=1});

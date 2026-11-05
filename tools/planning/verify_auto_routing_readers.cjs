/* Authoring-only QA for the approved first-session routing amendment. No inference. */
const fs = require('node:fs/promises');
const path = require('node:path');
const {chromium} = require('playwright');
(async () => {
  const root = process.cwd();
  const out = path.join(root, '.planning-build/auto-routing/readers');
  await fs.mkdir(out, {recursive:true});
  const report = {date:'2026-11-05', scope:'Routing document readers only; no native runtime or inference acceptance', readers:[], pageErrors:[]};
  const browser = await chromium.launch({headless:true});
  try {
    const page = await browser.newPage();
    page.on('pageerror', error => report.pageErrors.push(error.message));
    for (const [file, anchor] of [
      ['docs/planning/auto-routing-plan.html', 'items'],
      ['docs/planning/devworks-requirements.html', 'ROUTE-21'],
      ['docs/architecture/devworks-architecture.html', 'first-session-automatic-routing'],
      ['docs/architecture/implementation-plan.html', 'dw-107'],
    ]) {
      await page.goto('file://' + path.join(root, file));
      await page.locator('img').evaluateAll(xs => Promise.all(xs.map(x => x.decode())));
      if (await page.locator('#'+anchor).count() !== 1) throw Error('Missing anchor: '+file+' #'+anchor);
      const record = {file, images:await page.locator('img').count(), layouts:[]};
      for (const width of [1440,800]) {
        await page.setViewportSize({width,height:1000});
        await page.locator('#'+anchor).scrollIntoViewIfNeeded();
        const state = await page.evaluate(() => ({overflow:document.documentElement.scrollWidth>innerWidth+1,brokenImages:[...document.images].filter(x=>!x.complete||!x.naturalWidth).length}));
        if (state.overflow || state.brokenImages) throw Error(file+' '+width+' '+JSON.stringify(state));
        await page.screenshot({path:path.join(out,path.basename(file,'.html')+'-'+width+'.png')});
        record.layouts.push({width,...state});
      }
      for (const href of new Set(await page.locator('a[href]').evaluateAll(xs => xs.map(x=>x.getAttribute('href'))))) {
        if (!href || /^(https?:|data:|mailto:|javascript:)/.test(href)) continue;
        const [target,fragment] = href.split('#');
        const resolved = target ? path.resolve(root,path.dirname(file),decodeURIComponent(target.split('?')[0])) : path.join(root,file);
        await fs.access(resolved);
        if (fragment && resolved.endsWith('.html')) {
          const html = await fs.readFile(resolved,'utf8');
          const exists = await page.evaluate(({html,id})=>new DOMParser().parseFromString(html,'text/html').getElementById(id)!==null,{html,id:decodeURIComponent(fragment)});
          if (!exists) throw Error('Broken fragment '+file+' '+href);
        }
      }
      if (file.includes('devworks-architecture')) {
        for (const id of ['ROUTE-19','ROUTE-21','ROUTE-22','ROUTE-23','ROUTE-24']) {
          await page.locator('#req-search').fill(id);
          if (!await page.locator('.trace tbody tr:visible').filter({hasText:id}).count()) throw Error('Requirement filter '+id);
        }
        await page.locator('#req-search').fill('');
        await page.locator('#doc-search').fill('first session automatic routing');
        if (!await page.locator('#first-session-automatic-routing').isVisible()) throw Error('Section filter');
        await page.locator('#doc-search').fill('');
        record.filtersVerified = true;
      }
      report.readers.push(record);
    }
    if (report.pageErrors.length) throw Error(JSON.stringify(report.pageErrors));
    report.passed = true;
    await fs.writeFile(path.join(out,'reader-checks.json'),JSON.stringify(report,null,2)+'\n');
    console.log(JSON.stringify(report));
  } finally { await browser.close(); }
})().catch(e=>{console.error(e.stack);process.exitCode=1;});

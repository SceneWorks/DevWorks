/* Documentation prototype verification only; no network, credentials or native server. */
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path');
(async()=>{
 const root=process.cwd(),out=path.join(root,'.planning-build/mcp-server-amendment/prototype');
 await fs.mkdir(out,{recursive:true});
 const evidence=path.join(root,'docs/architecture/validation/mcp-server-amendment');await fs.mkdir(evidence,{recursive:true});
 const browser=await chromium.launch({headless:true}),checks=[],errors=[];
 try{
  const page=await browser.newPage({viewport:{width:1600,height:1100}});page.on('pageerror',e=>errors.push(e.message));
  await page.goto('file://'+path.join(root,'docs/planning/harness-workbench.html')+'?project=sw&screenshot=1&page=settings&settings=MCP');
  await page.getByRole('heading',{name:'DevWorks server',exact:true}).waitFor();
  const toggle=page.getByRole('checkbox',{name:'Enable DevWorks MCP server',exact:true});
  if(await toggle.isChecked())throw Error('Server enabled without opt-in');checks.push('incoming_server_disabled_by_default');
  if(!await page.getByRole('heading',{name:'External servers',exact:true}).count())throw Error('Outgoing client removed');checks.push('both_directions_visible');
  await page.locator('[data-wb="mcp-server-setup"]').click();
  const setup=await page.getByRole('dialog').innerText();
  for(const term of ['devworks','mcp','127.0.0.1','2026-07-28','2025-11-25','OAuth-only','protected client credential'])if(!setup.includes(term))throw Error('Setup missing '+term);
  await page.getByRole('dialog').getByLabel('Close dialog').click();checks.push('secret_free_stdio_and_http_setup');
  await toggle.check();await page.locator('[data-wb="mcp-client-edit"]').click();
  for(let i=0;i<3;i++)if(!await page.locator('input[name="read'+i+'"]').isChecked())throw Error('Read default');
  for(let i=0;i<2;i++)if(await page.locator('input[name="write'+i+'"]').isChecked())throw Error('Write default');
  checks.push('selected_reads_and_no_writes_default');
  await page.locator('input[name="name"]').fill('External coding harness');
  await page.locator('input[name="projects"]').fill('SceneWorks, DevWorks');
  await page.locator('#dialog-form').evaluate(f=>f.requestSubmit());await page.getByRole('dialog').waitFor({state:'detached'});
  let row=page.locator('tr').filter({hasText:'External coding harness'});
  if(!(await row.innerText()).includes('Read-only')||!(await row.innerText()).includes('SceneWorks, DevWorks'))throw Error('Saved scope/default');checks.push('scoped_client_saved');
  await page.screenshot({path:path.join(root,'docs/planning/ui-07/44-settings-mcp.png')});
  await row.locator('[data-wb="mcp-client-edit"]').click();await page.locator('input[name="write0"]').check();await page.locator('#dialog-form').evaluate(f=>f.requestSubmit());await page.getByRole('dialog').waitFor({state:'detached'});
  if(!(await row.innerText()).includes('Update work'))throw Error('Explicit write grant not saved');checks.push('explicit_sample_command_selection');
  await row.locator('[data-wb="mcp-client-revoke"]').click();
  if(!(await row.innerText()).includes('Revoked')||!(await row.innerText()).includes('generation 2'))throw Error('Revocation state');checks.push('sample_client_revocation');
  for(const width of [1600,1280]){await page.setViewportSize({width,height:1100});if(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth+1))throw Error('Overflow '+width);await page.screenshot({path:path.join(out,'mcp-settings-'+width+'.png')});}
  checks.push('settings_layouts_1600_and_1280');
  if(errors.length)throw Error(errors.join('\n'));
  const record={date:'2026-11-02',status:'Mock UI and setup wording only; no native transport/authentication/authority acceptance',checks,passed:true,pageErrors:errors,visibleTabsOpened:0};
  await fs.writeFile(path.join(evidence,'prototype-checks.json'),JSON.stringify(record,null,2)+'\n');console.log(JSON.stringify(record));
 }finally{await browser.close();}
})().catch(e=>{console.error(e.stack);process.exitCode=1});

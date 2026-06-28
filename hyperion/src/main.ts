// @ts-nocheck
// bOS Configurator view — ported from bos_copilot.py's webview JS.
// The only change vs. the Flask version: the 4 `/api/*` fetches are now Tauri
// `invoke()` commands implemented in src-tauri/src/lib.rs. All rendering /
// auto-grading / inline-highlight logic is unchanged.
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
// Theme system + wiki-editor component styles (M4). Loading this here is what makes
// the dark preset and the switcher take effect; the light tokens stay in index.html.
import "./styles.css";

// ---------- Tauri command shim (replaces the Flask /api/* routes) ----------
const api = {
  state:     () => invoke("app_state"),
  tree:      () => invoke("get_tree"),
  node:      (path) => invoke("get_node", { path }),       // rejects if not found
  playbooks: () => invoke("list_playbooks"),
  playbook:  (name) => invoke("get_playbook", { name }),
  // project store (SQLite, src-tauri/src/projects.rs)
  projects:  () => invoke("list_projects"),
  newProject:(name) => invoke("create_project", { name }),
  openProject:(id) => invoke("open_project", { id }),
  curProject:() => invoke("current_project"),
  importBos: (path, label) => invoke("import_bos", { path, label }),
  // per-project agent memory (src-tauri/src/projects.rs)
  memoryList:   () => invoke("memory_list"),
  memorySet:    (mtype, slug, body) => invoke("memory_set", { mtype, slug, body }),
  memoryDelete: (id) => invoke("memory_delete", { id }),
  // encrypted vault (src-tauri/src/vault.rs)
  vaultStatus: () => invoke("vault_status"),
  vaultUnlock: () => invoke("vault_unlock"),
  vaultLock:   () => invoke("vault_lock"),
  vaultList:   () => invoke("vault_list_secrets"),
  vaultSet:    (name, value) => invoke("vault_set_secret", { name, value }),
  vaultDelete: (name) => invoke("vault_delete_secret", { name }),
  vaultReveal: (name) => invoke("vault_reveal_secret", { name }),
  scanSecret:  (text) => invoke("scan_secret", { text }),
  // Microsoft Entra SSO (src-tauri/src/entra.rs)
  entraStatus:  () => invoke("entra_status"),
  entraSignIn:  () => invoke("entra_sign_in"),
  entraSignOut: () => invoke("entra_sign_out"),
  // agent runtime adapter (src-tauri/src/agent.rs)
  agentStatus: () => invoke("agent_status"),
  agentAsk:    (question, focusPath, agentId) => invoke("agent_ask", { question, focusPath, agentId }),
  // deterministic MCP/skill recommender (src-tauri/src/tooling.rs)
  recommendTools: (query) => invoke("recommend_tools", { query }),

  agentRoster:           () => invoke("agent_roster"),
  agentInstinctsGet:     (agentId) => invoke("agent_instincts_get", { agentId }),
  agentInstinctsSet:     (agentId, body) => invoke("agent_instincts_set", { agentId, body }),
  agentInstinctsHistory: (agentId) => invoke("agent_instincts_history", { agentId }),
  agentInstinctsRevert:  (agentId, version) => invoke("agent_instincts_revert", { agentId, version }),

  // context-file ingestion (src-tauri/src/ingest.rs + projects.rs)
  contextList:    () => invoke("context_list"),
  contextAddFile: (path) => invoke("context_add_file", { path }),
  contextDelete:  (id) => invoke("context_delete", { id }),

  // editable wiki pages (src-tauri/src/projects.rs)
  wikiList: () => invoke("wiki_list"),
  wikiGet:  (slug) => invoke("wiki_get", { slug }),         // null when no such page
  wikiSave: (slug, title, html) => invoke("wiki_save", { slug, title, html }),

  // bundled HTML-effectiveness artifact templates (src-tauri/src/artifacts.rs)
  artifactTemplatesList: () => invoke("artifact_templates_list"), // [{key,label,description}]
  artifactTemplateGet:   (key) => invoke("artifact_template_get", { key }), // {key,html}
  artifactGuideRefresh:  () => invoke("artifact_guide_refresh"),  // {refreshed,reason?|id,count,techniques}

  // project collaboration: pull requests + timeline + snapshot diff (src-tauri/src/collab.rs, lib.rs)
  prList:       () => invoke("pr_list"),
  prCreate:     (title, narrative, aiDocs) => invoke("pr_create", { title, narrative, aiDocs }),
  prGet:        (id) => invoke("pr_get", { id }),            // null when no such PR
  prCommentAdd: (prId, author, body) => invoke("pr_comment_add", { prId, author, body }),
  prSetStatus:  (id, status) => invoke("pr_set_status", { id, status }), // open|merged|closed
  prDelete:     (id) => invoke("pr_delete", { id }),
  timelineList: (limit) => invoke("timeline_list", { limit }),
  timelineAdd:  (kind, summary, detail) => invoke("timeline_add", { kind, summary, detail }),
  snapshotDiff: (fromId, toId) => invoke("snapshot_diff", { fromId, toId }),

  // vault-backed network registry (src-tauri/src/netreg.rs) — secrets sealed in the vault
  netList:   () => invoke("net_list"),
  netAdd:    (label, address, username, secret, notes) => invoke("net_add", { label, address, username, secret, notes }),
  netGet:    (id) => invoke("net_get", { id }),             // reveals secret; vault must be unlocked
  netDelete: (id) => invoke("net_delete", { id }),

  // ----- Group B (knowledge / quality) -----
  // missing-context suggestions for the open project (src-tauri/src/suggest.rs)
  contextSuggest:  (query) => invoke("context_suggest", { query }),
  // Milesight gateway config -> IoT topology (src-tauri/src/milesight.rs); no project needed
  milesightImport: (path) => invoke("milesight_import", { path }),
  // knowledge crawler: cache pages per project + deterministic "eureka" link finder (crawler.rs)
  crawlAdd:    (url) => invoke("crawl_add", { url }),
  crawlList:   () => invoke("crawl_list"),
  crawlGet:    (id) => invoke("crawl_get", { id }),
  crawlDelete: (id) => invoke("crawl_delete", { id }),
  crawlEureka: () => invoke("crawl_eureka"),
  crawlEurekaProposePr: () => invoke("crawl_eureka_propose_pr"),
  // multi-source registry + tiered sweep (projects.rs crawl_source / crawl_sweep)
  crawlSourceAdd:        (url, label, kind) => invoke("crawl_source_add", { url, label, kind }),
  crawlSourceList:       () => invoke("crawl_source_list"),
  crawlSourceSetEnabled: (id, enabled) => invoke("crawl_source_set_enabled", { id, enabled }),
  crawlSourceRemove:     (id) => invoke("crawl_source_remove", { id }),
  crawlSweep:            (smart) => invoke("crawl_sweep", { smart }),
  // code standard + self-audit of Hyperion's own sources (src-tauri/src/standard.rs)
  codeStandard: () => invoke("code_standard"),
  codeAudit:    () => invoke("code_audit"),
  // security scan + enterprise-readiness gate (src-tauri/src/security.rs)
  securityScan:        () => invoke("security_scan"),
  enterpriseGateCheck: () => invoke("enterprise_gate_check"),
  // export the bundled wiki to a chosen folder (src-tauri/src/export.rs)
  wikiExport: (dest) => invoke("wiki_export", { dest }),
};

const $ = (s) => document.querySelector(s), $$ = (s) => [...document.querySelectorAll(s)];
function esc(s){return (''+s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}

// ---------- icons by node type ----------
function icon(t){t=t||'';
  if(/Folder/.test(t)) return ['📁',''];
  if(/Program/.test(t)) return ['⚙','#5b6b7b'];
  if(/Gate/.test(t)) return ['◗','var(--bool)'];              // red logic-gate (AND/OR)
  if(/Boolean/.test(t)&&/Modbus|DPT|Slave|KNX/.test(t)) return ['◯','var(--bool)']; // red Modbus DPT ring
  if(/Boolean/.test(t)) return ['◷','var(--gate)'];           // orange clock-circle variable
  if(/Integer|Calculation/.test(t)) return ['¹²³','var(--int)'];
  if(/Float|Analog|Status|Temp|Byte/.test(t)) return ['A','#2e9e6a'];  // green analog "A"
  if(/String/.test(t)) return ['A','var(--str)'];
  if(/Time/.test(t)) return ['🕒',''];
  if(/Message/.test(t)) return ['✉','var(--run)'];
  if(/Trigger/.test(t)) return ['⤳','var(--str)'];
  if(/DPT|Modbus|KNX|Slave/.test(t)) return ['◯','var(--bool)'];
  return ['•','#8a98a8'];
}

// ---------- tree ----------
let byEl={};
function renderTree(node,parent,depth){
  if(node.path!==""){
    const div=document.createElement('div'); div.className='tn'; div.dataset.path=node.path;
    const row=document.createElement('div'); row.className='row';
    const tog=document.createElement('span'); tog.className='tog';
    const has=node.children.length>0; tog.textContent=has?'+':'';
    const [g,col]=icon(node.type);
    const ic=document.createElement('span'); ic.className='ic'; ic.textContent=g; if(col)ic.style.color=col;
    const lbl=document.createElement('span'); lbl.className='lbl'; lbl.textContent=node.name;
    row.append(tog,ic,lbl); div.appendChild(row); parent.appendChild(div); byEl[node.path]=div;
    const kids=document.createElement('div'); kids.className='kids';
    kids.style.display=depth<1?'block':'none';
    if(has) tog.textContent=kids.style.display==='block'?'-':'+';
    parent.appendChild(kids);
    const toggle=()=>{if(has){kids.style.display=kids.style.display==='none'?'block':'none';
      tog.textContent=kids.style.display==='block'?'-':'+';}};
    tog.onclick=e=>{e.stopPropagation();toggle();};
    lbl.onclick=ic.onclick=e=>{e.stopPropagation();
      $$('.tn.sel').forEach(x=>x.classList.remove('sel')); div.classList.add('sel');
      showNode(node.path);
      if(has && kids.style.display==='none') toggle();};
    node.children.forEach(c=>renderTree(c,kids,depth+1));
  } else node.children.forEach(c=>renderTree(c,parent,depth));
}
function expandTo(path){const parts=path.split('\\'); let acc=[];
  parts.forEach(p=>{acc.push(p); const el=byEl[acc.join('\\')];
    if(el){const kids=el.nextSibling; if(kids&&kids.className==='kids'){kids.style.display='block';
      const tog=el.querySelector('.tog'); if(tog.textContent)tog.textContent='-';}}});}
function navigate(path){expandTo(path); const el=byEl[path];
  $$('.tn.sel').forEach(x=>x.classList.remove('sel'));
  if(el){el.classList.add('sel'); el.scrollIntoView({block:'center'});}
  showNode(path);}

// ---------- right panel ----------
const TABS=['Settings','Values','Functions','Usages','Notes','Info'];
let CUR=null, TAB='Settings';
async function showNode(path){
  try{ CUR = await api.node(path); }
  catch(e){ CUR = {name:path.split('\\').pop(), path:path, type:'(folder)', settings:{}}; }
  TAB='Settings';
  $('#title').textContent=CUR.name||'';
  $('#status').innerHTML='Path: <code>'+esc(path)+'</code> &middot; read-only simulation';
  renderTabs(); renderPanel();
}
function renderTabs(){
  const usages=CUR&&CUR.referenced_by?CUR.referenced_by.length:0;
  $('#tabs').innerHTML=TABS.map(t=>{const lbl=t==='Usages'?`Usages (${usages})`:t;
    return `<div class="tab${t===TAB?' on':''}" data-t="${t}">${lbl}</div>`;}).join('');
  $$('#tabs .tab').forEach(el=>el.onclick=()=>{TAB=el.dataset.t;renderTabs();renderPanel();});
}
function cmdTree(list,start){
  if(!list||!list.length) return start?'<div class="cmds"><div class="cmd mut">(empty)</div></div>':'';
  let h='<div class="cmds">';
  list.forEach(c=>{
    h+='<div class="cmd c-'+esc((c.cmd||'').toLowerCase())+'" data-cmd="'+esc(c.text||c.cmd||'')+'"><span class="t">'+esc(c.text||c.cmd||'')+'</span>';
    if(c.target) h+='<span class="go" title="go to '+esc(c.target)+'" onclick="navigate(this.dataset.p)" data-p="'+esc(c.target)+'">&#128279;</span>';
    if(c.children&&c.children.length) h+=cmdTree(c.children);
    h+='</div>';
  });
  return h+'</div>';
}
let SUBTAB='Commands';
function renderPanel(){
  const b=$('#panbody'); if(!CUR){b.innerHTML='<div class="mut">Select a node.</div>';return;}
  if(TAB==='Settings') b.innerHTML=settingsView();
  else if(TAB==='Values') b.innerHTML=kvSection('Values',CUR.values)+kvSection('Settings',CUR.settings);
  else if(TAB==='Usages') b.innerHTML=usagesView();
  else if(TAB==='Functions') b.innerHTML='<div class="mut">Functions (methods like ReadValue(), Reset(), Send()) are invoked by RUN commands; not enumerated from the export.</div>';
  else if(TAB==='Notes') b.innerHTML='<div class="mut">No notes in export.</div>';
  else b.innerHTML='<div class="section"><div class="h">Info</div><div class="b">'
    +'<div class="kv"><b>Name</b><span class="v">'+esc(CUR.name||'')+'</span></div>'
    +'<div class="kv"><b>Type</b><span class="v">'+esc(CUR.type||'')+'</span></div>'
    +'<div class="kv"><b>Path</b><span class="v"><code>'+esc(CUR.path||'')+'</code></span></div></div></div>';
  $$('#panbody .subtab').forEach(el=>el.onclick=()=>{SUBTAB=el.dataset.s;renderPanel();});
  applyHighlight();
}
const SETGROUP={Address:'Address',SlaveAddressOld:'Address',
  SendValueCyclically:'Action',CyclicalSendingDelay:'Action',
  UseStatusAddress:'Status',StatusAddress:'Status',ReadCyclically:'Status'};
const SETLABEL={Address:'Address',SlaveAddressOld:'Slave Address (old)',
  SendValueCyclically:'Send Value Cyclically',CyclicalSendingDelay:'Cyclical Sending Delay',
  UseStatusAddress:'Use Status Address',StatusAddress:'Status Address',ReadCyclically:'Read Cyclically'};
const SETDESC={Address:'The address of the Modbus variable.',
  ReadCyclically:'If enabled, bOS reads (polls) this address from the device on each cycle.',
  SendValueCyclically:'If enabled, bOS writes this value to the device on each cycle.'};
// Modbus DPT settings rendered in collapsible Address/Action/Data/Status groups like the Configurator.
function groupedSettings(obj,type){
  if(!obj||!Object.keys(obj).length) return '';
  if(!/Modbus|DPT/.test(type||'')) return kvSection('Settings',obj);
  const groups={Address:[],Action:[],Data:[],Status:[],Other:[]};
  Object.entries(obj).forEach(([k,v])=>groups[SETGROUP[k]||'Other'].push([k,v]));
  let h='';
  ['Address','Action','Data','Status','Other'].forEach(g=>{
    if(!groups[g].length) return;
    h+='<div class="sec"><div class="sh" onclick="this.parentNode.classList.toggle(\'collapsed\')">'
      +'<span class="chev">&#9662;</span>'+esc(g)+'</div><div class="sb">';
    groups[g].forEach(([k,v])=>h+='<div class="srow" data-setting="'+esc(k)+'">'
      +'<span class="sk">'+esc(SETLABEL[k]||k)+'</span><span class="sv">'+esc(''+v)+'</span></div>');
    h+='</div></div>';
  });
  return h;
}
function settingsView(){
  const pg=CUR.program;
  if(pg){
    let h='<div class="section"><div class="h">General</div><div class="b">'
      +'<div class="kv"><b>Restart On Retrigger</b><span class="v">'
      +esc(''+(CUR.settings&&'RestartOnRetrigger' in CUR.settings?CUR.settings.RestartOnRetrigger:false))
      +'</span></div></div></div>';
    h+='<div class="descbox"><b>Restart On Retrigger</b><span class="dmut">The Restart On Retrigger setting defines the behaviour of the program execution when program is executed while previous execution is still running.</span></div>';
    h+='<div style="font-weight:600;margin:8px 0 3px">Triggers:</div>';
    h+='<div class="miniBtns"><span>+ Add</span><span>Edit</span><span>Delete</span><span>Copy</span><span>Paste</span><span>Cut</span></div>';
    h+='<div class="trigbox">'+(pg.triggers&&pg.triggers.length
        ? pg.triggers.map(t=>esc(t.text)+(t.target?' <span class="go" onclick="navigate(this.dataset.p)" data-p="'+esc(t.target)+'" style="visibility:visible">&#128279;</span>':'')).join('<br>')
        : '<span class="mut">(none)</span>')+'</div>';
    h+='<div class="subtabs"><div class="subtab'+(SUBTAB==='Commands'?' on':'')+'" data-s="Commands">Commands</div>'
      +'<div class="subtab'+(SUBTAB==='Abort'?' on':'')+'" data-s="Abort">Abort Commands</div></div>';
    h+='<div class="cmdbox">';
    h+='<div class="cmd c-start"><span class="t">Start</span></div>';
    h+=cmdTree(SUBTAB==='Commands'?pg.commands:pg.abort,true);
    h+='</div>';
    return h;
  }
  // non-program node: grouped settings + relations
  let h=groupedSettings(CUR.settings,CUR.type);
  if(CUR.consists_of&&CUR.consists_of.length){
    h+='<div class="section"><div class="h">Consists of ('+CUR.consists_of.length+')</div><div class="b">';
    CUR.consists_of.forEach(c=>{const [g,col]=icon(c.type);
      h+='<div class="kv"><span><span class="ic" style="color:'+(col||'#888')+'">'+g+'</span> '
        +'<span class="reflink" onclick="navigate(this.dataset.p)" data-p="'+esc(c.path)+'">'+esc(c.name)+'</span></span>'
        +'<span class="v mut">'+esc(c.type||'')+'</span></div>';});
    h+='</div></div>';
  }
  if(CUR.inputs&&CUR.inputs.length) h+=refSection('Inputs',CUR.inputs,'object','&larr;');
  if(CUR.output) h+=refSection('Output',[CUR.output],'object','&rarr;');
  if(CUR.writes&&CUR.writes.length) h+=refSection('Writes / references',CUR.writes,'object','&rarr;');
  return h||'<div class="mut">No settings.</div>';
}
function kvSection(title,obj){
  if(!obj||!Object.keys(obj).length) return '';
  let h='<div class="section"><div class="h">'+esc(title)+'</div><div class="b">';
  Object.entries(obj).forEach(([k,v])=>h+='<div class="kv"><b>'+esc(k)+'</b><span class="v">'+esc(''+v)+'</span></div>');
  return h+'</div></div>';
}
function refSection(title,arr,key,arrow){
  let h='<div class="section"><div class="h">'+esc(title)+' ('+arr.length+')</div><div class="b">';
  arr.forEach(r=>h+='<div class="kv" data-bind="'+esc(r[key])+'"><span>'+arrow+' <span class="reflink" onclick="navigate(this.dataset.p)" data-p="'
    +esc(r[key])+'">'+esc(r[key])+'</span></span><span class="v mut">.'+esc(''+r.property)+'</span></div>');
  return h+'</div></div>';
}
function usagesView(){
  const rb=CUR.referenced_by||[];
  if(!rb.length) return '<div class="mut">Not referenced by any other object.</div>';
  let h='<div class="section"><div class="h">Used by ('+rb.length+')</div><div class="b">';
  rb.forEach(r=>h+='<div class="kv"><span><span class="mut">'+esc(r.kind)+'</span> '
    +'<span class="reflink" onclick="navigate(this.dataset.p)" data-p="'+esc(r.by)+'">'+esc(r.by)+'</span></span>'
    +'<span class="v mut">.'+esc(''+r.property)+'</span></div>');
  return h+'</div></div>';
}

// ---------- guide (playbooks) ----------
let PB=null, STEP=-1;
// Per-step "done" state for the guided walkthrough (operator-driven; independent
// of the auto-graded check badge). Holds step indices the user marked complete.
const DONE=new Set();
// Normalize a step's optional `ui` guided-walkthrough actions to an array. The
// Rust loader already normalizes file-backed playbooks, but agent-emitted blocks
// reach loadGuideFromObject directly, so accept a single object or an array here
// too (mirrors how `highlight` is handled). Instructional only — never automated.
function uiList(s){ if(!s||!s.ui) return [];
  return (Array.isArray(s.ui)?s.ui:[s.ui]).filter(u=>u&&typeof u==='object'&&!Array.isArray(u)); }
// Fetch a playbook file by name, then hand the parsed object to the shared
// renderer/auto-grader below.
async function loadGuide(file){ loadGuideFromObject(await api.playbook(file)); }
// Render an already-parsed playbook OBJECT into the #guide panel and auto-grade
// each step against the live .bos. Used by loadGuide (the file dropdown) and by
// the agent dock's "Load playbook" button (an emitted ```playbook block).
// Throws on a structurally invalid playbook so callers can surface the message.
function loadGuideFromObject(pb){
  if(!pb || typeof pb!=='object' || Array.isArray(pb)) throw new Error('not a playbook object');
  if(typeof pb.feature!=='string' || !pb.feature.trim()) throw new Error('missing a "feature" name');
  if(!Array.isArray(pb.steps) || !pb.steps.length) throw new Error('needs a non-empty "steps" array');
  for(const s of pb.steps){ if(!s || typeof s!=='object' || Array.isArray(s)) throw new Error('every step must be a non-null object'); }
  PB=pb; STEP=-1; DONE.clear();
  $('#gtitle').textContent=PB.feature||'Guide';
  let h=''; if(PB.summary) h+='<div class="mut" style="margin-bottom:8px">'+esc(PB.summary)+'</div>';
  (PB.steps||[]).forEach((s,i)=>{const us=uiList(s);
    h+='<div class="gstep" data-i="'+i+'" onclick="gotoStep('+i+')">'
    +'<div><span class="gn">'+esc(''+(s.n||i+1))+'</span><span class="gt">'+esc(s.title||'')+'</span>'
    +(s.check?'<span class="badge-st st-todo" id="bst'+i+'">checking...</span>':'')
    +'<button type="button" class="gdone" id="gd'+i+'" onclick="event.stopPropagation();markStepDone('+i+')">Mark done</button></div>'
    +(s.check?'<div class="chklist" id="chk'+i+'"></div>':'')
    +(s.detail?'<div class="gd">'+esc(s.detail)+'</div>':'')
    +(us.length?'<div class="guiwalk">'+us.map(u=>'<div class="guirow">'
        +(u.app?'<span class="guiapp app-'+esc((''+u.app).toLowerCase())+'">'+esc(u.app)+'</span>':'')
        +(u.location?'<span class="guiloc">&#128205; '+esc(u.location)+'</span>':'')
        +'<div class="guiact">&#128073; '+esc(u.action||'')+'</div>'
        +(u.verify?'<div class="guiver">&#10003; Expect: '+esc(u.verify)+'</div>':'')
        +'</div>').join('')+'</div>':'')
    +(s.target?'<div class="gtgt reflink" onclick="event.stopPropagation();navigate(this.dataset.p)" data-p="'+esc(s.target)+'">&#128205; '+esc(s.target)+'</div>':'')
    +(s.diff?'<div class="diffwrap" data-i="'+i+'" onclick="event.stopPropagation()">'
       +'<div class="difftabs"><span class="dt before" data-m="before">Before</span>'
       +'<span class="dt after on" data-m="after">After</span></div>'
       +'<div class="diffnote">Preview only &mdash; not written to bOS</div>'
       +'<div class="diffbody"></div></div>':'')
    +'</div>';});
  $('#gbody').innerHTML=h||'<div class="mut">No steps.</div>';
  (PB.steps||[]).forEach((s,i)=>{if(s.diff) showDiff(i,'after');});
  $('#guide').style.display='block';
  updateGuideFooter();
  runChecks();
}
// ----- guided-walkthrough navigation + per-step done state -----
// Step counter + Back/Next enablement in the guide footer.
function updateGuideFooter(){
  const n=(PB&&PB.steps||[]).length;
  const pos=$('#gpos'); if(pos) pos.textContent = n
    ? ('Step '+(STEP<0?1:STEP+1)+' of '+n+(DONE.size?(' · '+DONE.size+' done'):'')) : '';
  const prev=$('#gprev'), next=$('#gnext');
  if(prev) prev.disabled = n===0 || STEP<=0;
  if(next) next.disabled = n===0 || STEP>=n-1;
}
function guideNext(){ const n=(PB&&PB.steps||[]).length; if(n) gotoStep(Math.min((STEP<0?-1:STEP)+1,n-1)); }
function guidePrev(){ const n=(PB&&PB.steps||[]).length; if(n) gotoStep(Math.max((STEP<0?0:STEP)-1,0)); }
// Toggle a step's operator-confirmed "done" state (independent of auto-grading).
function markStepDone(i){
  if(DONE.has(i)) DONE.delete(i); else DONE.add(i);
  const step=document.querySelector('#gbody .gstep[data-i="'+i+'"]');
  if(step) step.classList.toggle('done',DONE.has(i));
  const b=document.getElementById('gd'+i);
  if(b) b.textContent=DONE.has(i)?'✓ Done':'Mark done';
  updateGuideFooter();
}
// ----- per-step status: evaluate each step's check against the live .bos -----
const _nodeCache={};
async function getNode(path){
  if(_nodeCache[path]!==undefined) return _nodeCache[path];
  try{ _nodeCache[path]=await api.node(path); }catch(e){ _nodeCache[path]=null; }
  return _nodeCache[path];
}
function flatten(list,anc,out){ (list||[]).forEach(c=>{
  out.push({text:c.text||'',anc:anc}); flatten(c.children,anc.concat([c.text||'']),out);}); return out;}
async function evalCheck(chk){
  const n=await getNode(chk.node);
  const results=(Array.isArray(chk.all)?chk.all:[]).map(p=>{
    let ok=false;
    if('exists' in p) ok=!!n;
    else if(!n) ok=false;
    else if('referencedBy' in p) ok=(n.referenced_by||[]).some(r=>(r.by||'').includes(p.referencedBy));
    else if('writes' in p) ok=(n.writes||[]).some(r=>(r.object||'').includes(p.writes));
    else if('trigger' in p) ok=((n.program&&n.program.triggers)||[]).some(t=>(t.text||'').includes(p.trigger));
    else if('command' in p){const fl=flatten((n.program&&n.program.commands)||[],[],[]);
      ok=fl.some(x=>x.text.includes(p.command)&&(!p.under||x.anc.some(a=>a.includes(p.under))));}
    return {label:p.label||JSON.stringify(p),ok};
  });
  const pass=results.filter(r=>r.ok).length;
  const status = results.length===0 ? 'todo' : (pass===results.length ? 'done' : (pass===0 ? 'todo' : 'partial'));
  return {status,pass,total:results.length,results};
}
async function runChecks(){
  let done=0,total=0;
  for(let i=0;i<(PB.steps||[]).length;i++){const s=PB.steps[i]; if(!s.check) continue;
    total++; const res=await evalCheck(s.check);
    if(res.status==='done') done++;
    const badge=document.getElementById('bst'+i);
    if(badge){const map={done:['st-done','✓ Implemented'],partial:['st-partial','◐ Partial'],todo:['st-todo','○ To-do']};
      const [cls,txt]=map[res.status]; badge.className='badge-st '+cls; badge.textContent=txt;}
    const cl=document.getElementById('chk'+i);
    if(cl) cl.innerHTML=res.results.map(r=>'<div class="chkitem '+(r.ok?'chk-ok':'chk-no')+'">'
      +(r.ok?'&#10003; ':'&#10007; ')+esc(r.label)+'</div>').join('');
  }
  $('#gsummary').textContent=total?('· '+done+'/'+total+' implemented'):'';
}
function diffTree(list){
  if(!list||!list.length) return '<div class="cmds"><div class="cmd mut">(empty)</div></div>';
  let h='<div class="cmds">';
  list.forEach(c=>{const cls='cmd c-'+esc((c.cmd||'').toLowerCase())+(c.added?' added':'')+(c.removed?' removed':'');
    h+='<div class="'+cls+'"><span class="t">'+esc(c.text||c.cmd||'')+'</span>';
    if(c.children&&c.children.length) h+=diffTree(c.children);
    h+='</div>';});
  return h+'</div>';
}
async function showDiff(i,mode){
  const s=(PB.steps||[])[i]; const d=s&&s.diff; if(!d) return;
  const wrap=document.querySelector('.diffwrap[data-i="'+i+'"]'); if(!wrap) return;
  wrap.querySelectorAll('.dt').forEach(x=>x.classList.toggle('on',x.dataset.m===mode));
  let list;
  if(mode==='after') list=d.after||[];
  else if(d.before!==undefined) list=d.before;
  else { try{const n=await api.node(d.node); list=(n.program&&n.program[d.section||'commands'])||[];}catch(e){list=[];} }
  const head=(d.section==='triggers')?'<div class="cmd c-iftrigger"><span class="t">Triggers</span></div>'
    :'<div class="cmd c-start"><span class="t">Start</span></div>';
  wrap.querySelector('.diffbody').innerHTML='<div class="cmdbox">'+head+diffTree(list)+'</div>';
}
// ----- inline before/after highlight on the actual settings page -----
let ACTIVE_HL=null;             // array of {kind,node,field,before,after,warn}
function _np(s){return (''+s).replace(/\//g,'\\').toLowerCase();}
function hlChip(h){let s='';
  if(h.before!==undefined||h.after!==undefined){s+='<span class="ba">';
    if(h.before!==undefined) s+='<s>'+esc(h.before)+'</s>';
    s+='<span class="arr">&#8594;</span>';
    if(h.after!==undefined) s+='<b>'+esc(h.after)+'</b>'; s+='</span>';}
  if(h.warn) s+='<span class="hl-warn">&#9888; '+esc(h.warn)+'</span>';
  return s;}
function applyHighlight(){
  if(!ACTIVE_HL||!CUR) return;
  ACTIVE_HL.forEach(h=>{
    if(_np(h.node)!==_np(CUR.path)) return;
    let el=null;
    if(h.kind==='setting') el=document.querySelector('#panbody .srow[data-setting="'+h.field+'"]');
    else if(h.kind==='binding') el=[...document.querySelectorAll('#panbody .kv[data-bind]')].find(x=>(x.dataset.bind||'').includes(h.field));
    else if(h.kind==='command') el=[...document.querySelectorAll('#panbody .cmd[data-cmd]')].find(x=>(x.dataset.cmd||'').includes(h.field));
    if(!el) return;
    el.classList.add('hl-change');
    const c=hlChip(h); if(c){const sp=document.createElement('span'); sp.innerHTML=c; el.appendChild(sp);}
    el.scrollIntoView({block:'center'});
  });
}
function gotoStep(i){STEP=i; const s=(PB.steps||[])[i]; if(!s)return;
  $$('#gbody .gstep').forEach(x=>x.classList.toggle('active',+x.dataset.i===i));
  const cur=document.querySelector('#gbody .gstep[data-i="'+i+'"]');
  if(cur) cur.scrollIntoView({block:'nearest'});
  updateGuideFooter();
  $$('.tn.guide').forEach(x=>x.classList.remove('guide'));
  ACTIVE_HL = s.highlight ? (Array.isArray(s.highlight)?s.highlight:[s.highlight]) : null;
  if(s.target){navigate(s.target); const el=byEl[s.target]; if(el)el.classList.add('guide');}
  else if(CUR) renderPanel();}
function closeGuide(){$('#guide').style.display='none';$('#pbsel').value='';
  DONE.clear();
  $$('.tn.guide').forEach(x=>x.classList.remove('guide')); ACTIVE_HL=null; if(CUR) renderPanel();}

// ---------- Entra SSO + secrets vault (src-tauri/src/entra.rs, vault.rs) ----------
let vAuthed=false;
async function refreshAuth(){
  let st; try{ st=await api.entraStatus(); }catch(e){ st={authenticated:false,identity:null}; }
  vAuthed = !!st.authenticated;
  $('#vsignin').style.display  = vAuthed?'none':'';
  $('#vsignout').style.display = vAuthed?'':'none';
  $('#vwho').textContent = vAuthed && st.identity
    ? ('Signed in: '+(st.identity.name||st.identity.username||'Microsoft account'))
    : 'Not signed in';
}
function renderVaultStatus(st){
  const unlocked = !!(st && st.unlocked);
  vUnlocked = unlocked;
  const el=$('#vstatus');
  el.className='vstat '+(unlocked?'unlocked':'locked');
  el.textContent = unlocked
    ? ('Unlocked'+(st.count>=0?(' · '+st.count+' secret'+(st.count===1?'':'s')):''))
    : (st && st.exists ? 'Locked' : 'Locked (empty)');
  // Unlock requires an Entra sign-in (defense-in-depth, enforced in the backend too).
  $('#vunlock').disabled = unlocked || !vAuthed;
  $('#vunlock').title = vAuthed ? '' : 'Sign in with Microsoft first';
  $('#vlock').disabled   = !unlocked;
  $('#vname').disabled = $('#vval').disabled = $('#vsave').disabled = !unlocked;
}
async function refreshVault(){
  await refreshAuth();
  let st; try{ st=await api.vaultStatus(); }catch(e){ st={exists:false,unlocked:false,count:-1}; }
  renderVaultStatus(st);
  await refreshNet();   // the net registry list shows regardless of lock (only secrets need unlock)
  const list=$('#vlist');
  if(!st.unlocked){ list.innerHTML='<div class="mut" style="padding:8px">Unlock to view secret names.</div>'; return; }
  let names=[]; try{ names=await api.vaultList(); }catch(e){ names=[]; }
  list.innerHTML = names.length
    ? names.map(n=>'<div class="vi"><span>'+esc(n)+'</span><span>'
        +'<span class="rev" data-n="'+esc(n)+'">reveal</span>'
        +'<span class="del" data-n="'+esc(n)+'">delete</span></span></div>').join('')
    : '<div class="mut" style="padding:8px">No secrets yet.</div>';
  $$('#vlist .del').forEach(b=>b.onclick=async()=>{
    if(!confirm('Delete secret "'+b.dataset.n+'"?')) return;
    try{ await api.vaultDelete(b.dataset.n); await refreshVault(); }catch(e){ alert('Delete failed: '+e); }});
  $$('#vlist .rev').forEach(b=>b.onclick=async()=>{
    try{ const v=await api.vaultReveal(b.dataset.n); alert(b.dataset.n+' = '+v); }catch(e){ alert('Reveal failed: '+e); }});
}
function openVault(){ $('#vault').classList.add('on'); refreshVault(); }
function closeVault(){ $('#vault').classList.remove('on'); }
async function saveSecret(){
  const n=$('#vname').value.trim(), v=$('#vval').value;
  if(!n){ alert('Enter a secret name.'); return; }
  try{ await api.vaultSet(n,v); $('#vname').value=''; $('#vval').value=''; await refreshVault(); }
  catch(e){ alert('Save failed: '+e); }
}
async function scanGuard(){
  const t=$('#vscantext').value; let hits=[];
  try{ hits=await api.scanSecret(t); }catch(e){ hits=[]; }
  $('#vscanout').innerHTML = hits.length
    ? '<span class="hit">&#9888; '+hits.length+' possible secret'+(hits.length===1?'':'s')
      +' &mdash; store in the vault, not in plaintext:</span><br>'
      +hits.map(h=>esc(h.kind)+' <span class="mut">('+esc(h.detail)+')</span>').join('<br>')
    : '<span class="clean">&#10003; No obvious secrets found.</span>';
}

// ---------- vault-backed network registry (Group A; src-tauri/src/netreg.rs) ----------
// Per-project devices/logins. Metadata is stored in the project db; an optional secret
// is sealed into the vault (so adding/revealing a secret needs the vault unlocked). Lives
// in the Vault modal because it shares the unlocked-vault precondition. Escape everything.
let vUnlocked=false;   // mirrors the vault status so the net form can gate its secret field
async function refreshNet(){
  const list=$('#netlist'); let entries=[];
  try{ entries=await api.netList(); }
  catch(e){ list.innerHTML='<div class="mut" style="padding:8px">'+esc(''+e)+'</div>'; return; }
  list.innerHTML = entries.length
    ? entries.map(n=>'<div class="vi"><span>'+esc(n.label)+' <span class="mut">· '+esc(n.address)
        +(n.username?(' · '+esc(n.username)):'')+'</span>'+(n.has_secret?' &#128274;':'')+'</span><span>'
        +(n.has_secret?'<span class="rev" data-id="'+esc(n.id)+'">reveal</span>':'')
        +'<span class="del" data-id="'+esc(n.id)+'">delete</span></span></div>').join('')
    : '<div class="mut" style="padding:8px">No network entries yet. Add one below.</div>';
  $$('#netlist .del').forEach(b=>b.onclick=async()=>{
    if(!confirm('Delete this network entry?')) return;
    try{ await api.netDelete(Number(b.dataset.id)); await refreshNet(); }catch(e){ alert('Delete failed: '+e); }});
  $$('#netlist .rev').forEach(b=>b.onclick=async()=>{
    try{ const n=await api.netGet(Number(b.dataset.id));
      alert(n.label+'\naddress: '+(n.address||'')+'\nusername: '+(n.username||'')
        +'\nsecret: '+(n.secret||'(none)')+(n.notes?('\nnotes: '+n.notes):'')); }
    catch(e){ alert('Reveal failed: '+e); }});
  // The secret field needs an unlocked vault; mirror renderVaultStatus' gating.
  const sec=$('#netsecret');
  if(sec){ sec.disabled=!vUnlocked; sec.placeholder = vUnlocked
    ? 'secret (optional)' : 'secret (unlock the vault to add one)'; }
}
async function saveNet(){
  const label=($('#netlabel').value||'').trim();
  const address=($('#netaddr').value||'').trim();
  if(!label){ alert('Give the entry a label (e.g. Main PLC).'); return; }
  if(!address){ alert('Give the entry an address (e.g. 192.168.1.50).'); return; }
  const username=($('#netuser').value||'').trim()||null;
  const secret=($('#netsecret').value||'')||null;
  const notes=($('#netnotes').value||'').trim()||null;
  const b=$('#netsave'); b.disabled=true; b.textContent='Adding…';
  try{
    await api.netAdd(label,address,username,secret,notes);
    $('#netlabel').value=''; $('#netaddr').value=''; $('#netuser').value=''; $('#netsecret').value=''; $('#netnotes').value='';
    await refreshNet();
  }catch(e){ alert('Add network entry failed: '+e); }
  finally{ b.disabled=false; b.textContent='Add network entry'; }
}

// expose handlers referenced by inline onclick="" attributes
// ---------- agent dock (Phase 2, M2/M9) ----------
let agentBusy=false;
async function refreshAgentStatus(){
  const b=$('#aruntime');
  try{
    const st=await api.agentStatus();
    if(st.any){ b.textContent=st.active; b.classList.remove('off'); }
    else { b.textContent='no runtime'; b.classList.add('off'); }
  }catch(e){ b.textContent='status error'; b.classList.add('off'); }
}
function openAgent(){ $('#agent').classList.add('on'); refreshAgentStatus(); refreshRoster(); refreshToolHints(); if(memOpen) refreshMemory(); if(ctxOpen) refreshContext(); $('#aq').focus(); }
function closeAgent(){ $('#agent').classList.remove('on'); }
// ---------- suggested tools hint (M9) ----------
// A small, optional band of context-aware MCP/skill suggestions from the deterministic
// recommender (src-tauri/src/tooling.rs). Driven by what's loaded — whether a .bos is
// open and the kinds of ingested context files — plus the pending question. Each chip
// carries the "why now" as a tooltip. Purely advisory: it never sends or runs anything.
async function refreshToolHints(){
  const el=$('#atools'); if(!el) return;
  const q=($('#aq')&&$('#aq').value.trim())||null;
  let recs=[];
  try{ recs=await api.recommendTools(q); }catch(e){ recs=[]; }
  if(!recs.length){ el.innerHTML=''; el.style.display='none'; return; }
  el.style.display='';
  el.innerHTML='<span class="atoolslbl">Suggested tools</span>'
    + recs.map(r=>'<span class="atoolchip '+esc(r.kind)+'" title="'+esc(r.reason)+(r.invoke?' → '+r.invoke:'')+'">'
        +'<span class="atoolkind">'+esc(r.kind)+'</span>'+esc(r.name)+'</span>').join('');
}
// Minimal renderer: escape everything, then turn ``` fenced blocks into <pre>.
// A ```playbook fence is tagged and gets a "Load playbook" button that routes the
// block into the guide engine (loadGuideFromObject) so it renders + auto-grades.
// The raw JSON of each emitted block is kept here (not smuggled through an HTML
// attribute) and referenced by index from the button's inline handler.
const _pbBlocks=[];
function renderAnswer(text){
  const parts=(''+text).split('```');
  let html='';
  for(let i=0;i<parts.length;i++){
    if(i%2===0){ html+=esc(parts[i]); continue; }
    let body=parts[i]; const nl=body.indexOf('\n');
    const lang=(nl>=0?body.slice(0,nl):'').trim().toLowerCase();
    if(nl>=0) body=body.slice(nl+1);
    const clean=body.replace(/\n$/,'');
    if(lang==='playbook'){
      const pid=_pbBlocks.push(clean)-1;
      html+='<div class="pbtag">&#9654; Playbook'
        +'<button type="button" class="pbload" onclick="loadPlaybookFromBlock('+pid+',this)">&#9654; Load playbook</button></div>'
        +'<pre class="pb">'+esc(clean)+'</pre>'
        +'<div class="pberr" id="pberr'+pid+'"></div>';
    } else {
      html+='<pre>'+esc(clean)+'</pre>';
    }
  }
  return html;
}
// Parse + validate an emitted ```playbook block (by registry index) and route it
// into the guide engine. Surfaces a clear inline error instead of throwing.
function loadPlaybookFromBlock(pid, btn){
  const err=document.getElementById('pberr'+pid);
  if(err) err.textContent='';
  let pb;
  try{ pb=JSON.parse(_pbBlocks[pid]); }
  catch(e){ if(err) err.textContent='Could not load playbook — invalid JSON: '+e.message; return; }
  try{ loadGuideFromObject(pb); }
  catch(e){ if(err) err.textContent='Could not load playbook — '+e.message+'.'; return; }
  if(btn) btn.textContent='✓ Loaded';
}
// `meta` (bot only) carries the chosen agent + share-protocol handoffs returned by
// agent_ask, so each answer shows who replied and whether the Coordinator routed it.
function addMsg(role, text, runtime, meta){
  const box=document.createElement('div'); box.className='amsg '+role;
  let inner='';
  if(role==='bot'){
    let who=esc(runtime||'agent');
    if(meta && meta.agent){
      who+=' &middot; <span class="aagentname">'+esc(meta.agent.name)+'</span>'
        + (meta.routed ? ' <span class="mut">(auto-routed)</span>' : '');
    }
    inner+='<div class="who">'+who+'</div>';
  }
  else if(role==='err') inner+='<div class="who">error</div>';
  inner+=(role==='bot') ? renderAnswer(text) : esc(text);
  if(role==='bot' && meta && Array.isArray(meta.handoffs) && meta.handoffs.length){
    inner+='<div class="ahandoff">Consider also: '
      + meta.handoffs.map(h=>'<span class="achip">'+esc(h.name)+'</span>').join(' ')+'</div>';
  }
  box.innerHTML=inner;
  const m=$('#amsgs'); m.appendChild(box); m.scrollTop=m.scrollHeight;
  return box;
}
async function sendAsk(){
  if(agentBusy) return;
  const ta=$('#aq'); const q=ta.value.trim(); if(!q) return;
  const focusPath=($('#afocus').checked && CUR && CUR.path) ? CUR.path : null;
  const sel=$('#aagent'); const agentId=(sel && sel.value) ? sel.value : null;
  addMsg('user', q); ta.value='';
  agentBusy=true; const btn=$('#asend'); btn.disabled=true; btn.textContent='Asking…';
  const pending=addMsg('note','thinking…');
  try{
    const res=await api.agentAsk(q, focusPath, agentId);
    pending.remove(); addMsg('bot', res.answer, res.runtime, res);
  }catch(e){
    pending.remove(); addMsg('err', ''+e);
  }finally{
    agentBusy=false; btn.disabled=false; btn.textContent='Ask';
  }
}

// ---------- per-project agent memory (Phase 2, M5) ----------
// Operator-authored notes the agent loads into its grounding on every ask, so it
// remembers facts across sessions. Escape everything rendered; the type selector
// mirrors the backend's project|feature|reference|security categories.
let memOpen=false;
function toggleMem(){
  memOpen=!memOpen;
  $('#agentmem').classList.toggle('open',memOpen);
  $('#amembody').style.display=memOpen?'block':'none';
  if(memOpen) refreshMemory();
}
async function refreshMemory(){
  const list=$('#amemlist'); let notes=[];
  try{ notes=await api.memoryList(); }catch(e){ notes=[]; }
  $('#amemcount').textContent = notes.length ? (notes.length+' note'+(notes.length===1?'':'s')) : '';
  if(!notes.length){
    list.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No saved notes yet. Add one below — the agent loads them on every ask. Open a project first.</div>';
    return;
  }
  list.innerHTML=notes.map(n=>'<div class="amemitem"><div class="mtop">'
    +'<span class="amemtype '+esc(n.mtype)+'">'+esc(n.mtype)+'</span>'
    +'<span class="mslug">'+esc(n.slug)+'</span>'
    +'<span class="mdel" data-id="'+esc(n.id)+'">delete</span></div>'
    +'<div class="mbody">'+esc(n.body)+'</div></div>').join('');
  $$('#amemlist .mdel').forEach(b=>b.onclick=async()=>{
    if(!confirm('Delete this memory note?')) return;
    try{ await api.memoryDelete(Number(b.dataset.id)); await refreshMemory(); }
    catch(e){ alert('Delete note failed: '+e); }});
}
async function saveMemory(){
  const mtype=$('#amemtype').value;
  const slug=$('#amemslug').value.trim();
  const body=$('#amembodytext').value.trim();
  if(!slug){ alert('Give the note a short name (e.g. main-pump).'); return; }
  if(!body){ alert('Write something for the agent to remember.'); return; }
  try{
    await api.memorySet(mtype, slug, body);
    $('#amemslug').value=''; $('#amembodytext').value='';
    await refreshMemory();
  }catch(e){ alert('Save note failed: '+e); }
}

// ---------- agent roster + versioned instincts (Phase 2, M5) ----------
// The co-pilot is a roster of role-specialized agents. The "who answers" picker in
// the footer chooses the agent for the next ask (blank = the Coordinator routes);
// the collapsible panel views/edits each agent's instincts, which are versioned
// per project (append-only; revert is non-destructive). Escape everything rendered.
let rosOpen=false, rosAgents=[], rosCur=null;
async function refreshRoster(){
  try{ rosAgents=await api.agentRoster(); }catch(e){ rosAgents=[]; }
  // "Who answers" picker: Auto (Coordinator) + the specialists.
  const pick=$('#aagent');
  if(pick){
    const keep=pick.value;
    pick.innerHTML='<option value="">Auto (Coordinator routes)</option>'
      + rosAgents.filter(a=>!a.coordinator)
                 .map(a=>'<option value="'+esc(a.id)+'">'+esc(a.name)+'</option>').join('');
    pick.value=keep||'';
  }
  // Instincts editor agent select: all agents, flagged when customized.
  const sel=$('#arosagent');
  if(sel){
    const keep=sel.value||rosCur||(rosAgents[0]&&rosAgents[0].id)||'';
    sel.innerHTML=rosAgents.map(a=>'<option value="'+esc(a.id)+'">'+esc(a.name)
      +(a.customized?(' • v'+esc(a.version)):'')+'</option>').join('');
    if(keep) sel.value=keep;
  }
  if(rosOpen) await loadInstincts();
}
function toggleRos(){
  rosOpen=!rosOpen;
  $('#agentros').classList.toggle('open',rosOpen);
  $('#arosbodywrap').style.display=rosOpen?'block':'none';
  if(rosOpen) refreshRoster();
}
async function loadInstincts(){
  const sel=$('#arosagent'); if(!sel||!sel.value) return;
  rosCur=sel.value;
  $('#aroshist').innerHTML='';
  try{
    const d=await api.agentInstinctsGet(rosCur);
    $('#arosrole').textContent=d.role||'';
    $('#arosbody').value=d.body||'';
    $('#arosver').textContent = d.customized
      ? ('custom v'+d.version+(d.updated_at?(' · '+d.updated_at):''))
      : 'built-in default';
  }catch(e){
    // No project open (or read error): show the built-in role text read-only-ish.
    const a=rosAgents.find(x=>x.id===rosCur);
    $('#arosrole').textContent=a?a.role:'';
    $('#arosbody').value='';
    $('#arosver').textContent='Open a project to view and edit instincts.';
  }
}
async function saveInstincts(){
  if(!rosCur) return;
  const body=$('#arosbody').value.trim();
  if(!body){ alert('Instinct body cannot be empty.'); return; }
  try{
    const r=await api.agentInstinctsSet(rosCur, body);
    await refreshRoster();
    $('#arosver').textContent='saved → custom v'+r.version;
  }catch(e){ alert('Save instincts failed: '+e); }
}
async function revertInstincts(){
  if(!rosCur) return;
  if(!confirm('Revert this agent to its built-in default instincts? (kept as a new version)')) return;
  try{ await api.agentInstinctsRevert(rosCur, 0); await refreshRoster(); await loadInstincts(); }
  catch(e){ alert('Revert failed: '+e); }
}
async function showInstinctHistory(){
  if(!rosCur) return;
  const box=$('#aroshist');
  try{
    const hist=await api.agentInstinctsHistory(rosCur);
    if(!hist.length){
      box.innerHTML='<div class="mut" style="font-size:12px;padding:2px">No saved versions yet — using the built-in default.</div>';
      return;
    }
    box.innerHTML=hist.map(h=>'<div class="aroshrow">'
      +'<button type="button" class="arosrev" data-v="'+esc(h.version)+'">revert to this</button>'
      +'<b>v'+esc(h.version)+'</b> <span class="mut">'+esc(h.updated_at)+' · '+esc(h.chars)+' chars</span>'
      +'<div class="mbody">'+esc(h.preview)+(h.chars>120?'…':'')+'</div></div>').join('');
    $$('#aroshist .arosrev').forEach(b=>b.onclick=async()=>{
      try{ await api.agentInstinctsRevert(rosCur, Number(b.dataset.v)); await refreshRoster(); await loadInstincts(); }
      catch(e){ alert('Revert failed: '+e); }});
  }catch(e){ box.innerHTML='<div class="pberr">'+esc(''+e)+'</div>'; }
}

// ---------- context files (Phase 3, M1) ----------
// Ingested reference material (datasheets, Milesight CSV exports) the co-pilot
// retrieves from on every ask. Add a text/markdown/CSV/JSON file, a PDF, or a Word
// .docx via the OS file picker; it is extracted, chunked, and stored per project.
// Escape everything.
let ctxOpen=false;
function toggleCtx(){
  ctxOpen=!ctxOpen;
  $('#agentctx').classList.toggle('open',ctxOpen);
  $('#actxbody').style.display=ctxOpen?'block':'none';
  if(ctxOpen) refreshContext();
}
async function refreshContext(){
  const list=$('#actxlist'); let files=[];
  try{ files=await api.contextList(); }catch(e){ files=[]; }
  $('#actxcount').textContent = files.length ? (files.length+' file'+(files.length===1?'':'s')) : '';
  if(!files.length){
    list.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No context files yet. Add a datasheet (PDF, Word, text, CSV) or notes — the co-pilot retrieves from them on every ask. Open a project first.</div>';
    return;
  }
  list.innerHTML=files.map(f=>'<div class="actxitem"><div class="mtop">'
    +'<span class="actxkind">'+esc(f.kind||'?')+'</span>'
    +'<span class="actxname">'+esc(f.name)+'</span>'
    +'<span class="mdel" data-id="'+esc(f.id)+'">delete</span></div>'
    +'<div class="actxmeta mut">'+esc(f.chunks)+' chunk'+(f.chunks===1?'':'s')+' · '+fmtBytes(f.bytes)+'</div></div>').join('');
  $$('#actxlist .mdel').forEach(b=>b.onclick=async()=>{
    if(!confirm('Remove this context file? The co-pilot will no longer retrieve from it.')) return;
    try{ await api.contextDelete(Number(b.dataset.id)); await refreshContext(); }
    catch(e){ alert('Delete failed: '+e); }});
  // The recommender keys off the loaded file kinds — keep its chips in sync.
  refreshToolHints();
}
function fmtBytes(n){ n=Number(n)||0; if(n<1024) return n+' B'; if(n<1048576) return (n/1024).toFixed(1)+' KB'; return (n/1048576).toFixed(1)+' MB'; }
async function addContextFile(){
  let path;
  try{
    path=await openFileDialog({ multiple:false, title:'Add a context file',
      filters:[{ name:'Documents (PDF, Word, text, CSV, JSON)', extensions:['pdf','docx','txt','md','markdown','csv','tsv','log','json','yaml','yml','xml','ini','cfg','conf'] }] });
  }catch(e){ alert('Could not open the file picker: '+e); return; }
  if(!path) return;
  const btn=$('#actxadd'); btn.disabled=true; btn.textContent='Adding…';
  try{
    const r=await api.contextAddFile(path);
    await refreshContext();
    addMsg('note', 'Added "'+r.name+'" ('+r.chunks+' chunk'+(r.chunks===1?'':'s')+') to project context.');
  }catch(e){ alert('Add file failed: '+e); }
  finally{ btn.disabled=false; btn.textContent='Add file…'; }
}

// ---------- theme switcher (M4) ----------
// Swap the :root design-token set between the built-in Light default (index.html)
// and the Dark preset (styles.css), persist the choice to localStorage, and apply
// it on load. The whole product themes off these tokens — the configurator shell
// and the bundled wiki pages — so one toggle re-skins everything consistently.
const THEME_KEY = 'hyperion-theme';
function applyTheme(t){
  const v = (t==='dark') ? 'dark' : 'light';
  document.documentElement.setAttribute('data-theme', v);
  const sel=$('#themesel'); if(sel) sel.value=v;
  try{ localStorage.setItem(THEME_KEY, v); }catch(e){}
}
function initTheme(){
  let t='light';
  try{ t=localStorage.getItem(THEME_KEY)||'light'; }catch(e){}
  applyTheme(t);
  const sel=$('#themesel'); if(sel) sel.onchange=()=>applyTheme(sel.value);
}

// ---------- editable wiki pages (M4) ----------
// Operator-authored knowledge pages persisted per project (projects.rs wiki_page).
// The modal lists pages, loads one into the editor, and saves (upsert by slug). The
// backend slugifies the slug, validates, and runs the plaintext-secret guard, so a
// saved page can't smuggle a credential into the project DB. Escape rendered text.
async function refreshWikiList(selectSlug){
  const sel=$('#wikilist'); if(!sel) return;
  let pages=[];
  try{ pages=await api.wikiList(); }catch(e){ pages=[]; }
  sel.innerHTML='<option value="">- pick a page -</option>'
    + pages.map(p=>'<option value="'+esc(p.slug)+'">'+esc(p.title)+' ('+esc(p.slug)+')</option>').join('');
  if(selectSlug) sel.value=selectSlug;
  $('#wikimeta').textContent = pages.length
    ? (pages.length+' page'+(pages.length===1?'':'s'))
    : 'No pages yet — write one below.';
}
async function loadWikiPage(slug){
  const st=$('#wikistatus'); st.textContent='';
  if(!slug){ return; }
  let page=null;
  try{ page=await api.wikiGet(slug); }catch(e){ st.textContent='Load failed: '+e; return; }
  if(!page){ st.textContent='That page no longer exists.'; await refreshWikiList(); return; }
  $('#wikislug').value=page.slug||'';
  $('#wikititle').value=page.title||'';
  $('#wikihtml').value=page.html||'';
  st.textContent = page.updated_at ? ('Loaded · updated '+page.updated_at) : 'Loaded';
}
function newWikiPage(){
  $('#wikilist').value='';
  $('#wikislug').value=''; $('#wikititle').value=''; $('#wikihtml').value='';
  $('#wikistatus').textContent='New page — give it a slug, a title and some HTML.';
  $('#wikislug').focus();
}
async function saveWikiPage(){
  const slug=$('#wikislug').value.trim();
  const title=$('#wikititle').value.trim();
  const html=$('#wikihtml').value;
  if(!slug){ alert('Give the page a slug (e.g. network-registry).'); return; }
  if(!title){ alert('Give the page a title.'); return; }
  if(!html.trim()){ alert('The page needs some HTML content.'); return; }
  const btn=$('#wikisave'); btn.disabled=true; btn.textContent='Saving…';
  try{
    await api.wikiSave(slug, title, html);
    await refreshWikiList(slug);
    // The backend slugifies; reflect the canonical slug back into the field.
    const sel=$('#wikilist'); if(sel && sel.value) $('#wikislug').value=sel.value;
    $('#wikistatus').textContent='Saved.';
  }catch(e){ alert('Save page failed: '+e); $('#wikistatus').textContent=''; }
  finally{ btn.disabled=false; btn.textContent='Save page'; }
}
// ---------- artifact templates (Track 4; artifacts.rs) ----------
// A pickable library of the bundled HTML-effectiveness patterns. The catalog is
// static (compiled into the binary), so it's fetched once and cached on the select.
// "Insert template" drops the chosen pattern's full, themeable HTML into the editor
// as a starting point; the operator edits it and Saves (which still runs the
// plaintext-secret guard + length check in wiki_save). Advisory only — no execution.
async function refreshWikiTemplates(){
  const sel=$('#wikitpl'); if(!sel || sel.dataset.loaded) return;
  let tpls=[];
  try{ tpls=await api.artifactTemplatesList(); }catch(e){ tpls=[]; }
  if(!tpls.length) return;
  sel.innerHTML='<option value="">- artifact template -</option>'
    + tpls.map(t=>'<option value="'+esc(t.key)+'" title="'+esc(t.description)+'">'+esc(t.label)+'</option>').join('');
  sel.dataset.loaded='1';
}
async function insertWikiTemplate(){
  const sel=$('#wikitpl'); const key=sel && sel.value; if(!key){ return; }
  const ta=$('#wikihtml'); if(!ta) return;
  let res=null;
  try{ res=await api.artifactTemplateGet(key); }catch(e){ alert('Load template failed: '+e); return; }
  const html=(res && res.html)||'';
  if(!html){ return; }
  const cur=ta.value;
  if(!cur.trim()){
    ta.value=html;                                    // empty editor: the template becomes the page
  }else{
    const s=ta.selectionStart ?? cur.length, e=ta.selectionEnd ?? cur.length;
    ta.value=cur.slice(0,s)+html+cur.slice(e);        // otherwise splice at the cursor
  }
  ta.focus();
  $('#wikistatus').textContent='Inserted the "'+key+'" template — edit and Save.';
}
// Refresh the templates' "use when…" guidance live from the html-effectiveness guide
// (artifact_guide_refresh). Optional + non-fatal: with no HYPERION_FIRECRAWL_API_KEY,
// or on a failed fetch, the backend returns {refreshed:false, reason} which we just
// surface in the status line. The bundled template HTML is never altered; the derived
// notes are stored as project knowledge (so it needs an open project).
async function refreshArtifactGuide(){
  const btn=$('#wikitplrefresh');
  if(btn){ btn.disabled=true; btn.textContent='Refreshing…'; }
  try{
    const res=await api.artifactGuideRefresh();
    if(res && res.refreshed){
      const techs=(res.techniques||[]).join(', ');
      $('#wikistatus').textContent='Refreshed artifact guide — saved '+(res.count||0)+' technique note(s) to project knowledge'+(techs?(': '+techs):'')+'.';
    }else{
      $('#wikistatus').textContent=(res && res.reason) ? res.reason : 'Guide refresh did nothing.';
    }
  }catch(e){ $('#wikistatus').textContent='Guide refresh failed: '+e; }
  finally{ if(btn){ btn.disabled=false; btn.textContent='Refresh guide'; } }
}
function openWiki(){ $('#wiki').classList.add('on'); refreshWikiList(); refreshWikiTemplates(); }
function closeWiki(){ $('#wiki').classList.remove('on'); }

// ---------- project: PRs, timeline & snapshot diff (Group A; collab.rs + lib.rs) ----------
// One modal (#vcs) with three collapsible sections, all scoped to the open project
// (same "open a project first" contract as memory). Escape everything rendered.
function openVcs(){ $('#vcs').classList.add('on'); refreshVcs(); }
function closeVcs(){ $('#vcs').classList.remove('on'); }
function refreshVcs(){ if(prOpen) refreshPrs(); if(tlOpen) refreshTimeline(); if(diffOpen) refreshDiffPickers(); }

// ----- pull requests -----
const PR_ST={open:['st-todo','open'],merged:['st-done','merged'],closed:['st-na','closed']};
let prOpen=false, prDetail=null;   // prDetail = id of the PR shown in detail, or null for the list
function togglePr(){
  prOpen=!prOpen;
  $('#vcspr').classList.toggle('open',prOpen);
  $('#prbody').style.display=prOpen?'block':'none';
  if(prOpen){ prDetail=null; refreshPrs(); }
}
async function refreshPrs(){
  // Detail view takes over the pane when a PR is open.
  if(prDetail!=null){ await renderPrDetail(prDetail); return; }
  const pane=$('#prpane'); let prs=[];
  try{ prs=await api.prList(); }catch(e){ pane.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">'+esc(''+e)+'</div>'+prCreateForm(); wirePrCreate(); $('#prcount').textContent=''; return; }
  $('#prcount').textContent = prs.length ? (prs.length+' PR'+(prs.length===1?'':'s')) : '';
  const list = prs.length
    ? prs.map(p=>{const [cls,lbl]=PR_ST[p.status]||PR_ST.open;
        return '<div class="amemitem prrow" data-id="'+esc(p.id)+'"><div class="mtop">'
          +'<span class="badge-st '+cls+'">'+lbl+'</span>'
          +'<span class="mslug">'+esc(p.title)+'</span>'
          +'<span class="prcmt mut">&#128172; '+esc(p.comments)+'</span></div>'
          +'<div class="actxmeta mut">#'+esc(p.id)+' · '+esc(p.created_at)+'</div></div>';}).join('')
    : '<div class="mut" style="font-size:12px;padding:4px 2px">No pull requests yet. Open one below.</div>';
  pane.innerHTML='<div id="prlist" class="amemlist">'+list+'</div>'+prCreateForm();
  $$('#prlist .prrow').forEach(r=>r.onclick=()=>{ prDetail=Number(r.dataset.id); refreshPrs(); });
  wirePrCreate();
}
function prCreateForm(){
  return '<div class="vcsform">'
    +'<div class="vcsformh">New pull request</div>'
    +'<input id="prtitle" placeholder="title">'
    +'<textarea id="prnarr" rows="2" placeholder="narrative (human) — what & why"></textarea>'
    +'<textarea id="prdocs" rows="2" placeholder="ai_docs (agent notes) — optional"></textarea>'
    +'<button type="button" id="prcreate" class="appbtn">Create PR</button></div>';
}
function wirePrCreate(){ const b=$('#prcreate'); if(b) b.onclick=createPr; }
async function createPr(){
  const title=($('#prtitle').value||'').trim();
  if(!title){ alert('Give the pull request a title.'); return; }
  const narrative=($('#prnarr').value||'').trim()||null;
  const aiDocs=($('#prdocs').value||'').trim()||null;
  const b=$('#prcreate'); b.disabled=true; b.textContent='Creating…';
  try{ await api.prCreate(title,narrative,aiDocs); await refreshPrs(); }
  catch(e){ alert('Create PR failed: '+e); b.disabled=false; b.textContent='Create PR'; }
}
async function renderPrDetail(id){
  const pane=$('#prpane'); let pr=null;
  try{ pr=await api.prGet(id); }catch(e){ pane.innerHTML='<div class="mut">'+esc(''+e)+'</div>'; return; }
  if(!pr){ prDetail=null; await refreshPrs(); return; }
  const [cls,lbl]=PR_ST[pr.status]||PR_ST.open;
  let h='<div class="prback reflink" id="prback">&#8592; All pull requests</div>';
  h+='<div class="prdh"><span class="badge-st '+cls+'">'+lbl+'</span>'
    +'<span class="prtitle">'+esc(pr.title)+'</span>'
    +'<span class="mut" style="font-size:11px">#'+esc(pr.id)+' · '+esc(pr.created_at)+'</span></div>';
  if(pr.narrative) h+='<div class="section"><div class="h">Narrative</div><div class="b prdoc">'+renderAnswer(pr.narrative)+'</div></div>';
  if(pr.ai_docs)   h+='<div class="section"><div class="h">AI docs</div><div class="b prdoc">'+renderAnswer(pr.ai_docs)+'</div></div>';
  const cmts=Array.isArray(pr.comments)?pr.comments:[];
  h+='<div class="section"><div class="h">Comments ('+cmts.length+')</div><div class="b">';
  h+= cmts.length
    ? cmts.map(c=>'<div class="prcmtbubble"><div class="prcmth"><b>'+esc(c.author)+'</b> <span class="mut">'+esc(c.created_at)+'</span></div>'
        +'<div class="prcmtbody">'+esc(c.body)+'</div></div>').join('')
    : '<div class="mut" style="font-size:12px">No comments yet.</div>';
  h+='</div></div>';
  // Composer + status + delete.
  h+='<div class="vcsform"><div class="vcsformh">Add comment</div>'
    +'<input id="prcauthor" placeholder="your name">'
    +'<textarea id="prcbody" rows="2" placeholder="comment…"></textarea>'
    +'<button type="button" id="prcsend" class="appbtn">Add comment</button></div>';
  h+='<div class="prdactions"><label class="mut" style="font-size:12px">Status: '
    +'<select id="prstatus">'
    + ['open','merged','closed'].map(s=>'<option value="'+s+'"'+(s===pr.status?' selected':'')+'>'+s+'</option>').join('')
    +'</select></label>'
    +'<button type="button" id="prdel" class="appbtn prdanger">Delete PR</button></div>';
  pane.innerHTML=h;
  $('#prback').onclick=()=>{ prDetail=null; refreshPrs(); };
  $('#prcsend').onclick=async()=>{
    const author=($('#prcauthor').value||'').trim(); const body=($('#prcbody').value||'').trim();
    if(!author){ alert('Enter your name.'); return; }
    if(!body){ alert('Write a comment.'); return; }
    const b=$('#prcsend'); b.disabled=true; b.textContent='Sending…';
    try{ await api.prCommentAdd(id,author,body); await renderPrDetail(id); }
    catch(e){ alert('Add comment failed: '+e); b.disabled=false; b.textContent='Add comment'; }
  };
  $('#prstatus').onchange=async(e)=>{
    try{ await api.prSetStatus(id,e.target.value); await renderPrDetail(id); }
    catch(err){ alert('Set status failed: '+err); }
  };
  $('#prdel').onclick=async()=>{
    if(!confirm('Delete this pull request and its comments?')) return;
    try{ await api.prDelete(id); prDetail=null; await refreshPrs(); }
    catch(e){ alert('Delete PR failed: '+e); }
  };
}

// ----- timeline -----
let tlOpen=false;
function toggleTimeline(){
  tlOpen=!tlOpen;
  $('#vcstl').classList.toggle('open',tlOpen);
  $('#tlbody').style.display=tlOpen?'block':'none';
  if(tlOpen) refreshTimeline();
}
async function refreshTimeline(){
  const list=$('#tllist'); let events=[];
  try{ events=await api.timelineList(null); }catch(e){
    list.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">'+esc(''+e)+'</div>'; $('#tlcount').textContent=''; return; }
  $('#tlcount').textContent = events.length ? (events.length+' event'+(events.length===1?'':'s')) : '';
  list.innerHTML = events.length
    ? events.map(ev=>'<div class="amemitem"><div class="mtop">'
        +'<span class="amemtype">'+esc(ev.kind)+'</span>'
        +'<span class="mslug">'+esc(ev.summary)+'</span></div>'
        +(ev.detail?'<div class="mbody">'+esc(ev.detail)+'</div>':'')
        +'<div class="actxmeta mut">'+esc(ev.created_at)+'</div></div>').join('')
    : '<div class="mut" style="font-size:12px;padding:4px 2px">No timeline events yet. Add one below.</div>';
}
async function addTimeline(){
  const kind=$('#tlkind').value;
  const summary=($('#tlsummary').value||'').trim();
  const detail=($('#tldetail').value||'').trim()||null;
  if(!summary){ alert('Write a short summary for the event.'); return; }
  const b=$('#tladd'); b.disabled=true; b.textContent='Adding…';
  try{ await api.timelineAdd(kind,summary,detail); $('#tlsummary').value=''; $('#tldetail').value=''; await refreshTimeline(); }
  catch(e){ alert('Add timeline event failed: '+e); }
  finally{ b.disabled=false; b.textContent='Add event'; }
}

// ----- snapshot diff -----
let diffOpen=false;
function toggleDiff(){
  diffOpen=!diffOpen;
  $('#vcsdiff').classList.toggle('open',diffOpen);
  $('#diffbody').style.display=diffOpen?'block':'none';
  if(diffOpen) refreshDiffPickers();
}
function refreshDiffPickers(){
  const opts='<option value="">- snapshot -</option>'
    + projSnapshots.map(s=>'<option value="'+esc(s.id)+'">'+esc(s.label||('snapshot '+s.id))
        +(s.created_at?(' · '+esc(s.created_at)):'')+'</option>').join('');
  const from=$('#difffrom'), to=$('#diffto');
  const kf=from.value, kt=to.value;
  from.innerHTML=opts; to.innerHTML=opts;
  if(kf) from.value=kf; if(kt) to.value=kt;
  if(!projSnapshots.length)
    $('#diffout').innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">This project has no snapshots yet — import a .bos first.</div>';
}
async function runSnapshotDiff(){
  const from=$('#difffrom').value, to=$('#diffto').value;
  if(!from||!to){ alert('Pick two snapshots to compare.'); return; }
  if(from===to){ alert('Pick two different snapshots.'); return; }
  const out=$('#diffout'); out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">Comparing…</div>';
  let d;
  try{ d=await api.snapshotDiff(Number(from),Number(to)); }
  catch(e){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">'+esc(''+e)+'</div>'; return; }
  const added=d.added||[], removed=d.removed||[], changed=d.changed||[];
  if(!added.length && !removed.length && !changed.length){
    out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No differences between these two snapshots.</div>'; return; }
  let h='';
  const nodeRows=arr=>'<div class="cmdbox">'+arr.map(n=>'<div class="cmd '+(arr===added?'added':'removed')+'"><span class="t">'
      +esc(n.path)+(n.type?(' <span class="mut">('+esc(n.type)+')</span>'):'')+'</span></div>').join('')+'</div>';
  if(added.length)   h+='<div class="section"><div class="h">Added ('+added.length+')</div><div class="b">'+nodeRows(added)+'</div></div>';
  if(removed.length) h+='<div class="section"><div class="h">Removed ('+removed.length+')</div><div class="b">'+nodeRows(removed)+'</div></div>';
  if(changed.length){
    h+='<div class="section"><div class="h">Changed ('+changed.length+')</div><div class="b">';
    h+=changed.map(c=>'<div class="kv"><span><span class="reflink" onclick="navigate(this.dataset.p)" data-p="'+esc(c.path)+'">'
        +esc(c.path)+'</span> <span class="mut">'+esc(c.field||'(node)')+'</span></span>'
        +'<span class="ba"><s>'+esc(diffVal(c.before))+'</s><span class="arr">&#8594;</span><b>'+esc(diffVal(c.after))+'</b></span></div>').join('');
    h+='</div></div>';
  }
  out.innerHTML=h;
}
function diffVal(v){ if(v===null||v===undefined) return '∅'; if(typeof v==='object') return JSON.stringify(v); return ''+v; }

// ---------- Group B: knowledge & quality (#kq modal) ----------
// One modal with collapsible sections, mirroring the #vcs Project modal exactly
// (same .amem collapsibles, same "open a project first" contract where a command
// needs the project DB). Every section is a thin wrapper over a real invoke() with
// inline error handling. Read-only toward bOS. Escape everything rendered.
function openKq(){ $('#kq').classList.add('on'); }
function closeKq(){ $('#kq').classList.remove('on'); }
// Shared collapsible-section toggler (id-driven, like togglePr/toggleTimeline).
function kqToggle(secId, bodyId, fn){
  const sec=$(secId); const open=!sec.classList.contains('open');
  sec.classList.toggle('open',open);
  $(bodyId).style.display=open?'block':'none';
  if(open && fn) fn();
}
// Severity badge reusing the guide's badge-st palette (high=amber-red, medium=amber, low=grey).
const SEV={high:['st-partial','high'],medium:['st-partial','medium'],low:['st-todo','low'],
  critical:['st-partial','critical']};
function sevBadge(sev){ const [cls,lbl]=SEV[sev]||['st-na',esc(sev||'note')];
  return '<span class="badge-st '+cls+' sev-'+esc(sev||'na')+'">'+esc(lbl)+'</span>'; }
function kqErr(out, e){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">'+esc(''+e)+'</div>'; }
function kqBusy(out, msg){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">'+esc(msg)+'</div>'; }

// ----- context suggestions (suggest.rs) -----
async function runContextSuggest(){
  const out=$('#ksuglist'); const q=($('#ksugq').value||'').trim()||null;
  kqBusy(out,'Looking for gaps…');
  let recs=[];
  try{ recs=await api.contextSuggest(q); }catch(e){ kqErr(out,e); $('#ksugcount').textContent=''; return; }
  $('#ksugcount').textContent = recs.length ? (recs.length+' suggestion'+(recs.length===1?'':'s')) : '';
  if(!recs.length){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No gaps found — the project context looks complete.</div>'; return; }
  out.innerHTML=recs.map(r=>'<div class="amemitem"><div class="mtop">'
    +sevBadge(r.severity)
    +'<span class="amemtype">'+esc(r.kind)+'</span></div>'
    +'<div class="mbody">'+esc(r.message)+'</div></div>').join('');
}

// ----- Milesight import -> IoT topology (milesight.rs) -----
async function runMilesightImport(){
  const out=$('#mstopo'); const btn=$('#msimport');
  let path;
  try{ path=await openFileDialog({ multiple:false, directory:false, title:'Pick a Milesight gateway export',
        filters:[{ name:'Gateway config (JSON)', extensions:['json'] }] }); }
  catch(e){ alert('Could not open the file picker: '+e); return; }
  if(!path) return;
  btn.disabled=true; btn.textContent='Parsing…';
  kqBusy(out,'Parsing gateway export…');
  let topo;
  try{ topo=await api.milesightImport(path); }
  catch(e){ kqErr(out,e); btn.disabled=false; btn.textContent='Pick gateway export…'; return; }
  renderTopology(out, topo);
  btn.disabled=false; btn.textContent='Pick gateway export…';
}
function topoKv(label, val){ return '<div class="kv"><b>'+esc(label)+'</b><span class="v">'
  +(val===null||val===undefined||val===''?'<span class="mut">—</span>':esc(''+val))+'</span></div>'; }
function renderTopology(out, t){
  const g=t.gateway||{}, l=t.lora||{}, devs=Array.isArray(t.devices)?t.devices:[];
  let h='<div class="section"><div class="h">Gateway</div><div class="b">'
    +topoKv('Name',g.name)+topoKv('Model',g.model)+topoKv('EUI',g.eui)+topoKv('IP',g.ip)+'</div></div>';
  h+='<div class="section"><div class="h">LoRa</div><div class="b">'
    +topoKv('Region',l.region)+topoKv('Frequency',l.frequency)+'</div></div>';
  h+='<div class="section"><div class="h">Devices ('+devs.length+')</div><div class="b">';
  h+= devs.length
    ? devs.map(d=>{const [ic,col]=icon(d.type||'');
        return '<div class="kv"><span><span class="ic" style="color:'+(col||'#888')+'">'+ic+'</span> '
          +esc(d.name||'(unnamed)')+(d.type?(' <span class="mut">'+esc(d.type)+'</span>'):'')+'</span>'
          +'<span class="v mut">'+esc(d.dev_eui||'')+(d.last_seen?(' · '+esc(d.last_seen)):'')+'</span></div>';}).join('')
    : '<div class="mut" style="font-size:12px">No end-devices found in the export.</div>';
  h+='</div></div>';
  out.innerHTML=h;
}

// ----- knowledge crawler (crawler.rs + projects.rs) -----
async function refreshCrawl(){
  const list=$('#crawllist'); let docs=[];
  try{ docs=await api.crawlList(); }catch(e){ kqErr(list,e); $('#crawlcount').textContent=''; return; }
  $('#crawlcount').textContent = docs.length ? (docs.length+' page'+(docs.length===1?'':'s')) : '';
  list.innerHTML = docs.length
    ? docs.map(d=>'<div class="amemitem"><div class="mtop">'
        +'<span class="actxkind">'+esc(d.source||'web')+'</span>'
        +'<span class="mslug">'+esc(d.title||d.url)+'</span>'
        +'<span class="mdel" data-id="'+esc(d.id)+'">delete</span></div>'
        +'<div class="actxmeta mut">'+esc(d.url)+' · '+fmtBytes(d.bytes)+' · '+esc(d.fetched_at)+'</div></div>').join('')
    : '<div class="mut" style="font-size:12px;padding:4px 2px">No crawled pages yet. Add a URL above. Open a project first.</div>';
  $$('#crawllist .mdel').forEach(b=>b.onclick=async()=>{
    if(!confirm('Remove this crawled page?')) return;
    try{ await api.crawlDelete(Number(b.dataset.id)); await refreshCrawl(); }
    catch(e){ alert('Delete failed: '+e); }});
}
// ----- curated crawl source registry + tiered sweep (projects.rs crawl_source / crawl_sweep) -----
async function refreshCrawlSources(){
  const list=$('#crawlsrclist'); let srcs=[];
  try{ srcs=await api.crawlSourceList(); }catch(e){ kqErr(list,e); return; }
  list.innerHTML = srcs.length
    ? srcs.map(s=>'<div class="amemitem"><div class="mtop">'
        +'<span class="actxkind">'+esc(s.kind)+'</span>'
        +'<span class="mslug">'+esc(s.label||s.url)+'</span>'
        +'<label class="mut" style="font-size:11px;margin-left:auto;display:inline-flex;align-items:center;gap:3px">'
          +'<input type="checkbox" class="srcen" data-id="'+esc(s.id)+'"'+(s.enabled?' checked':'')+'> on</label>'
        +'<span class="mdel" data-id="'+esc(s.id)+'">delete</span></div>'
        +'<div class="actxmeta mut">'+esc(s.url)+'</div></div>').join('')
    : '<div class="mut" style="font-size:12px;padding:4px 2px">No sources curated yet. Add official docs/forum URLs above, then "Sweep now". Open a project first.</div>';
  $$('#crawlsrclist .srcen').forEach(c=>c.onclick=async()=>{
    try{ await api.crawlSourceSetEnabled(Number(c.dataset.id), c.checked); }
    catch(e){ alert('Update failed: '+e); await refreshCrawlSources(); }});
  $$('#crawlsrclist .mdel').forEach(b=>b.onclick=async()=>{
    if(!confirm('Remove this source? (cached pages are kept)')) return;
    try{ await api.crawlSourceRemove(Number(b.dataset.id)); await refreshCrawlSources(); }
    catch(e){ alert('Delete failed: '+e); }});
}
async function addCrawlSource(){
  const inp=$('#crawlsrcurl'); const url=(inp.value||'').trim();
  if(!url){ alert('Enter a source URL (e.g. https://wiki.comfortclick.com/...).'); return; }
  const label=($('#crawlsrclabel').value||'').trim();
  const kind=$('#crawlsrckind').value||'docs';
  try{
    await api.crawlSourceAdd(url, label||null, kind);
    inp.value=''; $('#crawlsrclabel').value='';
    await refreshCrawlSources();
  }catch(e){ alert('Add source failed: '+e); }
}
// Tiered sweep: CHEAP fetch+strip+store over every enabled source (deduped, safe to
// re-run), then an optional SMART eureka-distill pass. Refreshes the cached-page list.
async function sweepCrawl(){
  const btn=$('#crawlsweep'), st=$('#crawlsweepstatus');
  const smart=$('#crawlsweepsmart').checked;
  btn.disabled=true; const label=btn.textContent; btn.textContent='Sweeping…'; st.textContent='';
  try{
    const r=await api.crawlSweep(smart);
    await refreshCrawl();
    let msg=r.sources+' source'+(r.sources===1?'':'s')+' swept · '
      +r.created+' new, '+r.updated+' updated, '+r.unchanged+' unchanged, '+r.failed+' failed';
    if(smart) msg+=' · '+r.eureka_findings+' eureka finding'+(r.eureka_findings===1?'':'s');
    if(r.failed){ msg+=' — '+(r.errors||[]).map(e=>esc(e.url)).join(', '); }
    st.textContent=msg;
  }catch(e){ st.textContent='Sweep failed: '+e; }
  finally{ btn.disabled=false; btn.textContent=label; }
}
async function addCrawl(){
  const inp=$('#crawlurl'); const url=(inp.value||'').trim();
  if(!url){ alert('Enter a URL to crawl (e.g. https://wiki.comfortclick.com/...).'); return; }
  const btn=$('#crawladd'); btn.disabled=true; btn.textContent='Crawling…';
  try{
    const r=await api.crawlAdd(url);
    inp.value='';
    await refreshCrawl();
    $('#crawlstatus').textContent='Cached "'+(r.title||r.url)+'" ('+fmtBytes(r.bytes)+').';
  }catch(e){ alert('Crawl failed: '+e); $('#crawlstatus').textContent=''; }
  finally{ btn.disabled=false; btn.textContent='Crawl URL'; }
}
async function runEureka(){
  const out=$('#crawleureka'); kqBusy(out,'Scanning crawled pages…');
  let recs=[];
  try{ recs=await api.crawlEureka(); }catch(e){ kqErr(out,e); return; }
  if(!recs.length){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No eureka links — crawled pages add nothing new beyond your loaded context.</div>'; return; }
  out.innerHTML='<div class="section"><div class="h">Eureka — terms worth documenting ('+recs.length+')</div><div class="b">'
    + recs.map(r=>'<div class="amemitem"><div class="mtop">'
        +'<span class="amemtype">'+esc(r.term)+'</span>'
        +'<span class="prcmt mut">weight '+esc(r.weight)+'</span></div>'
        +'<div class="mbody">'+esc(r.message)+'</div>'
        +'<div class="actxmeta mut">source: '+esc(r.source)+'</div></div>').join('')
    +'</div></div>';
}
// Close the knowledge loop: draft the current eureka findings into a human-approvable
// in-app PR (backend secret-scans every field). Surfaces the created PR's id/title or a
// clear "nothing novel" message; refreshes the PR pane if it's open.
async function proposeEurekaPr(){
  const btn=$('#crawlproposebtn'), st=$('#crawlpropose');
  btn.disabled=true; const label=btn.textContent; btn.textContent='Proposing…'; st.textContent='';
  try{
    const r=await api.crawlEurekaProposePr();
    if(r && r.created){
      st.textContent='Opened PR #'+r.pr_id+' — "'+(r.title||'')+'" ('+r.count+' finding'+(r.count===1?'':'s')+'). Review it in the PRs panel.';
      if(typeof prOpen!=='undefined' && prOpen){ prDetail=null; await refreshPrs(); }
    }else{
      st.textContent=(r&&r.reason)?r.reason:'Nothing novel to propose.';
    }
  }catch(e){ st.textContent='Propose failed: '+e; }
  finally{ btn.disabled=false; btn.textContent=label; }
}

// ----- code standard + audit (standard.rs) -----
async function runCodeAudit(){
  const out=$('#codeout'); kqBusy(out,'Reading the standard & auditing sources…');
  let std, findings;
  try{ std=await api.codeStandard(); }catch(e){ kqErr(out,e); return; }
  try{ findings=await api.codeAudit(); }catch(e){ kqErr(out,e); return; }
  let h='<div class="section"><div class="h">'+esc(std.title||'Code standard')+'</div><div class="b">';
  h+='<div class="mbody" style="margin-bottom:6px">'+esc(std.summary||'')+'</div>';
  (std.rules||[]).forEach(r=>h+='<div class="amemitem"><div class="mtop">'
    +sevBadge(r.severity)+'<span class="amemtype">'+esc(r.applies_to)+'</span>'
    +'<span class="mslug">'+esc(r.title)+'</span></div>'
    +'<div class="actxmeta mut">fix: '+esc(r.fix)+'</div></div>');
  h+='</div></div>';
  h+='<div class="section"><div class="h">Audit findings ('+findings.length+')</div><div class="b">';
  h+= findings.length
    ? findings.map(f=>'<div class="amemitem"><div class="mtop">'
        +sevBadge(f.severity)+'<span class="amemtype">'+esc(f.rule)+'</span>'
        +'<span class="mslug">'+esc(f.path)+':'+esc(f.line)+'</span></div>'
        +'<div class="mbody">'+esc(f.message)+'</div>'
        +'<div class="actxmeta mut">suggested fix: '+esc(f.suggested_fix)+'</div></div>').join('')
    : '<div class="mut" style="font-size:12px">No deviations — sources are clean against the standard.</div>';
  h+='</div></div>';
  out.innerHTML=h;
}

// ----- security scan + enterprise gate (security.rs) -----
async function runSecurityScan(){
  const out=$('#secout'); kqBusy(out,'Scanning sources for risky patterns…');
  let findings;
  try{ findings=await api.securityScan(); }catch(e){ kqErr(out,e); return; }
  out.innerHTML='<div class="section"><div class="h">Security findings ('+findings.length+')</div><div class="b">'
    + (findings.length
        ? findings.map(f=>'<div class="amemitem"><div class="mtop">'
            +sevBadge(f.severity)+'<span class="amemtype">'+esc(f.kind)+'</span>'
            +'<span class="mslug">'+esc(f.path)+':'+esc(f.line)+'</span></div>'
            +'<div class="mbody">'+esc(f.message)+'</div></div>').join('')
        : '<div class="chkitem chk-ok">&#10003; No risky patterns found in the sources.</div>')
    +'</div></div>';
}
async function runGateCheck(){
  const out=$('#gateout'); kqBusy(out,'Evaluating enterprise-readiness gate…');
  let res;
  try{ res=await api.enterpriseGateCheck(); }catch(e){ kqErr(out,e); return; }
  const items=Array.isArray(res.items)?res.items:[];
  let h='<div class="section"><div class="h">Enterprise gate '
    +'<span class="badge-st '+(res.passed?'st-done':'st-todo')+'">'+(res.passed?'✓ passed':'○ not yet')+'</span>'
    +'</div><div class="b">';
  h+= items.map(it=>'<div class="amemitem"><div class="mtop">'
      +'<span class="chk'+(it.ok?'-ok':'-no')+'" style="font-weight:700">'+(it.ok?'&#10003;':'&#10007;')+'</span>'
      +'<span class="mslug">'+esc(it.name)+'</span></div>'
      +'<div class="mbody">'+esc(it.detail)+'</div></div>').join('');
  h+='</div></div>';
  out.innerHTML=h;
}

// ----- tool recommendations (tooling.rs; same recommender as the dock chips) -----
async function runRecommend(){
  const out=$('#reclist'); const q=($('#recq').value||'').trim()||null;
  kqBusy(out,'Matching tools to your context…');
  let recs=[];
  try{ recs=await api.recommendTools(q); }catch(e){ kqErr(out,e); return; }
  if(!recs.length){ out.innerHTML='<div class="mut" style="font-size:12px;padding:4px 2px">No recommendations for the current context.</div>'; return; }
  // The list is already in domain-priority order, so reading it top-to-bottom is the
  // suggested tool plan. Each item shows a copy-pasteable "how to run it" hint
  // (slash-command / MCP server·tool) for the operator or the external agent runtime
  // — Hyperion does not execute skills or MCP tools itself; this is guidance only.
  out.innerHTML='<div class="mut" style="font-size:11px;padding:0 2px 4px">Suggested tool plan, in order &mdash; run each in your agent runtime (advisory; Hyperion does not auto-run them).</div>'
    +recs.map((r,i)=>'<div class="amemitem"><div class="mtop">'
    +'<span class="mut" style="font-size:11px;font-family:Consolas,ui-monospace,monospace;margin-right:6px">'+(i+1)+'.</span>'
    +'<span class="actxkind '+esc(r.kind)+'">'+esc(r.kind)+'</span>'
    +'<span class="mslug">'+esc(r.name)+'</span></div>'
    +'<div class="mbody">'+esc(r.reason)+'</div>'
    +(r.invoke?'<div class="mbody" style="font-family:Consolas,ui-monospace,monospace;font-size:11.5px;color:var(--acc);margin-top:3px">'+esc(r.invoke)+'</div>':'')
    +'</div>').join('');
}

// ----- wiki export to a chosen folder (export.rs) -----
async function runWikiExport(){
  const out=$('#wikiexpout'); const btn=$('#wikiexpbtn');
  let dest;
  try{ dest=await openFileDialog({ directory:true, multiple:false, title:'Export wiki to folder' }); }
  catch(e){ alert('Could not open the folder picker: '+e); return; }
  if(!dest) return;
  btn.disabled=true; btn.textContent='Exporting…';
  kqBusy(out,'Writing wiki site…');
  try{
    const r=await api.wikiExport(dest);
    out.innerHTML='<div class="chkitem chk-ok" style="font-size:12px;padding:4px 2px">&#10003; Exported '
      +esc(r.written)+' file'+(r.written===1?'':'s')+'. Index: <code>'+esc(r.index_path)+'</code></div>';
  }catch(e){ kqErr(out,e); }
  finally{ btn.disabled=false; btn.textContent='Export to folder…'; }
}

Object.assign(window, { navigate, gotoStep, guideNext, guidePrev, markStepDone, closeGuide, showNode, showDiff, closeVault, closeAgent, closeWiki, closeVcs, closeKq, loadPlaybookFromBlock });

// ---------- config render (state + tree); reused after a project/snapshot change ----------
async function renderConfig(){
  try{
    const st = await api.state();
    document.title = (st.config||'bOS') + ' - bOS Configurator';
    $('#cfgname').textContent = st.config || '(no config)';
    $('#count').textContent = st.count || 0;
  }catch(e){ $('#cfgname').textContent='(load error)'; }
  try{ const t=await api.tree(); byEl={}; $('#tree').innerHTML=''; renderTree(t,$('#tree'),0); }catch(e){}
}

// ---------- project bar (SQLite store: list / create / open / import .bos) ----------
let activeProject=null;  // {id,name,path} or null
let projSnapshots=[];    // [{id,label,bos_filename,created_at,node_count}] for the snapshot-diff pickers
function setImportEnabled(){ $('#projimport').disabled = !activeProject; }

async function loadProjects(selectId){
  let list=[]; try{ list=await api.projects(); }catch(e){ list=[]; }
  const sel=$('#projsel'); sel.innerHTML='<option value="">- none -</option>';
  list.forEach(p=>{const o=document.createElement('option');o.value=p.id;
    o.textContent=p.name+' ('+p.snapshots+')';sel.appendChild(o);});
  if(selectId) sel.value=selectId;
}

function applyProjectView(view){
  activeProject = (view && view.active) ? view.active : null;
  projSnapshots = (view && Array.isArray(view.snapshots)) ? view.snapshots : [];
  if(activeProject) $('#projsel').value=activeProject.id;
  setImportEnabled();
  if($('#vcs') && $('#vcs').classList.contains('on')) refreshVcs();
}

async function openProjectById(id){
  if(!id){ activeProject=null; setImportEnabled(); if(memOpen) refreshMemory(); return; }
  try{ applyProjectView(await api.openProject(id)); await renderConfig(); if(memOpen) refreshMemory(); }
  catch(e){ alert('Open project failed: '+e); }
}

async function newProject(){
  const name=prompt('New project name:'); if(!name) return;
  try{ const p=await api.newProject(name); await loadProjects(p.id); await openProjectById(p.id); }
  catch(e){ alert('Create project failed: '+e); }
}

async function importBos(){
  if(!activeProject){ alert('Open or create a project first.'); return; }
  let path;
  try{ path=await openFileDialog({ multiple:false, directory:false,
        filters:[{ name:'bOS config', extensions:['bos'] }] }); }
  catch(e){ alert('File dialog failed: '+e); return; }
  if(!path) return;
  try{
    const res=await api.importBos(path, null);
    if(res && res.view) applyProjectView(res.view);
    await loadProjects(activeProject ? activeProject.id : '');
    await renderConfig();
  }catch(e){ alert('Import .bos failed: '+e); }
}

// ---------- boot ----------
async function init(){
  initTheme();
  await renderConfig();

  $('#search').oninput=e=>{const q=e.target.value.toLowerCase();
    $$('.tn').forEach(el=>{const hit=q&&el.dataset.path.toLowerCase().includes(q);
      el.classList.toggle('hit',!!hit);
      if(hit){expandTo(el.dataset.path); el.scrollIntoView({block:'nearest'});}});};

  try{ const list=await api.playbooks(); const sel=$('#pbsel');
    list.forEach(p=>{const o=document.createElement('option');o.value=p.file;
      o.textContent=p.feature+' ('+p.steps+')';sel.appendChild(o);});
  }catch(e){}
  $('#pbsel').onchange=e=>{if(e.target.value) loadGuide(e.target.value); else closeGuide();};
  $('#gbody').addEventListener('click',e=>{const t=e.target.closest('.dt');
    if(t){e.stopPropagation(); const w=t.closest('.diffwrap'); showDiff(+w.dataset.i,t.dataset.m);}});

  // project bar
  await loadProjects();
  try{ applyProjectView(await api.curProject()); }catch(e){}
  $('#projsel').onchange=e=>openProjectById(e.target.value);
  $('#projnew').onclick=newProject;
  $('#projimport').onclick=importBos;

  // vault + Entra SSO
  $('#vaultbtn').onclick=openVault;
  $('#vsignin').onclick=async()=>{
    const btn=$('#vsignin'); btn.disabled=true; $('#vwho').textContent='Opening browser — complete sign-in…';
    try{ await api.entraSignIn(); }catch(e){ alert('Sign-in failed: '+e); }
    btn.disabled=false; await refreshVault();
  };
  $('#vsignout').onclick=async()=>{ try{ await api.entraSignOut(); }catch(e){ alert(e); } await refreshVault(); };
  $('#vunlock').onclick=async()=>{ try{ await api.vaultUnlock(); await refreshVault(); }catch(e){ alert('Unlock failed: '+e); } };
  $('#vlock').onclick=async()=>{ try{ await api.vaultLock(); await refreshVault(); }catch(e){ alert(e); } };
  $('#vsave').onclick=saveSecret;
  $('#vscanbtn').onclick=scanGuard;
  $('#netsave').onclick=saveNet;
  $('#vault').addEventListener('click',e=>{ if(e.target.id==='vault') closeVault(); });

  // project: PRs, timeline & snapshot diff (Group A)
  $('#vcsbtn').onclick=openVcs;
  $('#prtoggle').onclick=togglePr;
  $('#tltoggle').onclick=toggleTimeline;
  $('#difftoggle').onclick=toggleDiff;
  $('#tladd').onclick=addTimeline;
  $('#diffcompare').onclick=runSnapshotDiff;
  $('#vcs').addEventListener('click',e=>{ if(e.target.id==='vcs') closeVcs(); });

  // wiki editor
  $('#wikibtn').onclick=openWiki;
  $('#wikilist').onchange=e=>loadWikiPage(e.target.value);
  $('#wikinew').onclick=newWikiPage;
  $('#wikitplins').onclick=insertWikiTemplate;
  $('#wikitplrefresh').onclick=refreshArtifactGuide;
  $('#wikisave').onclick=saveWikiPage;
  $('#wiki').addEventListener('click',e=>{ if(e.target.id==='wiki') closeWiki(); });

  // agent dock
  $('#agentbtn').onclick=openAgent;
  $('#asend').onclick=sendAsk;
  $('#aq').addEventListener('keydown',e=>{ if((e.ctrlKey||e.metaKey)&&e.key==='Enter'){ e.preventDefault(); sendAsk(); }});
  // agent memory (collapsible)
  $('#amemtoggle').onclick=toggleMem;
  $('#amemsave').onclick=saveMemory;
  // agent roster + instincts (collapsible)
  $('#arostoggle').onclick=toggleRos;
  $('#arosagent').onchange=loadInstincts;
  $('#arossave').onclick=saveInstincts;
  $('#arosrevert').onclick=revertInstincts;
  $('#aroshistbtn').onclick=showInstinctHistory;
  // context files (collapsible)
  $('#actxtoggle').onclick=toggleCtx;
  $('#actxadd').onclick=addContextFile;

  // knowledge & quality modal (Group B)
  $('#kqbtn').onclick=openKq;
  $('#kq').addEventListener('click',e=>{ if(e.target.id==='kq') closeKq(); });
  $('#ksugtoggle').onclick=()=>kqToggle('#kqsug','#ksugbody',runContextSuggest);
  $('#ksugrun').onclick=runContextSuggest;
  $('#mstoggle').onclick=()=>kqToggle('#kqms','#msbody',null);
  $('#msimport').onclick=runMilesightImport;
  $('#crawltoggle').onclick=()=>kqToggle('#kqcrawl','#crawlbody',()=>{ refreshCrawlSources(); refreshCrawl(); });
  $('#crawlsrcadd').onclick=addCrawlSource;
  $('#crawlsrcurl').addEventListener('keydown',e=>{ if(e.key==='Enter'){ e.preventDefault(); addCrawlSource(); }});
  $('#crawlsweep').onclick=sweepCrawl;
  $('#crawladd').onclick=addCrawl;
  $('#crawlurl').addEventListener('keydown',e=>{ if(e.key==='Enter'){ e.preventDefault(); addCrawl(); }});
  $('#crawleurekabtn').onclick=runEureka;
  $('#crawlproposebtn').onclick=proposeEurekaPr;
  $('#codetoggle').onclick=()=>kqToggle('#kqcode','#codebody',null);
  $('#coderun').onclick=runCodeAudit;
  $('#sectoggle').onclick=()=>kqToggle('#kqsec','#secbody',null);
  $('#secrun').onclick=runSecurityScan;
  $('#gaterun').onclick=runGateCheck;
  $('#rectoggle').onclick=()=>kqToggle('#kqrec','#recbody',runRecommend);
  $('#recrun').onclick=runRecommend;
  $('#wikiexptoggle').onclick=()=>kqToggle('#kqwikiexp','#wikiexpbody',null);
  $('#wikiexpbtn').onclick=runWikiExport;

  refreshAgentStatus();
  refreshRoster();
}
init();

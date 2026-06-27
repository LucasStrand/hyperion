// @ts-nocheck
// bOS Configurator view — ported from bos_copilot.py's webview JS.
// The only change vs. the Flask version: the 4 `/api/*` fetches are now Tauri
// `invoke()` commands implemented in src-tauri/src/lib.rs. All rendering /
// auto-grading / inline-highlight logic is unchanged.
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";

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

  agentRoster:           () => invoke("agent_roster"),
  agentInstinctsGet:     (agentId) => invoke("agent_instincts_get", { agentId }),
  agentInstinctsSet:     (agentId, body) => invoke("agent_instincts_set", { agentId, body }),
  agentInstinctsHistory: (agentId) => invoke("agent_instincts_history", { agentId }),
  agentInstinctsRevert:  (agentId, version) => invoke("agent_instincts_revert", { agentId, version }),
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
  PB=pb; STEP=-1;
  $('#gtitle').textContent=PB.feature||'Guide';
  let h=''; if(PB.summary) h+='<div class="mut" style="margin-bottom:8px">'+esc(PB.summary)+'</div>';
  (PB.steps||[]).forEach((s,i)=>{h+='<div class="gstep" data-i="'+i+'" onclick="gotoStep('+i+')">'
    +'<div><span class="gn">'+esc(''+(s.n||i+1))+'</span><span class="gt">'+esc(s.title||'')+'</span>'
    +(s.check?'<span class="badge-st st-todo" id="bst'+i+'">checking...</span>':'')+'</div>'
    +(s.check?'<div class="chklist" id="chk'+i+'"></div>':'')
    +(s.detail?'<div class="gd">'+esc(s.detail)+'</div>':'')
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
  runChecks();
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
  $$('.tn.guide').forEach(x=>x.classList.remove('guide'));
  ACTIVE_HL = s.highlight ? (Array.isArray(s.highlight)?s.highlight:[s.highlight]) : null;
  if(s.target){navigate(s.target); const el=byEl[s.target]; if(el)el.classList.add('guide');}
  else if(CUR) renderPanel();}
function closeGuide(){$('#guide').style.display='none';$('#pbsel').value='';
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
function openAgent(){ $('#agent').classList.add('on'); refreshAgentStatus(); refreshRoster(); if(memOpen) refreshMemory(); $('#aq').focus(); }
function closeAgent(){ $('#agent').classList.remove('on'); }
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

Object.assign(window, { navigate, gotoStep, closeGuide, showNode, showDiff, closeVault, closeAgent, loadPlaybookFromBlock });

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
  if(activeProject) $('#projsel').value=activeProject.id;
  setImportEnabled();
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
  $('#vault').addEventListener('click',e=>{ if(e.target.id==='vault') closeVault(); });

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
  refreshAgentStatus();
  refreshRoster();
}
init();

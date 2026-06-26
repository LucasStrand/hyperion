#!/usr/bin/env python3
"""
bos_copilot.py - local "bOS Co-pilot" that mimics the ComfortClick bOS
Configurator UI from your exported .bos config.

It loads your REAL object tree and renders it like the Configurator: a typed
icon tree on the left, and a Settings / Values / Functions / Usages / Notes /
Info panel on the right. For a Program it reconstructs the Triggers + the
ordered Commands tree (If / SetValue / Delay / Run / Comment) exactly as the
Configurator shows them - all read straight from the binary export.

A "Guide" picker (top-right) loads playbooks (playbooks/*.json) and walks you
through a change step-by-step, highlighting the node in the tree.

It NEVER writes to your bOS system. This is a read-only simulation; you apply
changes yourself in the real Configurator.

Usage:
    pip install nrbf flask
    python bos_copilot.py GB1625-Building.bos
        -> parses (cached to bos_map.json), serves http://127.0.0.1:5000
"""
import sys, os, json, threading, webbrowser, argparse

try:
    from flask import Flask, jsonify, abort
except ImportError:
    sys.exit("Missing dependency. Run:  pip install flask")

HERE = os.path.dirname(os.path.abspath(__file__))
PLAYBOOK_DIR = os.path.join(HERE, "playbooks")


def build_nodes(bos_path):
    """Return the list of summarized nodes, using bos_map.json as a cache."""
    cache = os.path.join(os.path.dirname(os.path.abspath(bos_path)), "bos_map.json")
    if os.path.exists(cache) and os.path.getmtime(cache) >= os.path.getmtime(bos_path):
        with open(cache, encoding="utf-8") as f:
            return json.load(f)
    sys.path.insert(0, HERE)
    try:
        import bos_explore as bx
    except ImportError:
        sys.exit("bos_explore.py must sit next to bos_copilot.py.")
    ex = bx.Explorer(bx.load(bos_path))
    summaries = [ex.summarize(h) for h in ex.hosts()]
    best = {}
    for s in summaries:
        p = str(s["path"] or s["name"])
        if p not in best or bx.richness(s) > bx.richness(best[p]):
            best[p] = s
    nodes = sorted(best.values(), key=lambda s: str(s["path"]))
    with open(cache, "w", encoding="utf-8") as f:
        json.dump(nodes, f, ensure_ascii=False, indent=1)
    return nodes


def build_tree(nodes):
    """Turn flat backslash-separated paths into a nested tree for the UI."""
    root = {"name": "(root)", "path": "", "children": {}, "node": None}
    by_path = {}
    for n in nodes:
        path = str(n.get("path") or n.get("name") or "")
        by_path[path] = n
        parts = [p for p in path.split("\\") if p]
        cur = root
        acc = []
        for part in parts:
            acc.append(part)
            child = cur["children"].get(part)
            if child is None:
                child = {"name": part, "path": "\\".join(acc), "children": {}, "node": None}
                cur["children"][part] = child
            cur = child
        cur["node"] = n

    def to_list(node):
        kids = [to_list(c) for c in node["children"].values()]
        kids.sort(key=lambda c: c["name"].lower())
        return {"name": node["name"], "path": node["path"],
                "type": (node["node"] or {}).get("type"), "children": kids}

    return to_list(root), by_path


def make_app(bos_path):
    nodes = build_nodes(bos_path)
    tree, by_path = build_tree(nodes)

    # --- precompute relations from the already-parsed nodes (no re-parse) ---
    def _norm(s):
        return str(s or "").replace("\\", "/")

    norm_index, ref_index, children_index = {}, {}, {}

    def _addref(target, by, prop, kind):
        if not target:
            return
        ref_index.setdefault(str(target), []).append({"by": by, "property": prop, "kind": kind})

    for n in nodes:
        p = str(n.get("path") or n.get("name") or "")
        norm_index[_norm(p)] = n
        for r in (n.get("inputs") or []):
            _addref(r.get("object"), p, r.get("property"), "reads")
        o = n.get("output")
        if o:
            _addref(o.get("object"), p, o.get("property"), "outputs to")
        for r in (n.get("writes") or []):
            _addref(r.get("object"), p, r.get("property"), "writes")
        if "\\" in p:
            parent, _, leaf = p.rpartition("\\")
            children_index.setdefault(parent, []).append(
                {"name": leaf, "path": p, "type": n.get("type")})

    app = Flask(__name__)

    @app.route("/")
    def index():
        html = HTML.replace("__CONFIG__", os.path.basename(bos_path)).replace(
            "__COUNT__", str(len(nodes)))
        resp = app.make_response(html)
        resp.headers["Cache-Control"] = "no-store, no-cache, must-revalidate"
        return resp

    @app.route("/api/tree")
    def api_tree():
        return jsonify(tree)

    @app.route("/api/node/<path:p>")
    def api_node(p):
        n = by_path.get(p.replace("/", "\\")) or norm_index.get(_norm(p))
        if not n:
            abort(404)
        key = str(n.get("path") or n.get("name") or "")
        out = dict(n)
        seen, uniq = set(), []
        for r in (n.get("writes") or []):
            sig = (r.get("object"), r.get("property"))
            if sig not in seen:
                seen.add(sig)
                uniq.append(r)
        out["writes"] = uniq
        out["consists_of"] = sorted(children_index.get(key, []),
                                    key=lambda c: (c.get("name") or "").lower())
        out["referenced_by"] = ref_index.get(key, [])
        return jsonify(out)

    @app.route("/api/playbooks")
    def api_playbooks():
        out = []
        if os.path.isdir(PLAYBOOK_DIR):
            for fn in sorted(os.listdir(PLAYBOOK_DIR)):
                if fn.endswith(".json") and not fn.startswith("_"):
                    try:
                        with open(os.path.join(PLAYBOOK_DIR, fn), encoding="utf-8") as f:
                            pb = json.load(f)
                        out.append({"file": fn, "feature": pb.get("feature", fn),
                                    "steps": len(pb.get("steps", []))})
                    except Exception as e:
                        out.append({"file": fn, "feature": f"(invalid: {e})", "steps": 0})
        return jsonify(out)

    @app.route("/api/playbook/<name>")
    def api_playbook(name):
        if "/" in name or "\\" in name or not name.endswith(".json"):
            abort(400)
        path = os.path.join(PLAYBOOK_DIR, name)
        if not os.path.exists(path):
            abort(404)
        with open(path, encoding="utf-8") as f:
            return jsonify(json.load(f))

    return app


HTML = r"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>__CONFIG__ - bOS Configurator</title>
<style>
:root{--bg:#ffffff;--ink:#1b1b1b;--mut:#6b7785;--line:#c9d2dc;--pan:#f3f5f8;
--pan2:#e8edf3;--sel:#2f7bd6;--selbg:#cfe2fb;--acc:#1f6fc4;--kw:#1f6fc4;
--set:#a8430f;--delay:#7a6a00;--run:#7a3aa8;--cmt:#2e7d32;--bool:#d24b3e;
--int:#1f6fc4;--str:#2e7d32;--gate:#c47a00;}
*{box-sizing:border-box}
body{margin:0;font:13px/1.45 "Segoe UI",system-ui,sans-serif;background:var(--bg);
color:var(--ink);height:100vh;display:flex;flex-direction:column;overflow:hidden}
/* title bar */
.titlebar{background:#2b3a4a;color:#dfe8f1;padding:5px 10px;display:flex;align-items:center;gap:8px;font-size:12px}
.titlebar .ico{width:16px;height:16px;background:#e8a33d;border-radius:3px;display:inline-block}
.titlebar .sp{margin-left:auto;color:#9fb0c0;letter-spacing:2px}
/* header */
.appbar{background:var(--pan);border-bottom:1px solid var(--line);padding:6px 10px;
display:flex;align-items:center;gap:12px}
.appbar .back{color:var(--mut);font-size:18px;cursor:default}
.appbar .title{flex:1;text-align:center;font-size:16px;color:#333}
.appbar select{font-size:12px;padding:3px 6px;border:1px solid var(--line);border-radius:4px;background:#fff}
/* toolbar */
.toolbar{background:var(--pan);border-bottom:1px solid var(--line);padding:4px 8px;
display:flex;align-items:center;gap:10px}
.toolbar .icons{color:var(--mut);font-size:15px;letter-spacing:6px;user-select:none}
.toolbar input{flex:0 0 240px;padding:4px 8px;border:1px solid var(--line);border-radius:4px}
/* main */
.cols{flex:1;display:flex;min-height:0}
.tree{width:34%;min-width:240px;border-right:1px solid var(--line);overflow:auto;background:#fff;padding:4px 0}
.panel{flex:1;display:flex;flex-direction:column;min-width:0;background:#fff}
/* tree nodes */
.tn{padding:1px 0;white-space:nowrap;cursor:default}
.tn .tog{display:inline-block;width:14px;text-align:center;color:var(--mut);cursor:pointer}
.tn .ic{display:inline-block;width:18px;text-align:center;font-size:12px}
.tn .lbl{padding:0 3px;border-radius:2px}
.tn .lbl:hover{background:var(--pan2)}
.tn.sel>.row>.lbl{background:var(--selbg);outline:1px solid var(--sel)}
.tn.hit>.row>.lbl{background:#ffe9a8}
.tn.guide>.row>.lbl{background:#ffd24d;outline:2px solid #e8a33d}
.tn .row{display:flex;align-items:center}
.kids{margin-left:13px;border-left:1px dotted #d8dee6;padding-left:1px}
/* tabs */
.tabs{display:flex;gap:2px;background:var(--pan);border-bottom:1px solid var(--line);padding:4px 8px 0}
.tab{padding:4px 12px;border:1px solid var(--line);border-bottom:none;border-radius:5px 5px 0 0;
background:var(--pan2);color:#444;cursor:pointer;font-size:12px}
.tab.on{background:#fff;color:#000;font-weight:600;position:relative;top:1px}
.panbody{flex:1;overflow:auto;padding:10px 12px}
.section{border:1px solid var(--line);border-radius:4px;margin-bottom:10px}
.section .h{background:var(--pan);padding:5px 8px;border-bottom:1px solid var(--line);font-weight:600}
.section .b{padding:6px 8px}
.kv{display:flex;justify-content:space-between;padding:3px 4px;border-bottom:1px solid #eef1f5}
.kv:last-child{border:none} .kv b{font-weight:600} .kv .v{color:#333}
.miniBtns{color:var(--mut);font-size:12px;margin:4px 0;user-select:none}
.miniBtns span{margin-right:12px}
.trigbox,.cmdbox{border:1px solid var(--line);border-radius:4px;padding:6px 8px;background:#fff;
font-family:Consolas,ui-monospace,monospace;font-size:13px}
.subtabs{display:flex;gap:2px;margin:8px 0 0}
.subtab{padding:3px 12px;border:1px solid var(--line);border-bottom:none;border-radius:4px 4px 0 0;
background:var(--pan2);cursor:pointer;font-size:12px}
.subtab.on{background:#fff;font-weight:600}
/* command tree */
.cmds{margin-left:14px;border-left:1px dotted #cfd6de;padding-left:6px}
.cmd{font-family:Consolas,ui-monospace,monospace;font-size:13px;line-height:1.55;padding:0 2px;border-radius:2px}
.cmd:hover{background:#f1f5fb}
.c-start{font-weight:600;color:#333}
.c-if>.t{color:var(--kw)} .c-setvalue>.t{color:var(--set)} .c-delay>.t{color:var(--delay)}
.c-run>.t{color:var(--run)} .c-comment>.t{color:var(--cmt);font-style:italic}
.c-iftrigger>.t{color:var(--kw)}
.cmd .go{cursor:pointer;color:var(--mut);margin-left:6px;visibility:hidden}
.cmd:hover .go{visibility:visible}
.mut{color:var(--mut)} code{color:#0a5}
.reflink{color:var(--acc);cursor:pointer} .reflink:hover{text-decoration:underline}
/* guide */
#guide{position:fixed;right:14px;bottom:34px;width:470px;max-height:72vh;overflow:auto;
background:#fff;border:1px solid var(--line);border-radius:8px;box-shadow:0 6px 24px rgba(0,0,0,.18);display:none}
#guide .gh{background:var(--sel);color:#fff;padding:7px 10px;font-weight:600;display:flex;align-items:center}
#guide .gh .x{margin-left:auto;cursor:pointer}
#guide .gb{padding:8px 10px}
.gstep{border:1px solid var(--line);border-radius:6px;padding:6px 8px;margin-bottom:7px;cursor:pointer}
.gstep.active{border-color:var(--sel);background:#f1f7ff}
.gstep .gn{display:inline-block;background:var(--sel);color:#fff;border-radius:50%;width:20px;height:20px;
text-align:center;line-height:20px;font-size:11px;margin-right:6px}
.gstep .gt{font-weight:600} .gstep .gd{color:#444;margin-top:3px;font-size:12px}
.gstep .gtgt{margin-top:3px;font-size:12px}
.diffwrap{margin-top:6px;border-top:1px dashed var(--line);padding-top:6px}
.difftabs{display:flex;gap:4px;margin-bottom:4px}
.dt{font-size:11px;padding:2px 9px;border:1px solid var(--line);border-radius:10px;cursor:pointer;background:#fff;color:#555}
.dt.on{background:var(--sel);color:#fff;border-color:var(--sel)}
.dt.before.on{background:#a8430f;border-color:#a8430f}
.diffbody .cmdbox{font-size:12px}
.cmd.added{background:#e6ffec} .cmd.added>.t::before{content:"+ ";color:#1a7f37;font-weight:700}
.cmd.removed{background:#ffebe9;text-decoration:line-through;opacity:.8}
.diffnote{font-size:11px;color:var(--mut);margin:2px 0 4px}
.badge-st{font-size:10px;font-weight:700;padding:1px 7px;border-radius:10px;margin-left:6px;vertical-align:middle}
.st-done{background:#e6ffec;color:#1a7f37;border:1px solid #9bdcab}
.st-partial{background:#fff5e0;color:#9a6700;border:1px solid #f0d28a}
.st-todo{background:#eef1f5;color:#5a6b7b;border:1px solid #cdd6df}
.st-na{background:#f3f5f8;color:#9aa7b5;border:1px solid #e0e6ec}
.chklist{margin-top:5px;font-size:12px}
.chkitem{padding:1px 0} .chk-ok{color:#1a7f37} .chk-no{color:#a8430f}
#gsummary{font-size:12px;font-weight:400;margin-left:8px;opacity:.95}
/* status bar */
.status{background:var(--pan);border-top:1px solid var(--line);padding:3px 10px;font-size:11px;color:#555;
display:flex;align-items:center;gap:6px}
.status .warn{color:#b06a00}
/* --- fidelity additions --- */
.ham{margin-left:12px;color:var(--mut);font-size:16px;user-select:none}
.toolbar .tb{font-size:15px;letter-spacing:3px}
.toolbar .tb i{font-style:italic;color:var(--acc);letter-spacing:0;margin:0 3px}
.toolbar .tb .gp{color:#2e9e6a} .toolbar .tb .rd{color:#c0392b} .toolbar .tb .og{color:#d98014}
.toolbar #crumb{background:#fff;color:#333;border:1px solid var(--line);border-radius:4px;padding:4px 8px;margin-right:4px}
.titlebar .sp{letter-spacing:6px}
/* grouped settings (Modbus DPT) */
.sec{border:1px solid var(--line);border-radius:3px;margin-bottom:9px;background:#fff}
.sec>.sh{background:var(--pan);padding:4px 8px;border-bottom:1px solid var(--line);font-weight:600;cursor:pointer;user-select:none;display:flex;align-items:center;gap:6px}
.sec>.sh .chev{color:var(--mut);font-size:11px;width:10px;display:inline-block}
.sec.collapsed>.sb{display:none}
.sec.collapsed>.sh .chev{transform:rotate(-90deg)}
.srow{display:flex;justify-content:space-between;align-items:center;padding:3px 10px;border-bottom:1px solid #eef1f5}
.srow:last-child{border:none}
.srow .sk{color:#333} .srow .sv{font-weight:600;color:#1b1b1b;text-align:right}
.descbox{border:1px solid var(--line);border-radius:3px;background:var(--pan);margin-top:6px;padding:6px 8px}
.descbox b{display:block;margin-bottom:2px}
.descbox .dmut{color:var(--mut)}
/* inline highlight + before/after chip */
.hl-change{outline:2px solid #e8a33d !important;background:#fff7e0 !important;border-radius:2px}
.ba{margin-left:10px;font-family:Consolas,ui-monospace,monospace;font-size:12px;white-space:nowrap}
.ba s{color:#b3261e;text-decoration:line-through;background:#ffe3df;padding:0 3px;border-radius:2px}
.ba b{color:#1a7f37;background:#e6ffec;padding:0 3px;border-radius:2px}
.ba .arr{color:var(--mut);margin:0 5px}
.hl-warn{display:inline-block;margin-left:8px;font-size:11px;font-weight:700;color:#b3261e;background:#ffe3df;border:1px solid #f3b4ad;border-radius:10px;padding:1px 8px;vertical-align:middle;white-space:normal}
</style></head>
<body>
<div class="titlebar"><span class="ico"></span> __CONFIG__ - bOS Configurator
  <span class="sp">&#9472; &#9633; &#10005;</span></div>
<div class="appbar">
  <span class="back">&#8249;</span>
  <span class="title" id="title">&nbsp;</span>
  <label class="mut" style="font-size:12px">Guide:
    <select id="pbsel"><option value="">- none -</option></select>
  </label>
  <span class="ham">&#9776;</span>
</div>
<div class="toolbar">
  <span class="icons tb">&#128190; &#128193; &#11014; &#11015; &#9881; &#8943; <i>fx</i> <b class="gp">+</b> <b class="rd">&#10005;</b> &#8676; &#9650; &#9660; &#8677; <span class="og">&#8634;&#8635;</span> ?</span>
  <input type="text" id="crumb" value="tak" readonly>
  <input type="text" id="search" placeholder="search nodes...">
</div>
<div class="cols">
  <div class="tree" id="tree"></div>
  <div class="panel">
    <div class="tabs" id="tabs"></div>
    <div class="panbody" id="panbody"><div class="mut">Select a node in the tree.</div></div>
  </div>
</div>
<div class="status">
  <span class="warn">&#9888;</span>
  <span>Users : 2026-06-26 11:52:50 &middot; Admin &middot; Login &middot; IP: 172.16.193.33 &middot; Device: LSEHILMAJO4 &middot; OS: Windows</span>
  <span id="status" style="margin-left:auto">Read-only simulation of __CONFIG__ &middot; __COUNT__ objects &middot; nothing is written to bOS</span>
</div>

<div id="guide">
  <div class="gh"><span id="gtitle">Guide</span><span id="gsummary"></span><span class="x" onclick="closeGuide()">&#10005;</span></div>
  <div class="gb" id="gbody"></div>
</div>

<script>
const $=s=>document.querySelector(s), $$=s=>[...document.querySelectorAll(s)];
function esc(s){return (''+s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));}

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
$('#search').oninput=e=>{const q=e.target.value.toLowerCase();
  $$('.tn').forEach(el=>{const hit=q&&el.dataset.path.toLowerCase().includes(q);
    el.classList.toggle('hit',!!hit);
    if(hit){expandTo(el.dataset.path); el.scrollIntoView({block:'nearest'});}});};
fetch('/api/tree').then(r=>r.json()).then(t=>renderTree(t,$('#tree'),0));

// ---------- right panel ----------
const TABS=['Settings','Values','Functions','Usages','Notes','Info'];
let CUR=null, TAB='Settings';
async function showNode(path){
  try{
    const r=await fetch('/api/node/'+encodeURIComponent(path).replace(/%5C/g,'\\'));
    CUR = r.ok ? await r.json()
               : {name:path.split('\\').pop(), path:path, type:'(folder)', settings:{}};
  }catch(e){CUR={name:path.split('\\').pop(), path:path, type:'(folder)', settings:{}};}
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
fetch('/api/playbooks').then(r=>r.json()).then(list=>{const sel=$('#pbsel');
  list.forEach(p=>{const o=document.createElement('option');o.value=p.file;
    o.textContent=p.feature+' ('+p.steps+')';sel.appendChild(o);});});
$('#pbsel').onchange=e=>{if(e.target.value) loadGuide(e.target.value); else closeGuide();};
async function loadGuide(file){
  PB=await (await fetch('/api/playbook/'+file)).json(); STEP=-1;
  $('#gtitle').textContent=PB.feature||'Guide';
  let h=''; if(PB.summary) h+='<div class="mut" style="margin-bottom:8px">'+esc(PB.summary)+'</div>';
  (PB.steps||[]).forEach((s,i)=>{h+='<div class="gstep" data-i="'+i+'" onclick="gotoStep('+i+')">'
    +'<div><span class="gn">'+(s.n||i+1)+'</span><span class="gt">'+esc(s.title||'')+'</span>'
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
  try{const r=await fetch('/api/node/'+encodeURIComponent(path).replace(/%5C/g,'\\'));
    _nodeCache[path]=r.ok?await r.json():null;}catch(e){_nodeCache[path]=null;}
  return _nodeCache[path];
}
function flatten(list,anc,out){ (list||[]).forEach(c=>{
  out.push({text:c.text||'',anc:anc}); flatten(c.children,anc.concat([c.text||'']),out);}); return out;}
async function evalCheck(chk){
  const n=await getNode(chk.node);
  const results=(chk.all||[]).map(p=>{
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
  const status=pass===results.length?'done':(pass===0?'todo':'partial');
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
  list.forEach(c=>{const cls='cmd c-'+(c.cmd||'').toLowerCase()+(c.added?' added':'')+(c.removed?' removed':'');
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
  else { try{const n=await (await fetch('/api/node/'+encodeURIComponent(d.node).replace(/%5C/g,'\\'))).json();
    list=(n.program&&n.program[d.section||'commands'])||[];}catch(e){list=[];} }
  const head=(d.section==='triggers')?'<div class="cmd c-iftrigger"><span class="t">Triggers</span></div>'
    :'<div class="cmd c-start"><span class="t">Start</span></div>';
  wrap.querySelector('.diffbody').innerHTML='<div class="cmdbox">'+head+diffTree(list)+'</div>';
}
$('#gbody').addEventListener('click',e=>{const t=e.target.closest('.dt');
  if(t){e.stopPropagation(); const w=t.closest('.diffwrap'); showDiff(+w.dataset.i,t.dataset.m);}});
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
</script>
</body></html>"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("config", help="path to your exported .bos file")
    ap.add_argument("--port", type=int, default=5000)
    ap.add_argument("--no-browser", action="store_true")
    a = ap.parse_args()
    if not os.path.exists(a.config):
        sys.exit(f"Config not found: {a.config}")
    os.makedirs(PLAYBOOK_DIR, exist_ok=True)
    print(f"Loading {a.config} ...")
    app = make_app(a.config)
    url = f"http://127.0.0.1:{a.port}/"
    print(f"bOS Co-pilot running at {url}  (Ctrl+C to stop)")
    if not a.no_browser:
        threading.Timer(1.0, lambda: webbrowser.open(url)).start()
    app.run(port=a.port, debug=False)


if __name__ == "__main__":
    main()

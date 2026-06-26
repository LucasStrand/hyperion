#!/usr/bin/env python3
"""
bos_explore.py - read-only explorer for ComfortClick bOS (.bos) config exports.

Parses the .NET BinaryFormatter object graph, reconstructs the bOS node tree,
and RESOLVES the references/bindings between objects (which signal feeds which
gate, what a task writes to, etc.).

Usage:
    python bos_explore.py CONFIG.bos                 # write bos_map.json + stats
    python bos_explore.py CONFIG.bos --find takstatus   # print matching nodes
    python bos_explore.py CONFIG.bos --find "Kylvatten" --full

Requires:  pip install nrbf
Read-only. Never writes to your bOS system.
"""
import sys, json, argparse
try:
    import nrbf
except ImportError:
    sys.exit("Missing dependency. Run:  pip install nrbf")

NAME = 'NodeSettingsInfo+<Name>k__BackingField'
VAL  = '<Value>k__BackingField'
VNAME= 'NodeValueInfo+<Name>k__BackingField'

# ComfortClick.Tasks.TaskCommands.If+ConditionTypes (best-effort; 0 = Equals is confirmed)
IF_COND   = {0:'=', 1:'!=', 2:'>', 3:'>=', 4:'<', 5:'<='}
# IfTrigger+ConditionTypes (6 = on change is confirmed)
TRIG_COND = {0:'=', 1:'!=', 2:'>', 3:'>=', 4:'<', 5:'<=', 6:'on change'}

def load(path):
    data = open(path, 'rb').read()
    p = nrbf.NRBFParser(data)
    p._resolve = lambda v: v          # disable the library's cyclic deep-resolve
    p.parse()
    return p.objects

class Explorer:
    def __init__(self, objs):
        self.objs = objs
    def cls(self, o): return o.get('__class__','') if isinstance(o,dict) else type(o).__name__
    def deref(self, v):
        seen=0
        while isinstance(v,dict) and tuple(v)==('__ref__',):
            v=self.objs.get(v['__ref__']); seen+=1
            if seen>50: break
        return v
    def as_list(self, v):
        v=self.deref(v)
        if isinstance(v,dict) and self.cls(v).startswith('System.Collections.Generic.List'):
            items=self.deref(v.get('_items')) or []
            return [self.deref(x) for x in items[:v.get('_size',len(items))]]
        if isinstance(v,list): return [self.deref(x) for x in v]
        return []
    def scalars(self, o):
        o=self.deref(o); out={}
        if isinstance(o,dict):
            for k,vv in o.items():
                dv=self.deref(vv)
                if isinstance(dv,(str,int,float,bool)) or dv is None: out[k]=dv
        return out
    # gather every ValueReference target reachable inside an object (cycle-safe)
    def find_refs(self, o, seen=None, depth=0):
        seen=seen if seen is not None else set()
        o=self.deref(o); res=[]
        if isinstance(o,dict):
            if id(o) in seen or depth>12: return res
            seen.add(id(o))
            if 'ValueReference' in self.cls(o):
                on=self.deref(o.get('ObjectName'))
                if isinstance(on,str) and on: res.append({'object': on, 'property': self.deref(o.get('PropertyName'))})
            for k,v in o.items():
                if k!='__class__': res+=self.find_refs(v,seen,depth+1)
        elif isinstance(o,list):
            for v in o: res+=self.find_refs(v,seen,depth+1)
        return res

    # ---- Program command-tree extraction (what a Program "consists of") ----
    def enumv(self, e):
        e=self.deref(e)
        return e.get('value__') if isinstance(e,dict) else None
    def ref_parts(self, ref):
        """Return (objectName, propertyName, functionName) of a ValueReference."""
        ref=self.deref(ref)
        if not isinstance(ref,dict): return (None,None,None)
        on=self.deref(ref.get('ObjectName')); pn=self.deref(ref.get('PropertyName'))
        fn=self.deref(ref.get('FunctionName'))
        return (on if isinstance(on,str) and on else None,
                pn if isinstance(pn,str) and pn else None,
                fn if isinstance(fn,str) and fn else None)
    def command(self, c, depth=0):
        """One command -> {cmd, text, target(real node path), children?} in Configurator notation."""
        c=self.deref(c)
        if not isinstance(c,dict) or depth>40: return None
        short=self.cls(c).split('.')[-1].split('+')[0]
        on,pn,fn=self.ref_parts(c.get('Reference'))
        ref=f"{on}.{pn}" if on and pn else (on or "?")
        node={'cmd':short}
        if on: node['target']=on
        def valstr():
            if self.deref(c.get('SetValueFromValue')) or self.deref(c.get('ValueFromReference')):
                von,vpn,_=self.ref_parts(c.get('ValueReference'))
                return f"{von}.{vpn}" if von and vpn else (von or "(ref)")
            return self.deref(c.get('Value'))
        if short=='If':
            op=IF_COND.get(self.enumv(c.get('Condition')),'?')
            node['text']=f"If {ref} {op} {valstr()}"
            node['children']=[x for x in (self.command(k,depth+1)
                              for k in self.as_list(c.get('CommandList'))) if x]
        elif short=='SetValue':
            node['text']=f"{ref} = (calculation)" if self.deref(c.get('Calculation')) \
                         else f"{ref} = {valstr()}"
        elif short=='Delay':
            node['text']=f"Delay: {self.deref(c.get('Time'))} Seconds"
        elif short=='Run':
            node['text']=f"{on}.{fn}()" if on and fn else (on or "Run")
        elif short=='Comment':
            txt=next((self.deref(v) for k,v in c.items()
                      if k!='__class__' and isinstance(self.deref(v),str) and self.deref(v)),None)
            node['text']=f"Comment: {txt}" if txt else "Comment"
        else:
            node['text']=short
        return node
    def program_detail(self, no):
        """Extract a Program's triggers + ordered command tree + abort commands."""
        out={'triggers':[],'commands':[],'abort':[]}
        for s in self.as_list(no.get('NodeSettings')):
            nm=self.deref(s.get(NAME)); val=self.deref(s.get(VAL))
            cl=val.get('CommandList') if isinstance(val,dict) else None
            if nm in ('Commands','AbortCommands'):
                steps=[x for x in (self.command(k) for k in self.as_list(cl)) if x]
                out['commands' if nm=='Commands' else 'abort']=steps
            elif nm=='Triggers' and isinstance(val,dict):
                for t in self.as_list(val.get('TriggerList')):
                    on,pn,_=self.ref_parts(t.get('Reference')); cv=self.enumv(t.get('Condition'))
                    tgt=f"{on}.{pn}" if on and pn else (on or "?")
                    if cv==6: txt=f"{tgt} OnChange"
                    else: txt=f"{tgt} {TRIG_COND.get(cv,'?')} {self.deref(t.get('Value'))}"
                    out['triggers'].append({'cmd':'IfTrigger','text':txt,
                                            **({'target':on} if on else {})})
        return out

    def hosts(self):
        return [o for o in self.objs.values()
                if isinstance(o,dict) and self.cls(o)=='BOSCommon.Node.Common.NodeHost']

    def summarize(self, host):
        no=self.deref(host.get('NodeObject'))
        d={'name':self.deref(host.get('Name')),'path':self.deref(host.get('Path')),
           'type':self.cls(no).replace('ComfortClick.Tasks.','').replace(', ComfortClick.Tasks',''),
           'settings':{},'inputs':[],'output':None,'writes':[],'values':{}}
        if not isinstance(no,dict): return d
        for s in self.as_list(no.get('NodeSettings')):
            nm=self.deref(s.get(NAME)); val=self.deref(s.get(VAL))
            if not isinstance(nm,str): nm=None
            if nm=='Type' and isinstance(val,dict) and 'value__' in val:
                d['settings']['Type']=val['value__']
            elif isinstance(val,(str,int,float,bool)) or val is None:
                if nm: d['settings'][nm]=val
            if nm=='InputValues':
                d['inputs']=self.find_refs(val)
            elif nm=='OutputValue':
                r=self.find_refs(val); d['output']=r[0] if r else None
            elif nm not in ('Type','InvertedOutputValue'):
                d['writes']+=self.find_refs(val)
        for v in self.as_list(no.get('NodeValues')):
            vn=self.deref(v.get(VNAME))
            if not isinstance(vn,str): vn=None
            cv=self.deref(v.get('<Value>k__BackingField'))
            if vn and (isinstance(cv,(str,int,float,bool)) or cv is None): d['values'][vn]=cv
        if d['type']=='Program':
            d['program']=self.program_detail(no)
        return d

def richness(s):  # prefer the real configured node over empty stub copies
    return len(s['settings'])+len(s['inputs'])+len(s['writes'])+len(s['values'])

def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('config'); ap.add_argument('--find',default=None)
    ap.add_argument('--full',action='store_true'); ap.add_argument('--json',default='bos_map.json')
    a=ap.parse_args()
    ex=Explorer(load(a.config))
    hosts=ex.hosts()
    summaries=[ex.summarize(h) for h in hosts]
    # dedupe by path, keep richest
    best={}
    for s in summaries:
        p=str(s["path"] or s["name"])
        if p not in best or richness(s)>richness(best[p]): best[p]=s
    nodes=sorted(best.values(), key=lambda s:str(s['path']))
    json.dump(nodes, open(a.json,'w',encoding='utf-8'), ensure_ascii=False, indent=1)
    # stats
    from collections import Counter
    types=Counter(s['type'] for s in nodes)
    print(f"parsed {len(ex.objs)} objects | {len(nodes)} unique nodes -> {a.json}")
    print("top node types:", dict(types.most_common(10)))
    if a.find:
        q=a.find.lower()
        hit=[s for s in nodes if q in str(s['path']).lower() or q in str(s['name']).lower()]
        print(f"\n{len(hit)} nodes match '{a.find}':")
        for s in hit:
            print(f"\n* {s['path']}  [{s['type']}]")
            if 'Type' in s['settings']: print(f"    GateType={s['settings']['Type']}")
            if s['inputs']:
                print(f"    INPUTS ({len(s['inputs'])}):")
                for r in s['inputs']: print(f"      <- {r['object']}  .{r['property']}")
            if s['output']: print(f"    OUTPUT -> {s['output']['object']} .{s['output']['property']}")
            elif s['type']=='Gate': print("    OUTPUT -> (unbound)")
            if s['writes']:
                for r in s['writes'][:8]: print(f"    WRITES -> {r['object']} .{r['property']}")
            if a.full and s['values']: print(f"    values={s['values']}")

if __name__=='__main__':
    main()

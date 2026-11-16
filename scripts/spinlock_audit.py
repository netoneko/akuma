#!/usr/bin/env python3
"""Classify every `static NAME: Spinlock<..>` by how its `lock()` / `try_lock()` sites are masked.

`docs/reference/spinlock-classification.md` explains the classes. A site counts as **masked** if it sits inside a
`with_irqs_disabled(..)` closure, or in a function that also creates an `IrqGuard` / `PreemptGuard` / calls `irq_save_mask`
(crude: text, not a call graph). Test files are excluded.

    scripts/spinlock_audit.py            # counts per class
    scripts/spinlock_audit.py --list     # every lock with its numbers
    scripts/spinlock_audit.py --unmasked # the unmasked sites of the MIXED locks (the ones worth reading)
"""
import os,re,collections,json,sys
ROOT=os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP={'target','.git','overlays','userspace','node_modules'}
files=[]
for dp,dn,fn in os.walk(ROOT):
    dn[:]=[d for d in dn if d not in SKIP]
    for f in fn:
        if f.endswith('.rs'): files.append(os.path.join(dp,f))
def is_test(rel): 
    b=os.path.basename(rel); return b in('tests.rs','test_support.rs') or b.endswith('_tests.rs') or '/tests/' in rel or 'kernel-tests' in rel
src={f:open(f,encoding='utf-8',errors='ignore').read() for f in files}
DECL=re.compile(r'^\s*(?:pub(?:\([a-z]+\))?\s+)?static\s+([A-Z][A-Z0-9_]*)\s*:\s*((?:[\w:]+::)?(?:Irq)?Spinlock)\s*<',re.M)
statics=[]
for f,t in src.items():
    rel=os.path.relpath(f,ROOT)
    if is_test(rel): continue
    for m in DECL.finditer(t):
        statics.append((m.group(1),m.group(2),rel,t.count('\n',0,m.start())+1))
def enclosed_in_irq(t,pos):
    # scan backwards for an unmatched '(' preceded by with_irqs_disabled
    depth=0;i=pos
    lim=max(0,pos-6000)
    while i>lim:
        i-=1
        c=t[i]
        if c==')': depth+=1
        elif c=='(':
            if depth==0:
                pre=t[max(0,i-24):i]
                if pre.rstrip().endswith('with_irqs_disabled') or pre.rstrip().endswith('without_interrupts'): return True
            else: depth-=1
        elif c=='{' and False: pass
    return False
def guard_in_fn(t,pos):
    # an IrqGuard / irq_save_mask earlier in the same fn body (crude: since last 'fn ')
    s=t.rfind('\n    fn ',0,pos); s2=t.rfind('\nfn ',0,pos); s3=t.rfind('pub fn ',0,pos)
    st=max(s,s2,s3,0)
    seg=t[st:pos]
    return bool(re.search(r'IrqGuard::new|irq_save_mask|PreemptGuard',seg))
res=[]
for name,ty,rel,line in statics:
    already=ty.endswith('IrqSpinlock')
    tot=wrapped=guarded=0; files_used=set(); tests=0
    for f,t in src.items():
        r=os.path.relpath(f,ROOT)
        for m in re.finditer(r'\b'+name+r'\s*\.\s*(lock|try_lock)\s*\(',t):
            if is_test(r): tests+=1; continue
            tot+=1; files_used.add(r)
            if enclosed_in_irq(t,m.start()): wrapped+=1
            elif guard_in_fn(t,m.start()): guarded+=1
    res.append(dict(name=name,type=ty,file=rel,line=line,already=already,sites=tot,wrapped=wrapped,guarded=guarded,test_sites=tests,files=len(files_used)))
c=collections.Counter()
for r in res:
    if r['already']: k='already IrqSpinlock'
    elif r['sites']==0: k='no production lock() sites found'
    elif r['wrapped']+r['guarded']==r['sites']: k='ALL sites masked'
    elif r['wrapped']+r['guarded']==0: k='NO site masked'
    else: k='MIXED'
    c[k]+=1; r['cls']=k
print(len(res),'static locks')
if '--list' in sys.argv:
    for x in sorted(res,key=lambda x:(x['cls'],x['file'])):
        print(f"{x['cls']:34} {x['name']:26} {x['file']}:{x['line']} sites={x['sites']} masked={x['wrapped']} guarded={x['guarded']}")
if '--unmasked' in sys.argv:
    for x in res:
        if x['cls']!='MIXED': continue
        print('##',x['name'],x['file'])
        for f,t in src.items():
            rel=os.path.relpath(f,ROOT)
            if is_test(rel): continue
            for m in re.finditer(r'\b'+x['name']+r'\s*\.\s*(lock|try_lock)\s*\(',t):
                if enclosed_in_irq(t,m.start()) or guard_in_fn(t,m.start()): continue
                ln=t.count(chr(10),0,m.start())+1
                print(f'   {rel}:{ln}: '+t.split(chr(10))[ln-1].strip()[:110])

for k,v in c.most_common(): print(f'{v:4} {k}')

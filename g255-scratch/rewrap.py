import sys, textwrap, re
# usage: rewrap.py FILE  -- rewraps every paragraph containing a line longer than WIDTH, among comment (///, //!, //)
# and markdown blockquote ('> ') or bullet-continuation paragraphs, keeping each paragraph's prefixes.
WIDTH=120
path=sys.argv[1]
only=set(int(x) for a in sys.argv[2:] for x in a.split())  # 1-based line numbers of long lines to fix (empty: all)
lines=open(path,encoding='utf-8').read().split('\n')
pat=re.compile(r'^(\s*(?:///|//!|//)\s|>\s)')
def prefix_of(l):
    m=pat.match(l)
    return m.group(1) if m else None
def is_break(l,p):
    if prefix_of(l)!=p: return True
    body=l[len(p):]
    if not body.strip(): return True
    if p.strip()=='>' and body.startswith('**Status'): return 'start'
    if body.lstrip().startswith('- '): return 'start'
    return False
fixed=0
i=0
out=list(lines)
targets=[n-1 for n in sorted(only)] if only else [k for k,l in enumerate(lines) if len(l)>WIDTH and prefix_of(l)]
done=set()
for t in targets:
    if t in done: continue
    p=prefix_of(out[t])
    if p is None: print('skip (no prefix):',t+1); continue
    # paragraph start
    s=t
    while s>0:
        b=is_break(out[s],p)
        if b=='start': break
        prev=out[s-1]
        if is_break(prev,p) is True: break
        s-=1
    e=t
    while e+1<len(out):
        b=is_break(out[e+1],p)
        if b: break
        e+=1
    first=out[s][len(p):]
    lead=re.match(r'^(\s*(?:- )?)',first).group(1)
    cont=' '*len(lead)
    text=' '.join(l[len(p):].strip() for l in out[s:e+1])
    text=text[len(lead.strip()):].strip() if lead.strip() else text
    wrapped=textwrap.wrap(text,width=WIDTH-len(p)-len(lead),break_long_words=False,break_on_hyphens=False)
    new=[p+lead+wrapped[0]]+[p+cont+w for w in wrapped[1:]]
    out[s:e+1]=new
    shift=len(new)-(e-s+1)
    targets=[x+shift if x>e else x for x in targets]
    done.update(range(s,s+len(new)))
    fixed+=1
open(path,'w',encoding='utf-8').write('\n'.join(out))
print(path,'paragraphs rewrapped:',fixed)

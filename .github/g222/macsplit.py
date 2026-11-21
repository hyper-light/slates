import sys
base=sys.argv[1]
def rows(p):
    try: L=open(p).read().splitlines()
    except OSError: return []
    return [l.split('\t') for l in L if l and not l.startswith('#') and l[0].isdigit()]
ring={}
for q,s,landed,written in (r for r in rows(base+'.ring') if len(r)==4):
    ring[int(landed)//1000]=(int(q),int(s),int(written))
def landed_ns(stamp,by):
    low=(by & 0xFFFFFFFF) & ~1; back=(low-(stamp & ~1)) & 0xFFFFFFFF; return by-back
tot=[];m=[]
for st,seen,parks in rows(base+'.client'):
    st,seen,parks=int(st),int(seen),int(parks)
    if st==0: continue
    L=landed_ns(st,seen); t=seen-L
    r=None
    for k in (L//1000, L//1000-1, L//1000+1):
        if k in ring: r=ring[k]; break
    tot.append((t,parks,r and (r[0],r[1],seen-r[2])))
tot.sort(key=lambda x:x[0])
pc=lambda a,p: a[min(len(a)-1,int(len(a)*p))] if a else 0
n=len(tot); print(f'client n {n} total p50/p99/p999/max us {pc(tot,.5)[0]/1e3:.0f} {pc(tot,.99)[0]/1e3:.0f} {pc(tot,.999)[0]/1e3:.0f} {tot[-1][0]/1e3:.0f}' if n else 'client n 0')
for name,lo in (('p99 tail',.99),('p999 tail',.999)):
    tail=tot[int(n*lo):]
    if not tail: continue
    j=[x[2] for x in tail if x[2]]
    med=lambda a: sorted(a)[len(a)//2]/1e3 if a else float('nan')
    parked=sum(1 for x in tail if x[1]>0)
    print(f'  {name}: n {len(tail)} parked {parked} parks med {sorted(x[1] for x in tail)[len(tail)//2]}; joined {len(j)}: queued med {med([a for a,_,_ in j]):.0f} served med {med([b for _,b,_ in j]):.0f} wake(written->seen) med {med([c for _,_,c in j]):.0f} us; max q/s/w {max([a for a,_,_ in j],default=0)/1e3:.0f}/{max([b for _,b,_ in j],default=0)/1e3:.0f}/{max([c for _,_,c in j],default=0)/1e3:.0f}')
    if j:
        big=collections=None
        dom={'queued':0,'served':0,'wake':0}
        for a,b,c in j: dom[max((('queued',a),('served',b),('wake',c)),key=lambda x:x[1])[0]]+=1
        print('    dominant hop:',dom)
j=[x[2] for x in tot if x[2]]
for i,nm in enumerate(('queued','served','wake')):
    a=sorted(x[i] for x in j)
    if a: print(f'  all joined {nm} p50/p99/p999 us {pc(a,.5)/1e3:.0f} {pc(a,.99)/1e3:.0f} {pc(a,.999)/1e3:.0f}')
P=rows(base+'.parks')
if P:
    w=sorted(int(r[1]) for r in P); print(f'  shard parks n {len(P)} waited p50/p99/p999/max us {pc(w,.5)/1e3:.0f} {pc(w,.99)/1e3:.0f} {pc(w,.999)/1e3:.0f} {w[-1]/1e3:.0f}')
if P:
    for nm,sel in (('timeout 0 (nap)',lambda t:t==0),('timed >0',lambda t:0<t<2**63),('untimed',lambda t:t>=2**63)):
        sub=[r for r in P if sel(int(r[0]))]
        over=sorted(int(r[1])-(0 if int(r[0])>=2**63 else int(r[0])) for r in sub)
        if over: print(f'  shard parks {nm}: n {len(over)} waited-past-timeout p50/p99/p999/max us {pc(over,.5)/1e3:.0f} {pc(over,.99)/1e3:.0f} {pc(over,.999)/1e3:.0f} {over[-1]/1e3:.0f}; >=1ms {sum(1 for o in over if o>=1e6)}')
    ends={}
    for r in P: ends[r[2]]=ends.get(r[2],0)+1
    print('  shard park ends',ends)
try:
    h=[l for l in open(base+'.parks') if l.startswith('# harvest_us_log2')][0].split()[2:]
    h=[int(x) for x in h]; t=sum(h)
    if t:
        lab=lambda i: '<1us' if i==0 else f'{2**(i-1)}-{2**i}us'
        print('  harvests n',t,'| '+' '.join(f'{lab(i)}:{c}' for i,c in enumerate(h) if c), '| >=512us', sum(h[10:]), '>=1ms', sum(h[11:]))
except (OSError,IndexError): pass

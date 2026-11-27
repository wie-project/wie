import glob,statistics as st
def load(p):
    d={}
    for f in sorted(glob.glob(p)):
        for l in open(f):
            q=l.split()
            if len(q)==4: d.setdefault((q[0],q[1]),[]).append((float(q[2]),q[3]))
    return d
m=load('mac*.txt'); w=load('wie*.txt')
print(f"{'kernel':9}{'pass':6}{'rosetta ms':>11}{'wie ms':>10}{'wie/ros':>9}{'wie min..max':>18} result")
for k in m:
    if k not in w: print(k,'missing'); continue
    a=st.median(x for x,_ in m[k]); b=st.median(x for x,_ in w[k]); xs=[x for x,_ in w[k]]
    same={r for _,r in m[k]}=={r for _,r in w[k]}
    print(f"{k[0]:9}{k[1]:6}{a:11.2f}{b:10.2f}{b/a:8.2f}x{min(xs):9.1f}..{max(xs):<7.1f} {'same' if same else 'DIFF '+str((m[k][0][1],w[k][0][1]))}  n={len(m[k])}/{len(w[k])}")

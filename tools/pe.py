# usage: pe.py imports <exe> <dllname> | pe.py exports <dll>
import sys,struct
mode,path=sys.argv[1],sys.argv[2]
d=open(path,'rb').read()
pe=struct.unpack_from('<I',d,0x3c)[0]
nsec=struct.unpack_from('<H',d,pe+6)[0]; optsz=struct.unpack_from('<H',d,pe+20)[0]; opt=pe+24
secs=[struct.unpack_from('<8sIIII',d,opt+optsz+i*40) for i in range(nsec)]
def off(r):
    for n,vs,va,rs,ro in secs:
        if va<=r<va+max(vs,rs): return r-va+ro
def s(o): return d[o:d.index(b'\0',o)].decode()
if mode=='exports':
    rva=struct.unpack_from('<I',d,opt+96)[0]; o=off(rva)
    n=struct.unpack_from('<I',d,o+24)[0]; names=off(struct.unpack_from('<I',d,o+32)[0])
    for i in range(n): print(s(off(struct.unpack_from('<I',d,names+4*i)[0])))
else:
    o=off(struct.unpack_from('<I',d,opt+96+8)[0])
    while True:
        oft,_,_,name,ft=struct.unpack_from('<IIIII',d,o)
        if not name: break
        if s(off(name)).lower()==sys.argv[3].lower():
            t=off(oft or ft)
            while (v:=struct.unpack_from('<I',d,t)[0]):
                print('ord',v&0xffff) if v&0x80000000 else print(s(off(v)+2)); t+=4
        o+=20

# Copyright (C) 2026 Doridian
# SPDX-License-Identifier: AGPL-3.0-or-later
import struct,sys,os
def u32(d,o): return struct.unpack_from('<I',d,o)[0]
def parse(path,outdir):
    d=open(path,'rb').read(); o=0
    ver=u32(d,o); o+=4
    nl=u32(d,o); o+=4; name=d[o:o+nl].decode('latin1'); o+=nl
    cnt=u32(d,o); o+=4
    print(f"== {path}: ver={ver} name={name!r} count={cnt} size={len(d)}")
    os.makedirs(outdir,exist_ok=True)
    n=0
    while o<len(d)-8:
        pl=u32(d,o); o+=4
        if pl==0 or pl>512 or o+pl>len(d): print("  ! bad path len",pl,"at",o-4); break
        p=d[o:o+pl].decode('latin1'); o+=pl
        dl=u32(d,o); o+=4
        if o+dl>len(d): print("  ! bad data len",dl,"for",p); break
        blob=d[o:o+dl]; o+=dl
        rel=p.replace('\\','/').lstrip('/')
        dst=os.path.join(outdir,rel)
        os.makedirs(os.path.dirname(dst),exist_ok=True)
        open(dst,'wb').write(blob)
        print(f"  [{n:2d}] {dl:9d}  {p}")
        n+=1
    print(f"  -> {n} entries, {len(d)-o} trailing bytes")
for f,od in (("m1.bin","out_config"),("m2.bin","out_client")):
    parse(f,od)

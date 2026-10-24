import subprocess,re
from pathlib import Path
root=Path(__file__).resolve().parent.parent/'.tools/gtk-sdk'
(root/'debs').mkdir(parents=True,exist_ok=True)
sim=subprocess.run(['apt-get','-s','--no-install-recommends','install','libgtk-4-dev'],capture_output=True,text=True,check=True).stdout
packages=[]
for line in sim.splitlines():
 m=re.match(r'Inst (\S+) \((\S+)',line)
 if m: packages.append(m[1]+'='+m[2])
if packages:
 subprocess.run(['apt-get','download',*packages],cwd=root/'debs',check=True)
for path in (root/'debs').glob('*.deb'):
 subprocess.run(['dpkg-deb','-x',str(path),str(root/'root')],check=True)
# Relocate .pc files only; use system headers/libraries for already installed dependencies.
for pc in (root/'root/usr').rglob('*.pc'):
 text=pc.read_text().replace('prefix=/usr\n','prefix='+str(root/'root/usr')+'\n')
 text=text.replace('libdir=/usr/lib/','libdir='+str(root/'root/usr/lib')+'/')
 pc.write_text(text)
print('SDK GTK lokal siap:',root/'root')

# Development symlinks can target runtime files already installed on the system.
import os
for link in (root/'root/usr/lib/x86_64-linux-gnu').glob('*.so'):
 if link.is_symlink() and not link.exists():
  name=Path(os.readlink(link)).name
  for base in [Path('/usr/lib/x86_64-linux-gnu'),Path('/lib/x86_64-linux-gnu')]:
   target=base/name
   if target.exists():
    link.unlink();link.symlink_to(target);break

#!/usr/bin/env python3
"""Bundle GTK and ELF dependencies, retaining host glibc/graphics drivers."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
app = Path(sys.argv[1])
lib = app / 'usr/lib'
lib.mkdir(parents=True, exist_ok=True)
# glibc and GPU dispatch/drivers belong to the target OS. Build baseline: Ubuntu 24.04.
exclude = re.compile(r'^(?:ld-linux.*|lib(?:c|m|pthread|dl|rt|resolv|util|nss_.*|anl)\.so.*|lib(?:GL|EGL|GLES|OpenGL|vulkan|drm|gbm).*)$')
queue = list((app / 'usr/bin').iterdir())
loaders = Path('/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders')
if loaders.exists():
    dest = lib / 'gdk-pixbuf-2.0/loaders'
    shutil.copytree(loaders, dest)
    queue += list(dest.glob('*.so'))
seen = set()
licenses = set()
while queue:
    file = queue.pop()
    result = subprocess.run(['ldd', str(file)], text=True, capture_output=True, check=True)
    if 'not found' in result.stdout:
        raise RuntimeError(result.stdout)
    for name, source in re.findall(r'^\s*(\S+)\s+=>\s+(/\S+)', result.stdout, re.M):
        if name in seen or exclude.match(name):
            continue
        seen.add(name)
        target = lib / name
        shutil.copy2(source, target, follow_symlinks=True)
        queue.append(target)
        owner = subprocess.run(['dpkg-query', '-S', str(Path(source).resolve())], text=True, capture_output=True)
        if owner.returncode:
            owner = subprocess.run(['dpkg-query', '-S', source], text=True, capture_output=True)
        if owner.returncode == 0:
            package = owner.stdout.split(': ', 1)[0].split(':', 1)[0]
            licenses.add(package)
for package in sorted(licenses):
    copyright = Path('/usr/share/doc') / package / 'copyright'
    if copyright.exists():
        dest = app / 'usr/share/doc/bundled-libraries' / package / 'copyright'
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(copyright, dest)
# Use a relocatable cache generated at launch, rather than host loader paths.
cache = lib / 'gdk-pixbuf-2.0/loaders.cache'
cache.write_text('')
query = shutil.which('gdk-pixbuf-query-loaders')
if not query:
    candidates = list(Path('/usr/lib').glob('*/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders'))
    query = str(candidates[0]) if candidates else None
if query:
    shutil.copy2(query, app / 'usr/bin/gdk-pixbuf-query-loaders')
schemas = app / 'usr/share/glib-2.0/schemas'
schemas.mkdir(parents=True)
for schema in Path('/usr/share/glib-2.0/schemas').glob('*.xml'):
    shutil.copy2(schema, schemas / schema.name)
for override in Path('/usr/share/glib-2.0/schemas').glob('*.override'):
    shutil.copy2(override, schemas / override.name)
subprocess.run(['glib-compile-schemas', str(schemas)], check=True)
source = Path('/usr/share/icons/Adwaita')
if source.exists():
    shutil.copytree(source, app / 'usr/share/icons/Adwaita', symlinks=True)
    copyright = Path('/usr/share/doc/adwaita-icon-theme/copyright')
    if copyright.exists():
        dest = app / 'usr/share/doc/bundled-libraries/adwaita-icon-theme/copyright'
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(copyright, dest)
hicolor = app / 'usr/share/icons/hicolor'
(hicolor / 'scalable/apps').mkdir(parents=True)
shutil.copy2('/usr/share/icons/hicolor/index.theme', hicolor / 'index.theme')
shutil.copy2(app / 'mygdrive.svg', hicolor / 'scalable/apps/mygdrive.svg')
print(f'Bundled {len(seen)} ELF libraries and {len(licenses)} license records')

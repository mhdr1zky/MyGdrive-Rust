#!/usr/bin/env python3
"""Install only the MyGdrive Rust application entry; does not enable autostart."""
import os
from pathlib import Path
root = Path(__file__).resolve().parent.parent
entry = Path(os.environ.get('XDG_DATA_HOME', str(Path.home() / '.local/share'))) / 'applications/io.mygdrive.Desktop.desktop'
if entry.exists() and 'X-MyGDrive-Managed=true' not in entry.read_text():
    raise SystemExit('Desktop entry already exists and is not managed by MyGdrive Rust.')
if not (root / 'target/debug/mygdrive-desktop').is_file():
    raise SystemExit('Build desktop first with ./build-desktop.sh')
entry.parent.mkdir(parents=True, exist_ok=True)
launcher = str(root / 'run-desktop.sh').replace('\\','\\\\').replace('"','\\"').replace('`','\\`').replace('$','\\$')
content = f'''[Desktop Entry]
Type=Application
Name=MyGdrive Rust
Comment=Manage Google Drive synchronization
Exec="{launcher}"
Icon={root / 'assets/mygdrive.svg'}
Terminal=false
Categories=Network;FileTransfer;
StartupWMClass=io.mygdrive.Desktop
X-MyGDrive-Managed=true
'''
temp = entry.with_suffix('.tmp')
temp.write_text(content)
temp.chmod(0o644)
temp.replace(entry)
print(entry)

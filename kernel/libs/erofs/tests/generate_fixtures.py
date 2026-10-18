# SPDX-License-Identifier: MPL-2.0

import os
import pathlib
import stat
import subprocess
import sys

os.umask(0o022)
root = pathlib.Path(sys.argv[1])
root.mkdir(parents=True,exist_ok=False)
source=root/'input'
source.mkdir()
(source/'dir').mkdir()
(source/'empty').touch()
(source/'small').write_bytes(b'EROFS inline\n')
(source/'dir/data').write_bytes(bytes(range(251))*8193)
os.link(source/'dir/data',source/'hardlink')
os.symlink('dir/data',source/'symlink')
os.setxattr(source/'small','user.test',b'attribute-value')
os.mkfifo(source/'fifo',0o600)
os.mknod(source/'char', stat.S_IFCHR | 0o600, os.makedev(1,3))
os.mknod(source/'block', stat.S_IFBLK | 0o600, os.makedev(259,703710))
for name, options in [('flat',[]),('lz4',['-z','lz4']),('lz4-full',['-z','lz4','-E','legacy-compress'])]:
    subprocess.run(['mkfs.erofs','--quiet','-b4096','-T0','-U00000000-0000-0000-0000-000000000000','--all-root','--workers=1',*options,str(root/(name+'.erofs')),str(source)],check=True)
    subprocess.run(['fsck.erofs',str(root/(name+'.erofs'))],check=True)

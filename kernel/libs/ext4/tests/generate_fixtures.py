# SPDX-License-Identifier: MPL-2.0

"""Generates a Linux ext4 image and a sparse, byte-exact test fixture."""

import os
import pathlib
import re
import struct
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
root.mkdir(parents=True, exist_ok=False)
image = root / 'journal.ext4'
payload = root / 'payload'
payload.write_bytes(bytes(range(251)) * 32)
with image.open('xb') as stream:
    stream.truncate(64 << 20)
env = dict(os.environ, E2FSPROGS_FAKE_TIME='1600000000', TZ='UTC')
uuid = '11111111-2222-3333-4444-555555555555'
subprocess.run([
    'mkfs.ext4', '-F', '-q', '-m0', '-b4096', '-I256', '-U' + uuid,
    '-O', 'has_journal,metadata_csum,metadata_csum_seed,64bit,extent,orphan_file,^resize_inode',
    '-J', 'size=4', '-E', 'lazy_itable_init=0,lazy_journal_init=0,hash_seed=' + uuid,
    str(image),
], env=env, check=True)
subprocess.run(['debugfs', '-w', '-R', f'write {payload} /payload', str(image)], env=env, check=True)
subprocess.run(['e2fsck', '-fn', str(image)], env=env, check=True)
def sparse(image, output):
    blocks = []
    with image.open('rb') as stream:
        for offset in range(0, image.stat().st_size, 4096):
            data = stream.read(4096)
            if any(data):
                blocks.append((offset, data))
    with output.open('xb') as stream:
        stream.write(b'EXT4SP01' + struct.pack('<QI', image.stat().st_size, len(blocks)))
        for offset, data in blocks:
            stream.write(struct.pack('<QI', offset, len(data)) + data)

sparse(image, root / 'journal.sparse')
(root / 'tools.txt').write_text(subprocess.run(['mkfs.ext4', '-V'], capture_output=True, text=True, check=True).stderr)
attributes = root / 'attributes.ext4'
subprocess.run(['cp', '--reflink=auto', str(image), str(attributes)], check=True)
value = root / 'attribute-value'
value.write_bytes(bytes(range(251)) * 4)
subprocess.run(['debugfs', '-w', '-R', f'write {payload} /shared', str(attributes)], env=env, check=True)
for name in ['/payload', '/shared']:
    subprocess.run(['debugfs', '-w', '-R', f'ea_set -f {value} {name} user.large', str(attributes)], env=env, check=True)
    subprocess.run(['debugfs', '-w', '-R', f'ea_set {name} user.small inline-value', str(attributes)], env=env, check=True)
subprocess.run(['e2fsck', '-fn', str(attributes)], env=env, check=True)
sparse(attributes, root / 'attributes.sparse')

shared = root / 'shared-attributes.ext4'
subprocess.run(['cp', '--reflink=auto', str(attributes), str(shared)], check=True)
blocks = []
for name in ['/payload', '/shared']:
    output = subprocess.run(['debugfs', '-R', f'stat {name}', str(shared)], capture_output=True, text=True, check=True).stdout
    blocks.append(int(re.search(r'File ACL: (\d+)', output).group(1)))
assert blocks[0] != blocks[1] and all(blocks)
subprocess.run(['debugfs', '-w', '-R', f'set_inode_field /shared file_acl {blocks[0]}', str(shared)], env=env, check=True)
subprocess.run(['debugfs', '-w', '-R', f'freeb {blocks[1]}', str(shared)], env=env, check=True)
def crc32c(seed, data):
    for byte in data:
        seed ^= byte
        for _ in range(8):
            seed = (seed >> 1) ^ (0x82f63b78 if seed & 1 else 0)
    return seed
with shared.open('r+b') as stream:
    stream.seek(1024 + 0x270)
    seed = struct.unpack('<I', stream.read(4))[0]
    stream.seek(blocks[0] * 4096)
    raw = bytearray(stream.read(4096))
    struct.pack_into('<I', raw, 4, 2)
    struct.pack_into('<I', raw, 16, 0)
    crc = crc32c(crc32c(seed, struct.pack('<Q', blocks[0])), raw)
    struct.pack_into('<I', raw, 16, crc)
    stream.seek(blocks[0] * 4096)
    stream.write(raw)
result = subprocess.run(['e2fsck', '-fy', str(shared)], env=env)
if result.returncode not in (0, 1):
    raise RuntimeError('shared attribute fixture validation failed')
subprocess.run(['debugfs', '-w', '-R', 'set_super_value lastcheck @1600000000', str(shared)], env=env, check=True)
subprocess.run(['e2fsck', '-fn', str(shared)], env=env, check=True)
sparse(shared, root / 'shared-attributes.sparse')

classic = root / 'classic.ext4'
subprocess.run(['cp', '--reflink=auto', str(image), str(classic)], check=True)
subprocess.run(['tune2fs', '-O', '^orphan_file', str(classic)], env=env, check=True)
subprocess.run(['e2fsck', '-fn', str(classic)], env=env, check=True)
sparse(classic, root / 'classic.sparse')

indexed = root / 'indexed.ext4'
subprocess.run(['cp', '--reflink=auto', str(image), str(indexed)], check=True)
empty = root / 'empty'
empty.touch()
commands = root / 'directories.debugfs'
commands.write_text('mkdir /indexed\n' + ''.join(f'write {empty} /indexed/name{i:04}\n' for i in range(1000)))
subprocess.run(['debugfs', '-w', '-f', str(commands), str(indexed)], env=env, check=True, stdout=subprocess.DEVNULL)
result = subprocess.run(['e2fsck', '-fyD', str(indexed)], env=env)
if result.returncode not in (0, 1):
    raise RuntimeError('directory indexing failed')
subprocess.run(['debugfs', '-w', '-R', 'set_super_value lastcheck @1600000000', str(indexed)], env=env, check=True)
subprocess.run(['e2fsck', '-fn', str(indexed)], env=env, check=True)
sparse(indexed, root / 'indexed.sparse')

lazy = root / 'lazy-tables.ext4'
with lazy.open('xb') as stream:
    stream.truncate(512 << 20)
subprocess.run([
    'mkfs.ext4', '-F', '-q', '-m0', '-b4096', '-I256', '-N64', '-U' + uuid,
    '-O', 'has_journal,metadata_csum,metadata_csum_seed,64bit,extent,orphan_file,^resize_inode',
    '-J', 'size=4', '-E', 'nodiscard,lazy_itable_init=1,lazy_journal_init=0,hash_seed=' + uuid,
    str(lazy),
], env=env, check=True)
subprocess.run(['debugfs', '-w', '-R', f'write {payload} /payload', str(lazy)], env=env, check=True)
with lazy.open('r+b') as stream:
    stream.seek(1024)
    superblock = stream.read(1024)
    inode_count = struct.unpack_from('<I', superblock, 0)[0]
    inodes_per_group = struct.unpack_from('<I', superblock, 0x28)[0]
    assert inode_count == 64
    tables = 0
    for group in range(inode_count // inodes_per_group):
        stream.seek(4096 + group * 64)
        descriptor = stream.read(64)
        flags = struct.unpack_from('<H', descriptor, 18)[0]
        if flags & 1 and not flags & 4:
            table = struct.unpack_from('<I', descriptor, 8)[0] | (struct.unpack_from('<I', descriptor, 40)[0] << 32)
            stream.seek(table * 4096)
            stream.write(bytes([0xa5]) * (inodes_per_group * 256))
            tables += 1
    assert tables > 0
subprocess.run(['e2fsck', '-fn', str(lazy)], env=env, check=True)
sparse(lazy, root / 'lazy-tables.sparse')

for block_size, journalled, checksummed in [(1024, True, True), (2048, True, True), (4096, False, True), (1024, True, False)]:
    name = f'rw-{block_size}' + ('' if journalled else '-nojournal') + ('' if checksummed else '-nocsum')
    volume = root / (name + '.ext4')
    with volume.open('xb') as stream:
        stream.truncate(64 << 20)
    features = '64bit,extent,^resize_inode'
    features += ',metadata_csum,metadata_csum_seed' if checksummed else ',^metadata_csum,^metadata_csum_seed,^uninit_bg'
    features += ',has_journal' if journalled else ',^has_journal'
    features += ',orphan_file' if journalled and checksummed else ',^orphan_file'
    command = ['mkfs.ext4', '-F', '-q', '-m0', f'-b{block_size}', '-I256', '-U' + uuid, '-O', features]
    if journalled:
        command += ['-J', 'size=4']
    command += ['-E', 'lazy_itable_init=0,lazy_journal_init=0,hash_seed=' + uuid, str(volume)]
    subprocess.run(command, env=env, check=True)
    subprocess.run(['debugfs', '-w', '-R', f'write {payload} /payload', str(volume)], env=env, check=True)
    subprocess.run(['e2fsck', '-fn', str(volume)], env=env, check=True)
    sparse(volume, root / (name + '.sparse'))

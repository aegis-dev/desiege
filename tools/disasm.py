# usage: disasm.py <exe> <export-name-substring | 0xADDRESS> [instructions=40]
# Disassembles a function of a 32-bit PE (by exported name or virtual address) with capstone.
# Calls/jumps to exports are annotated with the export name; reads of absolute addresses are
# shown as-is (globals). Needs: pip install capstone pefile
import sys
import capstone
import pefile

exe, what = sys.argv[1], sys.argv[2]
count = int(sys.argv[3]) if len(sys.argv) > 3 else 40
pe = pefile.PE(exe, fast_load=True)
pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY['IMAGE_DIRECTORY_ENTRY_EXPORT']])
base = pe.OPTIONAL_HEADER.ImageBase
names = {}
for e in pe.DIRECTORY_ENTRY_EXPORT.symbols:  # several exports can share one address (stubs)
    if e.name:
        names.setdefault(base + e.address, []).append(e.name.decode())
names = {a: ' | '.join(n) for a, n in names.items()}

if what.startswith('0x'):
    starts = [int(what, 16)]
else:
    starts = [a for a, n in names.items() if what in n]
    if not starts:
        sys.exit(f'no export matching {what}')

md = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_32)
for start in starts[:8]:
    print(f'\n== {names.get(start, hex(start))} @ {start:#x}')
    code = pe.get_data(start - base, count * 15)
    for n, ins in enumerate(md.disasm(code, start)):
        if n >= count:
            break
        note = ''
        if ins.mnemonic in ('call', 'jmp') and ins.op_str.startswith('0x'):
            note = names.get(int(ins.op_str, 16), '')
        print(f'  {ins.address:#x}: {ins.mnemonic:6} {ins.op_str:40} {note}')
        if ins.mnemonic == 'ret' and n > 2:
            break

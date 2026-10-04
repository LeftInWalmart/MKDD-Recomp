"""Symbolize the host profiler's raw samples (mkdd_hostprof_raw.txt) against the PDB.

The in-process profiler can't always load the (1 GB+) PDB of the debug build; this loads it
offline with dbghelp and aggregates the samples per function.

usage: python tools/symbolize_probe.py [rust/mkdd_hostprof_raw.txt] [rust/target/debug/mkdd-bevy.exe]
Prints the last probe window's samples aggregated by function.
"""
import ctypes
import ctypes.wintypes as wt
import re
import sys
from collections import defaultdict

probe = sys.argv[1] if len(sys.argv) > 1 else 'rust/mkdd_hostprof_raw.txt'
exe = sys.argv[2] if len(sys.argv) > 2 else 'rust/target/debug/mkdd-bevy.exe'

dbg = ctypes.WinDLL('dbghelp.dll')


class SYMBOL_INFO(ctypes.Structure):
    _fields_ = [('SizeOfStruct', wt.ULONG), ('TypeIndex', wt.ULONG), ('Reserved', ctypes.c_uint64 * 2),
                ('Index', wt.ULONG), ('Size', wt.ULONG), ('ModBase', ctypes.c_uint64), ('Flags', wt.ULONG),
                ('Value', ctypes.c_uint64), ('Address', ctypes.c_uint64), ('Register', wt.ULONG),
                ('Scope', wt.ULONG), ('Tag', wt.ULONG), ('NameLen', wt.ULONG), ('MaxNameLen', wt.ULONG),
                ('Name', ctypes.c_char * 1024)]


proc = wt.HANDLE(0x1234)
dbg.SymSetOptions(0x2 | 0x10)  # UNDNAME | LOAD_LINES off; publics + privates
assert dbg.SymInitialize(proc, None, False)
dbg.SymLoadModuleEx.restype = ctypes.c_uint64
dbg.SymLoadModuleEx.argtypes = [wt.HANDLE, wt.HANDLE, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint64,
                                wt.DWORD, ctypes.c_void_p, wt.DWORD]
BASE = 0x10000000
base = dbg.SymLoadModuleEx(proc, None, exe.encode(), None, BASE, 0, None, 0)
assert base, 'SymLoadModuleEx failed'


def name(off):
    si = SYMBOL_INFO()
    si.SizeOfStruct = 88
    si.MaxNameLen = 1000
    disp = ctypes.c_uint64()
    if dbg.SymFromAddr(proc, ctypes.c_uint64(base + off), ctypes.byref(disp), ctypes.byref(si)):
        n = si.Name.decode(errors='replace')
        return re.sub(r'::h[0-9a-f]{16}$', '', n)
    return f'exe+{off:#x}'


agg = {'cpu': defaultdict(int), 'gpu': defaultdict(int)}
for line in open(probe, encoding='utf-8'):
    tag, a, k = line.split()
    agg[tag][name(int(a, 16)) if not a.startswith('abs') else '(system dll) ' + a[3:]] += int(k)
for tag, d in agg.items():
    total = sum(d.values()) or 1
    print(f'host profile, {tag} thread ({total} samples)')
    for k, v in sorted(d.items(), key=lambda kv: -kv[1])[:int(__import__("os").environ.get("TOPN","40"))]:
        print(f'  {100 * v / total:5.1f}%  {k[:150]}')

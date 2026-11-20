#!/usr/bin/env python3
"""Translate a specialised, optimised AIR function (LLVM textual IR from
airspec.py -O) into GLSL 4.50 for glslang -> SPIR-V -> skyprog (ir3).

usage: air2glsl.py vs.ll fs.ll OUTDIR NAME
  writes OUTDIR/NAME.vert, OUTDIR/NAME.frag and OUTDIR/NAME.json (resource
  map: GLSL texture/sampler bindings -> Metal texture/sampler indices or
  constant sampler states, UBO bindings = Metal buffer indices).

Scope: what CoreAnimation's specialised shaders use (straight-line code or
acyclic branches, float/half/int scalars and vectors, constant-buffer loads,
2D sampling, a few AIR math intrinsics). Anything else stops with an error
naming it, so that the shader can be looked at instead of mistranslated.
"""
import json
import re
import struct
import sys


class Unsupported(Exception):
    pass


# --- lexing helpers -------------------------------------------------------

def split_top(s, sep=','):
    """split at top-level separators (outside <>, {}, (), [] and quotes)"""
    out, depth, cur, q = [], 0, '', False
    for ch in s:
        if ch == '"':
            q = not q
        if not q:
            if ch in '<{([':
                depth += 1
            elif ch in '>})]':
                depth -= 1
            elif ch == sep and depth == 0:
                out.append(cur.strip())
                cur = ''
                continue
        cur += ch
    if cur.strip():
        out.append(cur.strip())
    return out


def parse_type(s):
    """returns (type, rest); types are tuples:
    ('f', bits) ('i', bits) ('v', n, elem) ('s', [fields], packed) ('a', n, elem)
    ('p', pointee, addrspace) ('named', name) ('void',) ('opaque',)"""
    s = s.lstrip()
    m = re.match(r'(half|float|double|void|i\d+)\b', s)
    if m:
        t = m.group(1)
        rest = s[m.end():]
        if t == 'half':
            ty = ('f', 16)
        elif t == 'float':
            ty = ('f', 32)
        elif t == 'double':
            ty = ('f', 64)
        elif t == 'void':
            ty = ('void',)
        else:
            ty = ('i', int(t[1:]))
    elif s.startswith('<{') or s.startswith('{'):
        packed = s.startswith('<{')
        s2 = s[2:] if packed else s[1:]
        depth, i = 1, 0
        while depth:
            if s2[i] in '<{([':
                depth += 1
            elif s2[i] in '>})]':
                depth -= 1
            i += 1
        body = s2[:i - 1]
        rest = s2[i:]
        if packed:
            assert rest.startswith('>')
            rest = rest[1:]
        ty = ('s', [parse_type(f)[0] for f in split_top(body.strip(' }'))] if body.strip() else [], packed)
    elif s.startswith('<'):
        m = re.match(r'<(\d+) x ', s)
        el, rest = parse_type(s[m.end():])
        rest = rest.lstrip()
        assert rest.startswith('>'), s
        ty = ('v', int(m.group(1)), el)
        rest = rest[1:]
    elif s.startswith('['):
        m = re.match(r'\[(\d+) x ', s)
        el, rest = parse_type(s[m.end():])
        rest = rest.lstrip()
        assert rest.startswith(']'), s
        ty = ('a', int(m.group(1)), el)
        rest = rest[1:]
    elif s.startswith('%'):
        m = re.match(r'%("[^"]*"|[\w.:]+)', s)
        ty = ('named', m.group(1).strip('"'))
        rest = s[m.end():]
    else:
        raise Unsupported('type: ' + s[:40])
    while True:
        rest = rest.lstrip()
        m = re.match(r'addrspace\((\d+)\)\s*\*', rest)
        if m:
            ty = ('p', ty, int(m.group(1)))
            rest = rest[m.end():]
            continue
        if rest.startswith('*'):
            ty = ('p', ty, 0)
            rest = rest[1:]
            continue
        break
    return ty, rest


# --- module ---------------------------------------------------------------

class Module:
    def __init__(self, text):
        self.text = text
        self.types = {}
        for m in re.finditer(r'^%("[^"]*"|[\w.:]+) = type (.*)$', text, re.M):
            name = m.group(1).strip('"')
            if m.group(2).strip() == 'opaque':
                self.types[name] = ('opaque',)
            else:
                self.types[name] = parse_type(m.group(2))[0]
        self.globals = {}
        for m in re.finditer(r'^(@[\w.$]+) = (?:internal |private )?(?:unnamed_addr )?(?:local_unnamed_addr )?'
                             r'addrspace\(\d+\) (?:global|constant) (.*?), align \d+$', text, re.M):
            self.globals[m.group(1)] = m.group(2)
        self.meta = {}
        for m in re.finditer(r'^!(\d+) = (?:distinct )?!\{(.*)\}$', text, re.M):
            self.meta[int(m.group(1))] = m.group(2)
        fm = re.search(r'^define (.*?)@([\w.]+)\((.*)\) (?:local_unnamed_addr )?(?:#\d+ )?\{$', text, re.M)
        self.fname = fm.group(2)
        self.ret, _ = parse_type(fm.group(1).replace('noundef ', ''))
        self.params = []
        for p in split_top(fm.group(3)):
            ty, rest = parse_type(p)
            name = rest.split()[-1]
            self.params.append((ty, name))
        body = text[fm.end():]
        body = body[:re.search(r'^\}$', body, re.M).start()]
        self.blocks = []  # [(label, [lines])]
        label = '%d' % len(self.params)
        cur = []
        for line in body.split('\n'):
            line = line.split(';')[0].rstrip() if not line.strip().startswith(';') else ''
            line = re.sub(r', !(?:tbaa|range|alias\.scope|noalias|invariant\.load|nonnull)\s+!\d+', '', line)
            if not line.strip():
                continue
            m = re.match(r'^([\w.]+):', line)
            if m:
                if cur or self.blocks:
                    self.blocks.append((label, cur))
                label = m.group(1)
                cur = []
                continue
            cur.append(line.strip())
        self.blocks.append((label, cur))
        # stage metadata
        for stage in ('vertex', 'fragment'):
            m = re.search(r'^!air\.%s = !\{!(\d+)\}$' % stage, text, re.M)
            if m:
                self.stage = stage
                ent = split_top(self.meta[int(m.group(1))])
                self.out_md = [self.md_list(x) for x in self.md_refs(ent[1])]
                self.in_md = [self.md_list(x) for x in self.md_refs(ent[2])]

    def md_refs(self, ref):
        return [int(x[1:]) for x in split_top(self.meta[int(ref[1:])])]

    def md_list(self, n):
        return [x.strip() for x in split_top(self.meta[n])]

    def resolve(self, ty):
        while ty[0] == 'named':
            ty = self.types[ty[1]]
        return ty


def md_get(md, key, n=1):
    """value(s) after a !"key" entry"""
    for i, x in enumerate(md):
        if x == '!"%s"' % key:
            vals = md[i + 1:i + 1 + n]
            return [v.split()[-1].strip('"!') for v in vals] if n > 1 else vals[0].split()[-1].strip('"!')
    return None


def md_has(md, key):
    return '!"%s"' % key in md


# --- data layout (AIR: natural alignment, 3-vectors padded to 4) ----------

def size_align(mod, ty):
    ty = mod.resolve(ty)
    k = ty[0]
    if k == 'f' or k == 'i':
        b = max(1, ty[1] // 8)
        return b, b
    if k == 'v':
        es, _ = size_align(mod, ty[2])
        n = ty[1]
        alloc = es * (4 if n == 3 else n)
        return alloc, alloc
    if k == 'a':
        es, ea = size_align(mod, ty[2])
        es = (es + ea - 1) // ea * ea
        return es * ty[1], ea
    if k == 's':
        off, al = 0, 1
        for f in ty[1]:
            fs, fa = size_align(mod, f)
            if ty[2]:
                fa = 1
            off = (off + fa - 1) // fa * fa + fs
            al = max(al, fa)
        return (off + al - 1) // al * al, al
    if k == 'p':
        return 8, 8
    raise Unsupported('layout of %r' % (ty,))


def field_offset(mod, ty, idx):
    ty = mod.resolve(ty)
    off = 0
    for i, f in enumerate(ty[1]):
        fs, fa = size_align(mod, f)
        if ty[2]:
            fa = 1
        off = (off + fa - 1) // fa * fa
        if i == idx:
            return off, f
        off += fs
    raise Unsupported('field %d' % idx)


# --- GLSL types and constants -----------------------------------------------

def gtype(mod, ty):
    ty = mod.resolve(ty)
    if ty[0] == 'f':
        return 'float'
    if ty[0] == 'i':
        return 'bool' if ty[1] == 1 else 'int'
    if ty[0] == 'v':
        e = mod.resolve(ty[2])
        base = {'f': 'vec', 'i': 'bvec' if e[1] == 1 else 'ivec'}[e[0]]
        return '%s%d' % (base, ty[1]) if ty[1] > 1 else gtype(mod, e)
    raise Unsupported('glsl type of %r' % (ty,))


def fconst(tok):
    if tok.startswith('0xH'):
        h = int(tok[3:], 16)
        return struct.unpack('<e', struct.pack('<H', h))[0]
    if tok.startswith('0x'):
        return struct.unpack('<d', struct.pack('<Q', int(tok[2:], 16)))[0]
    return float(tok)


def flit(x):
    if x != x:
        return '(0.0/0.0)'
    if x in (float('inf'), float('-inf')):
        return '(%s1.0/0.0)' % ('-' if x < 0 else '')
    r = repr(float(x))
    return r if ('e' in r or '.' in r) else r + '.0'


# --- translation ------------------------------------------------------------

class Val:
    """an SSA value: GLSL expression (or per-field list for structs, or a
    pointer/texture/sampler descriptor)"""

    def __init__(self, kind, ty=None, expr=None, **kw):
        self.kind, self.ty, self.expr = kind, ty, expr
        self.__dict__.update(kw)


class Translator:
    def __init__(self, mod, varyings, res):
        self.mod = mod
        self.varyings = varyings  # varying name -> location
        self.res = res  # shared resource map
        self.vals = {}
        self.decls = []
        self.body = []
        self.ubos = {}  # metal buffer index -> size
        self.n = 0
        self.uses_fbfetch = False

    def tmp(self, gty, expr=None):
        self.n += 1
        name = 't%d' % self.n
        self.decls.append('%s %s;' % (gty, name))
        if expr is not None:
            self.body.append('%s = %s;' % (name, expr))
        return name

    # -- constants and operands --
    def const(self, ty, tok):
        ty = self.mod.resolve(ty)
        tok = tok.strip()
        if tok in ('undef', 'poison', 'zeroinitializer'):
            if ty[0] == 's':
                return Val('struct', ty, fields=[self.const(f, 'zeroinitializer') for f in ty[1]])
            if ty[0] == 'p':
                return Val('null', ty)
            g = gtype(self.mod, ty)
            if g == 'bool':
                return Val('v', ty, 'false')
            if g.startswith('bvec'):
                return Val('v', ty, '%s(false)' % g)
            return Val('v', ty, '%s(0)' % g)
        if ty[0] == 'f':
            return Val('v', ty, flit(fconst(tok)))
        if ty[0] == 'i':
            if ty[1] == 1:
                return Val('v', ty, 'true' if tok == 'true' else 'false')
            return Val('v', ty, str(int(tok)))
        if ty[0] == 'v':
            inner = tok.strip('<>').strip()
            els = split_top(inner)
            parts = [self.operand_typed(e).expr for e in els]
            return Val('v', ty, '%s(%s)' % (gtype(self.mod, ty), ', '.join(parts)))
        raise Unsupported('constant %r %s' % (ty, tok))

    def operand_typed(self, s):
        ty, rest = parse_type(s)
        return self.operand(ty, rest.strip())

    def operand(self, ty, tok):
        tok = tok.strip()
        for kw in ('nocapture ', 'readonly ', 'readnone ', 'noundef ', 'nonnull '):
            tok = tok.replace(kw, '')
        tok = tok.strip()
        if tok.startswith('%'):
            v = self.vals.get(tok)
            if v is None:
                raise Unsupported('value %s used before definition' % tok)
            return v
        if tok.startswith('@'):
            return Val('global', ty, name=tok)
        if tok.startswith('bitcast (') or tok.startswith('getelementptr'):
            return self.const_expr(ty, tok)
        return self.const(ty, tok)

    def const_expr(self, ty, tok):
        m = re.match(r'bitcast \((.*) to (.*)\)$', tok)
        if m:
            v = self.operand_typed(m.group(1))
            return v
        m = re.match(r'getelementptr (?:inbounds )?\((.*)\)$', tok)
        if m:
            parts = split_top(m.group(1))
            return self.gep(parts)
        raise Unsupported('constant expression ' + tok[:60])

    # -- pointers --
    def gep(self, parts):
        srcty, _ = parse_type(parts[0])
        base = self.operand_typed(parts[1])
        idx = [self.operand_typed(p) for p in parts[2:]]
        ty = srcty
        off = 0
        dyn = []
        # first index: array of srcty
        first = idx[0]
        sz, _ = size_align(self.mod, ty)
        if first.expr not in ('0',):
            if re.fullmatch(r'-?\d+', first.expr):
                off += int(first.expr) * sz
            else:
                dyn.append('(%s) * %d' % (first.expr, sz))
        for ix in idx[1:]:
            t = self.mod.resolve(ty)
            if t[0] == 's':
                o, ty = field_offset(self.mod, t, int(ix.expr))
                off += o
            elif t[0] in ('a', 'v'):
                es, ea = size_align(self.mod, t[2])
                if t[0] == 'a':
                    es = (es + ea - 1) // ea * ea
                if re.fullmatch(r'-?\d+', ix.expr):
                    off += int(ix.expr) * es
                else:
                    dyn.append('(%s) * %d' % (ix.expr, es))
                ty = t[2]
            else:
                raise Unsupported('gep into %r' % (t,))
        return self.ptr_add(base, off, dyn, ty)

    def ptr_add(self, base, off, dyn, ty):
        if base.kind == 'ptr':
            return Val('ptr', ('p', ty, 2), buf=base.buf, off=base.off + off, dyn=base.dyn + dyn, glob=base.glob)
        if base.kind == 'arg':
            return Val('ptr', ('p', ty, 2), buf=base.buf, off=off, dyn=dyn, glob=None)
        if base.kind == 'global':
            return Val('ptr', ('p', ty, 2), buf=None, off=off, dyn=dyn, glob=base.name)
        raise Unsupported('pointer arithmetic on %s' % base.kind)

    def load(self, ty, p):
        if p.kind in ('arg', 'global'):
            p = self.ptr_add(p, 0, [], ty)
        if p.kind != 'ptr':
            raise Unsupported('load from %s' % p.kind)
        if p.glob:
            if p.dyn:
                raise Unsupported('dynamic load from global ' + p.glob)
            return self.load_global(ty, p.glob, p.off)
        return self.load_ubo(ty, p.buf, p.off, p.dyn)

    def load_global(self, ty, name, off):
        init = self.mod.globals.get(name)
        if init is None:
            raise Unsupported('global ' + name)
        gty, rest = parse_type(init)
        data = self.const_bytes(gty, rest.strip())
        return self.from_bytes(ty, data[off:off + size_align(self.mod, ty)[0]])

    def const_bytes(self, ty, tok):
        ty = self.mod.resolve(ty)
        size, _ = size_align(self.mod, ty)
        if tok in ('zeroinitializer', 'undef'):
            return bytes(size)
        if ty[0] == 'f':
            x = fconst(tok)
            return struct.pack('<' + {16: 'e', 32: 'f', 64: 'd'}[ty[1]], x)
        if ty[0] == 'i':
            return int(tok).to_bytes(max(1, ty[1] // 8), 'little', signed=int(tok) < 0)
        if ty[0] in ('v', 'a'):
            els = split_top(tok.strip('<>[]'))
            out = b''.join(self.const_bytes(*[(lambda t: (t[0], t[1].strip()))(parse_type(e))][0]) for e in els)
            return out + bytes(size - len(out))
        raise Unsupported('constant data of %r' % (ty,))

    def from_bytes(self, ty, b):
        ty = self.mod.resolve(ty)
        if ty[0] == 'f':
            x = struct.unpack('<' + {16: 'e', 32: 'f'}[ty[1]], b)[0]
            return Val('v', ty, flit(x))
        if ty[0] == 'i':
            return Val('v', ty, str(int.from_bytes(b, 'little', signed=True)))
        if ty[0] == 'v':
            es, _ = size_align(self.mod, ty[2])
            parts = [self.from_bytes(ty[2], b[i * es:(i + 1) * es]).expr for i in range(ty[1])]
            return Val('v', ty, '%s(%s)' % (gtype(self.mod, ty), ', '.join(parts)))
        raise Unsupported('constant load of %r' % (ty,))

    def ubo_word(self, buf, off, dyn):
        """uint expression of the 32-bit word at byte offset off (+dyn)"""
        self.ubos[buf] = max(self.ubos.get(buf, 0), off + 4)
        if not dyn:
            return 'u%d.d[%d][%d]' % (buf, off // 16, (off % 16) // 4)
        o = '(%d + %s)' % (off, ' + '.join(dyn))
        self.ubos[buf] = max(self.ubos[buf], 65536)
        return 'u%d.d[%s / 16][(%s %% 16) / 4]' % (buf, o, o)

    def load_ubo(self, ty, buf, off, dyn):
        ty = self.mod.resolve(ty)
        if ty[0] == 'f' and ty[1] == 32:
            return Val('v', ty, 'uintBitsToFloat(%s)' % self.ubo_word(buf, off & ~3, dyn))
        if ty[0] == 'f' and ty[1] == 16:
            return Val('v', ty, 'unpackHalf2x16(%s).%s' % (self.ubo_word(buf, off & ~3, dyn), 'xy'[(off % 4) // 2]))
        if ty[0] == 'i' and ty[1] == 32:
            return Val('v', ty, 'int(%s)' % self.ubo_word(buf, off, dyn))
        if ty[0] == 'i' and ty[1] in (8, 16):
            sh = 8 * (off % 4)
            mask = (1 << ty[1]) - 1
            return Val('v', ty, 'int((%s >> %du) & %du)' % (self.ubo_word(buf, off & ~3, dyn), sh, mask))
        if ty[0] == 'v':
            es, _ = size_align(self.mod, ty[2])
            parts = [self.load_ubo(ty[2], buf, off + i * es, dyn).expr for i in range(ty[1])]
            return Val('v', ty, '%s(%s)' % (gtype(self.mod, ty), ', '.join(parts)))
        raise Unsupported('buffer load of %r' % (ty,))

    # -- instructions --
    def assign(self, name, ty, expr):
        g = gtype(self.mod, ty)
        t = self.tmp(g, expr)
        self.vals[name] = Val('v', ty, t)

    def comps(self, v, n):
        """list of n component expressions of a vector value"""
        if n == 1:
            return [v.expr]
        return ['%s.%s' % (self.paren(v.expr), 'xyzw'[i]) for i in range(n)] if n <= 4 else None

    def paren(self, e):
        return e if re.fullmatch(r'[\w.]+', e) else '(%s)' % e

    def vec_len(self, ty):
        ty = self.mod.resolve(ty)
        return ty[1] if ty[0] == 'v' else 1

    def run(self, ret_handler):
        mod = self.mod
        order = [b[0] for b in mod.blocks]
        index = {l: i for i, l in enumerate(order)}
        multi = len(order) > 1
        succ = {}
        for label, lines in mod.blocks:
            term = lines[-1]
            succ[label] = re.findall(r'label %([\w.]+)', term)
            for s in succ[label]:
                if index[s] <= index[label]:
                    raise Unsupported('loop (block %s -> %s)' % (label, s))
        # phis: declare up front
        phis = {}
        for label, lines in mod.blocks:
            for line in lines:
                m = re.match(r'(%[\w.]+) = phi (.*)$', line)
                if m:
                    ty, rest = parse_type(m.group(2))
                    ty = mod.resolve(ty)
                    if ty[0] in ('s', 'p'):
                        raise Unsupported('phi of %r' % (ty,))
                    t = self.tmp(gtype(mod, ty))
                    self.vals[m.group(1)] = Val('v', ty, t)
                    for inc in re.findall(r'\[\s*([^,\]]+?),\s*%([\w.]+)\s*\]', rest):
                        phis.setdefault((inc[1], label), []).append((t, ty, inc[0]))
        reach = {}
        if multi:
            for l in order[1:]:
                reach[l] = self.tmp('bool', 'false')
        for label, lines in mod.blocks:
            if multi and label != order[0]:
                self.body.append('if (%s) {' % reach[label])
            for line in lines[:-1]:
                if ' = phi ' in line:
                    continue
                self.instr(line)
            term = lines[-1]

            def go(target):
                out = []
                for (t, ty, tok) in phis.get((label, target), []):
                    out.append('%s = %s;' % (t, self.operand(ty, tok).expr))
                out.append('%s = true;' % reach[target])
                return out

            m = re.match(r'br i1 (.*), label %([\w.]+), label %([\w.]+)$', term)
            if m:
                c = self.operand(('i', 1), m.group(1)).expr
                self.body.append('if (%s) {' % c)
                self.body.extend(go(m.group(2)))
                self.body.append('} else {')
                self.body.extend(go(m.group(3)))
                self.body.append('}')
            elif re.match(r'br label %([\w.]+)$', term):
                self.body.extend(go(re.match(r'br label %([\w.]+)$', term).group(1)))
            elif term.startswith('ret '):
                ret_handler(self, term[4:])
            else:
                raise Unsupported('terminator ' + term)
            if multi and label != order[0]:
                self.body.append('}')

    def instr(self, line):
        mod = self.mod
        m = re.match(r'(%[\w.]+) = (\w+) (.*)$', line)
        if not m:
            if line.startswith('tail call void') or line.startswith('call void'):
                raise Unsupported('void call ' + line[:60])
            raise Unsupported('instruction ' + line[:60])
        dst, op, rest = m.groups()
        rest = re.sub(r'^(fast|nnan|ninf|nsz|arcp|contract|afn|reassoc|nuw|nsw|exact|inbounds)\s+', '', rest)
        while re.match(r'^(fast|nnan|ninf|nsz|arcp|contract|afn|reassoc|nuw|nsw|exact)\s+', rest):
            rest = re.sub(r'^\w+\s+', '', rest)
        binops = {'fadd': '+', 'fsub': '-', 'fmul': '*', 'fdiv': '/', 'add': '+', 'sub': '-', 'mul': '*',
                  'sdiv': '/', 'udiv': '/', 'and': '&', 'or': '|', 'xor': '^', 'shl': '<<', 'lshr': '>>', 'ashr': '>>'}
        if op in binops:
            ty, r = parse_type(rest)
            a, b = split_top(r)
            va, vb = self.operand(ty, a), self.operand(ty, b)
            g = gtype(mod, ty)
            if g in ('bool',) or g.startswith('bvec'):
                o = {'and': '&&', 'or': '||', 'xor': '^^'}[op]
                if g == 'bool':
                    self.assign(dst, ty, '%s %s %s' % (self.paren(va.expr), o, self.paren(vb.expr)))
                else:
                    n = self.vec_len(ty)
                    ca, cb = self.comps(va, n), self.comps(vb, n)
                    self.assign(dst, ty, '%s(%s)' % (g, ', '.join('%s %s %s' % (x, o, y) for x, y in zip(ca, cb))))
                return
            self.assign(dst, ty, '%s %s %s' % (self.paren(va.expr), binops[op], self.paren(vb.expr)))
            return
        if op in ('frem', 'srem', 'urem'):
            ty, r = parse_type(rest)
            a, b = split_top(r)
            va, vb = self.operand(ty, a), self.operand(ty, b)
            f = 'mod' if op == 'frem' else None
            if op == 'frem':
                # C fmod (sign of a) vs GLSL mod (sign of b)
                self.assign(dst, ty, '%s - %s * trunc(%s / %s)' % (self.paren(va.expr), self.paren(vb.expr), self.paren(va.expr), self.paren(vb.expr)))
            else:
                self.assign(dst, ty, '%s %% %s' % (self.paren(va.expr), self.paren(vb.expr)))
            return
        if op == 'fneg':
            ty, r = parse_type(rest)
            self.assign(dst, ty, '-%s' % self.paren(self.operand(ty, r).expr))
            return
        if op in ('fcmp', 'icmp'):
            pred, r = rest.split(' ', 1)
            ty, r = parse_type(r)
            a, b = split_top(r)
            va, vb = self.operand(ty, a), self.operand(ty, b)
            n = self.vec_len(ty)
            p = pred.lstrip('ofu').lstrip('s') if pred not in ('true', 'false') else pred
            cmap = {'lt': '<', 'le': '<=', 'gt': '>', 'ge': '>=', 'eq': '==', 'ne': '!='}
            fmap = {'lt': 'lessThan', 'le': 'lessThanEqual', 'gt': 'greaterThan', 'ge': 'greaterThanEqual',
                    'eq': 'equal', 'ne': 'notEqual'}
            key = re.sub(r'^(o|u|s)', '', pred)
            if key in ('rd', 'no'):  # ord / uno
                isnan = 'isnan(%s) || isnan(%s)' if n == 1 else None
                if n != 1:
                    raise Unsupported('vector fcmp ' + pred)
                e = isnan % (va.expr, vb.expr)
                self.assign(dst, ('i', 1), ('!(%s)' % e) if key == 'rd' else e)
                return
            rty = ('i', 1) if n == 1 else ('v', n, ('i', 1))
            if n == 1:
                self.assign(dst, rty, '%s %s %s' % (self.paren(va.expr), cmap[key], self.paren(vb.expr)))
            else:
                self.assign(dst, rty, '%s(%s, %s)' % (fmap[key], va.expr, vb.expr))
            return
        if op == 'select':
            parts = split_top(rest)
            cty, ctok = parse_type(parts[0])
            c = self.operand(cty, ctok)
            a = self.operand_typed(parts[1])
            b = self.operand_typed(parts[2])
            ty = a.ty
            if a.kind not in ('v',) or b.kind not in ('v',):
                if a.kind == b.kind == 'ptr' and a.buf == b.buf and a.glob == b.glob:
                    raise Unsupported('select of pointers')
                # texture/sampler/pointer selects: only constant conditions survive
                if c.expr == 'true':
                    self.vals[dst] = a
                    return
                if c.expr == 'false':
                    self.vals[dst] = b
                    return
                raise Unsupported('select of %s' % a.kind)
            if self.vec_len(cty) == 1:
                self.assign(dst, ty, '%s ? %s : %s' % (self.paren(c.expr), self.paren(a.expr), self.paren(b.expr)))
            else:
                self.assign(dst, ty, 'mix(%s, %s, %s)' % (b.expr, a.expr, c.expr))
            return
        if op == 'shufflevector':
            parts = split_top(rest)
            a = self.operand_typed(parts[0])
            b = self.operand_typed(parts[1])
            mty, mtok = parse_type(parts[2])
            na = self.vec_len(a.ty)
            elty = mod.resolve(a.ty)[2]
            if mtok.strip() == 'zeroinitializer':
                idx = [0] * mty[1]
            else:
                idx = [(-1 if 'undef' in e or 'poison' in e else int(e.split()[-1])) for e in split_top(mtok.strip().strip('<>'))]
            ca = self.comps(a, na)
            cb = self.comps(b, na) if b.kind == 'v' else None
            z = gtype(mod, elty) + '(0)'
            src = []
            for i in idx:
                if i < 0:
                    src.append(z)
                elif i < na:
                    src.append(ca[i])
                else:
                    src.append(cb[i - na] if cb else z)
            rty = ('v', len(idx), elty) if len(idx) > 1 else elty
            self.assign(dst, rty, '%s(%s)' % (gtype(mod, rty), ', '.join(src)))
            return
        if op == 'extractelement':
            parts = split_top(rest)
            v = self.operand_typed(parts[0])
            i = self.operand_typed(parts[1])
            elty = mod.resolve(v.ty)[2]
            if re.fullmatch(r'\d+', i.expr):
                self.assign(dst, elty, self.comps(v, self.vec_len(v.ty))[int(i.expr)])
            else:
                self.assign(dst, elty, '%s[%s]' % (self.paren(v.expr), i.expr))
            return
        if op == 'insertelement':
            parts = split_top(rest)
            v = self.operand_typed(parts[0])
            e = self.operand_typed(parts[1])
            i = self.operand_typed(parts[2])
            n = self.vec_len(v.ty)
            if not re.fullmatch(r'\d+', i.expr):
                raise Unsupported('dynamic insertelement')
            c = self.comps(v, n)
            c[int(i.expr)] = e.expr
            self.assign(dst, v.ty, '%s(%s)' % (gtype(mod, v.ty), ', '.join(c)))
            return
        if op == 'extractvalue':
            parts = split_top(rest)
            v = self.operand_typed(parts[0])
            for k in parts[1:]:
                v = v.fields[int(k)]
            self.vals[dst] = v
            return
        if op == 'insertvalue':
            parts = split_top(rest)
            v = self.operand_typed(parts[0])
            e = self.operand_typed(parts[1])
            ty = mod.resolve(v.ty)
            fields = list(v.fields) if v.kind == 'struct' else [self.const(f, 'undef') for f in ty[1]]
            fields[int(parts[2])] = e
            self.vals[dst] = Val('struct', ty, fields=fields)
            return
        if op == 'load':
            parts = split_top(rest)
            ty, _ = parse_type(parts[0])
            p = self.operand_typed(parts[1])
            v = self.load(ty, p)
            self.assign(dst, ty, v.expr)
            return
        if op == 'getelementptr':
            self.vals[dst] = self.gep(split_top(rest))
            return
        if op in ('fptrunc', 'fpext', 'bitcast', 'sitofp', 'uitofp', 'fptosi', 'fptoui', 'zext', 'sext', 'trunc',
                  'addrspacecast'):
            m2 = re.match(r'(.*) to (.*)$', rest)
            v = self.operand_typed(m2.group(1))
            ty, _ = parse_type(m2.group(2))
            if v.kind != 'v':
                self.vals[dst] = v  # pointer casts keep the descriptor
                return
            g = gtype(mod, ty)
            if op == 'bitcast':
                sg = gtype(mod, v.ty)
                if sg == g:
                    self.vals[dst] = v
                    return
                se = mod.resolve(v.ty)
                se = mod.resolve(se[2]) if se[0] == 'v' else se
                de = mod.resolve(ty)
                de = mod.resolve(de[2]) if de[0] == 'v' else de
                if se == ('f', 32) and de == ('i', 32):
                    self.assign(dst, ty, 'floatBitsToInt(%s)' % v.expr)
                elif se == ('i', 32) and de == ('f', 32):
                    self.assign(dst, ty, 'intBitsToFloat(%s)' % v.expr)
                elif se == ('f', 16) and de == ('i', 16):
                    n = self.vec_len(ty)
                    c = self.comps(v, n)
                    parts = ['int(packHalf2x16(vec2(%s, 0.0)) & 0xffffu)' % x for x in c]
                    self.assign(dst, ty, parts[0] if n == 1 else '%s(%s)' % (g, ', '.join(parts)))
                elif se == ('i', 16) and de == ('f', 16):
                    n = self.vec_len(ty)
                    c = self.comps(v, n)
                    parts = ['unpackHalf2x16(uint(%s) & 0xffffu).x' % x for x in c]
                    self.assign(dst, ty, parts[0] if n == 1 else '%s(%s)' % (g, ', '.join(parts)))
                else:
                    raise Unsupported('bitcast %s -> %s' % (sg, g))
                return
            if op in ('zext', 'sext') and gtype(mod, v.ty).startswith(('bool', 'bvec')):
                self.assign(dst, ty, '%s(%s)' % (g, v.expr))
                if op == 'sext':
                    self.vals[dst].expr = '-' + self.vals[dst].expr
                    self.assign(dst, ty, self.vals[dst].expr)
                return
            if op == 'trunc' and g.startswith(('bool', 'bvec')):
                self.assign(dst, ty, '%s(%s & 1)' % (g, self.paren(v.expr)))
                return
            self.assign(dst, ty, '%s(%s)' % (g, v.expr))
            return
        if op == 'call' or op == 'tail':
            self.call(dst, rest)
            return
        raise Unsupported('opcode ' + op)

    # -- AIR intrinsics --
    def call(self, dst, rest):
        mod = self.mod
        rest = re.sub(r'^call\s+', '', rest)
        rest = re.sub(r'^(fast|nnan|ninf|nsz|arcp|contract|afn|reassoc)\s+', '', rest)
        m = re.match(r'(.*?)@([\w.]+)\((.*)\)(?: #\d+)?$', rest)
        if not m:
            raise Unsupported('call ' + rest[:60])
        rty, _ = parse_type(m.group(1))
        fn = m.group(2)
        args = split_top(m.group(3))
        av = lambda i: self.operand_typed(args[i])
        base = re.sub(r'\.(v\d+)?[fiu]\d+$', '', fn)
        base = base.replace('air.fast_', 'air.')
        simple = {'air.fabs': 'abs', 'air.fmax': 'max', 'air.fmin': 'min', 'air.sqrt': 'sqrt', 'air.rsqrt': 'inversesqrt',
                  'air.floor': 'floor', 'air.ceil': 'ceil', 'air.fract': 'fract', 'air.trunc': 'trunc', 'air.rint': 'roundEven',
                  'air.round': 'round', 'air.exp2': 'exp2', 'air.log2': 'log2', 'air.exp': 'exp', 'air.log': 'log',
                  'air.pow': 'pow', 'air.powr': 'pow', 'air.sin': 'sin', 'air.cos': 'cos', 'air.tan': 'tan', 'air.atan2': 'atan',
                  'air.atan': 'atan', 'air.asin': 'asin', 'air.acos': 'acos',
                  'air.clamp': 'clamp', 'air.mix': 'mix', 'air.dot': 'dot', 'air.fwidth': 'fwidth', 'air.dfdx': 'dFdx',
                  'air.dfdy': 'dFdy', 'air.length': 'length', 'air.normalize': 'normalize', 'air.step': 'step',
                  'air.smoothstep': 'smoothstep', 'air.sign': 'sign', 'air.fma': 'fma', 'air.distance': 'distance',
                  'air.min': 'min', 'air.max': 'max', 'air.abs': 'abs', 'air.cross': 'cross'}
        if base in simple:
            ops = [av(i).expr for i in range(len(args))]
            self.assign(dst, rty, '%s(%s)' % (simple[base], ', '.join(ops)))
            return
        if base == 'air.saturate':
            self.assign(dst, rty, 'clamp(%s, 0.0, 1.0)' % av(0).expr)
            return
        if base in ('air.any', 'air.all'):
            v = av(0)
            if self.vec_len(v.ty) == 1:
                self.assign(dst, rty, v.expr)
            else:
                self.assign(dst, rty, '%s(%s)' % (base[4:], v.expr))
            return
        if base.startswith('air.convert'):
            # air.convert.<dst>.<src>
            self.assign(dst, rty, '%s(%s)' % (gtype(mod, rty), av(0).expr))
            return
        if fn in ('air.get_null_texture_2d', 'air.get_null_texture_3d'):
            self.vals[dst] = Val('tex', rty, null=True)
            return
        if fn.startswith('air.get_width_texture_2d') or fn.startswith('air.get_height_texture_2d'):
            tex = av(0)
            if tex.kind != 'tex' or tex.null:
                self.assign(dst, rty, '0')
                return
            t = self.res.tex_slot(tex.metal)
            self.assign(dst, rty, 'int(textureSize(sampler2D(tex%d, smp%d), %s).%s)' % (
                t, self.res.samp_slot(('const', 0x807bff0000080049)), av(1).expr if len(args) > 1 else '0',
                'x' if 'width' in fn else 'y'))
            return
        if fn.startswith('air.sample_texture_2d'):
            tex, samp, coord = av(0), av(1), av(2)
            # (tex, sampler, coord, i1, int2 offset, i1 has_offset, float bias/lod, float min_lod, i32 lod mode)
            mode = args[8].split()[-1] if len(args) > 8 else '0'
            off = av(4).expr
            elty = mod.resolve(rty)[1][0]
            if tex.kind == 'tex' and tex.null:
                fields = [Val('v', elty, 'vec4(0)'), Val('v', ('i', 8), '0')]
                self.vals[dst] = Val('struct', rty, fields=fields)
                return
            sname = self.sampler_expr(tex, samp)
            has_off = args[5].split()[-1] == 'true'
            lodv = av(6).expr
            if mode == '0':
                if lodv in ('0.0',) and not has_off:
                    e = 'texture(%s, %s)' % (sname, coord.expr)
                elif has_off:
                    e = 'textureOffset(%s, %s, %s, %s)' % (sname, coord.expr, off, lodv)
                else:
                    e = 'texture(%s, %s, %s)' % (sname, coord.expr, lodv)
            elif mode == '1':
                e = 'textureLod(%s, %s, %s)' % (sname, coord.expr, lodv)
            else:
                raise Unsupported('sample mode ' + mode)
            t = self.tmp('vec4', e)
            self.vals[dst] = Val('struct', rty, fields=[Val('v', elty, t), Val('v', ('i', 8), '0')])
            return
        raise Unsupported('intrinsic ' + fn)

    def sampler_expr(self, tex, samp):
        if tex.kind != 'tex':
            raise Unsupported('texture value %s' % tex.kind)
        t = self.res.tex_slot(tex.metal)
        if samp.kind == 'samp':
            s = self.res.samp_slot(('metal', samp.metal))
        elif samp.kind == 'global':
            bits = int(self.mod.globals[samp.name].split()[-1]) & (2 ** 64 - 1)
            s = self.res.samp_slot(('const', bits))
        elif samp.kind == 'ptr' and samp.glob:
            bits = int(self.mod.globals[samp.glob].split()[-1]) & (2 ** 64 - 1)
            s = self.res.samp_slot(('const', bits))
        else:
            raise Unsupported('sampler value %s' % samp.kind)
        return 'sampler2D(tex%d, smp%d)' % (t, s)


class Resources:
    """texture/sampler slots of one program (GLSL binding = slot)"""

    def __init__(self):
        self.tex = []
        self.samp = []

    def tex_slot(self, metal):
        if metal not in self.tex:
            self.tex.append(metal)
        return self.tex.index(metal)

    def samp_slot(self, key):
        if key not in self.samp:
            self.samp.append(key)
        return self.samp.index(key)


def gen_name(md):
    """varying name: the user name inside generated(<len>name...)"""
    g = md_get(md, 'air.vertex_output') or md_get(md, 'air.fragment_input')
    m = re.match(r'generated\((\d+)(\w+)', g or '')
    return m.group(2)[:int(m.group(1))] if m else g


def translate(vs_mod, fs_mod, name):
    res = Resources()
    out = {'name': name}
    varyings = {}
    # vertex outputs define the varying locations
    vtr = Translator(vs_mod, varyings, res)
    vhead = ['#version 450']
    vouts = []
    for i, md in enumerate(vs_mod.out_md):
        if md_has(md, 'air.position'):
            vouts.append(('pos', None))
        elif md_has(md, 'air.point_size'):
            vouts.append(('psize', None))
        elif md_has(md, 'air.vertex_output'):
            n = gen_name(md)
            loc = len(varyings)
            varyings[n] = loc
            vouts.append(('var', loc))
        else:
            raise Unsupported('vertex output %r' % md)
    for (ty, pname), md in zip(vs_mod.params, vs_mod.in_md):
        if md_has(md, 'air.vertex_input'):
            loc = int(md_get(md, 'air.location_index'))
            g = gtype(vs_mod, ty)
            vhead.append('layout(location = %d) in %s a%d;' % (loc, g, loc))
            vtr.vals[pname] = Val('v', ty, 'a%d' % loc)
        elif md_has(md, 'air.buffer'):
            vtr.vals[pname] = Val('arg', ty, buf=int(md_get(md, 'air.location_index')))
        elif md_has(md, 'air.vertex_id'):
            vtr.vals[pname] = Val('v', ty, 'gl_VertexIndex')
        elif md_has(md, 'air.instance_id'):
            vtr.vals[pname] = Val('v', ty, 'gl_InstanceIndex')
        else:
            raise Unsupported('vertex input %r' % md)
    vty = vs_mod.resolve(vs_mod.ret)
    vfields = vty[1] if vty[0] == 's' else [vty]
    for (kind, loc), fty in zip(vouts, vfields):
        if kind == 'var':
            vhead.append('layout(location = %d) out %s o%d;' % (loc, gtype(vs_mod, fty), loc))

    def vret(tr, tok):
        v = tr.operand_typed(tok)
        fields = v.fields if v.kind == 'struct' else [v]
        for (kind, loc), f in zip(vouts, fields):
            if kind == 'pos':
                tr.body.append('gl_Position = %s;' % f.expr)
            elif kind == 'psize':
                tr.body.append('gl_PointSize = %s;' % f.expr)
            else:
                tr.body.append('o%d = %s;' % (loc, f.expr))

    vtr.run(vret)

    ftr = Translator(fs_mod, varyings, res)
    fhead = ['#version 450']
    for (ty, pname), md in zip(fs_mod.params, fs_mod.in_md):
        if md_has(md, 'air.fragment_input'):
            n = gen_name(md)
            if n not in varyings:
                raise Unsupported('fragment input %s not written by the vertex shader' % n)
            loc = varyings[n]
            interp = 'flat ' if md_has(md, 'air.flat') else 'noperspective ' if md_has(md, 'air.no_perspective') else ''
            fhead.append('layout(location = %d) %sin %s i%d;' % (loc, interp, gtype(fs_mod, ty), loc))
            ftr.vals[pname] = Val('v', ty, 'i%d' % loc)
        elif md_has(md, 'air.position'):
            ftr.vals[pname] = Val('v', ty, 'gl_FragCoord')
        elif md_has(md, 'air.buffer'):
            ftr.vals[pname] = Val('arg', ty, buf=int(md_get(md, 'air.location_index')))
        elif md_has(md, 'air.texture'):
            ftr.vals[pname] = Val('tex', ty, metal=int(md_get(md, 'air.location_index')), null=False)
        elif md_has(md, 'air.sampler'):
            ftr.vals[pname] = Val('samp', ty, metal=int(md_get(md, 'air.location_index')))
        elif md_has(md, 'air.render_target'):
            # framebuffer fetch: the driver binds a copy of the colour attachment
            ftr.uses_fbfetch = True
            t = res.tex_slot('fb')
            ftr.vals[pname] = Val('v', ty, 'texelFetch(sampler2D(tex%d, smp%d), ivec2(gl_FragCoord.xy), 0)'
                                  % (t, res.samp_slot(('const', 0x807bff0000080049))))
        elif md_has(md, 'air.front_facing'):
            ftr.vals[pname] = Val('v', ty, 'gl_FrontFacing')
        else:
            raise Unsupported('fragment input %r' % md)
    fty = fs_mod.resolve(fs_mod.ret)
    ffields = fty[1] if fty[0] == 's' else [fty]
    fouts = []
    for md, f in zip(fs_mod.out_md, ffields):
        if md_has(md, 'air.render_target'):
            rt = int(md[md.index('!"air.render_target"') + 1].split()[-1])
            fhead.append('layout(location = %d) out vec4 c%d;' % (rt, rt))
            fouts.append(rt)
        else:
            raise Unsupported('fragment output %r' % md)

    def fret(tr, tok):
        v = tr.operand_typed(tok)
        fields = v.fields if v.kind == 'struct' else [v]
        for rt, f in zip(fouts, fields):
            n = tr.vec_len(f.ty)
            e = f.expr if n == 4 else 'vec4(%s%s)' % (f.expr, ', 0' * (4 - n) if n < 4 else '')
            tr.body.append('c%d = %s;' % (rt, e))

    ftr.run(fret)

    def ubo_decls(tr):
        out = []
        for b, size in sorted(tr.ubos.items()):
            n = min(4096, (size + 15) // 16)
            out.append('layout(std140, set = 0, binding = %d) uniform U%d { uvec4 d[%d]; } u%d;' % (b, b, n, b))
        return out

    for i in range(len(res.tex)):
        fhead.append('layout(set = 0, binding = %d) uniform texture2D tex%d;' % (i, i))
    for i in range(len(res.samp)):
        fhead.append('layout(set = 0, binding = %d) uniform sampler smp%d;' % (i, i))
    vert = '\n'.join(vhead + ubo_decls(vtr) + ['void main() {'] + ['    ' + d for d in vtr.decls] +
                     ['    ' + l for l in vtr.body] + ['}', ''])
    frag = '\n'.join(fhead + ubo_decls(ftr) + ['void main() {'] + ['    ' + d for d in ftr.decls] +
                     ['    ' + l for l in ftr.body] + ['}', ''])
    out['tex'] = res.tex
    out['samp'] = [list(s) for s in res.samp]
    out['vs_ubos'] = sorted(vtr.ubos)
    out['fs_ubos'] = sorted(ftr.ubos)
    out['fbfetch'] = ftr.uses_fbfetch
    return vert, frag, out


def main():
    vs = Module(open(sys.argv[1]).read())
    fs = Module(open(sys.argv[2]).read())
    outdir, name = sys.argv[3], sys.argv[4]
    try:
        vert, frag, info = translate(vs, fs, name)
    except Unsupported as e:
        sys.exit('air2glsl %s: unsupported: %s' % (name, e))
    open('%s/%s.vert' % (outdir, name), 'w').write(vert)
    open('%s/%s.frag' % (outdir, name), 'w').write(frag)
    json.dump(info, open('%s/%s.json' % (outdir, name), 'w'))


if __name__ == '__main__':
    main()

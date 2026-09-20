"""Minimal parser for the Steamworks SDK headers.

The generator cannot use libclang: the guest-facing dylib is built without a
macOS SDK, and the headers are parsed on hosts that only ship a C++ compiler.
The subset below is enough because the SDK headers are machine-regular -
anything it does not understand is reported instead of guessed at.
"""

import os
import re

# Seeded macros: the compiler-provided ones the SDK headers actually test.
# Everything else comes from the headers themselves as they are included, so a
# version that defines e.g. STEAMNETWORKINGSOCKETS_ENABLE_SDR keeps the
# interfaces that macro guards.
BUILTIN_MACROS = {
    "__APPLE__": "1",
    "__x86_64__": "1",
    "POSIX": "1",
    "VALVE_BIG_ENDIAN": "0",
}

# Annotation macros: dropped with their arguments, they carry no ABI meaning.
# Older SDKs spell them without the STEAM_ prefix, and neither spelling is
# always #defined in the headers, so they are stripped by name.
ANNOTATION_MACROS = [
    "ARRAY_COUNT_D", "ARRAY_COUNT", "BUFFER_COUNT", "CALL_BACK", "CALL_RESULT",
    "CLANG_ATTR", "DESC", "FLAT_NAME", "IGNOREATTR", "METHOD_DESC",
    "OUT_ARRAY_CALL", "OUT_ARRAY_COUNT", "OUT_BUFFER_COUNT", "OUT_STRING_COUNT",
    "OUT_STRING", "OUT_STRUCT",
]
ANNOTATION_MACROS = ["STEAM_" + m for m in ANNOTATION_MACROS] + ANNOTATION_MACROS

# Access-control macro: its argument is a run of ordinary virtual methods.
KEEP_ARGS_MACROS = ["STEAM_PRIVATE_API"]

DEFINE_INTERFACE_VERSION = re.compile(
    r'^#define\s*(?P<name>STEAM(?:\w*)_VERSION(?:\w*))\s*"(?P<version>.*)"')

FUNCTION_POINTER = "__fnptr"

UNUSED_HEADER = '#error "This file isn\'t used any more"'


class ParseError(Exception):
    pass


class Method:
    def __init__(self, name, ret, params, is_destructor=False):
        self.name = name
        self.ret = ret
        self.params = params
        self.is_destructor = is_destructor


class Param:
    def __init__(self, type, name):
        self.type = type
        self.name = name


class Interface:
    def __init__(self, name, version, sdkver, header):
        self.name = name
        self.version = version
        self.sdkver = sdkver
        self.header = header
        self.methods = []

    @property
    def full_name(self):
        return f"{self.name}_{self.version}"


def strip_comments(text):
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == '/' and i + 1 < n and text[i + 1] == '/':
            while i < n and text[i] != '\n':
                i += 1
        elif c == '/' and i + 1 < n and text[i + 1] == '*':
            end = text.find('*/', i + 2)
            i = n if end < 0 else end + 2
            out.append(' ')
        elif c in '"\'':
            quote, j = c, i + 1
            while j < n and text[j] != quote:
                j += 2 if text[j] == '\\' else 1
            out.append(text[i:min(j + 1, n)])
            i = j + 1
        else:
            out.append(c)
            i += 1
    return ''.join(out)


def _eval_cond(expr, macros):
    # defined() binds before macro expansion.
    expr = re.sub(r'defined\s*\(\s*(\w+)\s*\)', lambda m: str(int(m[1] in macros)), expr)
    expr = re.sub(r'defined\s+(\w+)', lambda m: str(int(m[1] in macros)), expr)
    for _ in range(4):
        expanded = re.sub(r'\b([A-Za-z_]\w*)\b',
                          lambda m: macros.get(m[1], m[1]) if m[1] != 'defined' else m[1],
                          expr)
        if expanded == expr:
            break
        expr = expanded
    expr = re.sub(r'\b(0[xX][0-9a-fA-F]+|\d+)[uUlL]+\b', r'\1', expr)
    expr = expr.replace('&&', ' and ').replace('||', ' or ')
    expr = re.sub(r'!(?!=)', ' not ', expr)
    # Anything still an identifier is an undefined macro, i.e. zero.
    expr = re.sub(r'\b(?!and\b|or\b|not\b)[A-Za-z_]\w*\b', '0', expr)
    try:
        return bool(eval(expr, {"__builtins__": {}}, {}))
    except Exception:
        raise ParseError(f"cannot evaluate #if {expr!r}")


DIRECTIVE = re.compile(r'^\s*#\s*(\w+)\b(.*)$', re.S)


class Preprocessor:
    """Evaluates directives only: conditionals, #define/#undef and #include.

    Header text is passed through unexpanded - the generator reads
    declarations, not preprocessed tokens.
    """

    def __init__(self, sdkdir, macros):
        self.sdkdir = sdkdir
        self.macros = dict(macros)
        self.out = []
        self.depth = 0

    def include(self, name):
        path = os.path.join(self.sdkdir, name)
        if not os.path.exists(path) or self.depth > 32:
            return
        raw = open(path, errors='replace').read().replace('\r\n', '\n')
        if UNUSED_HEADER in raw:
            return
        self.depth += 1
        self.run(strip_comments(raw))
        self.depth -= 1

    def run(self, text):
        stack = []  # [taken now, any branch taken, parent live]
        text = re.sub(r'\\\n', ' ', text)  # splice continuation lines
        for line in text.split('\n'):
            live = all(s[0] for s in stack)
            m = DIRECTIVE.match(line)
            if not m:
                self.out.append(line if live else '')
                continue
            kind, rest = m[1], m[2].strip()
            self.out.append('')
            if kind in ('if', 'ifdef', 'ifndef'):
                if not live:
                    stack.append([False, True, live])
                elif kind == 'ifdef':
                    val = bool(rest) and rest.split()[0] in self.macros
                    stack.append([val, val, live])
                elif kind == 'ifndef':
                    val = bool(rest) and rest.split()[0] not in self.macros
                    stack.append([val, val, live])
                else:
                    val = _eval_cond(rest, self.macros)
                    stack.append([val, val, live])
            elif kind == 'elif':
                if not stack:
                    raise ParseError("#elif without #if")
                taken = stack[-1][1]
                val = stack[-1][2] and not taken and _eval_cond(rest, self.macros)
                stack[-1][0], stack[-1][1] = val, taken or val
            elif kind == 'else':
                if not stack:
                    raise ParseError("#else without #if")
                stack[-1][0] = stack[-1][2] and not stack[-1][1]
                stack[-1][1] = True
            elif kind == 'endif':
                if not stack:
                    raise ParseError("#endif without #if")
                stack.pop()
            elif not live:
                continue
            elif kind == 'define':
                d = re.match(r'(\w+)\s*(\([^)]*\))?\s*(.*)$', rest)
                if d:
                    self.macros[d[1]] = d[3].strip()
            elif kind == 'undef':
                self.macros.pop(rest.split()[0] if rest else '', None)
            elif kind == 'include':
                local = re.match(r'"([^"]+)"', rest)
                if local:
                    self.include(os.path.basename(local[1]))
        if stack:
            raise ParseError("unterminated #if")


def _match_paren(text, start):
    """Index just past the ')' matching the '(' at `start`."""
    depth, i = 0, start
    while i < len(text):
        if text[i] == '(':
            depth += 1
        elif text[i] == ')':
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    raise ParseError("unbalanced parentheses")


def strip_macro_calls(text, names, keep_args):
    for name in names:
        while True:
            m = re.search(r'\b%s\s*\(' % re.escape(name), text)
            if not m:
                break
            end = _match_paren(text, m.end() - 1)
            args = text[m.end():end - 1] if keep_args else ''
            text = text[:m.start()] + args + text[end:]
    return text


def _split_top_level(text, sep):
    parts, depth, cur = [], 0, []
    for c in text:
        if c in '([{<':
            depth += 1
        elif c in ')]}>':
            depth -= 1
        if c == sep and depth == 0:
            parts.append(''.join(cur))
            cur = []
        else:
            cur.append(c)
    parts.append(''.join(cur))
    return parts


QUALIFIERS = {'const', 'volatile', 'unsigned', 'signed', 'struct', 'class', 'enum'}


def parse_param(text):
    text = text.strip()
    if not text or text == 'void':
        return None
    text = re.sub(r'=\s*[^,]+$', '', text).strip()  # default argument

    fnptr = re.match(r'^(.*)\(\s*(?:\w+\s+)?\*\s*(\w*)\s*\)\s*\(.*\)$', text, re.S)
    if fnptr:
        return Param('void *', fnptr[2] or None)

    array = re.search(r'\[[^\]]*\]\s*$', text)
    if array:
        text = text[:array.start()].strip()

    m = re.match(r'^(.*?)(\b\w+)$', text, re.S)
    if not m or re.search(r'[*&]\s*$', text):
        # An unnamed parameter, e.g. "EChatEntryType *".
        return Param(re.sub(r'\s+', ' ', text).strip() + (' *' if array else ''), None)
    prefix, last = m[1].strip(), m[2]
    words = set(prefix.replace('*', ' ').replace('&', ' ').split())
    if not prefix or (words <= QUALIFIERS and '*' not in prefix and '&' not in prefix):
        type, name = text, None
    else:
        type, name = prefix, last
    if array:
        type += ' *'
    return Param(re.sub(r'\s+', ' ', type).strip(), name)


def parse_interface_body(iface, body):
    body = strip_macro_calls(body, KEEP_ARGS_MACROS, keep_args=True)
    body = strip_macro_calls(body, ANNOTATION_MACROS, keep_args=False)

    chunks, depth, cur = [], 0, []
    i = 0
    while i < len(body):
        c = body[i]
        if c == '{':
            if depth == 0:
                chunks.append(''.join(cur))
                cur = []
                depth = 1
                i += 1
                while i < len(body) and depth:
                    depth += (body[i] == '{') - (body[i] == '}')
                    i += 1
                depth = 0
                continue
        elif c == ';' and depth == 0:
            chunks.append(''.join(cur))
            cur = []
            i += 1
            continue
        elif c in '([':
            depth += 1
        elif c in ')]':
            depth -= 1
        cur.append(c)
        i += 1
    chunks.append(''.join(cur))

    for chunk in chunks:
        chunk = re.sub(r'\b(public|protected|private)\s*:', ' ', chunk)
        chunk = re.sub(r'\s+', ' ', chunk).strip()
        if not chunk.startswith('virtual'):
            if re.search(r'\bvirtual\b', chunk):
                raise ParseError(f"{iface.full_name}: unhandled declaration {chunk!r}")
            continue
        decl = chunk[len('virtual'):].strip()
        decl = re.sub(r'=\s*0$', '', decl).strip()
        decl = re.sub(r'\b(const|override|final)\s*$', '', decl).strip()

        open_paren = decl.find('(')
        if open_paren < 0:
            raise ParseError(f"{iface.full_name}: cannot parse {chunk!r}")
        head = decl[:open_paren].strip()
        params_text = decl[open_paren + 1:decl.rfind(')')]

        if '~' in head:
            # Itanium places two slots (complete and deleting) here.
            iface.methods.append(Method(head[head.index('~'):], 'void', [],
                                        is_destructor=True))
            continue
        m = re.match(r'^(.*?)(\b\w+)$', head, re.S)
        if not m:
            raise ParseError(f"{iface.full_name}: cannot parse {head!r}")
        ret, name = re.sub(r'\s+', ' ', m[1]).strip(), m[2]
        params = [p for p in (parse_param(t) for t in _split_top_level(params_text, ',')) if p]
        iface.methods.append(Method(name, ret, params))


class Sdk:
    def __init__(self, version, path):
        self.version = version
        self.path = path
        self.versions = {}      # STEAMAPPS -> STEAMAPPS_INTERFACE_VERSION008
        self.interfaces = {}    # ISteamApps -> Interface
        self.typedefs = {}
        self.enums = set()
        self.records = set()


def load_sdk(version, path):
    """Preprocess the whole SDK as one translation unit and read it."""
    sdk = Sdk(version, path)

    pp = Preprocessor(path, BUILTIN_MACROS)
    # steam_api.h first so the headers it includes get their usual macro
    # environment; the rest pick up interfaces it does not pull in.
    heads = ['steam_api.h', 'steam_gameserver.h']
    heads += sorted(f for f in os.listdir(path) if f.endswith('.h') and f not in heads)
    for head in heads:
        pp.include(head)
    text = '\n'.join(pp.out)

    for name, value in pp.macros.items():
        m = DEFINE_INTERFACE_VERSION.match(f'#define {name} {value}')
        if m:
            key = m['name'].replace('_INTERFACE_VERSION', '').replace('_VERSION', '')
            sdk.versions[key] = m['version']

    for m in re.finditer(r'\btypedef\s+([^;{}]+?)\s*\(\s*(?:\w+\s+)?\*\s*(\w+)\s*\)\s*\([^;]*?\)\s*;', text):
        sdk.typedefs[m[2]] = FUNCTION_POINTER
    for m in re.finditer(r'\btypedef\s+([^;{}()]+?)\s+(\w+)\s*(\[[^\]]*\])?\s*;', text):
        sdk.typedefs.setdefault(m[2], re.sub(r'\s+', ' ', m[1]).strip())
    for m in re.finditer(r'\benum\s+(?:class\s+)?(\w+)', text):
        sdk.enums.add(m[1])
    for m in re.finditer(r'\b(?:struct|class|union)\s+(\w+)', text):
        sdk.records.add(m[1])
    # Anonymous tags named by a typedef: "typedef enum { ... } EResult;".
    for m in re.finditer(r'\btypedef\s+(enum|struct|class|union)\b[^{;]*\{', text):
        i, depth = m.end(), 1
        while i < len(text) and depth:
            depth += (text[i] == '{') - (text[i] == '}')
            i += 1
        tail = re.match(r'\s*(\w+)\s*;', text[i:])
        if tail:
            (sdk.enums if m[1] == 'enum' else sdk.records).add(tail[1])

    for m in re.finditer(r'\bclass\s+(ISteam\w+)\s*(?::[^{;]*)?\{', text):
        name = m[1]
        key = name[1:].upper()
        i, depth = m.end(), 1
        while i < len(text) and depth:
            depth += (text[i] == '{') - (text[i] == '}')
            i += 1
        if key not in sdk.versions or name in sdk.interfaces:
            continue
        iface = Interface(name, sdk.versions[key], version, name)
        parse_interface_body(iface, text[m.end():i - 1])
        sdk.interfaces[name] = iface
    return sdk


def sdk_sort_key(version):
    m = re.match(r'^(\d+)([a-z]*)$', version)
    if not m:
        raise ParseError(f"unrecognised SDK version {version!r}")
    return (int(m[1]), m[2])


def discover_sdks(root):
    found = []
    for entry in sorted(os.listdir(root)):
        m = re.match(r'^steamworks_sdk_(.+)$', entry)
        if m and os.path.isdir(os.path.join(root, entry)):
            found.append((m[1], os.path.join(root, entry)))
    return sorted(found, key=lambda e: sdk_sort_key(e[0]))

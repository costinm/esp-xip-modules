"""Reading the API.md files of this repository (the format of ssh-mesh and DMesh): `# `name` API (N)` headings,
numbered `## N. `method` — summary` sections, markdown tables for fields, prose in between. Shared by gen_abi.py
(the host ABI) and gen_catalog.py (the methods of main and of the modules)."""
import re


def num(text):
    return int(text.strip("` "), 0)


def tables(lines):
    """Rows of every markdown table in `lines`, each a list of cells without the backticks of one-token cells."""
    rows = []
    for line in lines:
        line = line.strip()
        if not line.startswith("|") or set(line) <= set("|-: "):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        rows.append([c[1:-1] if c.startswith("`") and c.endswith("`") and c.count("`") == 2 else c for c in cells])
    return rows[1:]   # the header row


def prose(lines):
    """The paragraphs of a section: text outside tables, code blocks and **Key:** lines."""
    out, code = [], False
    for line in lines:
        if line.startswith("```"):
            code = not code
            continue
        if code or line.startswith(("|", "**", "###", "<!--")):
            continue
        out.append(line)
    return " ".join(" ".join(out).split())


def sections(text, level):
    """[(heading, [lines])] for the headings of exactly `level` hashes."""
    marker, out, cur = "#" * level + " ", [], None
    for line in text.split("\n"):
        if line.startswith(marker):
            cur = (line[len(marker):].strip(), [])
            out.append(cur)
        elif line.startswith("#") and not line.startswith("#" * (level + 1)) and cur is not None:
            cur = None   # a heading of a shallower level ends the section
        elif cur is not None:
            cur[1].append(line)
    return out


def sections_top(text):
    return sections(text, 1)


def top_text(text, name):
    """The body of the level-1 section `name`, with its level-2 headings kept."""
    for h, body in sections(text, 1):
        if h == name:
            return "\n".join(body)
    raise KeyError(name)


def module_api(text):
    """A module or main API.md: [(component name, id, [(method name, id, summary, request fields, response fields)])].
    A field is (tag, name, type)."""
    out = []
    for heading, body in sections(text, 1):
        m = re.match(r"`(\w+)` API \((\d+)\)", heading)
        if m:
            methods = _methods(body)
            # "### Request (all methods)" / "### Response (all methods)" before the first method: shared by those
            # that have no table of their own
            shared = {"Request": [], "Response": []}
            target, rows = None, []
            for line in body + ["## end"]:
                if line.startswith("## "):
                    if target:
                        shared[target] = [(int(t), n, ty) for t, n, ty, *_ in tables(rows) if t.isdigit()]
                    break
                if line.startswith("### "):
                    if target:
                        shared[target] = [(int(t), n, ty) for t, n, ty, *_ in tables(rows) if t.isdigit()]
                    head = line[4:].strip()
                    target = head.split(" ")[0] if head.endswith("(all methods)") and head.split(" ")[0] in shared else None
                    rows = []
                else:
                    rows.append(line)
            methods = [(n, i, sm, req or shared["Request"], resp or shared["Response"]) for n, i, sm, req, resp in methods]
            out.append((m.group(1), int(m.group(2)), methods))
    return out


def _methods(body):
    methods = []
    cur = None
    for line in body:
        h = re.match(r"## (\d+)\. `(\w+)`(?: — (.*))?$", line)
        if h:
            cur = {"id": int(h.group(1)), "name": h.group(2), "summary": h.group(3) or "", "lines": []}
            methods.append(cur)
        elif cur is not None:
            cur["lines"].append(line)
    result = []
    for m in methods:
        req, resp = [], []
        target = None
        rows = []
        for line in m["lines"] + ["### end"]:
            if line.startswith("### "):
                if target is not None:
                    fields = [(int(t), n, ty) for t, n, ty, *_ in tables(rows) if t.isdigit()]
                    (req if target == "Request" else resp).extend(fields)
                target = line[4:].strip() if line[4:].strip() in ("Request", "Response") else None
                rows = []
            else:
                rows.append(line)
        result.append((m["name"], m["id"], m["summary"], req, resp))
    return result


def module_types(text):
    """The `# Types` section of an API.md: {type name: [(tag, field name, type)]}."""
    out = {}
    try:
        body = top_text(text, "Types")
    except KeyError:
        return out
    for heading, lines in sections(body, 2):
        m = re.match(r"`(\w+)`", heading)
        if m:
            out[m.group(1)] = [(int(t), n, ty) for t, n, ty, *_ in tables(lines) if t.isdigit()]
    return out

#!/usr/bin/env python3
"""Turn the Markdown produced by `cargo doc-md` into a small set of GitHub wiki pages.

    python3 wiki_api.py <doc-md directory> <wiki directory>

`cargo doc-md` writes one file per module and one per type with methods: well over a hundred
files for this workspace, which a flat wiki cannot present. This script folds them into:

    API.md                 the API home: one row per crate, grouped by layer
    _Sidebar.md            the same groups as a navigation sidebar
    API-vauban-types.md    one page per crate: crate doc, table of contents, then every module
                           as a section, item headings demoted and de-duplicated

A crate whose page would exceed `SPLIT_BYTES` is split into one page per top-level module
(`API-vauban-catalog-views.md`), the crate page becoming an index of those.

Links are rewritten: `(#item)` anchors to the GitHub anchor of the folded heading,
`(module.md#x)` to the right page, `(../other_crate/index.md)` to `API-other-crate`.
Standard library only.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

PREFIX = "API"
SPLIT_BYTES = 400_000

# Layers, in the order they appear on the home page and in the sidebar. A crate that is not
# listed lands in "Other".
LAYERS: list[tuple[str, list[str]]] = [
    ("Server", ["vauban-cli", "vauban-session", "vauban-tds", "vauban-compat"]),
    ("Language", ["vauban-parser", "vauban-binder", "vauban-planner", "vauban-executor", "vauban-sysfn"]),
    ("Data", ["vauban-types", "vauban-catalog", "vauban-txn", "vauban-storage", "vauban-errors"]),
]

RE_LINK = re.compile(r"\]\(([^)\s]+)\)")
RE_HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*$")
RE_MODULE_ENTRY = re.compile(r"^### \[`([^`]+)`\]\(([^)]+)\)")


def github_anchor(heading: str) -> str:
    """The anchor GitHub derives from a heading: lower case, punctuation dropped, spaces to `-`."""
    text = heading.strip().replace("`", "")
    text = re.sub(r"[^\w\- ]", "", text.lower())
    return text.replace(" ", "-")


def page_name(crate: str, module: str | None = None) -> str:
    base = f"{PREFIX}-{crate.replace('_', '-')}"
    return f"{base}-{module.replace('_', '-')}" if module else base


def first_sentence(text: str) -> str:
    """First sentence of the first paragraph, without link brackets."""
    paragraph = text.strip().split("\n\n", 1)[0].replace("\n", " ")
    m = re.match(r"(.+?\.)(\s|$)", paragraph)
    return (m.group(1) if m else paragraph).replace("[", "").replace("]", "")


def read_index(crate_dir: Path) -> tuple[str, list[tuple[str, str]]]:
    """Crate doc (before `## Modules`) and the ordered list of (module path, file)."""
    text = (crate_dir / "index.md").read_text(encoding="utf-8")
    head, _, rest = text.partition("\n## Modules")
    doc = "\n".join(head.splitlines()[1:]).strip()  # drop the `# crate` title
    modules = [(m.group(1), m.group(2)) for m in map(RE_MODULE_ENTRY.match, rest.splitlines()) if m]
    return doc, modules


def section_anchor(module: str) -> str:
    title = f"Methods of `{module}`" if module.split("::")[-1][:1].isupper() else f"Module `{module}`"
    return github_anchor(title)


def fold_module(crate: str, module: str, text: str, level: int) -> tuple[str, dict[str, str]]:
    """One module file as a section. Returns the section and its doc-md anchor -> new anchor map."""
    lines = text.splitlines()
    out: list[str] = []
    anchors: dict[str, str] = {}
    skip_contents = False
    for line in lines:
        if line.startswith("**") and line.endswith("**") and " > " in line:
            continue  # breadcrumb
        if line.startswith("# Module:") or line.startswith("# Module "):
            continue  # replaced by our own section heading
        if line.strip() == "## Contents":
            skip_contents = True
            continue
        if skip_contents:
            if line.strip() == "---":
                skip_contents = False
            continue
        m = RE_HEADING.match(line)
        if m:
            hashes, title = m.groups()
            new_level = min(len(hashes) - 1 + level, 6)
            if "::" in title:  # an item: `crate::module::Item`, made unique by its path
                short = title.removeprefix(f"{crate}::")
                out.append(f"{'#' * new_level} `{short}`")
                anchors[github_anchor(title.split("::")[-1])] = github_anchor(f"`{short}`")
            else:  # a section such as Fields, Examples
                out.append(f"{'#' * new_level} {title}")
            continue
        out.append(line)
    # `cargo doc-md` lists the methods of a type as a pseudo-module `module::Type`.
    title = f"Methods of `{module}`" if module.split("::")[-1][:1].isupper() else f"Module `{module}`"
    section = f"{'#' * level} {title}\n\n" + "\n".join(out).strip() + "\n"
    return section, anchors


def rewrite_links(text: str, crate: str, anchors: dict[str, str], own_page: str, pages: dict[str, str]) -> str:
    def repl(m: re.Match[str]) -> str:
        target = m.group(1)
        if "://" in target or "::" in target or target.startswith(PREFIX):
            return m.group(0)  # external, unresolved rustdoc path, or a page this script wrote
        path, _, anchor = target.partition("#")
        if not path:
            return f"](#{anchors.get(anchor, anchor)})"
        parts = [p for p in path.split("/") if p not in ("", ".")]
        while ".." in parts:
            i = parts.index("..")
            del parts[max(i - 1, 0):i + 1]
        if parts and parts[-1].endswith(".md"):
            parts[-1] = parts[-1][:-3]
        if parts and parts[-1] == "index":
            parts.pop()
        other = parts[0] if parts and parts[0] != crate and parts[0] in pages else None
        page = pages.get(other, own_page) if other else own_page
        if not other and parts:
            anchor = anchors.get(anchor, section_anchor("::".join(parts)) if not anchor else anchor)
        return f"]({page}#{anchor})" if anchor else f"]({page})"

    return RE_LINK.sub(repl, text)


def build_crate(crate_dir: Path, pages: dict[str, str]) -> dict[str, str]:
    crate = crate_dir.name
    own = pages[crate]
    doc, modules = read_index(crate_dir)
    sections: list[tuple[str, str, dict[str, str]]] = []
    for module, file in modules:
        text = (crate_dir / file).read_text(encoding="utf-8")
        section, anchors = fold_module(crate, module, text, level=2)
        sections.append((module, section, anchors))
    total = sum(len(s) for _, s, _ in sections) + len(doc)
    result: dict[str, str] = {}
    if total <= SPLIT_BYTES:
        anchors = {k: v for _, _, a in sections for k, v in a.items()}
        toc = "\n".join(f"- [`{m}`](#{section_anchor(m)})" for m, _, _ in sections)
        body = f"# {crate.replace('_', '-')}\n\n{doc}\n\n## Contents\n\n{toc}\n\n" + "\n\n".join(s for _, s, _ in sections)
        result[own] = rewrite_links(body, crate, anchors, own, pages)
        return result
    # Too big: one page per top-level module, the crate page indexes them.
    groups: dict[str, list[tuple[str, str, dict[str, str]]]] = {}
    for module, section, anchors in sections:
        groups.setdefault(module.split("::")[0], []).append((module, section, anchors))
    toc = []
    for top, group in groups.items():
        name = page_name(crate, top)
        anchors = {k: v for _, _, a in group for k, v in a.items()}
        body = f"# {crate.replace('_', '-')} · `{top}`\n\nPart of [{crate.replace('_', '-')}]({own}).\n\n" + "\n\n".join(s for _, s, _ in group)
        result[name] = rewrite_links(body, crate, anchors, name, pages)
        toc.append(f"- [`{top}`]({name}) · {len(group)} module(s)")
    result[own] = rewrite_links(f"# {crate.replace('_', '-')}\n\n{doc}\n\n## Modules\n\n" + "\n".join(toc) + "\n", crate, {}, own, pages)
    return result


def main() -> int:
    if sys.argv[1:] in (["-h"], ["--help"]):
        print(__doc__)
        return 0
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    crates = sorted(p for p in src.iterdir() if p.is_dir() and (p / "index.md").exists())
    pages = {c.name: page_name(c.name) for c in crates}
    docs = {c.name: read_index(c)[0] for c in crates}
    written: dict[str, str] = {}
    for c in crates:
        written.update(build_crate(c, pages))

    def rows(names: list[str]) -> list[str]:
        return [f"| [{n.replace('_', '-')}]({pages[n]}) | {first_sentence(docs[n])} |" for n in names]

    by_layer = []
    listed: set[str] = set()
    for layer, names in LAYERS:
        present = [n.replace("-", "_") for n in names if n.replace("-", "_") in pages]
        listed.update(present)
        if present:
            by_layer.append((layer, present))
    rest = [n for n in pages if n not in listed]
    if rest:
        by_layer.append(("Other", rest))

    home = ["# API", "", "Rust API of the workspace, one page per crate, generated from the source documentation.", ""]
    sidebar = ["**[API](API)**", ""]
    for layer, names in by_layer:
        home += [f"## {layer}", "", "| Crate | Purpose |", "|---|---|", *rows(names), ""]
        sidebar += [f"**{layer}**", "", *[f"- [{n.replace('_', '-')}]({pages[n]})" for n in names], ""]
    written[PREFIX] = "\n".join(home)
    written["_Sidebar"] = "\n".join(sidebar)

    for old in dst.glob(f"{PREFIX}*.md"):
        old.unlink()
    for name, text in written.items():
        (dst / f"{name}.md").write_text(text, encoding="utf-8")
    print(f"{len(written)} page(s) written to {dst}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

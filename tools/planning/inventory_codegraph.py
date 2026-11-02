"""Read-only source surface inventory for architecture review, not app code.

Reads immutable Git blobs, uses a C# syntax tree for attributed declarations,
and records source hashes and exact spans. Declarations are candidates for
semantic review; syntax coverage alone never proves behavioral parity.
"""
import argparse
import hashlib
import json
import re
import subprocess
from collections import Counter
from pathlib import Path

from tree_sitter import Language, Parser
import tree_sitter_c_sharp


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args])


def descendants(node):
    yield node
    for child in node.named_children:
        yield from descendants(child)


def inventory(repo, revision):
    sha = git(repo, "rev-parse", revision).decode().strip()
    parser = Parser(Language(tree_sitter_c_sharp.language()))
    files = git(repo, "ls-tree", "-r", "--name-only", sha).decode().splitlines()
    rows, source_files, errors = [], [], []
    for path in files:
        if not path.startswith("src/") or not path.endswith(".cs"):
            continue
        if any(x in path for x in [".Tests/", "/Fixtures/", "/obj/", "/bin/"]):
            continue
        data = git(repo, "show", f"{sha}:{path}")
        if not any(x in data for x in [b"McpServerTool(", b"McpServerResource(", b"[Http", b".MapGet(", b".MapPost(", b"IJobCommand<", b"IConsumer<", b"ICodeExtractor", b"IBackendEndpointExtractor", b"WorkToolDefinition", b"JobTypes"]) and not (path.startswith("src/CodeGraph.Extractors.") or path.startswith("src/CodeGraph.Services/Pipeline/") or path.endswith(("/CargoManifestExtractor.cs", "/SwiftPackageManifestExtractor.cs", "/GraphAssistant.ToolDefinitions.cs"))):
            continue
        tree = parser.parse(data)
        constants = dict(re.findall(r'const\s+string\s+(\w+)\s*=\s*"([^"\n]+)"', data.decode()))
        def text(node):
            return data[node.start_byte:node.end_byte].decode() if node else ""
        relevant = []
        for node in descendants(tree.root_node):
            if node.type == "method_declaration":
                attrs = [x for c in node.named_children if c.type == "attribute_list" for x in descendants(c) if x.type == "attribute"]
                tags = [text(x) for x in attrs]
                mcp = next((x for x in tags if x.startswith("McpServerTool(")), None)
                resource = next((x for x in tags if x.startswith("McpServerResource(")), None)
                http = [x for x in tags if re.match(r"Http(Get|Post|Put|Delete|Patch|Head|Options)\b", x)]
                if not mcp and not http and not resource:
                    continue
                parent = node.parent
                while parent and parent.type not in ["class_declaration", "record_declaration"]:
                    parent = parent.parent
                method = text(node.child_by_field_name("name"))
                owner = text(parent.child_by_field_name("name")) if parent else ""
                route = []
                if parent:
                    route = [text(x) for c in parent.named_children if c.type == "attribute_list" for x in descendants(c) if x.type == "attribute" and text(x).startswith("Route(")]
                desc = next((x for x in tags if x.startswith("Description(")), "")
                body = node.child_by_field_name("body")
                calls = sorted(set(text(x.child_by_field_name("function")) for x in descendants(body or node) if x.type == "invocation_expression"))
                extra = {"owner": owner, "method": method, "declaration": text(node)[:text(node).find("{")].strip(), "description": desc, "calls": calls}
                if mcp:
                    match = re.search(r'Name\s*=\s*"([^"]+)"', mcp)
                    constant = re.search(r'Name\s*=\s*(\w+)', mcp)
                    name = match[1] if match else constants.get(constant[1]) if constant else None
                    if not name:
                        errors.append(f"Unresolved MCP name {path}:{node.start_point.row + 1}")
                        continue
                    operations = re.findall(r'Description\("Operation: ([^"\n]+)', text(node))
                    relevant.append(("mcp", name, node, {**extra, "attribute": mcp, "operationDescriptions": operations}))
                if resource:
                    match = re.search(r'UriTemplate\s*=\s*"([^"]+)"', resource)
                    if not match:
                        errors.append(f"Unresolved resource {path}:{node.start_point.row + 1}")
                    else:
                        relevant.append(("resource", match[1], node, {**extra, "attribute": resource}))
                for tag in http:
                    relevant.append(("http", f"{owner}.{method}:{tag}", node, {**extra, "routePrefix": route, "attribute": tag}))
            elif node.type == "class_declaration":
                bases = next((x for x in node.named_children if x.type == "base_list"), None)
                base_text = text(bases)
                if "IJobCommand<" in base_text:
                    relevant.append(("job", text(node.child_by_field_name("name")), node, {"bases": base_text, "calls": sorted(set(text(x.child_by_field_name("function")) for x in descendants(node) if x.type == "invocation_expression"))}))
                elif re.search(r"\b(ICodeExtractor|IBackendEndpointExtractor)\b", base_text):
                    relevant.append(("extractor", text(node.child_by_field_name("name")), node, {"bases": base_text}))
                elif "IConsumer<" in base_text:
                    relevant.append(("consumer", text(node.child_by_field_name("name")), node, {"bases": base_text, "calls": sorted(set(text(x.child_by_field_name("function")) for x in descendants(node) if x.type == "invocation_expression"))}))
                elif (path.startswith("src/CodeGraph.Extractors.") and not text(node.child_by_field_name("name")).endswith(("Exception", "Options"))) or path.endswith(("/CargoManifestExtractor.cs", "/SwiftPackageManifestExtractor.cs")):
                    relevant.append(("semantic_adapter", text(node.child_by_field_name("name")), node, {"bases": base_text}))
                elif path.startswith("src/CodeGraph.Services/Pipeline/") and text(node.child_by_field_name("name")) in ["IndexingPipeline", "CrossRepoLinker"]:
                    relevant.append(("index_stage", text(node.child_by_field_name("name")) + ":" + path.split("/")[-1], node, {"methods": [text(x.child_by_field_name("name")) for x in node.named_children[-1].named_children if x.type == "method_declaration"]}))
            elif node.type == "invocation_expression":
                fn = text(node.child_by_field_name("function"))
                if re.search(r"\.Map(Get|Post|Put|Delete|Patch|Methods)$", fn):
                    relevant.append(("minimal_http", fn + ":" + text(node.child_by_field_name("arguments"))[:100], node, {}))
            elif node.type == "implicit_object_creation_expression" and path.endswith("/WorkTools.cs"):
                args = next((x for x in node.named_children if x.type == "argument_list"), None)
                values = [text(x) for x in args.named_children] if args else []
                if values and values[0] in constants:
                    relevant.append(("work_tool", constants[values[0]], node, {"definition": text(node), "argumentType": values[2] if len(values) > 2 else ""}))
            elif node.type == "implicit_object_creation_expression" and path.endswith("/GraphAssistant.ToolDefinitions.cs"):
                args = next((x for x in node.named_children if x.type == "argument_list"), None)
                values = [text(x) for x in args.named_children] if args else []
                if values and re.fullmatch(r'"[a-z_]+"',values[0]):
                    relevant.append(("assistant_tool", values[0][1:-1], node, {"definition":text(node)}))
        if relevant:
            source_files.append({"path": path, "sha256": hashlib.sha256(data).hexdigest(), "syntaxErrors": tree.root_node.has_error})
        for kind, name, node, extra in relevant:
            row = {"id": f"{kind}:{path}:{name}", "kind": kind, "name": name, "source": {"path": path, "startLine": node.start_point.row + 1, "endLine": node.end_point.row + 1, "spanSha256": hashlib.sha256(data[node.start_byte:node.end_byte]).hexdigest()}, **extra}
            rows.append(row)
    routes_path = "CodeGraphWeb/src/app/app.routes.ts"
    data = git(repo, "show", f"{sha}:{routes_path}")
    source_files.append({"path": routes_path, "sha256": hashlib.sha256(data).hexdigest(), "parser": "bounded path string scan; route nesting/component refs reviewed separately"})
    for match in re.finditer(rb"path:\s*'([^']*)'", data):
        line = data[:match.start()].count(b"\n") + 1
        rows.append({"id": f"ui:{routes_path}:{line}", "kind": "ui", "name": match[1].decode(), "source": {"path": routes_path, "startLine": line, "endLine": line}})
    return {"title": "CodeGraph source surface candidates", "status": "Static declaration inventory; semantic mapping and runtime parity are separate", "sourceRevision": sha, "sourceFiles": source_files, "counts": dict(Counter(r["kind"] for r in rows)), "errors": errors, "entries": rows}


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repository", type=Path, required=True)
    ap.add_argument("--revision", required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    result = inventory(args.repository, args.revision)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"revision": result["sourceRevision"], "files": len(result["sourceFiles"]), "counts": result["counts"], "errors": result["errors"]}))

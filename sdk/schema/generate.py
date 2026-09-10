#!/usr/bin/env python3
"""Generate JSON Schema from the serde structs used by the thin HTTP clients.

No extra dependency enters polis-core. This intentionally small Rust/serde
reader rejects unsupported fields/attributes, rather than fabricating a shape.
Run --check in CI when contracts change. Schemas describe serde JSON bodies;
HTTP GET parameters use the explicit query mapping beside each endpoint.
"""
import argparse
import copy
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = Path(__file__).with_name("api-v1.schema.json")
ROOTS = ["ForgetRequest", "ForgetReceipt", "Scope", "SearchRequest", "AnswerPack", "ContextRequest", "ContextBlock", "IngestRequest", "IngestReceipt", "RememberRequest", "WriteReceipt", "ClaimWrite", "ClaimQuery", "Claim", "EvidenceRequest", "EvidenceRecord", "TraceRequest", "RetrievalTrace", "DecisionRequest"]


def split_fields(body):
    depth = 0
    start = 0
    quoted = False
    for index, ch in enumerate(body):
        if ch == '"' and (index == 0 or body[index - 1] != "\\"):
            quoted = not quoted
        if quoted:
            continue
        if ch in "<([{": depth += 1
        elif ch in ">)]}": depth -= 1
        elif ch == "," and depth == 0:
            if body[start:index].strip(): yield body[start:index].strip()
            start = index + 1
    if body[start:].strip(): yield body[start:].strip()


def wire_name(name, attrs, container):
    explicit = re.search(r'\brename\s*=\s*"([^"]+)"', attrs)
    if explicit: return explicit[1]
    rename = re.search(r'rename_all\s*=\s*"([^"]+)"', container)
    policy = rename[1] if rename else None
    if policy == "camelCase":
        parts = name.split("_")
        return parts[0] + "".join(p.title() for p in parts[1:])
    if policy == "snake_case":
        return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()
    if policy is not None: raise ValueError(f"unsupported rename policy {policy}")
    return name


def generate():
    definitions = {}
    sources = {}
    for path in sorted((ROOT / "crates/polis-core/src").glob("*.rs")):
        # Public contracts contain line comments, no block comments within fields.
        source = re.sub(r"(?m)^\s*//.*$", "", path.read_text())
        source = re.sub(r"(?m)(?<=,)\s*//[^\n]*", "", source)
        for match in re.finditer(r"((?:\s*#\[[^\]]*\])*)\s*pub (struct|enum) (\w+)\s*\{([^{}]*)\}", source):
            attrs, kind, name, body = match.groups()
            if "Serialize" in attrs:
                sources[name] = (attrs, kind, body)

    def resolve_type(typ):
        typ = typ.strip()
        if typ in {"String", "&str"}: return {"type": "string"}
        if typ == "bool": return {"type": "boolean"}
        if typ in {"f32", "f64"}: return {"type": "number"}
        if re.fullmatch(r"[ui](8|16|32|64|128|size)", typ):
            return {"type": "integer", **({"minimum": 0} if typ.startswith("u") else {})}
        if typ in {"Value", "serde_json::Value"}: return {}
        for prefix in ("Option", "Vec"):
            if typ.startswith(prefix + "<") and typ.endswith(">"):
                inner = resolve_type(typ[len(prefix) + 1:-1])
                return {"anyOf": [inner, {"type": "null"}]} if prefix == "Option" else {"type": "array", "items": inner}
        map_match = re.fullmatch(r"(?:std::collections::)?(?:BTreeMap|HashMap)<String,\s*(.+)>", typ)
        if map_match: return {"type": "object", "additionalProperties": resolve_type(map_match[1])}
        name = typ.split("::")[-1]
        resolve(name)
        return {"$ref": f"#/$defs/{name}"}

    def resolve(name):
        if name in definitions: return
        if name not in sources: raise ValueError(f"unsupported or undiscovered type: {name}")
        attrs, kind, body = sources[name]
        definitions[name] = {}  # break potential reference cycles
        if kind == "enum":
            variants = []
            for item in split_fields(body):
                field_attrs = " ".join(re.findall(r"#\[([^]]+)\]", item))
                variant = re.sub(r"#\[[^]]+\]", "", item).strip()
                tagged = re.search(r'tag\s*=\s*"([^"]+)"', attrs)
                content = re.search(r'content\s*=\s*"([^"]+)"', attrs)
                payload = re.fullmatch(r"(\w+)\((.+)\)", variant)
                if payload and tagged and content:
                    variants.append({"type": "object", "properties": {
                        tagged[1]: {"const": wire_name(payload[1], field_attrs, attrs)},
                        content[1]: resolve_type(payload[2])}, "required": [tagged[1], content[1]]})
                elif re.fullmatch(r"\w+", variant) and not tagged:
                    variants.append(wire_name(variant, field_attrs, attrs))
                else: raise ValueError(f"unsupported enum representation: {name}")
            definitions[name] = {"oneOf": variants} if tagged else {"type": "string", "enum": variants}
            return
        properties, required = {}, []
        for item in split_fields(body):
            field_attrs = " ".join(re.findall(r"#\[([^]]+)\]", item))
            cleaned = re.sub(r"#\[[^]]+\]", "", item).strip()
            field = re.fullmatch(r"pub\s+(\w+)\s*:\s*(.+)", cleaned, re.S)
            if not field: raise ValueError(f"unsupported field in {name}: {cleaned}")
            rust_name, typ = field.groups()
            # Supported serde subset; fail loudly if a custom serializer appears.
            if re.search(r"\b(with|serialize_with|deserialize_with|untagged|tag|content|skip)\b", field_attrs):
                raise ValueError(f"unsupported serde attribute in {name}.{rust_name}: {field_attrs}")
            if "flatten" in field_attrs:
                nested = typ.split("::")[-1].strip()
                resolve(nested)
                obj = definitions[nested]
                if obj.get("type") != "object": raise ValueError("flatten requires an object")
                properties.update(copy.deepcopy(obj["properties"]))
                required.extend(obj.get("required", []))
                continue
            key = wire_name(rust_name, field_attrs, attrs)
            properties[key] = resolve_type(typ)
            if not typ.startswith("Option<") and not re.search(r"\bdefault\b", attrs + " " + field_attrs):
                required.append(key)
        definitions[name] = {"type": "object", "properties": properties}
        if required: definitions[name]["required"] = required

    for root in ROOTS: resolve(root)
    return {"$schema": "https://json-schema.org/draft/2020-12/schema", "$id": "urn:polis:api:v1",
            "title": "Polis SDK HTTP contracts (generated from serde source)", "$defs": dict(sorted(definitions.items())),
            "x-endpoints": {
                "POST /v1/memory/decisions": {"request": "DecisionRequest", "response": "WriteReceipt"},
                "POST /v1/memory/events": {"request": "IngestRequest", "response": "IngestReceipt"},
                "POST /v1/memory/claims": {"request": "ClaimWrite", "response": "Claim"},
                "GET /v1/memory/claims": {"request": "ClaimQuery", "response": {"type": "object", "properties": {"claims": {"type": "array", "items": {"$ref": "#/$defs/Claim"}}}}},
                "POST /v1/memory/remember": {"request": "RememberRequest", "response": "WriteReceipt"},
                "GET /v1/memory/context": {"request": "ContextRequest", "response": "ContextBlock"},
                "GET /v1/memory/evidence/:seq": {"request": "EvidenceRequest", "response": "EvidenceRecord"},
                "GET /v1/memory/traces": {"request": "TraceRequest", "response": {"type": "object", "properties": {"traces": {"type": "array", "items": {"$ref": "#/$defs/RetrievalTrace"}}}}},
                "GET /v1/memory/answer-pack": {"request": "SearchRequest", "response": "AnswerPack"}},
            "x-http-error": {"type": "object", "properties": {"error": {"type": "string"}}, "required": ["error"]},
            "x-query-mapping": {"scope": ["principal", "project", "agent", "run", "org", "include_shared"],
                "filter": ["roles", "after", "before", "valid_at", "known_at"], "maxTokens": "max_tokens", "maxBytes": "max_bytes", "traceId": "trace_id", "candidateLimit": "candidate_limit"}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    schema = generate()
    text = json.dumps(schema, indent=2, sort_keys=True) + "\n"
    server_output = ROOT / "crates/polis-server/src/api-v1.schema.json"
    if args.check:
        if any(not p.exists() or p.read_text() != text for p in (OUTPUT, server_output)):
            raise SystemExit("SDK schema is stale; run python3 sdk/schema/generate.py")
    else:
        OUTPUT.write_text(text)
        server_output.write_text(text)

#!/usr/bin/env python3
"""Regenerate the embedded model catalog from models.dev data.

Source: https://models.dev/api.json — the published aggregate of the
models.dev repository (https://github.com/anomalyco/models.dev).
The generated file is committed so builds stay offline-friendly.

Usage:
    python3 scripts/build_catalog.py                  # fetch latest api.json
    python3 scripts/build_catalog.py --input api.json # use a local copy
"""

import argparse
import json
import pathlib
import urllib.request

DEFAULT_NPM = {
    "@ai-sdk/openai": ("openai", "https://api.openai.com/v1"),
    "@ai-sdk/anthropic": ("anthropic", "https://api.anthropic.com/v1"),
    "@ai-sdk/google": ("google", "https://generativelanguage.googleapis.com/v1beta"),
    "@ai-sdk/xai": ("openai", "https://api.x.ai/v1"),
    "@ai-sdk/groq": ("openai", "https://api.groq.com/openai/v1"),
    "@ai-sdk/mistral": ("openai", "https://api.mistral.ai/v1"),
    "@ai-sdk/cerebras": ("openai", "https://api.cerebras.ai/v1"),
    "@ai-sdk/togetherai": ("openai", "https://api.together.xyz/v1"),
    "@ai-sdk/deepinfra": ("openai", "https://api.deepinfra.com/v1/openai"),
    "@ai-sdk/perplexity": ("openai", "https://api.perplexity.ai"),
    "@ai-sdk/cohere": ("openai", "https://api.cohere.com/compatibility/v1"),
}

# SDKs that need account-scoped setup Halis cannot derive from a base URL.
UNSUPPORTED_NPM = {
    "@ai-sdk/azure",
    "@ai-sdk/amazon-bedrock",
    "@ai-sdk/amazon-bedrock/mantle",
    "@ai-sdk/google-vertex",
    "@ai-sdk/google-vertex/anthropic",
}


def protocol_for(npm: str, api):
    if npm in UNSUPPORTED_NPM:
        return "", ""
    if npm in DEFAULT_NPM:
        return DEFAULT_NPM[npm]
    if api and "${" not in api:
        return "openai", api.rstrip("/")
    return "", ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", help="path to a downloaded api.json")
    ap.add_argument("--url", default="https://models.dev/api.json")
    ap.add_argument("--out", default="crates/halis-core/src/catalog/data.json")
    args = ap.parse_args()

    if args.input:
        raw = pathlib.Path(args.input).read_bytes()
    else:
        raw = urllib.request.urlopen(args.url, timeout=90).read()
    data = json.loads(raw)

    providers = []
    models = []
    for pid in sorted(data.keys()):
        p = data[pid]
        chat = [
            (mid, m)
            for mid, m in (p.get("models") or {}).items()
            if (m.get("type") or "chat") == "chat"
        ]
        if not chat:
            continue
        npm_default = p.get("npm") or ""
        env = p.get("env") or []
        p_protocol, p_base = protocol_for(npm_default, p.get("api"))
        idx = len(providers)
        providers.append(
            {
                "id": pid,
                "name": p.get("name") or pid,
                "env": env,
                "protocol": p_protocol,
                "base": p_base,
            }
        )
        for mid, m in sorted(chat, key=lambda x: x[0]):
            mprov = m.get("provider") or {}
            npm = mprov.get("npm") or npm_default
            api = mprov.get("api") or p.get("api")
            protocol, base = protocol_for(npm, api)
            flags = 0
            if m.get("tool_call"):
                flags |= 1
            if "image" in (m.get("modalities") or {}).get("input", []):
                flags |= 2
            if m.get("reasoning"):
                flags |= 4
            if m.get("status") == "deprecated":
                flags |= 8
            limit = m.get("limit") or {}
            cost = m.get("cost") or {}
            entry = {
                "p": idx,
                "id": mid,
                "n": m.get("name") or mid,
                "f": flags,
                "r": int(str(m.get("release_date") or "").replace("-", "") or 0),
            }
            if limit.get("context"):
                entry["cx"] = limit["context"]
            if limit.get("output"):
                entry["mo"] = limit["output"]
            if cost.get("input") is not None:
                entry["pi"] = cost["input"]
            if cost.get("output") is not None:
                entry["po"] = cost["output"]
            if protocol != p_protocol:
                entry["pp"] = protocol
            if protocol and base != p_base:
                entry["pb"] = base
            models.append(entry)

    out = {"providers": providers, "models": models}
    dest = pathlib.Path(args.out)
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(json.dumps(out, separators=(",", ":"), ensure_ascii=False))

    usable = sum(1 for e in models if e.get("pp", providers[e["p"]]["protocol"]))
    print(f"providers with chat models: {len(providers)}")
    print(f"chat models: {len(models)} (callable: {usable})")
    print(f"wrote {dest} ({dest.stat().st_size} bytes)")


if __name__ == "__main__":
    main()

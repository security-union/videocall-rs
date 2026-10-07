"""Pseudonymisation of participant identities in score results (doc section 9.5, decision T8)."""

import hashlib
import hmac
import re

from cq_prom import promql_string, re2_escape

IDENTITY_LABELS = ("from_peer", "peer_id", "customer_email")
IDENTITY_KEYED = ("participants", "audio_loss_share", "stability", "transport", "zero_packet_receivers", "talker_A")
IDENTITY_TEXT = ("detail", "flags", "max_staleness_pair", "max_freeze_pair", "excluded_reporters",
                 "rust_bot_reporters")
MIN_IDENTITY_LEN = 3


def identities_to_hide(manifest, datasets, real):
    """Real meetings: every identity. Runs: humans and identities outside the manifest; bot ids stay."""
    seen = {labels[k] for data in datasets.values() for series in data.values()
            for labels, _ in series for k in IDENTITY_LABELS if labels.get(k)}
    parts = [p for p in manifest["participants"] if p["user_id"] is not None]
    if real:
        return seen | {p["user_id"] for p in parts}
    bots = {p["user_id"] for p in parts if p["fleet"] != "human"}
    return (seen | {p["user_id"] for p in parts if p["fleet"] == "human"}) - bots


def raw_manifest_identities(manifest):
    """Every non-bot user_id string in a manifest that may not have passed validation yet."""
    parts = manifest.get("participants") if isinstance(manifest, dict) else None
    return {p["user_id"] for p in parts or [] if isinstance(p, dict) and isinstance(p.get("user_id"), str)
            and p.get("fleet") not in ("browser", "rust")}


def pseudonym(identity, salt):
    return "p-" + hmac.new(salt, identity.encode(), hashlib.sha256).hexdigest()[:10]


def text_pseudonymiser(identities, salt):
    """Replaces each identity, including its escaped form inside a PromQL regex matcher."""
    mapping = {}
    for i in identities:
        if len(i) >= MIN_IDENTITY_LEN:
            mapping[i] = mapping[promql_string(re2_escape(i))[1:-1]] = pseudonym(i, salt)
    if not mapping:
        return lambda text: text
    names = sorted(mapping, key=len, reverse=True)
    rx = re.compile(r"(?<![\w.@+-])(?:" + "|".join(map(re.escape, names)) + r")(?![\w.@+-])")
    return lambda text: rx.sub(lambda m: mapping[m.group(0)], text)


def pseudonymise(obj, identities, salt):
    """Copy of obj with identities replaced, only in the fields that carry them."""
    sub = text_pseudonymiser(identities, salt)

    def walk(x, keyed=False, text=False):
        if isinstance(x, str):
            return sub(x) if text else x
        if isinstance(x, dict):
            return {(sub(k) if keyed else k): walk(v, k in IDENTITY_KEYED, text or k in IDENTITY_TEXT)
                    for k, v in x.items()}
        if isinstance(x, list):
            return [walk(v, False, text) for v in x]
        return x

    return walk(obj)

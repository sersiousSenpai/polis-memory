"""One final-context token ceiling, applied after every adapter renders.

Keyless mode uses a conservative UTF-8 byte ceiling and labels it as such.
Scored OpenAI runs require the answer model's actual tiktoken encoding; no
silent four-characters-per-token estimate is accepted as a measurement.
"""
class ContextTokenizer:
    def __init__(self, provider, model, stub=False):
        self.encoding = None
        self.name = "utf8-byte-upper-bound (keyless only)"
        if not stub:
            if provider != "openai":
                raise ValueError("Scored contexts require a verified tokenizer; Anthropic adapter pending")
            import tiktoken
            self.encoding = tiktoken.encoding_for_model(model)
            self.name = f"tiktoken:{self.encoding.name}"

    def count(self, text):
        return len(self.encoding.encode(text, disallowed_special=())) if self.encoding else len(text.encode("utf-8"))

    def trim(self, text, ceiling):
        if ceiling < 1:
            raise ValueError("context ceiling must be positive")
        if self.encoding:
            out = self.encoding.decode(self.encoding.encode(text, disallowed_special=())[:ceiling])
            # A sliced token may decode to replacement bytes; verify final rendering.
            while self.count(out) > ceiling:
                out = out[:-1]
            return out
        return text.encode("utf-8")[:ceiling].decode("utf-8", errors="ignore")

# Homebrew

`polis-memory.rb` here is the formula cargo-dist generates for a release —
this copy was produced by a local `dist build --artifacts=global` at 0.1.0
and carries placeholder URLs/sha256 until a tag builds the real artifacts.
The release workflow writes the real formula into the tap repository
`sersiousSenpai/homebrew-tap` (the `publish-homebrew-formula` job needs that
repository to exist and a `HOMEBREW_TAP_TOKEN` secret with contents:write on
it — both outward steps, `docs/distribution.md`). Users then run:

```sh
brew install sersiousSenpai/tap/polis-memory
```

Homebrew core (`brew install polis-memory`) is the plan's target after 30
days of tap releases.

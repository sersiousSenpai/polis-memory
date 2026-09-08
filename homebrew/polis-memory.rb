class PolisMemory < Formula
  desc "Polis Memory — the crate integrators add: the Polis handle implementing MemoryApi over the store, retrieval (the answer pack, timeline, map), the autonomous gardener (organize, compaction, observations, the semantic index), bundle export and the markdown mirror. Re-exports polis-core, polis-store, polis-llm and polis-embed."
  homepage "https://github.com/sersiousSenpai/polis-memory"
  version "0.1.0"
  if OS.mac?
    if Hardware::CPU.arm?
      url "https://github.com/sersiousSenpai/polis-memory/releases/download/v0.1.0/polis-memory-aarch64-apple-darwin.tar.xz"
    end
    if Hardware::CPU.intel?
      url "https://github.com/sersiousSenpai/polis-memory/releases/download/v0.1.0/polis-memory-x86_64-apple-darwin.tar.xz"
    end
  end
  if OS.linux?
    if Hardware::CPU.arm?
      url "https://github.com/sersiousSenpai/polis-memory/releases/download/v0.1.0/polis-memory-aarch64-unknown-linux-gnu.tar.xz"
    end
    if Hardware::CPU.intel?
      url "https://github.com/sersiousSenpai/polis-memory/releases/download/v0.1.0/polis-memory-x86_64-unknown-linux-gnu.tar.xz"
    end
  end
  license "Apache-2.0"

  BINARY_ALIASES = {
    "aarch64-apple-darwin": {},
    "aarch64-unknown-linux-gnu": {},
    "x86_64-apple-darwin": {},
    "x86_64-pc-windows-gnu": {},
    "x86_64-unknown-linux-gnu": {}
  }

  def target_triple
    cpu = Hardware::CPU.arm? ? "aarch64" : "x86_64"
    os = OS.mac? ? "apple-darwin" : "unknown-linux-gnu"

    "#{cpu}-#{os}"
  end

  def install_binary_aliases!
    BINARY_ALIASES[target_triple.to_sym].each do |source, dests|
      dests.each do |dest|
        bin.install_symlink bin/source.to_s => dest
      end
    end
  end

  def install
    if OS.mac? && Hardware::CPU.arm?
      bin.install "polis"
    end
    if OS.mac? && Hardware::CPU.intel?
      bin.install "polis"
    end
    if OS.linux? && Hardware::CPU.arm?
      bin.install "polis"
    end
    if OS.linux? && Hardware::CPU.intel?
      bin.install "polis"
    end

    install_binary_aliases!

    # Homebrew will automatically install these, so we don't need to do that
    doc_files = Dir["README.*", "readme.*", "LICENSE", "LICENSE.*", "CHANGELOG.*"]
    leftover_contents = Dir["*"] - doc_files

    # Install any leftover files in pkgshare; these are probably config or
    # sample files.
    pkgshare.install(*leftover_contents) unless leftover_contents.empty?
  end
end

# Homebrew binary formula for the modelstat daemon (plan D9/§8). Installs BOTH
# static binaries — the collector `modelstat` + the summariser engine
# `modelstat-summarizer` — from the GitHub Release. Lives in the tap repo
# (modelstat/homebrew-tap) as `Formula/modelstat.rb`; the version + sha256 lines
# are rewritten on every release by the `agent-released` listener workflow
# (bump-formula.yml). The values below are a placeholder pinned to a real tag.
class Modelstat < Formula
  desc "Local AI-usage collector + on-device summariser engine"
  homepage "https://modelstat.ai"
  version "0.0.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/modelstat/modelstat/releases/download/daemon-#{version}/modelstat-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/modelstat/modelstat/releases/download/daemon-#{version}/modelstat-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/modelstat/modelstat/releases/download/daemon-#{version}/modelstat-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/modelstat/modelstat/releases/download/daemon-#{version}/modelstat-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "modelstat", "modelstat-summarizer"
  end

  def caveats
    <<~EOS
      Pair this device and start the background collector with:
        modelstat
      Choose where sessions are summarised (cloud / on-device / your org's engine):
        modelstat mode
    EOS
  end

  test do
    assert_match "daemon-", shell_output("#{bin}/modelstat --version")
    assert_match "summarizer-", shell_output("#{bin}/modelstat-summarizer --version")
  end
end

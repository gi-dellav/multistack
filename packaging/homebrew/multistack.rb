class Multistack < Formula
  desc "Open source lightweight TUI for parallel agent management"
  homepage "https://github.com/gi-dellav/multistack"
  version "1.0.2"
  license "GPL-3.0-only"

  depends_on "zerostack"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.2/multistack-x86_64-apple-darwin.tar.gz"
      sha256 "066f92615560c57b07a41d199ef72d5d0a1ecf9b94df8ed44b836cec109a2133"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.2/multistack-aarch64-apple-darwin.tar.gz"
      sha256 "0019dfc4b32d63c1392aa264aed2253c1e0c2fb09216f8e2cc269bbfb8bb49b5"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.2/multistack-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0019dfc4b32d63c1392aa264aed2253c1e0c2fb09216f8e2cc269bbfb8bb49b5"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.2/multistack-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0019dfc4b32d63c1392aa264aed2253c1e0c2fb09216f8e2cc269bbfb8bb49b5"
    end
  end

  def install
    bin.install Dir["multistack*"].first => "multistack"
  end

  test do
    assert_match(/^multistack /, shell_output("#{bin}/multistack --version"))
  end
end

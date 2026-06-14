class Multistack < Formula
  desc "Open source lightweight TUI for parallel agent management"
  homepage "https://github.com/gi-dellav/multistack"
  version "1.0.0-rc6"
  license "GPL-3.0-only"

  depends_on "zerostack"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0-rc6/multistack-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0-rc6/multistack-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0-rc6/multistack-x86_64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0-rc6/multistack-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install Dir["multistack*"].first => "multistack"
  end

  test do
    assert_match(/^multistack /, shell_output("#{bin}/multistack --version"))
  end
end

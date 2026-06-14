class Multistack < Formula
  desc "Open source lightweight TUI for parallel agent management"
  homepage "https://github.com/gi-dellav/multistack"
  version "1.0.0"
  license "GPL-3.0-only"

  depends_on "zerostack"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0/multistack-x86_64-apple-darwin.tar.gz"
      sha256 "35b66a71bd22b79622d77ce3d60e3b49e31662d10e3d3f0d8b320017a5025b58"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0/multistack-aarch64-apple-darwin.tar.gz"
      sha256 "04ed7521de1b33c4a548c5e2f1da98196746b265a88b4235700ffd47648ab77c"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0/multistack-x86_64-unknown-linux-musl.tar.gz"
      sha256 "7719594297026a725bd57db7a640e2b2b84d51b4980225b6992a2d4d44fa3092"
    else
      url "https://github.com/gi-dellav/multistack/releases/download/v1.0.0/multistack-aarch64-unknown-linux-musl.tar.gz"
      sha256 "fa4a346450d57463d01e474e63cd862cf7834cb475722692a09f5a88b2ba1b5c"
    end
  end

  def install
    bin.install Dir["multistack*"].first => "multistack"
  end

  test do
    assert_match(/^multistack /, shell_output("#{bin}/multistack --version"))
  end
end

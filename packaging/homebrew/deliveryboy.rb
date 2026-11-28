class Deliveryboy < Formula
  desc "Release CLI driven by your repository config"
  homepage "https://deliveryboy.app/"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/SerenitySoftware/deliveryboy/releases/download/v0.4.0/deliveryboy-0.4.0-aarch64-apple-darwin.tar.gz"
      sha256 "d2ba5b9e00b8871bc64a266599c7ce6a6536369989f6247d64bf016876800b8d"
    end

    on_intel do
      url "https://github.com/SerenitySoftware/deliveryboy/releases/download/v0.4.0/deliveryboy-0.4.0-x86_64-apple-darwin.tar.gz"
      sha256 "15735535c13b58a9e5dd72a6de07e4ebd8d1968a30a0306b0a9535a178a5f550"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/SerenitySoftware/deliveryboy/releases/download/v0.4.0/deliveryboy-0.4.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "98d816227964d0cafd1d10cde2f83dab9f3516dbdf776e41f3384ee35ce35600"
    end

    on_intel do
      url "https://github.com/SerenitySoftware/deliveryboy/releases/download/v0.4.0/deliveryboy-0.4.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "12a4ef04f1c275246829b2f09c73d5bcef7b026b82a8b1dbaddee5639b8ce9fd"
    end
  end

  def install
    bin.install "deliver"
  end

  test do
    assert_equal "deliver #{version}", shell_output("#{bin}/deliver --version").strip
  end
end

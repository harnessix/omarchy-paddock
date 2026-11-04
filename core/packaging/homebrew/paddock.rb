# Homebrew formula for paddock. It builds from the source tarball of a
# release. The release workflow fills in @VERSION@, @SHA256@, @REPO@ and the
# @VM_*@ values of the guest files, and attaches the result to the release
# as paddock.rb. Copy that file into a tap.
class Paddock < Formula
  desc "Run any coding-agent harness with full auto-approval inside an isolated sandbox"
  homepage "https://github.com/@REPO@"
  url "https://github.com/@REPO@/releases/download/v@VERSION@/paddock-@VERSION@.tar.gz"
  sha256 "@SHA256@"
  license "MIT"

  depends_on "rust" => :build

  # The guest files of the VM backend. The release workflow takes the URLs
  # and sha256 values from packaging/vm/assets.toml.
  on_macos do
    on_arm do
      resource "vm-kernel" do
        url "@VM_KERNEL_URL@"
        sha256 "@VM_KERNEL_SHA256@"
      end

      resource "vm-rootfs" do
        url "@VM_ROOTFS_URL@"
        sha256 "@VM_ROOTFS_SHA256@"
      end
    end
  end

  def install
    # paddock update and paddock doctor read the releases of this repository.
    ENV["PADDOCK_RELEASE_REPO"] = "@REPO@"
    system "cargo", "install", *std_cargo_args
    return unless OS.mac?

    # paddock-vm owns the Linux VM of the VM backend. The Virtualization
    # framework needs the virtualization entitlement, and an ad hoc
    # signature carries it. Without it, a VM start fails.
    system "codesign", "--force", "--sign", "-",
           "--entitlements", "packaging/macos/paddock-vm.entitlements",
           bin/"paddock-vm"
    return unless Hardware::CPU.arm?

    # paddock finds the guest files here when `paddock vm setup` did not
    # install its own copy.
    (pkgshare/"vm").install "packaging/vm/assets.toml"
    # A staged file keeps the last part of its URL: kernel, rootfs.img.
    resource("vm-kernel").stage { (pkgshare/"vm").install "kernel" }
    resource("vm-rootfs").stage { (pkgshare/"vm").install "rootfs.img" }
  end

  test do
    assert_match "paddock #{version}", shell_output("#{bin}/paddock --version")
    if OS.mac?
      assert_match "com.apple.security.virtualization",
                   shell_output("codesign -d --entitlements - #{bin}/paddock-vm 2>&1")
    end
  end
end

# The Homebrew tap's Formula/lsync.rb. In the repo this is a template:
# pkg/fill.sh fills in the version and the checksums from a release's
# SHA256SUMS (pkg/README.md).
class Lsync < Formula
  desc "Native Self-hosted LiveSync client that keeps Obsidian vault folders in sync"
  homepage "https://github.com/nytafar/livesync-native"
  version "@VERSION@"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    url "https://github.com/nytafar/livesync-native/releases/download/v@VERSION@/lsync-@VERSION@-universal-apple-darwin.tar.gz"
    sha256 "@SHA256_universal-apple-darwin@"
  end

  on_linux do
    on_intel do
      url "https://github.com/nytafar/livesync-native/releases/download/v@VERSION@/lsync-@VERSION@-x86_64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_x86_64-unknown-linux-musl@"
    end
    on_arm do
      url "https://github.com/nytafar/livesync-native/releases/download/v@VERSION@/lsync-@VERSION@-aarch64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_aarch64-unknown-linux-musl@"
    end
  end

  def install
    bin.install "lsync"
    doc.install "README.md"
  end

  # The same semantics as the LaunchAgent `lsync service install` writes:
  # started at load, restarted only when it exits with an error, not a
  # Background process (which throttles I/O and timers). Use this or
  # `lsync service install`, not both: the second service is refused.
  service do
    run [opt_bin/"lsync", "serve"]
    run_at_load true
    keep_alive successful_exit: false
    throttle_interval 10
    process_type :standard
    log_path var/"log/lsync.log"
    error_log_path var/"log/lsync.log"
  end

  def caveats
    <<~EOS
      Set up an account and a vault, then start the service:
        lsync account add home --url https://couch.example.net --user me
        lsync vault add notes --db vault --folder ~/Notes
        brew services start lsync

      The service's log is #{var}/log/lsync.log.
      Run either `brew services start lsync` or `lsync service install`, not both.
      A vault under ~/Documents, ~/Desktop, ~/Downloads or iCloud Drive needs
      Full Disk Access for #{opt_bin}/lsync.
    EOS
  end

  test do
    assert_match "lsync #{version}", shell_output("#{bin}/lsync --version")
    assert_match "account", shell_output("#{bin}/lsync --help")
  end
end

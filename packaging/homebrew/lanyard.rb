# Homebrew cask for Lanyard.
#
# To publish: create a tap repository (github.com/justin06lee/homebrew-tap),
# copy this file to Casks/lanyard.rb there, and fill in the sha256 of the
# released DMG (`shasum -a 256 Lanyard_<version>_aarch64.dmg`). Users then:
#
#   brew tap justin06lee/tap
#   brew install --cask lanyard
cask "lanyard" do
  version "0.2.1"
  sha256 "REPLACE_WITH_DMG_SHA256"

  url "https://github.com/justin06lee/lanyard/releases/download/v#{version}/Lanyard_#{version}_aarch64.dmg"
  name "Lanyard"
  desc "Floating name tags for your Claude Code sessions"
  homepage "https://github.com/justin06lee/lanyard"

  depends_on macos: ">= :big_sur"

  app "Lanyard.app"

  caveats <<~EOS
    Lanyard needs Accessibility access to see which window has focus:
    System Settings › Privacy & Security › Accessibility
  EOS
end

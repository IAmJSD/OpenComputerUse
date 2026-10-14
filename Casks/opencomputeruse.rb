cask "opencomputeruse" do
  version "0.8.0"
  sha256 "89b8142dbde217f61d02a4f0c998012e6799e377dc1aca9ac84e96a10b0863d7"

  url "https://github.com/IAmJSD/OpenComputerUse/releases/download/v#{version}/OpenComputerUse.zip"
  name "OpenComputerUse"
  desc "Background computer use for agents, as an MCP server"
  homepage "https://github.com/IAmJSD/OpenComputerUse"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  depends_on macos: :sonoma

  app "OpenComputerUse.app"
  binary "#{appdir}/OpenComputerUse.app/Contents/MacOS/opencomputeruse"

  zap trash: "~/Library/Application Support/OpenComputerUse"
end

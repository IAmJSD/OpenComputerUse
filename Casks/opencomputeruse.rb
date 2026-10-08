cask "opencomputeruse" do
  version "0.3.0"
  sha256 "f5e2c31a9546f8f3fa9e0fc92c983a621fdf2a6cda8f7a0fe52e64913adbafb1"

  url "https://github.com/IAmJSD/OpenComputerUse/releases/download/v#{version}/OpenComputerUse.zip"
  name "OpenComputerUse"
  desc "Background computer use for agents, as an MCP server"
  homepage "https://github.com/IAmJSD/OpenComputerUse"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true

  app "OpenComputerUse.app"
  binary "#{appdir}/OpenComputerUse.app/Contents/MacOS/opencomputeruse"

  zap trash: "~/Library/Application Support/OpenComputerUse"
end

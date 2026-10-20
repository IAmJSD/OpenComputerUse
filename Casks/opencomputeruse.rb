cask "opencomputeruse" do
  version "0.11.1"
  sha256 "dc991921bfe902e5f65b4835b4cf399e1acc20def2a0d7dc6bcec8e3d64a97ec"

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

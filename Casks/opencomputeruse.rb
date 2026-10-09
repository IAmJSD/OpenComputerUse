cask "opencomputeruse" do
  version "0.6.0"
  sha256 "9be7081a3b5118e116bcef8724ca52b60ff8b4b6f2a393fb441a1ac5de070124"

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
